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

    fn file_of(&self, topic: &str) -> Option<PathBuf> {
        let p = self.dir.join(format!("{topic}.md"));
        p.is_file().then_some(p)
    }

    /// `list` 的输出：每个 topic 是内置、还是哪份文件、多久前改的。
    /// 排查"我明明放进去了"时，这一行就够（2026-10-29 有人在这上面耗了半小时）。
    fn list_text(&self) -> String {
        let mut out = format!("目录：{}\n", self.dir.display());
        for t in self.topics() {
            match self.file_of(&t) {
                Some(p) => {
                    let ago = std::fs::metadata(&p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|m| m.elapsed().ok())
                        .map(|d| {
                            let s = d.as_secs();
                            match s {
                                0..=59 => "刚刚".to_string(),
                                60..=3599 => format!("{} 分钟前", s / 60),
                                3600..=86399 => format!("{} 小时前", s / 3600),
                                _ => format!("{} 天前", s / 86400),
                            }
                        })
                        .unwrap_or_else(|| "?".into());
                    let builtin = if self.builtins.contains_key(&t) { "（覆盖内置）" } else { "" };
                    out.push_str(&format!("- {t}{builtin}  文件 {}  · {ago}\n", p.display()));
                }
                None => out.push_str(&format!("- {t}  内置\n")),
            }
        }
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
                 目录：{}（`action=list` 看每个 topic 是内置还是哪份文件、多久前改的）。\
                 取不到时会告诉你这个目录和现有清单——**不要**据此要求主人「再去放一遍」，先看是不是目录不对。\
                 导入/复刻外部技能前取 `import-skill`；沉淀技能前取 `distill`。\
                 把一次性的做法变成可复用规范，用 `publish_playbook`（需要主人确认）；改坏了用 `rollback_playbook` 回上一版。",
                self.dir.display()
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["get", "list"], "description": "默认 get" },
                    "topic": { "type": "string", "description": "规范名（action=get 时必填）" }
                }
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
        if args.get("action").and_then(|v| v.as_str()) == Some("list") {
            return Ok(ToolOutput {
                content: vec![Block::Paragraph(self.list_text())],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            });
        }
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
                format!(
                    "没有这份规范：{topic}。目录 {}\n现有：{}",
                    self.dir.display(),
                    self.topics().join("、")
                ),
                false,
            )
        })?;
        // 标明来源：模型据此能判断"我读到的是文件还是内置"，人也能一眼看出目录对不对
        let src = match self.file_of(topic) {
            Some(p) => format!("来源：{}", p.display()),
            None => "来源：内置（无同名文件）".to_string(),
        };
        Ok(ToolOutput {
            content: vec![Block::Paragraph(src), Block::Paragraph(text)],
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
    fn list_shows_dir_and_file_age() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("teacher-notice.md"), "# 老师通知\n").unwrap();
        let t = PlaybookTool::new(dir.path(), vec![("distill", "内置")]);
        let text = t.list_text();
        assert!(text.contains(&dir.path().display().to_string()), "{text}");
        assert!(text.contains("teacher-notice  文件"), "{text}");
        assert!(text.contains("刚刚"), "{text}");
        assert!(text.contains("distill  内置"), "{text}");
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

// ---------------------------------------------------------------- publish

/// `publish_playbook(topic, path)`：把 `/ws` 里写好的规范落盘，需要主人确认。
///
/// 与 `publish_skill` 同一形态（D23）：SelfModify → 审批 → 覆盖留 `.bak`；
/// `builtin` 主题必须显式 `override=true`。**代码从不读 playbook**，所以写它不构成提权。
pub struct PublishPlaybookTool {
    dir: PathBuf,
    builtins: std::collections::BTreeSet<String>,
}

impl PublishPlaybookTool {
    pub fn new(dir: impl Into<PathBuf>, builtins: Vec<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            dir: dir.into(),
            builtins: builtins.into_iter().map(str::to_owned).collect(),
        })
    }
}

#[async_trait]
impl Tool for PublishPlaybookTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "publish_playbook".into(),
            description: "把 `/ws` 里写好的操作规范（SOP）装进本机 playbooks 目录，以后用 `playbook` 取。\
用于把「这类资料怎么抽结构 / 这个流程怎么做」沉淀下来，下次直接复用。\
覆盖已有规范会先备份到 `.bak/`；内置规范必须显式 `override=true`。正文 ≤20000 字。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "_approval": {
                        "type": "object",
                        "description": "审批卡上给非程序员看的解释；装/覆盖规范一定会触发审批，所以必填",
                        "properties": {
                            "why": { "type": "string", "description": "一句人话：这份规范解决什么问题" },
                            "impact": { "type": "string", "description": "影响面：只读 / 写文件 / 装软件 / 向设备发指令 / 改自身配置" }
                        },
                        "required": ["why", "impact"]
                    },
                    "topic": { "type": "string", "description": "规范名：小写字母/数字/-/_，≤40" },
                    "path": { "type": "string", "description": "草稿路径（/ws 下的 .md 文件）" },
                    "override": { "type": "boolean", "description": "覆盖内置规范时必须为 true" }
                },
                "required": ["topic", "path", "_approval"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 改配置目录属于外部副作用，失败不自动重发
        EffectClass::External
    }

    fn domain(&self) -> harness_kernel::Domain {
        harness_kernel::Domain::SelfModify
    }

    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let topic = args.get("topic").and_then(|v| v.as_str()).unwrap_or("?");
        let dest = self.dir.join(format!("{topic}.md"));
        // 审批发生在执行之前，拿不到 workspace 句柄，读不了 /ws 里的草稿；
        // 这里只报"新增还是覆盖 + 旧版有多大"，不做假 diff。
        let old = std::fs::metadata(&dest).ok().map(|m| m.len());
        Some(match (self.builtins.contains(topic), old) {
            (true, _) => format!("覆盖内置规范 `{topic}`（会先备份）"),
            (false, Some(bytes)) => format!(
                "覆盖规范 `{topic}`（现有 {} 行，覆盖前会备份）",
                bytes / 40
            ),
            (false, None) => format!("新增规范 `{topic}`"),
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let topic = args
            .get("topic")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 topic", false))?;
        if !valid_topic(topic) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "topic 只能用小写字母/数字/-/_（≤40）",
                false,
            ));
        }
        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 path", false))?;
        let src = crate::tools::fs::resolve(&ctx.workspace.root, raw)?;
        let body = std::fs::read_to_string(&src)
            .map_err(|e| ToolError::new(ErrorCode::NotFound, format!("读不到草稿：{e}"), false))?;
        let body = body.trim();
        if body.is_empty() {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "草稿是空的", false));
        }
        if body.chars().count() > MAX_CHARS {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("正文超过 {MAX_CHARS} 字上限"),
                false,
            ));
        }
        let existed = self.dir.join(format!("{topic}.md")).exists();
        let override_builtin = args.get("override").and_then(|v| v.as_bool()).unwrap_or(false);
        if self.builtins.contains(topic) && !override_builtin {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("`{topic}` 是内置规范；要覆盖请显式传 override=true（会先备份）"),
                false,
            ));
        }

        std::fs::create_dir_all(&self.dir).map_err(|e| {
            ToolError::new(ErrorCode::Unavailable, format!("playbooks 目录不可用：{e}"), false)
        })?;
        let dest = self.dir.join(format!("{topic}.md"));
        let backup_dir = self.dir.join(crate::selfmod::BACKUP_DIR);
        // 覆盖先备份（保留 5 版）；写入走原子替换
        crate::selfmod::backup_file(&dest, &backup_dir)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
        crate::selfmod::write_atomic(&dest, format!("{body}\n").as_bytes())
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;

        let digest: String = {
            use sha2::{Digest, Sha256};
            Sha256::digest(body.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
        };
        let chars = body.chars().count();
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已装好规范 `{topic}`（{chars} 字）：{}\n以后用 `playbook(topic=\"{topic}\")` 取。",
                dest.display()
            ))],
            doc_writes: vec![crate::selfmod::record(
                "playbook",
                topic,
                if existed { "update" } else { "add" },
                body.as_bytes(),
                serde_json::json!({ "path": dest.to_string_lossy(), "chars": chars, "sha256": digest }),
            )],
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod publish_tests {
    use super::*;
    use harness_kernel::{CallId, ConversationId, RunId, Workspace};
    use tokio_util::sync::CancellationToken;

    async fn run(tool: &dyn Tool, ws: &Workspace, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let env = crate::NullEnv;
        let ctx = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: ConversationId("c".into()),
            actor: "owner",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
            run: RunId("r".into()),
            call: CallId("call".into()),
            workspace: ws,
            env: &env,
            cancel: CancellationToken::new(),
        };
        tool.execute(args, &ctx).await
    }

    #[tokio::test]
    async fn publishes_and_backs_up_previous_version() {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        let books = dir.path().join("playbooks");
        std::fs::create_dir_all(&ws_root).unwrap();
        let ws = Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into() };
        let tool = PublishPlaybookTool::new(&books, vec![]);

        std::fs::write(ws_root.join("v1.md"), "第一版怎么抽结构").unwrap();
        let out = run(tool.as_ref(), &ws, serde_json::json!({"topic": "kb-textbook", "path": "/ws/v1.md"}))
            .await
            .unwrap();
        assert_eq!(out.doc_writes.len(), 1);
        assert!(books.join("kb-textbook.md").exists());

        std::fs::write(ws_root.join("v2.md"), "第二版：改了栏目名").unwrap();
        run(tool.as_ref(), &ws, serde_json::json!({"topic": "kb-textbook", "path": "/ws/v2.md"}))
            .await
            .unwrap();
        assert!(std::fs::read_to_string(books.join("kb-textbook.md")).unwrap().contains("第二版"));
        let baks: Vec<_> = std::fs::read_dir(books.join(crate::selfmod::BACKUP_DIR)).unwrap().flatten().collect();
        assert_eq!(baks.len(), 1, "覆盖必须留一份备份");
    }

    #[tokio::test]
    async fn rollback_restores_previous_version_and_stays_reversible() {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        let books = dir.path().join("playbooks");
        std::fs::create_dir_all(&ws_root).unwrap();
        let ws = Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into() };
        let publish = PublishPlaybookTool::new(&books, vec![]);
        let rollback = RollbackPlaybookTool::new(&books);

        std::fs::write(ws_root.join("v1.md"), "第一版").unwrap();
        run(publish.as_ref(), &ws, serde_json::json!({"topic": "kb-x", "path": "/ws/v1.md"})).await.unwrap();
        std::fs::write(ws_root.join("v2.md"), "第二版").unwrap();
        run(publish.as_ref(), &ws, serde_json::json!({"topic": "kb-x", "path": "/ws/v2.md"})).await.unwrap();
        assert!(std::fs::read_to_string(books.join("kb-x.md")).unwrap().contains("第二版"));

        run(rollback.as_ref(), &ws, serde_json::json!({"topic": "kb-x"})).await.unwrap();
        let now = std::fs::read_to_string(books.join("kb-x.md")).unwrap();
        assert!(now.contains("第一版"), "{now}");
        // 回滚本身可再回滚：第二次应回到"第二版"
        run(rollback.as_ref(), &ws, serde_json::json!({"topic": "kb-x"})).await.unwrap();
        let again = std::fs::read_to_string(books.join("kb-x.md")).unwrap();
        assert!(again.contains("第二版"), "{again}");
    }

    #[tokio::test]
    async fn rollback_without_backup_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        let ws = Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into() };
        let rollback = RollbackPlaybookTool::new(dir.path().join("playbooks"));
        let err = run(rollback.as_ref(), &ws, serde_json::json!({"topic": "nope"})).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn builtin_needs_explicit_override() {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        std::fs::write(ws_root.join("x.md"), "改内置").unwrap();
        let ws = Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into() };
        let tool = PublishPlaybookTool::new(dir.path().join("playbooks"), vec!["distill"]);

        let err = run(tool.as_ref(), &ws, serde_json::json!({"topic": "distill", "path": "/ws/x.md"}))
            .await
            .unwrap_err();
        assert!(err.message.contains("override"), "{}", err.message);

        run(tool.as_ref(), &ws, serde_json::json!({"topic": "distill", "path": "/ws/x.md", "override": true}))
            .await
            .unwrap();
        assert!(dir.path().join("playbooks/distill.md").exists());
    }
}

/// `rollback_playbook(topic)`：回到上一版。回滚本身也可逆（当前版本先备份）。
pub struct RollbackPlaybookTool {
    dir: PathBuf,
}

impl RollbackPlaybookTool {
    pub fn new(dir: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self { dir: dir.into() })
    }

    fn backup_dir(&self) -> PathBuf {
        self.dir.join(crate::selfmod::BACKUP_DIR)
    }

    fn file_name(topic: &str) -> String {
        format!("{topic}.md")
    }
}

#[async_trait]
impl Tool for RollbackPlaybookTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "rollback_playbook".into(),
            description: "把一份规范回滚到上一版。当前版本会先备份，所以回滚本身也能再回滚。\
只影响 `playbooks/<topic>.md`，不影响技能与授权。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "_approval": {
                        "type": "object",
                        "description": "审批卡上给非程序员看的解释；回滚一定会触发审批，所以必填",
                        "properties": {
                            "why": { "type": "string", "description": "一句人话：为什么要退回上一版" },
                            "impact": { "type": "string", "description": "影响面：只读 / 写文件 / 装软件 / 向设备发指令 / 改自身配置" }
                        },
                        "required": ["why", "impact"]
                    },
                    "topic": { "type": "string", "description": "规范名" }
                },
                "required": ["topic", "_approval"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::External
    }

    fn domain(&self) -> harness_kernel::Domain {
        harness_kernel::Domain::SelfModify
    }

    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let topic = args.get("topic").and_then(|v| v.as_str()).unwrap_or("?");
        Some(format!(
            "把规范 `{topic}` 回滚到上一版（当前版本会先备份）"
        ))
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let topic = args
            .get("topic")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 topic", false))?;
        if !valid_topic(topic) {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "topic 不合法", false));
        }
        let (ts, backup) = crate::selfmod::newest_backup(&self.backup_dir(), &Self::file_name(topic))
            .ok_or_else(|| {
            ToolError::new(
                ErrorCode::NotFound,
                format!("`{topic}` 没有备份可回滚（可能从未覆盖过）"),
                false,
            )
        })?;
        let body = std::fs::read_to_string(&backup)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("读备份失败：{e}"), false))?;
        let dest = self.dir.join(Self::file_name(topic));
        // 当前版本也备份，回滚本身可逆
        crate::selfmod::backup_file(&dest, &self.backup_dir())
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
        crate::selfmod::write_atomic(&dest, body.as_bytes())
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;

        let chars = body.trim().chars().count();
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已回滚规范 `{topic}` 到 {ts} 那一版（{chars} 字）；刚才的版本已备份。"
            ))],
            doc_writes: vec![crate::selfmod::record(
                "playbook",
                topic,
                "rollback",
                body.as_bytes(),
                serde_json::json!({ "path": dest.to_string_lossy(), "chars": chars, "to": ts }),
            )],
            artifacts: Vec::new(),
        })
    }
}
