//! `service`：让 agent 自己在运行时加/删第三方服务。
//!
//! 分工：**主人只放 key 文件**；剩下的（写配置、连一次、试调、改参数）由 agent 做。
//! 写进 `services.d/<name>.toml`，下次启动自动加载；本次立即注册，不用重启。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

use crate::mcp::{McpHub, McpServer};
use crate::services::{HttpServiceTool, Service};

pub struct ServiceTool {
    dir: PathBuf,
    /// 凭据只允许放在这个目录下（默认 `<config>/secrets`）
    secrets_root: PathBuf,
    hub: Arc<McpHub>,
    http: Arc<HttpServiceTool>,
    proxy: crate::proxy::ProxyConfig,
}

impl ServiceTool {
    pub fn new(
        dir: impl Into<PathBuf>,
        secrets_root: impl Into<PathBuf>,
        hub: Arc<McpHub>,
        http: Arc<HttpServiceTool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            dir: dir.into(),
            secrets_root: secrets_root.into(),
            hub,
            http,
            proxy: Default::default(),
        })
    }

    pub fn with_proxy(mut self: Arc<Self>, proxy: crate::proxy::ProxyConfig) -> Arc<Self> {
        // 先解包再重建，避免 Arc::get_mut 失败
        let _ = &self;
        Arc::new(Self {
            dir: self.dir.clone(),
            secrets_root: self.secrets_root.clone(),
            hub: Arc::clone(&self.hub),
            http: Arc::clone(&self.http),
            proxy,
        })
    }

    /// 凭据路径规则：必须是宿主机绝对路径（允许 `~/`），且落在 secrets 目录内。
    ///
    /// 这条限制挡的是"把 ~/.ssh/id_rsa 当 token 发给远端"这种路径——
    /// 一旦允许任意文件，模型只要猜对路径就能把私钥塞进 Authorization 头里带走。
    fn token_path(&self, raw: &str) -> Result<PathBuf, String> {
        if raw.contains("..") {
            return Err("凭据路径不允许包含 ..".into());
        }
        let expanded = expand(raw);
        if !expanded.is_absolute() {
            return Err(format!(
                "凭据必须用宿主机绝对路径或 ~/ 开头：{raw}（例如 ~/.config/chuan/secrets/xxx_token）"
            ));
        }
        let canon = expanded.canonicalize().map_err(|e| {
            let hint = match e.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    "：运行用户读不到。secrets 目录需要可进入（0750 root:chuan），\
                     文件需要可读（0440 root:chuan）"
                }
                std::io::ErrorKind::NotFound => "：文件不存在，检查路径与文件名",
                _ => "",
            };
            format!("凭据文件不可用：{raw}（{e}）{hint}")
        })?;
        let root = self
            .secrets_root
            .canonicalize()
            .unwrap_or_else(|_| self.secrets_root.clone());
        if !canon.starts_with(&root) {
            return Err(format!(
                "凭据只能放在 {} 下（收到 {raw}）",
                root.display()
            ));
        }
        if !canon.is_file() {
            return Err(format!("不是普通文件：{raw}"));
        }
        Ok(canon)
    }

    fn fragment(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.toml"))
    }

    fn list(&self) -> String {
        let mut out = String::new();
        let mcp = self.hub.names();
        if mcp.is_empty() {
            out.push_str("MCP 服务：无\n");
        } else {
            out.push_str(&format!("MCP 服务：{}\n", mcp.join("、")));
        }
        let http = self.http.names();
        if http.is_empty() {
            out.push_str("HTTP 服务：无\n");
        } else {
            out.push_str(&format!("HTTP 服务：{}\n", http.join("、")));
        }
        out.push_str(&format!("配置文件目录：{}", self.dir.display()));
        out
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && !s.starts_with(['.', '-', '_'])
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// 把"落盘结果"翻译成人话：成功给出路径，失败**明确说重启会丢**。
fn save_line(r: &Result<PathBuf, String>) -> String {
    match r {
        Ok(p) => format!("配置已写入 {}", p.display()),
        Err(e) => format!(
            "⚠️ 已在本进程生效，但**写入配置失败，重启后会丢失**：{e}\n\
             → 让主人在宿主上执行：sudo install -d -m 0770 -o root -g chuan <配置目录>/services.d"
        ),
    }
}

/// 写服务片段时的 HTTP 专属字段（MCP 用不到，传默认值）。
#[derive(Default)]
struct HttpExtra<'a> {
    token_key: Option<&'a str>,
    query: Option<&'a std::collections::BTreeMap<String, String>>,
    body: Option<&'a std::collections::BTreeMap<String, String>>,
    sign: Option<&'a crate::auth::SignConfig>,
}

/// 用 JSON 的转义规则写 TOML 基本字符串：两者的 \\ \" \n 语义一致。
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn inline_table(m: &std::collections::BTreeMap<String, String>) -> String {
    let parts: Vec<String> = m.iter().map(|(k, v)| format!("{k} = {}", q(v))).collect();
    format!("{{ {} }}", parts.join(", "))
}

fn write_fragment(
    dir: &Path,
    name: &str,
    kind: &str,
    base: &str,
    token_file: Option<&str>,
    header: Option<&str>,
    tools: &[String],
    catalog: Option<&str>,
    extra: HttpExtra<'_>,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut s = String::new();
    if kind == "mcp" {
        s.push_str("kind = \"mcp\"\n");
    }
    s.push_str(&format!("base = {}\n", q(base)));
    if let Some(t) = token_file {
        s.push_str(&format!("token_file = {}\n", q(t)));
    }
    if let Some(h) = header {
        s.push_str(&format!("header = {}\n", q(h)));
    }
    if let Some(k) = extra.token_key {
        s.push_str(&format!("token_key = {}\n", q(k)));
    }
    if let Some(m) = extra.query {
        if !m.is_empty() {
            s.push_str(&format!("query = {}\n", inline_table(m)));
        }
    }
    if let Some(m) = extra.body {
        if !m.is_empty() {
            s.push_str(&format!("body = {}\n", inline_table(m)));
        }
    }
    if let Some(sign) = extra.sign {
        s.push_str("\n[sign]\n");
        if let Some(k) = &sign.secret_key {
            s.push_str(&format!("secret_key = {}\n", q(k)));
        }
        s.push_str(&format!("canonical = {}\n", q(&sign.canonical)));
        let enc = match sign.encode {
            crate::auth::Encode::Base64 => "base64",
            crate::auth::Encode::Base64Url => "base64url",
            crate::auth::Encode::Hex => "hex",
        };
        s.push_str(&format!("encode = \"{enc}\"\n"));
    }
    if !tools.is_empty() {
        let list: Vec<String> = tools.iter().map(|t| format!("\"{t}\"")).collect();
        s.push_str(&format!("tools = [{}]\n", list.join(", ")));
    }
    if let Some(c) = catalog {
        s.push_str(&format!("catalog = \"{c}\"\n"));
    }
    let path = dir.join(format!("{name}.toml"));
    std::fs::write(&path, s).map_err(|e| e.to_string())?;
    Ok(path)
}

#[async_trait]
impl Tool for ServiceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "service".into(),
            description: "在运行时添加/删除第三方服务（`kind=http|mcp`）。\
主人只负责把 key 放进凭据文件；**写配置、连接、试调由你来做**。\
加完立刻可用（不用重启），并写入 services.d/ 供下次启动加载。\
加 MCP 服务会当场握手，失败会返回原因，按原因改参数再 add 一次即可。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "add", "remove"] },
                    "name": { "type": "string", "description": "服务名（小写字母/数字/-/_）" },
                    "kind": { "type": "string", "enum": ["http", "mcp"], "description": "默认 http" },
                    "base": { "type": "string", "description": "服务根地址，如 https://mcp.dida365.com" },
                    "token_file": { "type": "string", "description": "凭据文件路径（主人放好的）" },
                    "header": {
                        "type": "string",
                        "description": "鉴权头模板，默认 `Authorization: Bearer {token}`；传空串表示不放 header"
                    },
                    "token_key": { "type": "string", "description": "凭据文件里用哪个键（key = value 形式）" },
                    "query": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "查询参数注入，值可用 {token} {sign} {ts_s} {ts_ms}，例如 {access_token=\"{token}\"}"
                    },
                    "body": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "请求体字段注入（飞书加签放这里），值可用同样的模板变量"
                    },
                    "sign": {
                        "type": "object",
                        "properties": {
                            "secret_key": { "type": "string", "description": "secret 在凭据文件里的键名；默认取第 2 行" },
                            "canonical": { "type": "string", "description": "待签串模板，钉钉 {ts_ms}\\n{secret}，飞书 {ts_s}\\n{secret}" },
                            "encode": { "type": "string", "enum": ["base64", "base64url", "hex"] }
                        },
                        "description": "HMAC-SHA256 签名（webhook 加签）"
                    },
                    "tools": {
                        "type": "array", "items": { "type": "string" },
                        "description": "MCP：只暴露这些工具（可选）"
                    },
                    "catalog": { "type": "string", "enum": ["names", "full", "summary"] }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::External
    }

    fn domain(&self) -> harness_kernel::Domain {
        // 改配置 + 建立对外连接：属于"改自身"
        harness_kernel::Domain::SelfModify
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 action", false))?;

        match action {
            "list" => Ok(ToolOutput {
                content: vec![Block::Paragraph(self.list())],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            }),
            "remove" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 name", false))?;
                let file = self.fragment(name);
                let _ = std::fs::remove_file(&file);
                let m = self.hub.remove(name);
                let h = self.http.remove(name);
                Ok(ToolOutput {
                    content: vec![Block::Paragraph(format!(
                        "{name}：配置已删除（mcp={m}, http={h}）。重启后也不会再加载。"
                    ))],
                    doc_writes: Vec::new(),
                    artifacts: Vec::new(),
                })
            }
            "add" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 name", false))?;
                if !valid_name(&name) {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        "服务名只能用小写字母/数字/-/_（≤32，且不以 . - _ 开头）",
                        false,
                    ));
                }
                let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("http");
                let base = args
                    .get("base")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 base", false))?
                    .to_owned();
                if !base.starts_with("https://") && !base.starts_with("http://") {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        "base 必须以 https:// 或 http:// 开头",
                        false,
                    ));
                }
                let token_file = args.get("token_file").and_then(|v| v.as_str()).map(str::to_owned);
                if let Some(t) = &token_file {
                    self.token_path(t)
                        .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
                }
                let header = args.get("header").and_then(|v| v.as_str()).map(str::to_owned);
                let token_key =
                    args.get("token_key").and_then(|v| v.as_str()).map(str::to_owned);
                let map_of = |key: &str| -> std::collections::BTreeMap<String, String> {
                    args.get(key)
                        .and_then(|v| v.as_object())
                        .map(|o| {
                            o.iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let query = map_of("query");
                let body = map_of("body");
                let sign = args.get("sign").and_then(|v| v.as_object()).map(|o| {
                    crate::auth::SignConfig {
                        secret_key: o
                            .get("secret_key")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                        canonical: o
                            .get("canonical")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{ts_ms}\\n{secret}")
                            .to_owned(),
                        encode: crate::auth::Encode::parse(
                            o.get("encode").and_then(|v| v.as_str()).unwrap_or("base64"),
                        ),
                    }
                });
                let tools: Vec<String> = args
                    .get("tools")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|t| t.as_str()).map(str::to_owned).collect())
                    .unwrap_or_default();
                let catalog = args.get("catalog").and_then(|v| v.as_str()).map(str::to_owned);

                match kind {
                    "mcp" => {
                        let server = McpServer::new(
                            name.clone(),
                            base.clone(),
                            token_file.as_ref().map(|t| self.token_path(t)).transpose().map_err(
                                |e| ToolError::new(ErrorCode::InvalidArgs, e, false),
                            )?,
                            header.clone().unwrap_or_else(|| {
                                "Authorization: Bearer {token}".into()
                            }),
                            vec![],
                            tools.clone(),
                            catalog.clone().unwrap_or_else(|| "names".into()),
                            match (self.proxy.services, self.proxy.reqwest()) {
                                (crate::proxy::Mode::Always, Some(p)) => Some(p),
                                _ => None,
                            },
                        )
                        .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
                        // 先连一次：连不上就不写配置，避免把坏配置留在磁盘上
                        let count = self.hub.upsert(server).await.map_err(|e| {
                            ToolError::new(
                                ErrorCode::Unavailable,
                                format!("连接失败，配置未写入：{e}"),
                                false,
                            )
                        })?;
                        let saved = write_fragment(
                            &self.dir,
                            &name,
                            "mcp",
                            &base,
                            token_file.as_deref(),
                            header.as_deref(),
                            &tools,
                            catalog.as_deref(),
                            HttpExtra::default(),
                        );
                        Ok(ToolOutput {
                            content: vec![Block::Paragraph(format!(
                                "已接入 MCP 服务 `{name}`：握手成功，{count} 个工具可用。\n{}
现在就能用：`mcp(server=\"{name}\", tool=\"__list__\")` 看清单。",
                                save_line(&saved)
                            ))],
                            doc_writes: Vec::new(),
                            artifacts: Vec::new(),
                        })
                    }
                    _ => {
                        let svc = Service {
                            name: name.clone(),
                            base: base.clone(),
                            token_file: token_file.as_ref().map(|t| self.token_path(t)).transpose().map_err(
                                |e| ToolError::new(ErrorCode::InvalidArgs, e, false),
                            )?,
                            header: header.clone().unwrap_or_else(|| {
                                "Authorization: Bearer {token}".into()
                            }),
                            extra_headers: Vec::new(),
                            token_key: token_key.clone(),
                            query: query.clone(),
                            body: body.clone(),
                            sign: sign.clone(),
                        };
                        // 探活：至少证明 DNS/TLS/端点能通（不看状态码，API 常对 HEAD/根路径回 404）
                        let probe = self.http.probe(&svc).await;
                        self.http.upsert(Service {
                            name: name.clone(),
                            base: base.clone(),
                            ..svc
                        });
                        let saved = write_fragment(
                            &self.dir,
                            &name,
                            "http",
                            &base,
                            token_file.as_deref(),
                            header.as_deref(),
                            &[],
                            None,
                            HttpExtra {
                                token_key: token_key.as_deref(),
                                query: Some(&query),
                                body: Some(&body),
                                sign: sign.as_ref(),
                            },
                        );
                        Ok(ToolOutput {
                            content: vec![Block::Paragraph(format!(
                                "已接入 HTTP 服务 `{name}`：{probe}；{}\n调用：`http(service=\"{name}\", method=\"GET\", path=\"/…\")`\n\
                                 如果接口文档还没写，把 API 说明整理成 playbook，下次就能直接查。",
                                save_line(&saved)
                            ))],
                            doc_writes: Vec::new(),
                            artifacts: Vec::new(),
                        })
                    }
                }
            }
            other => Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("未知 action：{other}"),
                false,
            )),
        }
    }
}

/// `~/...` 展开
pub fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        return PathBuf::from(format!(
            "{}/{rest}",
            std::env::var("HOME").unwrap_or_default()
        ));
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated() {
        assert!(valid_name("dida"));
        assert!(valid_name("my-svc_2"));
        assert!(!valid_name("Dida"));
        assert!(!valid_name(".hidden"));
        assert!(!valid_name("../x"));
    }

    #[test]
    fn fragment_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_fragment(
            dir.path(),
            "dida",
            "mcp",
            "https://mcp.dida365.com",
            Some("~/.config/chuan/secrets/dida_token"),
            None,
            &["create_task".to_string()],
            Some("names"),
            HttpExtra::default(),
        )
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("kind = \"mcp\""));
        assert!(text.contains("base = \"https://mcp.dida365.com\""));
        assert!(text.contains("tools = [\"create_task\"]"));
    }
}
