//! 操作规范（SOP）按需取：`playbook(topic)`。
//!
//! 为什么不塞进 system prompt：每天都要为它付 token，而且绝大多数轮次用不到。
//! 为什么不做成「技能」：技能是 stdin→stdout 的可执行纯函数，而 SOP 是给模型看的文字；
//! 让 SKILL.md 进上下文就破坏了"技能说明永不进 prompt"的防线。这里是一条显式的、
//! 由模型主动发起的取用通道，边界清楚。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};

/// 单份规范的上限，防止有人把整本书塞进来。
const MAX_CHARS: usize = 20_000;

pub struct PlaybookTool {
    dir: PathBuf,
    builtins: BTreeMap<String, String>,
}

impl PlaybookTool {
    /// `builtins` 是随二进制发布的默认文本；`dir/<topic>.md` 存在时优先生效（可覆盖）。
    pub fn new(dir: impl Into<PathBuf>, builtins: Vec<(&'static str, &'static str)>) -> Arc<Self> {
        Arc::new(Self {
            dir: dir.into(),
            builtins: builtins.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
        })
    }

    fn topics(&self) -> Vec<String> {
        let mut out: Vec<String> = self.builtins.keys().cloned().collect();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("md") {
                    continue;
                }
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    if !out.iter().any(|t| t == stem) {
                        out.push(stem.to_owned());
                    }
                }
            }
        }
        out.sort();
        out
    }

    fn load(&self, topic: &str) -> Option<String> {
        let path = self.dir.join(format!("{topic}.md"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            if !text.trim().is_empty() {
                return Some(truncate(text));
            }
        }
        self.builtins.get(topic).map(|t| truncate(t.clone()))
    }
}

fn truncate(mut s: String) -> String {
    if s.chars().count() > MAX_CHARS {
        s = s.chars().take(MAX_CHARS).collect::<String>();
        s.push_str("\n\n…（超出上限，已截断；完整内容见 playbooks 目录）");
    }
    s
}

fn valid_topic(topic: &str) -> bool {
    !topic.is_empty()
        && topic.len() <= 40
        && topic
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[async_trait]
impl Tool for PlaybookTool {
    fn spec(&self) -> ToolSpec {
        let topics = self.topics().join("、");
        ToolSpec {
            name: "playbook".into(),
            description: format!(
                "取一份本机的操作规范（SOP），按它办事。可用：{topics}。\
                 导入/复刻外部技能前取 `import-skill`；沉淀技能前取 `distill`。"
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "topic": { "type": "string", "description": "规范名" } },
                "required": ["topic"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let topic = args
            .get("topic")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 topic", false))?;
        if !valid_topic(topic) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "topic 只能是小写字母/数字/-/_",
                false,
            ));
        }
        let text = self.load(topic).ok_or_else(|| {
            ToolError::new(
                ErrorCode::NotFound,
                format!("没有这份规范：{topic}（可用：{}）", self.topics().join("、")),
                false,
            )
        })?;
        Ok(ToolOutput {
            content: vec![Block::Paragraph(text)],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

#[allow(dead_code)]
fn _unused(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_builtins_and_lists_dir_overrides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("extra.md"), "自定义流程").unwrap();
        let t = PlaybookTool::new(dir.path(), vec![("import-skill", "内置导入流程")]);
        let spec = t.spec();
        assert!(spec.description.contains("import-skill"), "{}", spec.description);
        assert!(spec.description.contains("extra"), "{}", spec.description);
    }

    #[test]
    fn dir_file_wins_over_builtin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("import-skill.md"), "覆盖版").unwrap();
        let t = PlaybookTool::new(dir.path(), vec![("import-skill", "内置版")]);
        assert_eq!(t.load("import-skill").as_deref(), Some("覆盖版"));
    }

    #[test]
    fn unknown_topic_and_bad_names_are_rejected() {
        let t = PlaybookTool::new("/nonexistent", vec![("import-skill", "x")]);
        assert!(t.load("nope").is_none());
        assert!(!valid_topic("../etc"));
        assert!(!valid_topic("Upper"));
        assert!(valid_topic("import-skill"));
    }

    #[test]
    fn oversized_text_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.md"), "x".repeat(MAX_CHARS + 10)).unwrap();
        let t = PlaybookTool::new(dir.path(), vec![]);
        let text = t.load("big").unwrap();
        assert!(text.chars().count() <= MAX_CHARS + 40);
        assert!(text.contains("已截断"));
    }
}
