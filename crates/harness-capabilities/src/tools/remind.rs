//! `remind`：安排一个将来会被执行的提示（一次性或周期）。
//!
//! 存储走 doc 容器（kind=`reminder`）而不是新表：提醒就是"应用语义的结论"，
//! 与记忆同源、可审计（DocUpserted 进账本）、`/forget` 那套删除能力直接复用。
//! 到点由内核的调度器起一次普通 run —— 所以"每天 9 点汇总新闻"里的取数与整理
//! 仍然走工具与模型，不需要为定时任务另写一套执行逻辑。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use harness_kernel::{
    schedule, Block, DocId, DocOp, DocWrite, EffectClass, ErrorCode, Tool, ToolCtx, ToolError,
    ToolOutput, ToolSpec,
};

const MAX_TEXT: usize = 500;

pub struct RemindTool {
    tz_min: i32,
    catalog: Option<Arc<crate::skills::SkillCatalog>>,
    identity: Arc<harness_kernel::Directory>,
}

impl RemindTool {
    pub fn new(tz_min: i32) -> Self {
        Self { tz_min, catalog: None, identity: Arc::new(Default::default()) }
    }

    /// 身份表：把发起者解析成 user，并把提醒记在"这个人"名下。
    pub fn with_identity(mut self, dir: Arc<harness_kernel::Directory>) -> Self {
        self.identity = dir;
        self
    }

    /// 有技能目录时才能建「钉住脚本」的定时任务（到点直接跑脚本，不经过模型）。
    pub fn with_skills(mut self, catalog: Arc<crate::skills::SkillCatalog>) -> Self {
        self.catalog = Some(catalog);
        self
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn fnv(bytes: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 同一个会话 + 同样的内容 + 同样的计划 → 同一条提醒（重复说不会堆叠）。
pub fn reminder_id(conv: &str, text: &str, schedule: &serde_json::Value) -> DocId {
    DocId(format!(
        "reminder-{:016x}",
        fnv(&format!("{conv}|{text}|{schedule}"))
    ))
}

#[async_trait]
impl Tool for RemindTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remind".into(),
            description:
                "安排一个定时任务，形态（优先级：message > skill > 纯 prompt）：\
① `message`：到点**只发这句文本**，不跑模型、不跑脚本。适合“提醒我周五穿红色校服”；\
② `skill`：到点跑一个已安装技能，stdout 原样发回（格式固定、时延稳定）；\
②b `skill` + `polish`：技能只负责**拉数据**，拿到 stdout 后交给模型按 polish 指令加工（翻译/分类/排版）——\
   “数据靠脚本、表达靠模型”的场景用这个；\
③ 都不填：到点把 `text` 当作用户消息重新跑一次（适合需要临场判断的开放任务）。\
用户在说“每天/工作日/每周 X 的几点”时用它，不要自己 sleep 等待。\
时间要自己算成绝对时间（消息里带“现在”），一次性的用 once + at。"
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "到点后要执行/汇报的内容，写成一句完整的指令"
                    },
                    "message": {
                        "type": "string",
                        "description": "可选：到点要原样发出的提醒文本（简单提醒用这个，不跑模型）"
                    },
                    "polish": {
                        "type": "string",
                        "description": "可选（配合 skill）：脚本跑完后，把 stdout 交给模型加工的指令。用于\"脚本固定拉数据 + 模型翻译/分类/排版\""
                    },
                    "skill": {
                        "type": "object",
                        "description": "可选：钉住一个已安装技能，到点直接跑它（不经过模型）",
                        "properties": {
                            "name": { "type": "string" },
                            "args": { "type": "object" }
                        },
                        "required": ["name"]
                    },
                    "schedule": {
                        "type": "object",
                        "description": "本地时间。kind=weekdays 即周一至周五",
                        "properties": {
                            "kind": { "type": "string", "enum": ["once", "daily", "weekdays", "weekly"] },
                            "time": { "type": "string", "description": "HH:MM，本地时间" },
                            "days": {
                                "type": "array",
                                "items": { "type": "integer" },
                                "description": "weekly 用：0=周日 … 6=周六"
                            },
                            "at": { "type": "string", "description": "once 用：YYYY-MM-DDTHH:MM" }
                        },
                        "required": ["kind"]
                    }
                },
                "required": ["text", "schedule"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 同一 (会话, 文本, 计划) 得到同一个 doc id，重复安排等价
        EffectClass::Idempotent { key: "remind".into() }
    }

    fn domain(&self) -> harness_kernel::Domain {
        // 会安排未来的自主执行：属于"改自身行为"，确认一次
        harness_kernel::Domain::SelfModify
    }

    /// 审批卡必须回答"到底什么时候触发"——只给文案等于让人盲批。
    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let raw = args.get("schedule")?;
        let sched = schedule::parse(raw, self.tz_min).ok()?;
        let next = sched.next_after(now_ms(), self.tz_min)?;
        let when = schedule::format_with_weekday(next, self.tz_min);
        let rule = sched.describe();
        if let Some(m) = args.get("message").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty())
        {
            return Some(format!("静态提醒 · {when}（{rule}）\n{m}"));
        }
        if let Some(name) = args
            .get("skill")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
        {
            let extra = if args.get("polish").and_then(|v| v.as_str()).is_some() {
                "，再交给模型翻译/分类"
            } else {
                ""
            };
            return Some(format!("到点跑技能 `{name}`{extra} · {when}（{rule}）"));
        }
        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
        Some(format!("到点让模型执行 · {when}（{rule}）\n{text}"))
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 text", false))?
            .to_owned();
        if text.chars().count() > MAX_TEXT {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("text 太长（上限 {MAX_TEXT} 字），请压缩成一句指令"),
                false,
            ));
        }
        let raw = args
            .get("schedule")
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 schedule", false))?;
        let sched = schedule::parse(raw, self.tz_min)
            .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;

        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(m) = &message {
            if m.chars().count() > MAX_TEXT {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("message 太长（上限 {MAX_TEXT} 字）"),
                    false,
                ));
            }
        }
        let polish = args
            .get("polish")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(p) = &polish {
            if p.chars().count() > 2_000 {
                return Err(ToolError::new(ErrorCode::InvalidArgs, "polish 太长（上限 2000 字）", false));
            }
            if args.get("skill").is_none() {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    "polish 只配合 skill 使用（先有脚本产出，才有东西可加工）",
                    false,
                ));
            }
        }
        let skill = args.get("skill").cloned();
        if let Some(s) = &skill {
            let name = s
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "skill.name 必填", false))?;
            match (&self.catalog, self.catalog.as_ref().and_then(|c| c.get(name))) {
                (_, Some(_)) => {}
                (Some(_), None) => {
                    return Err(ToolError::new(
                        ErrorCode::NotFound,
                        format!("没有这个技能：{name}（不能钉住不存在的脚本）"),
                        false,
                    ));
                }
                (None, None) => {
                    return Err(ToolError::new(
                        ErrorCode::Unavailable,
                        "当前没有技能目录，无法创建钉住脚本的任务",
                        false,
                    ));
                }
            }
        }

        let now = now_ms();
        let next = sched.next_after(now, self.tz_min).ok_or_else(|| {
            ToolError::new(
                ErrorCode::InvalidArgs,
                "这个时间已经过去了；一次性任务请给将来的时间",
                false,
            )
        })?;

        // conv 形如 `telegram:81865154:2`
        let mut parts = ctx.conv.0.split(':');
        let channel = parts.next().unwrap_or("telegram").to_owned();
        let chat = parts.next().unwrap_or_default().to_owned();
        if chat.is_empty() {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "无法从会话标识里取到聊天 id",
                false,
            ));
        }

        // 提醒是"自己提醒自己"：记在 user 名下，默认投自己的通道
        let user = self.identity.resolve(&channel, &chat);
        let mut targets: Vec<serde_json::Value> = self
            .identity
            .channels(&user)
            .into_iter()
            .map(|b| serde_json::json!({ "channel": b.channel, "addr": b.addr }))
            .collect();
        if targets.is_empty() {
            targets.push(serde_json::json!({ "channel": channel, "addr": chat }));
        }

        let id = reminder_id(&ctx.conv.0, &text, raw);
        let doc = DocWrite {
            id: id.clone(),
            kind: "reminder".into(),
            scope: "family".into(),
            payload: serde_json::json!({
                "text": text,
                "schedule": raw,
                "skill": skill,
                "polish": polish,
                "message": message,
                "next_at": next,
                "channel": channel,
                "chat": chat,
                "actor": ctx.actor,
                "user": user,
                "targets": targets,
                "created_at": now,
            }),
            version: 1,
            target: None,
            op: DocOp::Upsert,
        };

        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已安排：{}（下次 {}）\n内容：{}\n取消：/reminders del {}",
                sched.describe(),
                schedule::format_local(next, self.tz_min),
                text,
                &id.0
            ))],
            doc_writes: vec![doc],
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_reminder_gets_same_id() {
        let s = serde_json::json!({"kind":"weekdays","time":"09:00"});
        assert_eq!(reminder_id("c", "汇总", &s), reminder_id("c", "汇总", &s));
        assert_ne!(reminder_id("c", "汇总", &s), reminder_id("c2", "汇总", &s));
    }

    #[test]
    fn approval_hint_shows_resolved_time() {
        let tool = RemindTool::new(480);
        let hint = tool
            .approval_hint(&serde_json::json!({
                "text": "穿校服",
                "message": "明天记得穿红色校服",
                "schedule": {"kind":"once","at":"2030-01-01T20:00"}
            }))
            .unwrap();
        assert!(hint.contains("静态提醒"), "{hint}");
        assert!(hint.contains("2030-01-01 20:00"), "必须写出具体时间：{hint}");
        assert!(hint.contains("明天记得穿红色校服"), "{hint}");
        assert!(!hint.contains("参数"), "{hint}");
    }

    #[test]
    fn approval_hint_marks_skill_and_prompt_forms() {
        let tool = RemindTool::new(480);
        let h = tool
            .approval_hint(&serde_json::json!({
                "text": "跑一下",
                "skill": {"name":"hn-top"},
                "schedule": {"kind":"weekdays","time":"09:00"}
            }))
            .unwrap();
        assert!(h.contains("hn-top") && h.contains("工作日"), "{h}");
        let h = tool
            .approval_hint(&serde_json::json!({
                "text": "整理日程",
                "schedule": {"kind":"daily","time":"08:00"}
            }))
            .unwrap();
        assert!(h.contains("让模型执行"), "{h}");
    }

    #[tokio::test]
    async fn pinned_skill_must_exist() {
        use harness_kernel::{ConversationId, RunId, Workspace};
        let dir = tempfile::tempdir().unwrap();
        let cat = crate::skills::SkillCatalog::new(dir.path());
        let tool = RemindTool::new(480).with_skills(cat);
        let env = crate::NullEnv;
        let ws = Workspace { run: RunId("r".into()), root: "/tmp".into() };
        let ctx = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: ConversationId("telegram:1:0".into()),
            actor: "42",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
            run: RunId("r".into()),
            call: harness_kernel::CallId("c".into()),
            workspace: &ws,
            env: &env,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let err = tool
            .execute(
                serde_json::json!({
                    "text": "跑一下",
                    "schedule": {"kind":"daily","time":"09:00"},
                    "skill": {"name":"不存在的技能"}
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("没有这个技能"), "{}", err.message);
    }

    #[test]
    fn weekday_schedule_is_accepted() {
        let tz = 480;
        let s = schedule::parse(&serde_json::json!({"kind":"weekdays","time":"09:00"}), tz).unwrap();
        assert!(s.next_after(now_ms(), tz).is_some());
    }
}
