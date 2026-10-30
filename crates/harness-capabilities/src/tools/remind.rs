//! `remind`：安排 / 修改 / 取消一个将来会被执行的提示（一次性或周期）。
//!
//! 存储走 doc 容器（kind=`reminder`）而不是新表：提醒就是"应用语义的结论"，
//! 与记忆同源、可审计（DocUpserted 进账本）、`/forget` 那套删除能力直接复用。
//! 到点由内核的调度器起一次普通 run —— 所以"每天 9 点汇总新闻"里的取数与整理
//! 仍然走工具与模型，不需要为定时任务另写一套执行逻辑。
//!
//! 两个工具分工（2026-10-28 加）：
//! - `remind`（写：add/update/del）→ `self_modify` 域，要审批；
//! - `reminders`（读：list/get）→ 只读，不审批。
//!
//! 为什么必须拆：`domain()` 是按工具的，不分参数。若把 list 也塞进 `remind`，
//! 查一眼自己的任务也得弹审批卡，等于逼人橡皮图章（`DECISIONS.md` 的教训）。
//!
//! 为什么必须能改：此前只能 add，模型想改一个参数就只剩"重建"这一条路，
//! 而 `reminder_id` 含 text/schedule 的哈希、conv 里还带 epoch，
//! 重建极易算不出同一个 id → 每天收到两份日报（板上真实发生）。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use harness_kernel::{
    schedule, Block, Doc, DocId, DocOp, DocWrite, EffectClass, ErrorCode, Tool, ToolCtx, ToolError,
    ToolOutput, ToolSpec,
};

const MAX_TEXT: usize = 500;
const MAX_LIST: usize = 20;
const KIND: &str = "reminder";


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

/// `ctx.conv` 形如 `telegram:<chat>:<epoch>`；取回 (channel, chat)。
fn conv_parts(ctx: &ToolCtx<'_>) -> (String, String) {
    let mut parts = ctx.conv.0.split(':');
    let channel = parts.next().unwrap_or("telegram").to_owned();
    let chat = parts.next().unwrap_or_default().to_owned();
    (channel, chat)
}

/// 提醒是"自己提醒自己"：只看得到、改得动自己名下的。
fn is_mine(d: &Doc, me: &str, actor: &str) -> bool {
    d.payload["user"]
        .as_str()
        .map(|u| u == me)
        .unwrap_or_else(|| d.payload["actor"].as_str() == Some(actor))
}

fn trunc(s: &str, n: usize) -> String {
    let one: String = s.lines().next().unwrap_or("").chars().take(n).collect();
    one
}

/// 单条提醒的完整描述：规则、下次时间、技能与参数、加工指令、内容。
/// `skill.args` 必须显示——否则模型（和主人）都无从知道这条任务当前用的什么参数。
/// `message` 与 `text` 不是"两种可选写法"，是**互斥的执行模式**：有 `message` 就到点静态直发，
/// 不跑模型、不跑技能——`text` 里写的"调钉钉"永远不会执行。两者并存时必须在回执与
/// `reminders get` 里说清楚，否则模型会以为"改了没生效"，然后删了重建（板上真实发生）。
fn mode_note(payload: &serde_json::Value) -> Option<String> {
    let has_msg = payload["message"].as_str().is_some_and(|s| !s.trim().is_empty());
    let has_text = payload["text"].as_str().is_some_and(|s| !s.trim().is_empty());
    if has_msg {
        Some(if has_text {
            "生效：静态 message（**不跑模型/技能**）——text 里的指令不会执行".to_string()
        } else {
            "生效：静态 message".to_string()
        })
    } else if payload["skill"].is_object() {
        Some("生效：到点跑技能（不跑模型）".to_string())
    } else {
        Some("生效：text（到点跑一轮模型）".to_string())
    }
}

fn describe(d: &Doc, tz_min: i32) -> String {
    let desc = schedule::parse(&d.payload["schedule"], tz_min)
        .map(|s| s.describe())
        .unwrap_or_else(|_| "?".into());
    let next = d.payload["next_at"].as_i64().unwrap_or(0);
    let mut out = format!(
        "- `{}` · {desc} · 下次 {}\n",
        d.id.0,
        schedule::format_local(next, tz_min)
    );
    if let Some(name) = d.payload["skill"]["name"].as_str() {
        let args = &d.payload["skill"]["args"];
        let args = if args.is_null() {
            "{}".to_owned()
        } else {
            args.to_string()
        };
        out.push_str(&format!("  技能 `{name}` · 参数 {args}\n"));
    }
    if let Some(p) = d.payload["polish"].as_str() {
        out.push_str(&format!("  加工：{}\n", trunc(p, 120)));
    }
    if let Some(m) = d.payload["message"].as_str() {
        out.push_str(&format!("  到点原样发出：{}\n", trunc(m, 120)));
    }
    // 投递目标：多通道之后必须能看出来这条会发到哪（D27）
    let ch = d.payload["channel"].as_str().unwrap_or("");
    let addr = d.payload["chat"].as_str().unwrap_or("");
    if !ch.is_empty() {
        out.push_str(&format!("  投到：{ch} · {addr}\n"));
    }
    let text = d.payload["text"].as_str().unwrap_or("?");
    out.push_str(&format!("  内容：{text}\n"));
    if let Some(note) = mode_note(&d.payload) {
        out.push_str(&format!("  {note}\n"));
        if d.payload["message"].as_str().is_some_and(|s| !s.trim().is_empty()) && !text.is_empty() {
            out.push_str(
                "  ⚠ 同时有 message 与 text：到点只发 message，text 里的动作（如调钉钉）不会执行\n",
            );
        }
    }
    out
}

#[async_trait]
impl Tool for RemindTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remind".into(),
            description:
                "安排/修改/取消一个定时任务。**三种形态互斥，优先级 message > skill > text**：\
① `message`：到点**只发这句文本**，不跑模型、不跑脚本。适合“提醒我周五穿红色校服”；\
② `skill`：到点跑一个已安装技能，stdout 原样发回（格式固定、时延稳定）；\
②b `skill` + `polish`：技能只负责**拉数据**，拿到 stdout 后交给模型按 polish 指令加工（翻译/分类/排版）——\
   “数据靠脚本、表达靠模型”的场景用这个；\
③ 都不填：到点把 `text` 当作用户消息重新跑一次（适合需要临场判断的开放任务）。\
用户在说“每天/工作日/每周 X 的几点”时用它，不要自己 sleep 等待。\
时间要自己算成绝对时间（消息里带“现在”），一次性的用 once + at。\n\
`action=update`：只写要改的字段（text/schedule/skill/polish/message），其余保持不变。\
**改文案前先看这条是什么形态**：`reminders get` 会打印「生效：…」。\
若生效形态是 `text`（要到点跑模型、发钉钉），就改 `text`；\
只给 `message` 会把它**降级成静态直发**，原来 text 里的钉钉调用将不再执行。\
改了技能参数就用它，**不要删了重建**（重建会因 id 哈希变了而变成两条任务）。\
`action=del`：取消一条任务。\n\
查看已有任务（含技能参数、加工指令）用 `reminders`。"
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "update", "del"],
                        "description": "默认 add；update / del 必须给 id"
                    },
                    "id": {
                        "type": "string",
                        "description": "任务 id（先 reminders list 拿到），update / del 必填"
                    },
                    "text": {
                        "type": "string",
                        "description": "到点后要执行/汇报的内容，写成一句完整的指令"
                    },
                    "channel": {
                        "type": "string",
                        "description": "可选：提醒投到哪个通道（如 telegram / weixin）。不填就用当前通道——也就是你现在说话的这个，等价于\"最近活跃\""
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
                "required": []
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // add 本身幂等（同一 (会话, 文本, 计划) 得到同一个 doc id）；
        // 但 update/del 不在幂等集合里，崩溃后重放会改错/误删 → 整体按 External 处理
        EffectClass::External
    }

    fn domain(&self) -> harness_kernel::Domain {
        // 会安排未来的自主执行：属于"改自身行为"，确认一次
        harness_kernel::Domain::SelfModify
    }

    fn tier(&self) -> u8 {
        // 子 agent 的会话标识是 `sub:<父会话>#<n>`，取不到真实 chat id：
        // 在那里建提醒会投递到错误目标，所以只给主会话
        1
    }

    /// 审批卡必须回答"到底改了什么/取消哪一条"——只给文案等于让人盲批。
    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        match args.get("action").and_then(|v| v.as_str()).unwrap_or("add") {
            "del" => {
                let id = args["id"].as_str().unwrap_or("?");
                return Some(format!(
                    "取消定时任务 `{id}`\n（不确定内容就先 reminders get 核对）"
                ));
            }
            "update" => {
                let id = args["id"].as_str().unwrap_or("?");
                let mut what: Vec<&str> = Vec::new();
                for k in ["text", "schedule", "skill", "polish", "message"] {
                    if args.get(k).map(|v| !v.is_null()).unwrap_or(false) {
                        what.push(k);
                    }
                }
                let fields = if what.is_empty() {
                    "（没给任何字段）".to_owned()
                } else {
                    what.join(" / ")
                };
                let mut out = format!("修改定时任务 `{id}` · 改 {fields}");
                let hint_sched = args
                    .get("schedule")
                    .and_then(|raw| schedule::parse(raw, self.tz_min).ok())
                    .and_then(|s| s.next_after(now_ms(), self.tz_min).map(|next| (s, next)));
                if let Some((sched, next)) = hint_sched {
                    out.push_str(&format!(
                        "\n新计划：{}（{}）",
                        sched.describe(),
                        schedule::format_with_weekday(next, self.tz_min)
                    ));
                }
                if let Some(name) = args.get("skill").and_then(|s| s.get("name")).and_then(|v| v.as_str()) {
                    out.push_str(&format!("\n新技能：`{name}`"));
                }
                return Some(out);
            }
            _ => {}
        }
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
        match args.get("action").and_then(|v| v.as_str()).unwrap_or("add") {
            "add" => self.add(args, ctx).await,
            "update" => self.update(args, ctx).await,
            "del" => self.delete(args, ctx).await,
            other => Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("action 只能是 add/update/del，收到 {other}"),
                false,
            )),
        }
    }
}

impl RemindTool {
    async fn add(
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

        // 提醒是"自己提醒自己"：记在 user 名下。
        // 投递目标默认就是**当前通道**——你能建提醒，说明你刚在这个通道说过话，
        // 所以它天然就是"最近活跃"的那个绑定。要投别处就显式给 channel（D27）。
        let user = self.identity.resolve(&channel, &chat);
        let bindings = self.identity.channels(&user);
        let (target_channel, target_addr) = match args.get("channel").and_then(|v| v.as_str()) {
            Some(want) if !want.trim().is_empty() && want != channel => {
                let b = bindings
                    .iter()
                    .find(|b| b.channel == want.trim())
                    .ok_or_else(|| {
                        let have: Vec<String> = bindings
                            .iter()
                            .map(|b| format!("{}:{}", b.channel, b.addr))
                            .collect();
                        ToolError::new(
                            ErrorCode::InvalidArgs,
                            format!(
                                "这个用户没有绑定通道 {want}。可用：{}",
                                if have.is_empty() { "（无）".into() } else { have.join("、") }
                            ),
                            false,
                        )
                    })?;
                (b.channel.clone(), b.addr.clone())
            }
            _ => (channel.clone(), chat.clone()),
        };
        // 只投一个目标：多投会让一句话同时打到 TG 和微信，而微信那边每条都是额度。
        let targets: Vec<serde_json::Value> =
            vec![serde_json::json!({ "channel": target_channel, "addr": target_addr })];

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
                "channel": target_channel,
                "chat": target_addr,
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

    /// 取一条属于自己的提醒；update/del 共用。
    async fn load_own(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<Doc, ToolError> {
        let id = args["id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::new(
                    ErrorCode::InvalidArgs,
                    "需要 id（先 reminders list 拿，别猜）",
                    false,
                )
            })?;
        let doc = ctx
            .docs
            .get(id)
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?
            .ok_or_else(|| {
                ToolError::new(ErrorCode::NotFound, format!("没有这条定时任务：{id}"), false)
            })?;
        if doc.kind != KIND {
            return Err(ToolError::new(
                ErrorCode::NotFound,
                format!("{id} 不是定时任务（kind={}）", doc.kind),
                false,
            ));
        }
        let (channel, chat) = conv_parts(ctx);
        let me = self.identity.resolve(&channel, &chat);
        if !is_mine(&doc, &me, ctx.actor) {
            return Err(ToolError::new(
                ErrorCode::NotPermitted,
                "只能查看/修改/取消自己的定时任务",
                false,
            ));
        }
        Ok(doc)
    }

    /// 只改给定字段，id / 归属 / 投递目标不变——比"删了重建"安全得多。
    async fn update(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let doc = self.load_own(&args, ctx).await?;
        let mut payload = doc.payload.clone();
        let mut changed: Vec<&str> = Vec::new();
        let now = now_ms();

        if let Some(t) = args
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if t.chars().count() > MAX_TEXT {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("text 太长（上限 {MAX_TEXT} 字）"),
                    false,
                ));
            }
            payload["text"] = serde_json::json!(t);
            changed.push("text");
        }

        if let Some(raw) = args.get("schedule").filter(|v| !v.is_null()) {
            schedule::parse(raw, self.tz_min)
                .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
            payload["schedule"] = raw.clone();
            changed.push("schedule");
        }

        // message / polish：给空串视为清除（提醒从"跑技能"退回"跑模型"要能改回去）
        for key in ["message", "polish"] {
            if let Some(v) = args.get(key) {
                let val = v.as_str().map(str::trim).filter(|s| !s.is_empty());
                payload[key] = match val {
                    Some(s) => serde_json::json!(s),
                    None => serde_json::Value::Null,
                };
                changed.push(key);
            }
        }

        if let Some(skill) = args.get("skill") {
            if skill.is_null() {
                payload["skill"] = serde_json::Value::Null;
            } else {
                let name = skill.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::new(ErrorCode::InvalidArgs, "skill.name 必填", false)
                })?;
                if let Some(cat) = &self.catalog {
                    if cat.get(name).is_none() {
                        return Err(ToolError::new(
                            ErrorCode::NotFound,
                            format!("没有这个技能：{name}"),
                            false,
                        ));
                    }
                }
                payload["skill"] = skill.clone();
            }
            changed.push("skill");
        }

        if changed.is_empty() {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "update 至少要给一个要改的字段（text/schedule/skill/polish/message）",
                false,
            ));
        }
        if payload["polish"].is_string() && !payload["skill"].is_object() {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "polish 只配合 skill 使用（先有脚本产出，才有东西可加工）",
                false,
            ));
        }

        let sched = schedule::parse(&payload["schedule"], self.tz_min)
            .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
        let next = sched.next_after(now, self.tz_min).ok_or_else(|| {
            ToolError::new(
                ErrorCode::InvalidArgs,
                "这个时间已经过去了；一次性任务请给将来的时间",
                false,
            )
        })?;
        payload["next_at"] = serde_json::json!(next);
        payload["updated_at"] = serde_json::json!(now);

        let write = DocWrite {
            id: doc.id.clone(),
            kind: KIND.into(),
            scope: doc.scope.clone(),
            payload,
            version: doc.version + 1,
            target: None,
            op: DocOp::Upsert,
        };
        let after = Doc {
            id: write.id.clone(),
            kind: write.kind.clone(),
            scope: write.scope.clone(),
            payload: write.payload.clone(),
            version: write.version,
            valid_from: now,
            valid_to: None,
        };
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已修改 `{}`：改了 {}\n{}\n{}",
                write.id.0,
                changed.join(" / "),
                mode_note(&write.payload).unwrap_or_default(),
                describe(&after, self.tz_min)
            ))],
            doc_writes: vec![write],
            artifacts: Vec::new(),
        })
    }

    async fn delete(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let doc = self.load_own(&args, ctx).await?;
        let removed = ctx
            .docs
            .delete(&doc.id.0)
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
        if !removed {
            return Err(ToolError::new(
                ErrorCode::NotFound,
                format!("没有这条定时任务：{}", doc.id.0),
                false,
            ));
        }
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已取消 `{}`\n{}",
                doc.id.0,
                describe(&doc, self.tz_min)
            ))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

/// `reminders`：只读。看得到 id、规则、下次时间、**技能与参数**、加工指令。
///
/// 和 `remind` 拆开是因为 `domain()` 按工具粒度：查一眼也要审批的话，
/// 人只会无脑点「同意」，真正的危险动作反而被放松（`AGENTS.md` §8 的教训）。
pub struct RemindersTool {
    tz_min: i32,
    identity: Arc<harness_kernel::Directory>,
}

impl RemindersTool {
    pub fn new(tz_min: i32) -> Self {
        Self { tz_min, identity: Arc::new(Default::default()) }
    }

    pub fn with_identity(mut self, dir: Arc<harness_kernel::Directory>) -> Self {
        self.identity = dir;
        self
    }
}

#[async_trait]
impl Tool for RemindersTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "reminders".into(),
            description: "列出/查看自己的定时任务：id、触发规则、下次时间、技能与参数、加工指令、内容。\n\
                用法：list（全部）、get（一条，需要 id）。\n\
                改参数用 remind(action=\"update\", id=…)；取消用 remind(action=\"del\", id=…)。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get"] },
                    "id": { "type": "string", "description": "任务 id，get 必填" }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    fn tier(&self) -> u8 {
        // 同 remind：子 agent 没有真实 chat id，看不出"自己的"提醒
        1
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let (channel, chat) = conv_parts(ctx);
        let me = self.identity.resolve(&channel, &chat);
        let action = args["action"].as_str().unwrap_or("list");
        let id = args["id"].as_str().map(str::trim).unwrap_or("");

        let text = match action {
            "list" => {
                let mut docs = ctx
                    .docs
                    .list(KIND, 100)
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
                docs.retain(|d| is_mine(d, &me, ctx.actor));
                docs.sort_by_key(|d| d.payload["next_at"].as_i64().unwrap_or(i64::MAX));
                if docs.is_empty() {
                    "还没有定时任务。".to_owned()
                } else {
                    let total = docs.len();
                    let mut out = format!("定时任务（{total} 条）：\n");
                    for d in docs.iter().take(MAX_LIST) {
                        out.push_str(&describe(d, self.tz_min));
                    }
                    if total > MAX_LIST {
                        out.push_str(&format!("…（还有 {} 条未显示）\n", total - MAX_LIST));
                    }
                    out.push_str("\n改参数：remind(action=\"update\", id=…)；取消：remind(action=\"del\", id=…)");
                    out
                }
            }
            "get" => {
                if id.is_empty() {
                    return Err(ToolError::new(ErrorCode::InvalidArgs, "get 需要 id", false));
                }
                let doc = ctx
                    .docs
                    .get(id)
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?
                    .ok_or_else(|| {
                        ToolError::new(ErrorCode::NotFound, format!("没有这条定时任务：{id}"), false)
                    })?;
                if !is_mine(&doc, &me, ctx.actor) {
                    return Err(ToolError::new(
                        ErrorCode::NotPermitted,
                        "只能查看自己的定时任务",
                        false,
                    ));
                }
                describe(&doc, self.tz_min)
            }
            other => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("action 只能是 list/get，收到 {other}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 只在测试里用的文档存储：看 list/get/delete 有没有被正确调用。
    #[derive(Default)]
    struct FakeDocs {
        docs: Mutex<Vec<Doc>>,
        deleted: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl harness_kernel::DocReader for FakeDocs {
        async fn list(&self, kind: &str, limit: usize) -> Result<Vec<Doc>, String> {
            Ok(self
                .docs
                .lock()
                .unwrap()
                .iter()
                .filter(|d| d.kind == kind)
                .take(limit)
                .cloned()
                .collect())
        }
        async fn get(&self, id: &str) -> Result<Option<Doc>, String> {
            Ok(self
                .docs
                .lock()
                .unwrap()
                .iter()
                .find(|d| d.id.0 == id)
                .cloned())
        }
        async fn delete(&self, id: &str) -> Result<bool, String> {
            self.deleted.lock().unwrap().push(id.to_owned());
            let mut g = self.docs.lock().unwrap();
            let before = g.len();
            g.retain(|d| d.id.0 != id);
            Ok(g.len() != before)
        }
    }

    fn doc(id: &str, user: &str, text: &str, skill: serde_json::Value) -> Doc {
        Doc {
            id: DocId(id.into()),
            kind: KIND.into(),
            scope: "family".into(),
            payload: serde_json::json!({
                "text": text,
                "schedule": {"kind": "weekdays", "time": "09:00"},
                "skill": skill,
                "polish": "翻译成中文日报",
                "next_at": 0,
                "user": user,
                "actor": user,
            }),
            version: 1,
            valid_from: 0,
            valid_to: None,
        }
    }

    fn text_of(out: &ToolOutput) -> String {
        match &out.content[0] {
            Block::Paragraph(t) => t.clone(),
            other => panic!("{other:?}"),
        }
    }

    /// 技能参数必须出现在列表里：否则"技能升级了、任务还用旧参数"看不出来（板上事故）。
    #[tokio::test]
    async fn list_shows_skill_args_and_hides_other_users() {
        let docs = FakeDocs::default();
        docs.docs.lock().unwrap().push(doc(
            "reminder-1",
            "1000",
            "汇总 HN 首页",
            serde_json::json!({"name": "hn-top", "args": {"sort": "score"}}),
        ));
        docs.docs.lock().unwrap().push(doc(
            "reminder-2",
            "9999",
            "别人的任务",
            serde_json::json!({"name": "x"}),
        ));

        let env = crate::NullEnv;
        let ws = harness_kernel::Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp".into() };
        let ctx = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: harness_kernel::ConversationId("telegram:1000:1".into()),
            actor: "1000",
            outbox: &harness_kernel::NullOutbox,
            docs: &docs,
            run: harness_kernel::RunId("r".into()),
            call: harness_kernel::CallId("c".into()),
            workspace: &ws,
            env: &env,
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let out = RemindersTool::new(480)
            .execute(serde_json::json!({"action":"list"}), &ctx)
            .await
            .unwrap();
        let t = text_of(&out);
        assert!(t.contains("reminder-1"), "{t}");
        assert!(t.contains("hn-top"), "必须显示技能名: {t}");
        assert!(t.contains("\"score\""), "必须显示技能参数: {t}");
        assert!(t.contains("翻译成中文日报"), "必须显示加工指令: {t}");
        assert!(!t.contains("reminder-2"), "不该看到别人的任务: {t}");
    }

    /// 改参数只动给的字段：text/计划不变、id 不变、next_at 重算。
    /// 这是替代"删了重建"的路径——重建会因 id 哈希变化产生两条任务。
    #[tokio::test]
    async fn update_patches_only_given_fields() {
        let docs = FakeDocs::default();
        docs.docs.lock().unwrap().push(doc(
            "reminder-1",
            "1000",
            "汇总 HN 首页",
            serde_json::json!({"name": "hn-top", "args": {}}),
        ));

        let env = crate::NullEnv;
        let ws = harness_kernel::Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp".into() };
        let ctx = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: harness_kernel::ConversationId("telegram:1000:1".into()),
            actor: "1000",
            outbox: &harness_kernel::NullOutbox,
            docs: &docs,
            run: harness_kernel::RunId("r".into()),
            call: harness_kernel::CallId("c".into()),
            workspace: &ws,
            env: &env,
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let out = RemindTool::new(480)
            .execute(
                serde_json::json!({
                    "action": "update",
                    "id": "reminder-1",
                    "skill": {"name": "hn-top", "args": {"sort": "score"}}
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.doc_writes.len(), 1);
        let w = &out.doc_writes[0];
        assert_eq!(w.id.0, "reminder-1", "id 必须保持不变");
        assert_eq!(w.payload["text"], "汇总 HN 首页", "没给的字段不能被清掉");
        assert_eq!(w.payload["skill"]["args"]["sort"], "score");
        assert_eq!(w.payload["polish"], "翻译成中文日报");
        assert_eq!(w.version, 2);
        assert!(w.payload["next_at"].as_i64().unwrap() > 0, "next_at 必须重算");
        assert!(text_of(&out).contains("skill"), "回执要说清改了什么");
    }

    /// 改 message 会让 text 模式的提醒降级成静态直发（钉钉那条就断了）。
    /// 回执必须把"生效形态"和"两者并存"说出来，否则模型以为没生效。
    #[tokio::test]
    async fn update_message_warns_about_downgrade() {
        let docs = FakeDocs::default();
        // 手工构造：fixture 的 `doc()` 默认带 polish（会触发"polish 必须配 skill"的校验）
        docs.docs.lock().unwrap().push(harness_kernel::Doc {
            id: harness_kernel::DocId("reminder-1".into()),
            kind: "reminder".into(),
            scope: "family".into(),
            payload: serde_json::json!({
                "chat": "1000",
                "channel": "telegram",
                "user": "1000",
                "actor": "1000",
                "text": "到点调 dingtalk 发通知，再把文本输出到 TG",
                "schedule": {"kind": "once", "at": "2099-01-01T07:00"},
                "next_at": 4_070_905_200_000i64,
            }),
            version: 1,
            valid_from: 0,
            valid_to: None,
        });
        let env = crate::NullEnv;
        let ws = harness_kernel::Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp".into() };
        let ctx = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: harness_kernel::ConversationId("telegram:1000:1".into()),
            actor: "1000",
            outbox: &harness_kernel::NullOutbox,
            docs: &docs,
            run: harness_kernel::RunId("r".into()),
            call: harness_kernel::CallId("c".into()),
            workspace: &ws,
            env: &env,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let out = RemindTool::new(480)
            .execute(
                serde_json::json!({"action":"update","id":"reminder-1","message":"新文案"}),
                &ctx,
            )
            .await
            .unwrap();
        let t = text_of(&out);
        assert!(t.contains("静态 message"), "要说清生效形态：{t}");
        assert!(t.contains("不会执行"), "要警告 text 里的动作不会跑：{t}");
    }

    #[tokio::test]
    async fn del_refuses_other_users_reminder() {
        let docs = FakeDocs::default();
        docs.docs.lock().unwrap().push(doc(
            "reminder-2",
            "9999",
            "别人的任务",
            serde_json::Value::Null,
        ));

        let env = crate::NullEnv;
        let ws = harness_kernel::Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp".into() };
        let ctx = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: harness_kernel::ConversationId("telegram:1000:1".into()),
            actor: "1000",
            outbox: &harness_kernel::NullOutbox,
            docs: &docs,
            run: harness_kernel::RunId("r".into()),
            call: harness_kernel::CallId("c".into()),
            workspace: &ws,
            env: &env,
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let err = RemindTool::new(480)
            .execute(serde_json::json!({"action":"del","id":"reminder-2"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.message.contains("只能"), "{}", err.message);
        assert!(docs.deleted.lock().unwrap().is_empty(), "不能真删");
    }

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
            run: harness_kernel::RunId("r".into()),
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

    /// 默认投「当前通道」——你能建提醒，说明你刚在这个通道说过话（D27）。
    #[tokio::test]
    async fn reminder_defaults_to_current_channel() {
        use harness_kernel::{Binding, ConversationId, Directory, RunId, User, Workspace};
        let dir = Arc::new(Directory {
            users: vec![User {
                id: "owner".into(),
                name: None,
                channels: vec![
                    Binding::parse("telegram:42").unwrap(),
                    Binding::parse("weixin:abc@im.wechat").unwrap(),
                ],
            }],
        });
        let tool = RemindTool::new(480).with_identity(dir);
        let env = crate::NullEnv;
        let ws = Workspace { run: RunId("r".into()), root: "/tmp".into() };
        let ctx = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: ConversationId("telegram:42:0".into()),
            actor: "42",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
            run: RunId("r".into()),
            call: harness_kernel::CallId("c".into()),
            workspace: &ws,
            env: &env,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        // 哪怕这个人还绑了微信，默认也只投当前这条会话所在的通道
        let out = tool
            .execute(
                serde_json::json!({
                    "text": "提醒我带伞",
                    "schedule": {"kind":"once","at":"2030-01-01T20:00"}
                }),
                &ctx,
            )
            .await
            .unwrap();
        let doc = &out.doc_writes[0];
        assert_eq!(doc.payload["channel"], "telegram");
        assert_eq!(doc.payload["chat"], "42");
        assert_eq!(
            doc.payload["targets"].as_array().unwrap().len(),
            1,
            "只投一个目标，别同时打到微信"
        );
    }

    /// 显式指定通道：投到那个绑定；没有的通道要报错并列出可用的。
    #[tokio::test]
    async fn reminder_can_target_another_channel() {
        use harness_kernel::{Binding, ConversationId, Directory, RunId, User, Workspace};
        let dir = Arc::new(Directory {
            users: vec![User {
                id: "owner".into(),
                name: None,
                channels: vec![
                    Binding::parse("telegram:42").unwrap(),
                    Binding::parse("weixin:abc@im.wechat").unwrap(),
                ],
            }],
        });
        let tool = RemindTool::new(480).with_identity(dir);
        let env = crate::NullEnv;
        let ws = Workspace { run: RunId("r".into()), root: "/tmp".into() };
        let ctx = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: ConversationId("telegram:42:0".into()),
            actor: "42",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
            run: RunId("r".into()),
            call: harness_kernel::CallId("c".into()),
            workspace: &ws,
            env: &env,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let base = serde_json::json!({
            "text": "提醒我带伞",
            "schedule": {"kind":"once","at":"2030-01-01T20:00"}
        });
        let mut want = base.clone();
        want["channel"] = serde_json::json!("weixin");
        let out = tool.execute(want, &ctx).await.unwrap();
        let doc = &out.doc_writes[0];
        assert_eq!(doc.payload["channel"], "weixin");
        assert_eq!(doc.payload["chat"], "abc@im.wechat");

        let mut bad = base;
        bad["channel"] = serde_json::json!("feishu");
        let err = tool.execute(bad, &ctx).await.unwrap_err();
        assert!(err.message.contains("没有绑定通道 feishu"), "{}", err.message);
        assert!(err.message.contains("telegram:42"), "要列出可用绑定：{}", err.message);
    }

    #[test]
    fn weekday_schedule_is_accepted() {
        let tz = 480;
        let s = schedule::parse(&serde_json::json!({"kind":"weekdays","time":"09:00"}), tz).unwrap();
        assert!(s.next_after(now_ms(), tz).is_some());
    }
}
