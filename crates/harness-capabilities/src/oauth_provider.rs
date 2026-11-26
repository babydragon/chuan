//! `oauth_manifest`：把 OAuth provider 清单装进配置目录。
//!
//! 目的：部署在家里、只通过通道交流的环境里，加一个 OAuth 服务不该让人 SSH 上去
//! 手动放文件。agent 起草 → 主人审批 → 落盘；helper 每次调用都重读 manifest，
//! 所以**不需要重启**任何进程。
//!
//! 边界：
//! - 只写 `<config>/oauth/providers.d/<id>.toml`，不碰 `chuan.toml`（D48）；
//! - 清单里不允许出现密钥字面量（只能给 `client_secret_file` 这类路径）；
//! - `output.token_file` 必须落在 helper 的状态目录（`<state>/oauth/`）下，
//!   否则 core 会读到别的进程可能写坏的文件；
//! - 覆盖先备份到 `.backup/`，删除进 `.trash/`。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use harness_kernel::{Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};

/// 清单上限：真实 provider 不到 3KB，超过这个数基本是贴错了文件。
const MAX_BYTES: u64 = 16 * 1024;

pub struct OauthManifestTool {
    /// `<config>/oauth/providers.d`
    dir: PathBuf,
    /// `<state>/oauth`：`output.token_file` 必须在这下面
    oauth_root: PathBuf,
}

impl OauthManifestTool {
    pub fn new(
        dir: impl Into<PathBuf>,
        oauth_root: impl Into<PathBuf>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            dir: dir.into(),
            oauth_root: oauth_root.into(),
        })
    }

    fn path_of(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.toml"))
    }

    fn list(&self) -> String {
        let mut entries: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("toml") {
                    continue;
                }
                if let Some(id) = p.file_stem().and_then(|s| s.to_str()) {
                    entries.push((id.to_owned(), p));
                }
            }
        }
        entries.sort();
        if entries.is_empty() {
            return format!(
                "还没有安装 provider 清单（目录 {}）。\n\
                 内置默认只有 `cloudflare-mcp`（手动回调）；要自动回调或其他 provider，\
                 用 oauth_manifest(action=\"install\", path=\"/ws/…toml\") 装一份。",
                self.dir.display()
            );
        }
        let mut out = format!("已安装 {} 个 provider 清单（{}）：\n", entries.len(), self.dir.display());
        for (id, p) in entries {
            match read_manifest_value(&p) {
                Ok(v) => {
                    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("?");
                    let cb = v
                        .get("callback")
                        .and_then(|c| c.get("mode"))
                        .and_then(Value::as_str)
                        .unwrap_or("manual");
                    let token = v
                        .get("output")
                        .and_then(|o| o.get("token_file"))
                        .and_then(Value::as_str)
                        .unwrap_or("（默认）");
                    out.push_str(&format!("- `{id}`（{kind}，回调 {cb}）→ token: {token}\n"));
                }
                Err(e) => out.push_str(&format!("- `{id}`：清单读不出来（{e}）\n")),
            }
        }
        out
    }
}

#[async_trait]
impl Tool for OauthManifestTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "oauth_manifest".into(),
            description: "管理 OAuth provider 清单（接第三方 OAuth 服务的第一步）。\n\
                清单是一份 TOML：授权端点/发现地址、scope、回调方式（manual / opentunnel）、\
                token 写到哪。装好之后**不用重启**：helper 每次调用都重读清单。\n\
                完整流程：① 在 /ws 写清单草稿 → ② oauth_manifest(action=\"install\")，\
                主人批准 → ③ 用 mcp(server=\"oauth\", tool=\"auth_begin\") 拿授权链接发给主人 → \
                ④ 主人点完授权后，用 service(action=\"add\", kind=\"mcp\") 把服务接进核心\
                （此时 token 已存在，握手会成功，立即生效）。\n\
                清单里不能有密钥字面量，只能写 client_secret_file 这类路径。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "_approval": {
                        "type": "object",
                        "description": "审批卡上给非程序员看的解释；改自身配置一定触发审批，所以必填",
                        "properties": {
                            "why": { "type": "string", "description": "一句人话：接的是哪个服务、用来做什么" },
                            "impact": { "type": "string", "description": "影响面：例如「新增只读的 Cloudflare 授权」「覆盖已有清单」" }
                        },
                        "required": ["why", "impact"]
                    },
                    "action": { "type": "string", "enum": ["list", "install", "remove"] },
                    "path": { "type": "string", "description": "install 时必填：/ws 下的清单草稿" },
                    "id": { "type": "string", "description": "install/remove 时的 provider id；install 省略则取清单里的 id" }
                },
                "required": ["action", "_approval"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::External
    }

    /// 改的是配置目录：子 agent 不能自己扩权（与 `role` 一致）。
    fn tier(&self) -> u8 {
        1
    }

    fn domain(&self) -> harness_kernel::Domain {
        harness_kernel::Domain::SelfModify
    }

    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("?");
        let id = args.get("id").and_then(Value::as_str).unwrap_or("（按清单里的 id）");
        Some(match action {
            "install" => {
                let dest = if id.starts_with("（") {
                    format!("{}/<id>.toml", self.dir.display())
                } else {
                    self.path_of(id).display().to_string()
                };
                let state = if self.path_of(id).exists() {
                    "覆盖已有清单（先备份）"
                } else {
                    "新增清单"
                };
                format!("安装 OAuth provider 清单 `{id}` → {dest}；{state}")
            }
            "remove" => format!("删除 OAuth provider 清单 `{id}`（进回收站，可捞回）"),
            _ => format!("oauth_manifest {action}"),
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 action", false))?;

        match action {
            "list" => Ok(ToolOutput {
                content: vec![Block::Paragraph(self.list())],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            }),
            "install" => {
                let raw = args
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 path", false))?;
                let src = crate::tools::fs::resolve(&ctx.workspace.root, raw)?;
                let meta = std::fs::metadata(&src).map_err(|e| {
                    ToolError::new(ErrorCode::NotFound, format!("读不到草稿：{e}"), false)
                })?;
                if meta.len() > MAX_BYTES {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("清单超过 {MAX_BYTES} 字节上限"),
                        false,
                    ));
                }
                let text = std::fs::read_to_string(&src).map_err(|e| {
                    ToolError::new(ErrorCode::NotFound, format!("读不到草稿：{e}"), false)
                })?;
                let value = validate(&text, &self.oauth_root)
                    .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if let Some(want) = args.get("id").and_then(Value::as_str) {
                    if want != id {
                        return Err(ToolError::new(
                            ErrorCode::InvalidArgs,
                            format!("参数 id=`{want}` 与清单里的 id=`{id}` 不一致"),
                            false,
                        ));
                    }
                }
                std::fs::create_dir_all(&self.dir).map_err(|e| {
                    ToolError::new(
                        ErrorCode::Unavailable,
                        format!("providers 目录不可用：{e}"),
                        false,
                    )
                })?;
                let dest = self.path_of(&id);
                let existed = dest.exists();
                crate::selfmod::backup_file(&dest, &self.dir.join(crate::selfmod::BACKUP_DIR))
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
                crate::selfmod::write_atomic(&dest, format!("{}\n", text.trim()).as_bytes())
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
                let scopes = value
                    .get("authorize")
                    .and_then(|a| a.get("scopes"))
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_else(|| "（未声明）".into());
                let mode = value
                    .get("callback")
                    .and_then(|c| c.get("mode"))
                    .and_then(Value::as_str)
                    .unwrap_or("manual");
                Ok(ToolOutput {
                    content: vec![Block::Paragraph(format!(
                        "已{} provider 清单 `{id}` → {}（回调 {mode}；scope: {scopes}）。\n\
                         不用重启：helper 每次调用都会重读。下一步用 \
                         `mcp(server=\"oauth\", tool=\"auth_begin\", args={{\"provider\":\"{id}\"}})` \
                         拿授权链接；授权完成后用 \
                         `service(action=\"add\", name=\"...\", kind=\"mcp\", base=..., token_file=...)` \
                         把服务接进核心。",
                        if existed { "覆盖" } else { "安装" },
                        dest.display()
                    ))],
                    doc_writes: vec![crate::selfmod::record(
                        "oauth-provider",
                        &id,
                        if existed { "update" } else { "add" },
                        text.as_bytes(),
                        json!({ "path": dest.to_string_lossy(), "callback": mode, "scopes": scopes }),
                    )],
                    artifacts: Vec::new(),
                })
            }
            "remove" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 id", false))?;
                if !valid_id(id) {
                    return Err(ToolError::new(ErrorCode::InvalidArgs, "provider id 不合法", false));
                }
                let file = self.path_of(id);
                let moved = crate::selfmod::trash(&file, &self.dir.join(crate::selfmod::TRASH_DIR))
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
                Ok(ToolOutput {
                    content: vec![Block::Paragraph(match moved {
                        Some(p) => format!(
                            "已删除 provider 清单 `{id}`；原文件在 {}，需要恢复就移回 providers.d/。\n\
                             已写出的 token 不会被删——不再用这个服务的话，记得把 services.d 里对应条目也删掉。",
                            p.display()
                        ),
                        None => format!("`{id}` 本来就没有清单文件（可能用的是内置默认）。"),
                    })],
                    doc_writes: vec![crate::selfmod::record(
                        "oauth-provider",
                        id,
                        "remove",
                        b"",
                        json!({}),
                    )],
                    artifacts: Vec::new(),
                })
            }
            other => Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("未知 action：{other}"),
                false,
            )),
        }
    }
}

fn read_manifest_value(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let table: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
    serde_json::to_value(table).map_err(|e| e.to_string())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with(['.', '-', '_'])
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// 清单校验：拦住"看着像配置、实际会把密钥写进文件"和明显无效的字段。
///
/// 只做**结构**校验；endpoint 是否与 discovery 一致属于运行期的事（helper 会报）。
pub fn validate(text: &str, oauth_root: &Path) -> Result<Value, String> {
    let value = read_manifest_text(text)?;
    let obj = value
        .as_object()
        .ok_or("清单必须是 TOML 表（顶层是键值，不是数组或标量）")?;

    match obj.get("schema").and_then(Value::as_u64) {
        Some(1) => {}
        Some(n) => return Err(format!("schema 只支持 1（收到 {n}）")),
        None => return Err("缺少 schema = 1".into()),
    }
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .ok_or("缺少 id（字符串）")?;
    if !valid_id(id) {
        return Err(format!("id 不合法：{id}（小写字母/数字/-/_/.，≤64）"));
    }
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("缺少 kind（mcp 或 oauth2）")?;
    if !matches!(kind, "mcp" | "oauth2") {
        return Err(format!("kind 只能是 mcp / oauth2（收到 {kind}）"));
    }
    let allow_private = obj.get("allow_private").and_then(Value::as_bool) == Some(true);

    // 至少要有一个能拿到端点的来源
    let discovery = obj.get("discovery").and_then(Value::as_str);
    let authorize_url = obj
        .get("authorize")
        .and_then(|a| a.get("url"))
        .and_then(Value::as_str);
    let token_url = obj.get("token").and_then(|t| t.get("url")).and_then(Value::as_str);
    if discovery.is_none() && token_url.is_none() {
        return Err("至少要有 discovery 或 [token] url".into());
    }
    for (label, url) in [
        ("discovery", discovery),
        ("authorize.url", authorize_url),
        ("token.url", token_url),
    ] {
        if let Some(u) = url {
            if !u.starts_with("https://") && !(allow_private && u.starts_with("http://")) {
                return Err(format!(
                    "{label} 必须是 https://（本机测试才允许 allow_private = true 用 http://）"
                ));
            }
        }
    }

    if let Some(mode) = obj.get("client").and_then(|c| c.get("mode")).and_then(Value::as_str) {
        if !matches!(mode, "dcr" | "static") {
            return Err(format!("[client] mode 只能是 dcr / static（收到 {mode}）"));
        }
    }

    let mode = obj
        .get("callback")
        .and_then(|c| c.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("manual");
    if !matches!(mode, "manual" | "opentunnel" | "relay" | "loopback") {
        return Err(format!(
            "[callback] mode 只能是 manual / opentunnel / relay / loopback（收到 {mode}）"
        ));
    }
    if let Some(port) = obj.get("callback").and_then(|c| c.get("port")).and_then(Value::as_u64) {
        if !(1..=65535).contains(&port) {
            return Err(format!("[callback] port 不合法：{port}"));
        }
    }

    let token_file = obj
        .get("output")
        .and_then(|o| o.get("token_file"))
        .and_then(Value::as_str);
    if let Some(t) = token_file {
        let p = Path::new(t);
        if !p.is_absolute() || t.contains("..") {
            return Err("output.token_file 必须是绝对路径且不含 ..".into());
        }
        if !p.starts_with(oauth_root) {
            return Err(format!(
                "output.token_file 必须写在 {} 下（helper 的状态目录），收到 {t}",
                oauth_root.display()
            ));
        }
    }

    reject_secrets(&value)?;
    Ok(value)
}

fn read_manifest_text(text: &str) -> Result<Value, String> {
    let table: toml::Value = toml::from_str(text).map_err(|e| format!("TOML 解析失败：{e}"))?;
    serde_json::to_value(table).map_err(|e| e.to_string())
}

/// 清单只描述"怎么拿 token"，不能携带 token 本身：一旦允许字面量，
/// 它就会随配置被 agent、备份和日志读走。
fn reject_secrets(value: &Value) -> Result<(), String> {
    const FORBIDDEN: [&str; 6] =
        ["client_secret", "refresh_token", "access_token", "password", "secret", "token"];
    fn walk(v: &Value, path: &str) -> Result<(), String> {
        match v {
            Value::Object(o) => {
                for (k, child) in o {
                    if FORBIDDEN.contains(&k.as_str()) && child.as_str().is_some_and(|s| !s.trim().is_empty())
                    {
                        return Err(format!(
                            "清单里不能出现密钥字面量 `{path}{k}`；只能写路径（如 client_secret_file）"
                        ));
                    }
                    walk(child, &format!("{path}{k}."))?;
                }
                Ok(())
            }
            Value::Array(a) => {
                for (i, child) in a.iter().enumerate() {
                    walk(child, &format!("{path}{i}."))?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(value, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/var/lib/chuan/oauth")
    }

    const GOOD: &str = r#"
schema = 1
id = "demo"
kind = "mcp"
discovery = "https://example.com/.well-known/oauth-protected-resource"

[client]
mode = "dcr"

[callback]
mode = "opentunnel"

[output]
token_file = "/var/lib/chuan/oauth/demo/access_token"
"#;

    #[test]
    fn accepts_a_normal_manifest() {
        let v = validate(GOOD, &root()).unwrap();
        assert_eq!(v["id"], "demo");
    }

    #[test]
    fn rejects_secret_literals() {
        let bad = GOOD.replace("[client]\nmode = \"dcr\"", "[client]\nmode = \"dcr\"\nclient_secret = \"x\"");
        let e = validate(&bad, &root()).unwrap_err();
        assert!(e.contains("密钥字面量"), "{e}");
    }

    #[test]
    fn rejects_token_file_outside_helper_dir() {
        let bad = GOOD.replace("/var/lib/chuan/oauth/demo", "/etc/chuan/secrets/demo");
        let e = validate(&bad, &root()).unwrap_err();
        assert!(e.contains("必须写在"), "{e}");
    }

    #[test]
    fn rejects_unknown_callback_mode_and_http() {
        let bad = GOOD.replace("opentunnel", "carrier-pigeon");
        assert!(validate(&bad, &root()).is_err());
        let bad = GOOD.replace("https://example.com", "http://example.com");
        assert!(validate(&bad, &root()).is_err());
    }

    #[test]
    fn rejects_bad_id() {
        let bad = GOOD.replace("id = \"demo\"", "id = \"Demo/../x\"");
        assert!(validate(&bad, &root()).is_err());
    }
}
