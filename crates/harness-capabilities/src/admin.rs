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
    /// OAuth helper 写的 `access_token` 落在 `<state>/oauth/<provider>/`；
    /// 它是第二个合法凭据来源（helper 拥有写权限，核心只读）。
    oauth_root: Option<PathBuf>,
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
            oauth_root: None,
            hub,
            http,
            proxy: Default::default(),
        })
    }

    /// 允许把 OAuth helper 写出的 `<state>/oauth/**` 当 `token_file`。
    pub fn with_oauth_root(mut self: Arc<Self>, oauth_root: impl Into<PathBuf>) -> Arc<Self> {
        let _ = &self;
        Arc::new(Self {
            dir: self.dir.clone(),
            secrets_root: self.secrets_root.clone(),
            oauth_root: Some(oauth_root.into()),
            hub: Arc::clone(&self.hub),
            http: Arc::clone(&self.http),
            proxy: self.proxy.clone(),
        })
    }

    pub fn with_proxy(mut self: Arc<Self>, proxy: crate::proxy::ProxyConfig) -> Arc<Self> {
        // 先解包再重建，避免 Arc::get_mut 失败
        let _ = &self;
        Arc::new(Self {
            dir: self.dir.clone(),
            secrets_root: self.secrets_root.clone(),
            oauth_root: self.oauth_root.clone(),
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
        let oauth = self
            .oauth_root
            .as_ref()
            .map(|r| r.canonicalize().unwrap_or_else(|_| r.clone()));
        let allowed = canon.starts_with(&root)
            || oauth.as_ref().map(|r| canon.starts_with(r)).unwrap_or(false);
        if !allowed {
            let extra = oauth
                .as_ref()
                .map(|r| format!("，或 OAuth 助手写入的 {} 下", r.display()))
                .unwrap_or_default();
            return Err(format!("凭据只能放在 {} 下{extra}（收到 {raw}）", root.display()));
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
            let listed: Vec<String> = http
                .iter()
                .map(|n| match self.http.get(n) {
                    Some(s) if s.expose_to_skills => format!("{n}（技能可用）"),
                    _ => n.clone(),
                })
                .collect();
            out.push_str(&format!("HTTP 服务：{}\n", listed.join("、")));
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
    extra_headers: Vec<(String, String)>,
    /// 技能网关授权（D41）。`None` = 不写；`Some(false)` 也显式写出来，
    /// 防止同名旧片段残留"已开放"的状态。
    expose_to_skills: Option<bool>,
    allow_methods: Vec<String>,
    allow_paths: Vec<String>,
    header_allow: std::collections::BTreeMap<String, Vec<String>>,
}

/// 技能网关相关参数的解析与校验（`service` 工具与审批卡共用一套）。
type GatewayArgs = (
    bool,
    Vec<String>,
    Vec<String>,
    std::collections::BTreeMap<String, Vec<String>>,
);

fn parse_gateway_args(args: &serde_json::Value) -> Result<GatewayArgs, String> {
    use std::collections::BTreeMap;
    let expose = args.get("expose_to_skills").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut methods: Vec<String> = args
        .get("allow_methods")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.as_str())
                .map(|m| m.trim().to_ascii_uppercase())
                .filter(|m| !m.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let paths: Vec<String> = args
        .get("allow_paths")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str())
                .map(|p| p.trim().to_owned())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let header_allow: BTreeMap<String, Vec<String>> = args
        .get("header_allow")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| {
                    let vals: Vec<String> = v
                        .as_array()?
                        .iter()
                        .filter_map(|x| x.as_str())
                        .map(|x| x.trim().to_owned())
                        .filter(|x| !x.is_empty())
                        .collect();
                    if vals.is_empty() {
                        None
                    } else {
                        Some((k.trim().to_owned(), vals))
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    if !expose {
        if !methods.is_empty() || !paths.is_empty() || !header_allow.is_empty() {
            return Err(
                "allow_methods / allow_paths / header_allow 只在 expose_to_skills = true 时有效；\
                 不想开放就别写它们（别留一份看起来开放的清单）"
                    .into(),
            );
        }
        return Ok((false, methods, paths, header_allow));
    }

    methods.sort();
    methods.dedup();
    if methods.is_empty() || paths.is_empty() {
        return Err("expose_to_skills = true 时必须同时给出非空的 allow_methods 与 allow_paths".into());
    }
    for m in &methods {
        if !matches!(m.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err(format!("allow_methods 里有不支持的方法：{m}"));
        }
    }
    for p in &paths {
        if !p.starts_with('/') || p.contains("://") || p.contains("..") || p.contains(char::is_whitespace) {
            return Err(format!("allow_paths 里的 `{p}` 不合法：必须以 / 开头，且不含协议、.. 或空白"));
        }
    }
    for name in header_allow.keys() {
        let l = name.trim().to_ascii_lowercase();
        if matches!(
            l.as_str(),
            "authorization"
                | "proxy-authorization"
                | "cookie"
                | "set-cookie"
                | "host"
                | "content-length"
                | "transfer-encoding"
                | "connection"
        ) {
            return Err(format!("header_allow 里不允许 `{name}`：鉴权与长度由宿主决定"));
        }
    }
    Ok((true, methods, paths, header_allow))
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
    if !extra.extra_headers.is_empty() {
        let pairs: Vec<String> = extra
            .extra_headers
            .iter()
            .map(|(k, v)| format!("[{}, {}]", q(k), q(v)))
            .collect();
        s.push_str(&format!("extra_headers = [{}]\n", pairs.join(", ")));
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
    // 顶层标量/数组必须在任何 `[table]` 之前写完，否则会被 TOML 归到上一个表里。
    if let Some(expose) = extra.expose_to_skills {
        s.push_str(&format!(
            "expose_to_skills = {}\n",
            if expose { "true" } else { "false" }
        ));
        if expose {
            let list = |v: &[String]| {
                v.iter().map(|x| q(x)).collect::<Vec<_>>().join(", ")
            };
            s.push_str(&format!("allow_methods = [{}]\n", list(&extra.allow_methods)));
            s.push_str(&format!("allow_paths = [{}]\n", list(&extra.allow_paths)));
        }
    }
    if !tools.is_empty() {
        let list: Vec<String> = tools.iter().map(|t| format!("\"{t}\"")).collect();
        s.push_str(&format!("tools = [{}]\n", list.join(", ")));
    }
    if let Some(c) = catalog {
        s.push_str(&format!("catalog = \"{c}\"\n"));
    }
    if !extra.header_allow.is_empty() {
        s.push_str("\n[header_allow]\n");
        for (k, vals) in &extra.header_allow {
            let list = vals.iter().map(|v| q(v)).collect::<Vec<_>>().join(", ");
            s.push_str(&format!("{k} = [{list}]\n"));
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
    let path = dir.join(format!("{name}.toml"));
    // 覆盖前备份（保留 5 版），写入走原子替换：断电不留半截 TOML
    crate::selfmod::backup_file(&path, &dir.join(crate::selfmod::BACKUP_DIR))?;
    crate::selfmod::write_atomic(&path, s.as_bytes())?;
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
要让**技能脚本**也能调用它，先取 `playbook(\"service-gateway\")` 按步骤做：\
加 `expose_to_skills = true` 与 `allow_methods` / `allow_paths` / `header_allow`\
（技能看不到凭据，只能发清单内的请求；审批卡会列出整份清单）。\
加 MCP 服务会当场握手，失败会返回原因，按原因改参数再 add 一次即可。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "_approval": {
                        "type": "object",
                        "description": "审批卡上给非程序员看的解释；改自身配置一定会触发审批，所以必填",
                        "properties": {
                            "why": { "type": "string", "description": "一句人话：接哪个服务、为什么需要" },
                            "impact": { "type": "string", "description": "影响面：只读 / 写文件 / 装软件 / 向设备发指令 / 改自身配置" }
                        },
                        "required": ["why", "impact"]
                    },
                    "action": { "type": "string", "enum": ["list", "add", "remove", "rollback"] },
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
                    "extra_headers": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "固定请求头（如 Content-Type），字面值、不做模板替换。鉴权头请用 header"
                    },
                    "expose_to_skills": {
                        "type": "boolean",
                        "description": "是否允许技能经宿主网关调用（默认 false）。打开后技能只能发 allow_methods / allow_paths / header_allow 允许的请求；凭据仍由宿主注入，技能看不到"
                    },
                    "allow_methods": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "技能可用的 HTTP 方法（GET/POST/PUT/PATCH/DELETE）。expose_to_skills=true 时必须非空"
                    },
                    "allow_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "技能可用的路径（以 / 结尾按前缀匹配，否则精确匹配），如 [\"/JNAP/\"]"
                    },
                    "header_allow": {
                        "type": "object",
                        "additionalProperties": { "type": "array", "items": { "type": "string" } },
                        "description": "技能可设置的请求头及允许取值（名字不区分大小写，取值精确匹配），如 {\"X-JNAP-Action\": [\"GetDeviceInfo\"]}"
                    },
                    "tools": {
                        "type": "array", "items": { "type": "string" },
                        "description": "MCP：只暴露这些工具（可选）"
                    },
                    "catalog": { "type": "string", "enum": ["names", "full", "summary"] }
                },
                "required": ["action", "_approval"]
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

    /// 审批卡必须让人看清"批准之后技能能打到什么"（D41）：
    /// 目的地、方法、路径、可设置的头（动作）、凭据文件都写出来。
    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let action = args.get("action")?.as_str()?;
        if action == "remove" {
            let name = args.get("name")?.as_str()?;
            return Some(format!(
                "删除服务 `{name}`：技能对这个服务的入口会一并收回，之后的请求会直接失败"
            ));
        }
        if action != "add" {
            return None;
        }
        let name = args.get("name")?.as_str()?;
        let base = args.get("base")?.as_str()?;
        let mut out = format!("接入服务 `{name}` → {base}\n");
        if let Some(t) = args.get("token_file").and_then(|v| v.as_str()) {
            out.push_str(&format!("凭据文件：{t}\n"));
        }
        if let Some(h) = args.get("header").and_then(|v| v.as_str()) {
            if !h.trim().is_empty() {
                out.push_str(&format!("鉴权头模板：{h}\n"));
            }
        }
        match parse_gateway_args(args) {
            Err(e) => out.push_str(&format!("\n⚠️ 技能网关参数不合法（执行会失败）：{e}")),
            Ok((false, _, _, _)) => {
                out.push_str("\n只有模型能用 http 工具调用；技能拿不到入口。")
            }
            Ok((true, methods, paths, header_allow)) => {
                let svc = crate::services::Service {
                    name: name.to_owned(),
                    base: base.to_owned(),
                    expose_to_skills: true,
                    allow_methods: methods,
                    allow_paths: paths,
                    header_allow,
                    ..Default::default()
                };
                out.push_str("\n批准后，技能可以调用它（且仅限下面这些）：\n");
                out.push_str(&svc.skill_grant());
                out.push_str("\n\n生效后写入 services.d/，重启不丢；删掉服务即收回。");
            }
        }
        Some(out)
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
                // 软删除：进回收站，说清楚怎么捞回来
                let moved = crate::selfmod::trash(&file, &self.dir.join(crate::selfmod::TRASH_DIR))
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
                let m = self.hub.remove(name);
                let h = self.http.remove(name);
                Ok(ToolOutput {
                    content: vec![Block::Paragraph(match moved {
                        Some(p) => format!(
                            "{name}：配置已删除（mcp={m}, http={h}）。重启后不会再加载。\n\
                             原文件已移到 {}，需要恢复就把它移回 services.d/。",
                            p.display()
                        ),
                        None => format!("{name}：本来就没有配置文件（内存里 mcp={m}, http={h}）。"),
                    })],
                    doc_writes: vec![crate::selfmod::record(
                        "service",
                        name,
                        "remove",
                        b"",
                        serde_json::json!({ "mcp": m, "http": h }),
                    )],
                    artifacts: Vec::new(),
                })
            }
            "rollback" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 name", false))?;
                let backup_dir = self.dir.join(crate::selfmod::BACKUP_DIR);
                let file_name = format!("{name}.toml");
                let Some((ts, backup)) = crate::selfmod::newest_backup(&backup_dir, &file_name)
                else {
                    return Err(ToolError::new(
                        ErrorCode::NotFound,
                        format!("`{name}` 没有历史版本可回滚"),
                        false,
                    ));
                };
                let body = std::fs::read(&backup).map_err(|e| {
                    ToolError::new(ErrorCode::Unavailable, format!("读备份失败：{e}"), false)
                })?;
                let path = self.fragment(name);
                crate::selfmod::backup_file(&path, &backup_dir)
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
                crate::selfmod::write_atomic(&path, &body)
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
                Ok(ToolOutput {
                    content: vec![Block::Paragraph(format!(
                        "`{name}` 已回滚到 {ts} 那一版。\n\
                         **当前进程仍用旧配置**：services.d/ 只在启动时加载，重启后生效。"
                    ))],
                    doc_writes: vec![crate::selfmod::record(
                        "service",
                        name,
                        "rollback",
                        &body,
                        serde_json::json!({ "to": ts }),
                    )],
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
                // `service add` 只带得动 base/token/header 这些，**带不动**宿主侧的下载边界
                // （redirect_hosts / redirect 规则）。同名重加会把它们清空，而且写出的
                // services.d 片段会盖掉 chuan.toml —— 下一次启动就默默失去白名单。
                if let Some(old) = self.http.get(&name) {
                    if !old.redirect_hosts.is_empty() || !old.redirect_rules.is_empty() {
                        return Err(ToolError::new(
                            ErrorCode::NotPermitted,
                            format!(
                                "`{name}` 已有宿主侧配置（redirect_hosts / redirect 规则），\
                                 运行期重加会把它清空，写出的片段还会在下次启动时覆盖 chuan.toml。\n\
                                 要改这个服务，请直接改配置文件里对应的 [services.{name}]，然后重启。"
                            ),
                            false,
                        ));
                    }
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

                let extra_headers: Vec<(String, String)> = args
                    .get("extra_headers")
                    .and_then(|v| v.as_object())
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| {
                                v.as_str().map(|v| (k.trim().to_owned(), v.trim().to_owned()))
                            })
                            .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                let (expose, allow_methods, allow_paths, header_allow) =
                    parse_gateway_args(&args)
                        .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
                if kind == "mcp" && expose {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        "expose_to_skills 只支持 HTTP 服务（MCP 的 tools/list 已经在协议层管住）",
                        false,
                    ));
                }

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
                            extra_headers: extra_headers.clone(),
                            token_key: token_key.clone(),
                            query: query.clone(),
                            body: body.clone(),
                            sign: sign.clone(),
                            redirect_hosts: Vec::new(),
                            redirect_rules: Vec::new(),
                            expose_to_skills: expose,
                            allow_methods: allow_methods.clone(),
                            allow_paths: allow_paths.clone(),
                            header_allow: header_allow.clone(),
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
                                extra_headers: extra_headers.clone(),
                                expose_to_skills: Some(expose),
                                allow_methods: allow_methods.clone(),
                                allow_paths: allow_paths.clone(),
                                header_allow: header_allow.clone(),
                            },
                        );
                        let body = match &saved {
                            Ok(p) => std::fs::read(p).unwrap_or_default(),
                            Err(_) => Vec::new(),
                        };
                        Ok(ToolOutput {
                            content: vec![Block::Paragraph(format!(
                                "已接入 HTTP 服务 `{name}`：{probe}；{}\n调用：`http(service=\"{name}\", method=\"GET\", path=\"/…\")`\n\
                                 如果接口文档还没写，把 API 说明整理成 playbook，下次就能直接查。",
                                save_line(&saved)
                            ))],
                            doc_writes: vec![crate::selfmod::record(
                                "service",
                                &name,
                                "add",
                                &body,
                                serde_json::json!({
                                    "base": base,
                                    "kind": kind,
                                    "expose_to_skills": expose,
                                    "allow_methods": allow_methods,
                                    "allow_paths": allow_paths,
                                }),
                            )],
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

    /// 技能网关授权必须写进 services.d 片段，并且能被 `ServiceFile` 重新解析
    /// ——"重启不丢"就是这条（D41）。
    #[test]
    fn gateway_grant_survives_restart() {
        #[derive(serde::Deserialize)]
        struct Frag {
            base: String,
            extra_headers: Vec<(String, String)>,
            expose_to_skills: bool,
            allow_methods: Vec<String>,
            allow_paths: Vec<String>,
            header_allow: std::collections::BTreeMap<String, Vec<String>>,
        }
        let dir = tempfile::tempdir().unwrap();
        let methods = vec!["POST".to_string()];
        let paths = vec!["/JNAP/".to_string()];
        let headers = std::collections::BTreeMap::from([(
            "X-JNAP-Action".to_owned(),
            vec!["GetDeviceInfo".to_owned()],
        )]);
        let p = write_fragment(
            dir.path(),
            "velop",
            "http",
            "http://10.84.1.1",
            Some("~/.config/chuan/secrets/velop_auth"),
            Some("X-JNAP-Authorization: Basic {token}"),
            &[],
            None,
            HttpExtra {
                extra_headers: vec![("Content-Type".into(), "text/plain".into())],
                expose_to_skills: Some(true),
                allow_methods: methods.clone(),
                allow_paths: paths.clone(),
                header_allow: headers.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let f: Frag = toml::from_str(&text).unwrap();
        assert_eq!(f.base, "http://10.84.1.1");
        assert_eq!(f.extra_headers, vec![("Content-Type".to_string(), "text/plain".to_string())]);
        assert!(f.expose_to_skills);
        assert_eq!(f.allow_methods, methods);
        assert_eq!(f.allow_paths, paths);
        assert_eq!(f.header_allow, headers);
        // 顶层键必须在任何 [table] 之前，否则会被归到上一个表里
        assert!(text.find("allow_paths").unwrap() < text.find("[header_allow]").unwrap());
    }

    #[test]
    fn gateway_args_fail_closed() {
        // 只给 allow_* 却没打开 expose：拒绝，避免"看起来开放"
        let err = parse_gateway_args(&serde_json::json!({
            "allow_methods": ["POST"], "allow_paths": ["/JNAP/"]
        }))
        .unwrap_err();
        assert!(err.contains("expose_to_skills = true"), "{err}");
        // 打开了却给空白名单：拒绝，不能退化成"全放开"
        let err = parse_gateway_args(&serde_json::json!({
            "expose_to_skills": true, "allow_methods": ["POST"]
        }))
        .unwrap_err();
        assert!(err.contains("非空"), "{err}");
        // 鉴权头不能由技能设置
        let err = parse_gateway_args(&serde_json::json!({
            "expose_to_skills": true,
            "allow_methods": ["POST"],
            "allow_paths": ["/JNAP/"],
            "header_allow": {"Authorization": ["x"]}
        }))
        .unwrap_err();
        assert!(err.contains("不允许"), "{err}");
    }

    /// 审批卡本身：`service add` 时，主人必须能在一屏里看清
    /// 目的地 / 方法 / 路径 / 动作 / 凭据文件，且**看不到凭据值**（D41）。
    #[test]
    fn approval_card_shows_gateway_grant() {
        let dir = tempfile::tempdir().unwrap();
        let http = HttpServiceTool::new(vec![], dir.path().join("net.jsonl")).unwrap();
        let tool = ServiceTool::new(
            dir.path().join("services.d"),
            dir.path().join("secrets"),
            McpHub::new(vec![]),
            http,
        );
        let args = serde_json::json!({
            "action": "add",
            "name": "velop",
            "base": "http://10.84.1.1",
            "token_file": "~/.config/chuan/secrets/velop_admin",
            "header": "X-JNAP-Authorization: Basic {token}",
            "extra_headers": { "Content-Type": "text/plain" },
            "expose_to_skills": true,
            "allow_methods": ["POST"],
            "allow_paths": ["/JNAP/"],
            "header_allow": { "X-JNAP-Action": ["GetDeviceInfo"] },
            "_approval": { "why": "读路由器状态", "impact": "向设备发指令" }
        });
        let text = tool.approval_hint(&args).expect("add 必须给卡");
        for needle in [
            "http://10.84.1.1",
            "POST",
            "/JNAP/",
            "X-JNAP-Action",
            "GetDeviceInfo",
            "velop_admin",
            "X-JNAP-Authorization",
            "重启不丢",
        ] {
            assert!(text.contains(needle), "卡上缺少 {needle}：\n{text}");
        }
        assert!(!text.contains("YWRtaW4"), "卡上不能出现凭据值：\n{text}");
    }

    /// 审批卡的核心字段：目的地、方法、路径、动作、凭据来源（D41）。
    #[test]
    fn grant_summary_lists_core_fields() {        let svc = crate::services::Service {
            name: "velop".into(),
            base: "http://10.84.1.1".into(),
            expose_to_skills: true,
            allow_methods: vec!["POST".into()],
            allow_paths: vec!["/JNAP/".into()],
            header_allow: std::collections::BTreeMap::from([(
                "X-JNAP-Action".to_owned(),
                vec!["GetDeviceInfo".to_owned()],
            )]),
            ..Default::default()
        };
        let text = svc.skill_grant();
        for needle in ["velop", "http://10.84.1.1", "POST", "/JNAP/", "X-JNAP-Action", "宿主注入"] {
            assert!(text.contains(needle), "缺少 {needle}：{text}");
        }
    }
}
