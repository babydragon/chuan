//! 角色目录：让 agent 自己注册"小弟"（`DECISIONS.md` D19）。
//!
//! 文件布局（每角色两个文件，改完不需要重启）：
//!
//! ```text
//! ~/.config/chuan/roles/coder.toml   # 说明/模型/工具/预算
//! ~/.config/chuan/roles/coder.md     # 角色提示词（追加在父前缀之后）
//! ```
//!
//! 提示词单独放 `.md` 而不是塞进 TOML：免得为了转义引号把 AI 写的正文搞坏，
//! 也方便主人直接编辑。
//!
//! 安全边界不在这里：角色最终仍要过 `[agents] tools` 白名单与审批域，
//! 这个模块只负责"存/取/列"。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, RoleSource, RoleSpec, Tool, ToolCtx, ToolError, ToolOutput,
    ToolSpec,
};
use serde::Deserialize;

const MAX_PROMPT: usize = 8_000;
const MAX_NAME: usize = 32;

#[derive(Debug, Default, Deserialize)]
struct RoleFile {
    description: Option<String>,
    model: Option<String>,
    tools: Option<Vec<String>>,
    max_turns: Option<u32>,
    max_tokens: Option<u64>,
    wall_ms: Option<u64>,
}

/// 从目录读角色。每次调用都读盘——角色文件很小，热加载比缓存重要。
pub struct DiskRoles {
    dir: PathBuf,
}

impl DiskRoles {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.chars().next().map(|c| c.is_ascii_lowercase()).unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn load_role(dir: &Path, name: &str) -> Option<(String, RoleSpec)> {
    let text = std::fs::read_to_string(dir.join(format!("{name}.toml"))).ok()?;
    let file: RoleFile = toml::from_str(&text).ok()?;
    let prompt = std::fs::read_to_string(dir.join(format!("{name}.md")))
        .ok()
        .map(|p| p.trim_end().to_owned())
        .filter(|p| !p.is_empty());
    Some((
        name.to_owned(),
        RoleSpec {
            description: file.description,
            system_prompt: prompt,
            model: file.model,
            tools: file.tools,
            max_turns: file.max_turns.unwrap_or(0),
            max_tokens: file.max_tokens.unwrap_or(0),
            wall_ms: file.wall_ms.unwrap_or(0),
        },
    ))
}

fn names(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("toml") {
                return None;
            }
            p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_owned())
        })
        .filter(|n| valid_name(n))
        .collect();
    out.sort();
    out
}

impl RoleSource for DiskRoles {
    fn all(&self) -> Vec<(String, RoleSpec)> {
        names(&self.dir)
            .into_iter()
            .filter_map(|n| load_role(&self.dir, &n))
            .collect()
    }
}

impl RoleSource for RoleTool {
    fn all(&self) -> Vec<(String, RoleSpec)> {
        names(&self.dir)
            .into_iter()
            .filter_map(|n| load_role(&self.dir, &n))
            .collect()
    }
}

/// `role`：列 / 加 / 删。域是 `SelfModify`，每次都要人批。
pub struct RoleTool {
    dir: PathBuf,
}

impl RoleTool {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn list(&self) -> String {
        let ns = names(&self.dir);
        if ns.is_empty() {
            return "还没有角色。用 role(action=\"add\", …) 建一个。".to_owned();
        }
        let mut out = format!("可用角色（{} 个）：\n", ns.len());
        for (name, spec) in RoleSource::all(self) {
            let desc = spec.description.unwrap_or_else(|| "（无说明）".into());
            let tools = spec
                .tools
                .map(|t| t.join("/"))
                .unwrap_or_else(|| "默认白名单".into());
            out.push_str(&format!("- `{name}`：{desc}（工具：{tools}）\n"));
        }
        out.push_str("\n派活：delegate(task=\"…\", role=\"<名字>\")");
        out
    }
}

#[async_trait]
impl Tool for RoleTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "role".into(),
            description: "管理可派给子 agent 的角色（提示词 + 工具子集 + 模型 + 预算）。\n\
                注册后父会话立刻能看到，下一次 delegate 就能用，不需要重启。\n\
                只能收窄：角色的工具仍受 [agents] tools 白名单与审批域限制。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "add", "remove"] },
                    "name": { "type": "string", "description": "小写字母/数字/-/_，≤32 字符" },
                    "description": { "type": "string", "description": "一句话说明（会显示给主模型和你我）" },
                    "prompt": { "type": "string", "description": "角色提示词（写进 <name>.md，≤8000 字）" },
                    "tools": {
                        "type": "array", "items": { "type": "string" },
                        "description": "该角色可用工具（只能是免审批域；越权会被过滤）"
                    },
                    "model": { "type": "string" },
                    "max_turns": { "type": "integer" },
                    "max_tokens": { "type": "integer" },
                    "wall_ms": { "type": "integer" }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::External
    }

    /// 改的是配置目录：子 agent 不能自己扩权。
    fn tier(&self) -> u8 {
        1
    }

    fn domain(&self) -> harness_kernel::Domain {
        harness_kernel::Domain::SelfModify
    }

    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let action = args["action"].as_str().unwrap_or("?");
        let name = args["name"].as_str().unwrap_or("?");
        if action != "add" {
            return Some(format!("role {action} {name}"));
        }
        // 审批卡必须能看清"这个角色能干什么"：工具、模型、提示词开头。
        // 只写 "role add coder" 等于让人盲批（D5 的橡皮图章问题）。
        let mut hint = format!("role add {name}");
        if let Some(t) = args["tools"].as_array() {
            let tools: Vec<&str> = t.iter().filter_map(|v| v.as_str()).collect();
            if !tools.is_empty() {
                hint.push_str(&format!("；工具 {}", tools.join("/")));
            }
        }
        if let Some(m) = args["model"].as_str() {
            hint.push_str(&format!("；模型 {m}"));
        }
        if let Some(p) = args["prompt"].as_str() {
            let head: String = p.trim().chars().take(80).collect();
            hint.push_str(&format!("；提示词「{head}{}」", if p.chars().count() > 80 { "…" } else { "" }));
        }
        Some(hint)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let text = match args["action"].as_str().unwrap_or("") {
            "list" => self.list(),
            "add" => self.add(&args)?,
            "remove" => self.remove(&args)?,
            other => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("action 只能是 list/add/remove，收到 {other}"),
                    false,
                ))
            }
        };
        Ok(ToolOutput {
            content: vec![Block::Paragraph(text)],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

impl RoleTool {
    fn add(&self, args: &serde_json::Value) -> Result<String, ToolError> {
        let name = args["name"].as_str().unwrap_or("").trim();
        if !valid_name(name) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "name 只能是小写字母开头、由小写字母/数字/-/_ 组成，≤32 字符",
                false,
            ));
        }
        let prompt = args["prompt"].as_str().unwrap_or("").trim();
        if prompt.chars().count() > MAX_PROMPT {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "prompt 太长（>8000 字）", false));
        }
        if prompt.is_empty() && args["description"].as_str().unwrap_or("").trim().is_empty() {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "至少要给 description 或 prompt，否则角色没有意义",
                false,
            ));
        }

        std::fs::create_dir_all(&self.dir)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("建角色目录失败: {e}"), false))?;

        // 先写提示词，再写 toml：反过来会出现"角色存在但没提示词"的短暂状态。
        if !prompt.is_empty() {
            std::fs::write(self.dir.join(format!("{name}.md")), format!("{prompt}\n")).map_err(
                |e| ToolError::new(ErrorCode::Unavailable, format!("写 {name}.md 失败: {e}"), false),
            )?;
        }
        let mut toml_text = String::new();
        if let Some(d) = args["description"].as_str() {
            toml_text.push_str(&format!("description = {}\n", toml_str(d)));
        }
        if let Some(m) = args["model"].as_str() {
            toml_text.push_str(&format!("model = {}\n", toml_str(m)));
        }
        if let Some(t) = args["tools"].as_array() {
            let list: Vec<String> = t.iter().filter_map(|v| v.as_str()).map(toml_str).collect();
            toml_text.push_str(&format!("tools = [{}]\n", list.join(", ")));
        }
        for key in ["max_turns", "max_tokens", "wall_ms"] {
            if let Some(n) = args[key].as_u64() {
                toml_text.push_str(&format!("{key} = {n}\n"));
            }
        }
        let path = self.dir.join(format!("{name}.toml"));
        std::fs::write(&path, toml_text).map_err(|e| {
            ToolError::new(ErrorCode::Unavailable, format!("写 {} 失败: {e}", path.display()), false)
        })?;

        // 自检：写出去的东西必须能被读回来，否则等于悄悄失败。
        match load_role(&self.dir, name) {
            Some(_) => Ok(format!(
                "已注册角色 `{name}`。\n下一次 delegate(role=\"{name}\") 即可用；`/agents` 之外不需要重启。"
            )),
            None => Err(ToolError::new(
                ErrorCode::Internal,
                "角色文件写成功但读不回来（格式错误），请检查",
                false,
            )),
        }
    }

    fn remove(&self, args: &serde_json::Value) -> Result<String, ToolError> {
        let name = args["name"].as_str().unwrap_or("").trim();
        if !valid_name(name) {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "name 不合法", false));
        }
        let toml_path = self.dir.join(format!("{name}.toml"));
        let md_path = self.dir.join(format!("{name}.md"));
        if !toml_path.exists() {
            return Err(ToolError::new(
                ErrorCode::NotFound,
                format!("没有角色 `{name}`"),
                false,
            ));
        }
        std::fs::remove_file(&toml_path)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("删除失败: {e}"), false))?;
        let _ = std::fs::remove_file(&md_path);
        Ok(format!("已删除角色 `{name}`。"))
    }
}

/// TOML 基本字符串转义：引号、反斜杠、换行。
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rules() {
        assert!(valid_name("coder"));
        assert!(valid_name("tutor-2"));
        assert!(!valid_name("Coder"));
        assert!(!valid_name("../etc"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
    }

    #[test]
    fn toml_escapes() {
        assert_eq!(toml_str("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(toml_str("多行\n文本"), "\"多行\\n文本\"");
    }
}
