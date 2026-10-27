//! Turn Loop：把 Store 与 TurnDriver 接起来，落实三段式事务。
//!
//! 事务边界（见 `docs/INTERFACES.md §2.1`）：
//! ① `turn.started` / ② `msg.assistant + tool.call_started`（意图）
//! ③ `tool.call_dispatched`（执行前）→ 无事务执行 → `call_finished + docs + msg.tool_result`
//! ④ `turn.finished` / `run.finished`

use std::time::{Duration, Instant};

use crate::driver::{FinishReason, Step, Turn, TurnDriver, TurnSpec};
use crate::error::EngineError;
use crate::effect::EffectClass;
use crate::env::ExecutionEnv;
use crate::error::StoreError;
use crate::event::{EventKind, NewEvent};
use crate::ids::{ConversationId, RunId, Timestamp, TurnId, now_ms};
use crate::record::{Doc, DocWrite, ToolCallRecord, UsageRecord};
use crate::store::Store;
use crate::tool::{ToolCallRequest, ToolCtx};
use crate::tool_registry::ToolRegistry;
use crate::view::{RunStatus, ToolState};
use crate::driver::EventSink;

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub conv: ConversationId,
    pub run: RunId,
    pub text: String,
    pub model: String,
    pub max_turns: u32,
    pub wall_clock_ms: u64,
    pub workspace: crate::tool::Workspace,
    pub system_prompt: Option<String>,
    /// 发起者标识（用于身份解析与审计）
    pub actor: String,
    /// 当轮附带的图片（走多模态；不进历史）
    pub images: Vec<crate::driver::ImagePart>,
    /// 历史裁剪预算（估算字符数）；超出后按"轮"整组丢弃旧内容
    pub context_budget_chars: usize,
}

#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub text: String,
    pub status: RunStatus,
    /// 本轮实际发起过的工具名（按调用顺序）。用于判断"这轮有没有产出"，
    /// 比看渲染事件可靠：渲染只是展示，这里才是账本事实。
    pub tools: Vec<String>,
}

/// 执行工具前的审批门。返回 true 才真正执行。
#[async_trait::async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn ask(
        &self,
        conv: &ConversationId,
        call: &crate::ids::CallId,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<bool, String>;
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("budget exceeded: {0}")]
    Budget(String),
    #[error("approval failed: {0}")]
    Approval(String),
}

pub struct Runner<'a, S: Store, D: TurnDriver> {
    pub store: &'a S,
    pub driver: &'a D,
    pub tools: &'a ToolRegistry,
    pub env: &'a dyn ExecutionEnv,
    pub gate: Option<&'a dyn ApprovalGate>,
    pub outbox: Option<&'a dyn crate::tool::Outbox>,
    pub docs: Option<&'a dyn crate::tool::DocReader>,
}

impl<'a, S: Store, D: TurnDriver> Runner<'a, S, D> {
    pub async fn run(
        &self,
        req: RunRequest,
        sink: EventSink,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<RunOutcome, RunnerError> {
        let conv = req.conv.clone();
        let run = req.run.clone();
        // 0 = 不限：只靠 max_turns 与手工 /cancel 收口（长任务优先）
        let mut deadline = if req.wall_clock_ms == 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(req.wall_clock_ms))
        };
        let mut used: Vec<String> = Vec::new();

        // run.started
        let mut tx = self.store.begin_write().await?;
        tx.append(NewEvent {
            conv: conv.clone(),
            run: run.clone(),
            turn: None,
            kind: EventKind::RunStarted,
            payload: serde_json::json!({ "text": req.text }),
            dedupe_key: Some(format!("run-started-{}", run.0)),
        })
        .await?;
        tx.commit().await?;

        let history = {
            let mut rd = self.store.begin_read().await?;
            rd.messages(conv.clone(), 0, 50).await?.msgs
        };
        let (history, trimmed) = crate::context::select(&history, req.context_budget_chars);
        if trimmed > 0 {
            tracing::info!(trimmed, "context trimmed to budget");
        }

        let turn_sink = sink.clone();
        let mut turn = self
            .driver
            .begin(
                TurnSpec {
                    conv: conv.clone(),
                    run: run.clone(),
                    model: req.model.clone(),
                    max_turns: req.max_turns,
                    wall_clock_ms: req.wall_clock_ms,
                    system_prompt: req.system_prompt.clone(),
                    images: req.images.clone(),
                },
                &history,
                &self.tools.specs(),
                sink,
            )
            .await?;

        let mut turn_no: u32 = 0;
        let mut recorded_usage: usize = 0;

        loop {
            if cancel.is_cancelled() {
                return self.finish_interrupted(&conv, &run, turn_no, "cancelled").await;
            }
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    self.finish_interrupted(&conv, &run, turn_no, "timeout").await?;
                    return Err(RunnerError::Budget("wall clock".into()));
                }
            }
            if turn_no >= req.max_turns {
                self.finish_interrupted(&conv, &run, turn_no, "max_turns").await?;
                return Err(RunnerError::Budget("max turns".into()));
            }
            turn_no += 1;
            let turn_id = TurnId(format!("{}-t{turn_no}", run.0));

            // turn.started
            let mut tx = self.store.begin_write().await?;
            tx.append(NewEvent {
                conv: conv.clone(),
                run: run.clone(),
                turn: Some(turn_id.clone()),
                kind: EventKind::TurnStarted,
                payload: serde_json::json!({ "turn_no": turn_no }),
                dedupe_key: None,
            })
            .await?;
            tx.commit().await?;

            turn_sink.emit(crate::driver::StreamEvent::TurnStarted { turn_no });
            let step = turn.step().await;
            self.record_usage(&mut *turn, &conv, &run, &mut recorded_usage).await?;

            match step {
                Err(EngineError::Cancelled) => {
                    return self.finish_interrupted(&conv, &run, turn_no, "cancelled").await;
                }
                Err(e) => {
                    let mut tx = self.store.begin_write().await?;
                    tx.append(NewEvent {
                        conv: conv.clone(),
                        run: run.clone(),
                        turn: Some(turn_id.clone()),
                        kind: EventKind::TurnFinished,
                        payload: serde_json::json!({ "status": "failed", "error": e.to_string() }),
                        dedupe_key: None,
                    })
                    .await?;
                    tx.append(NewEvent {
                        conv: conv.clone(),
                        run: run.clone(),
                        turn: None,
                        kind: EventKind::RunFinished,
                        payload: serde_json::json!({ "status": "failed" }),
                        dedupe_key: None,
                    })
                    .await?;
                    tx.commit().await?;
                    return Err(e.into());
                }
                Ok(Step::Model { .. }) => continue,
                Ok(Step::Done { text, finish }) => {
                    let mut tx = self.store.begin_write().await?;
                    tx.append(NewEvent {
                        conv: conv.clone(),
                        run: run.clone(),
                        turn: Some(turn_id.clone()),
                        kind: EventKind::MsgAssistant,
                        payload: serde_json::json!({ "text": text }),
                        dedupe_key: None,
                    })
                    .await?;
                    tx.append(NewEvent {
                        conv: conv.clone(),
                        run: run.clone(),
                        turn: Some(turn_id.clone()),
                        kind: EventKind::TurnFinished,
                        payload: serde_json::json!({ "stop_reason": format!("{finish:?}") }),
                        dedupe_key: None,
                    })
                    .await?;
                    tx.append(NewEvent {
                        conv: conv.clone(),
                        run: run.clone(),
                        turn: None,
                        kind: EventKind::RunFinished,
                        payload: serde_json::json!({ "status": "done" }),
                        dedupe_key: None,
                    })
                    .await?;
                    tx.commit().await?;
                    return Ok(RunOutcome { text, status: RunStatus::Done, tools: used });
                }
                Ok(Step::Tools { text, calls }) => {
                    self.persist_assistant_and_intents(&conv, &run, &turn_id, &text, &calls)
                        .await?;

                    for call in &calls {
                        used.push(call.name.clone());
                        if cancel.is_cancelled() {
                            return self.finish_interrupted(&conv, &run, turn_no, "cancelled").await;
                        }
                        self.execute_one(
                            &mut *turn,
                            &conv,
                            &run,
                            &turn_id,
                            call,
                            &req.workspace,
                            &cancel,
                            &mut deadline,
                            &req.actor,
                        )
                        .await?;
                        if cancel.is_cancelled() {
                            return self.finish_interrupted(&conv, &run, turn_no, "cancelled").await;
                        }
                    }

                    let mut tx = self.store.begin_write().await?;
                    tx.append(NewEvent {
                        conv: conv.clone(),
                        run: run.clone(),
                        turn: Some(turn_id.clone()),
                        kind: EventKind::TurnFinished,
                        payload: serde_json::json!({ "stop_reason": "tool_calls" }),
                        dedupe_key: None,
                    })
                    .await?;
                    tx.commit().await?;
                    self.record_usage(&mut *turn, &conv, &run, &mut recorded_usage).await?;
                }
            }
        }
    }

    async fn persist_assistant_and_intents(
        &self,
        conv: &ConversationId,
        run: &RunId,
        turn: &TurnId,
        text: &str,
        calls: &[ToolCallRequest],
    ) -> Result<(), RunnerError> {
        let mut tx = self.store.begin_write().await?;
        let calls_json: Vec<serde_json::Value> = calls
            .iter()
            .map(|c| serde_json::json!({ "call_id": c.call.0, "name": c.name, "args": c.args }))
            .collect();
        tx.append(NewEvent {
            conv: conv.clone(),
            run: run.clone(),
            turn: Some(turn.clone()),
            kind: EventKind::MsgAssistant,
            payload: serde_json::json!({ "text": text, "tool_calls": calls_json }),
            dedupe_key: None,
        })
        .await?;

        let mut records = Vec::with_capacity(calls.len());
        for c in calls {
            let tool = self.tools.get(&c.name).ok_or_else(|| RunnerError::ToolNotFound(c.name.clone()))?;
            let effect = tool.effect();
            records.push(ToolCallRecord {
                call: c.call.clone(),
                name: c.name.clone(),
                args: c.args.clone(),
                idem_key: idem_key(&c.name, &c.args),
                effect,
            });
        }
        tx.open_tool_calls(conv.clone(), run.clone(), Some(turn.clone()), records).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn execute_one(
        &self,
        turn: &mut dyn Turn,
        conv: &ConversationId,
        run: &RunId,
        turn_id: &TurnId,
        call: &ToolCallRequest,
        workspace: &crate::tool::Workspace,
        run_cancel: &tokio_util::sync::CancellationToken,
        deadline: &mut Option<Instant>,
        actor: &str,
    ) -> Result<(), RunnerError> {
        let tool = self.tools.get(&call.name).ok_or_else(|| RunnerError::ToolNotFound(call.name.clone()))?;

        if let Some(gate) = self.gate {
            let asked_at = Instant::now();
            let allowed = gate
                .ask(conv, &call.call, &call.name, &call.args)
                .await
                .map_err(RunnerError::Approval)?;
            // 等人确认的时间不算进 wall clock：预算约束的是模型/工具的耗时，
            // 不是主人回消息的速度（否则走开一会儿回来就超时）。
            if let Some(d) = deadline.as_mut() {
                *d += Instant::now().saturating_duration_since(asked_at);
            }
            if !allowed {
                let mut tx = self.store.begin_write().await?;
                tx.close_tool_call(call.call.clone(), "denied", "用户拒绝".into()).await?;
                tx.append(NewEvent {
                    conv: conv.clone(),
                    run: run.clone(),
                    turn: Some(turn_id.clone()),
                    kind: EventKind::MsgToolResult,
                    payload: serde_json::json!({
                        "call_id": call.call.0, "ok": false, "preview": "已拒绝执行",
                    }),
                    dedupe_key: None,
                })
                .await?;
                tx.commit().await?;
                turn.on_tool_result(call.call.clone(), false, "已拒绝执行".into()).await?;
                return Ok(());
            }
        }

        // 执行前一刻：dispatched
        let mut tx = self.store.begin_write().await?;
        tx.mark_dispatched(call.call.clone()).await?;
        tx.commit().await?;

        let ctx = ToolCtx {
            conv: conv.clone(),
            actor,
            outbox: self.outbox.unwrap_or(&crate::tool::NullOutbox),
            docs: self.docs.unwrap_or(&crate::tool::NullDocReader),
            run: run.clone(),
            call: call.call.clone(),
            workspace,
            env: self.env,
            cancel: run_cancel.clone(),
        };
        // 工具若不理会取消令牌，这里也要能立刻退出
        let result = tokio::select! {
            r = tool.execute(call.args.clone(), &ctx) => r,
            _ = run_cancel.cancelled() => {
                let mut tx = self.store.begin_write().await?;
                tx.abandon_tool_call(call.call.clone(), "cancelled").await?;
                tx.append(NewEvent {
                    conv: conv.clone(),
                    run: run.clone(),
                    turn: Some(turn_id.clone()),
                    kind: EventKind::MsgToolResult,
                    payload: serde_json::json!({
                        "call_id": call.call.0, "ok": false, "preview": "已取消",
                    }),
                    dedupe_key: None,
                })
                .await?;
                tx.commit().await?;
                turn.on_tool_result(call.call.clone(), false, "已取消".into()).await?;
                return Ok(());
            }
        };

        let (ok, preview) = match result {
            Ok(out) => {
                let preview = render_preview(&out.content);
                let mut tx = self.store.begin_write().await?;
                for d in out.doc_writes {
                    tx.upsert_doc(conv.clone(), run.clone(), into_doc(d)).await?;
                }
                tx.close_tool_call(call.call.clone(), "ok", preview.clone()).await?;
                tx.append(NewEvent {
                    conv: conv.clone(),
                    run: run.clone(),
                    turn: Some(turn_id.clone()),
                    kind: EventKind::MsgToolResult,
                    payload: serde_json::json!({
                        "call_id": call.call.0, "ok": true, "preview": preview,
                    }),
                    dedupe_key: None,
                })
                .await?;
                tx.commit().await?;
                (true, preview)
            }
            Err(e) => {
                let preview = e.message.clone();
                let mut tx = self.store.begin_write().await?;
                tx.close_tool_call(call.call.clone(), "error", preview.clone()).await?;
                tx.append(NewEvent {
                    conv: conv.clone(),
                    run: run.clone(),
                    turn: Some(turn_id.clone()),
                    kind: EventKind::MsgToolResult,
                    payload: serde_json::json!({
                        "call_id": call.call.0, "ok": false, "preview": preview,
                    }),
                    dedupe_key: None,
                })
                .await?;
                tx.commit().await?;
                (false, preview)
            }
        };

        turn.on_tool_result(call.call.clone(), ok, preview).await?;
        Ok(())
    }

    async fn record_usage(
        &self,
        turn: &mut dyn Turn,
        conv: &ConversationId,
        run: &RunId,
        recorded: &mut usize,
    ) -> Result<(), RunnerError> {
        let snap = turn.usage();
        while *recorded < snap.records.len() {
            let u: UsageRecord = snap.records[*recorded].clone();
            let mut tx = self.store.begin_write().await?;
            tx.record_usage(conv.clone(), run.clone(), u).await?;
            tx.commit().await?;
            *recorded += 1;
        }
        Ok(())
    }

    async fn finish_interrupted(
        &self,
        conv: &ConversationId,
        run: &RunId,
        turn_no: u32,
        reason: &str,
    ) -> Result<RunOutcome, RunnerError> {
        // 未完成的调用一律 abandoned，避免下次启动被误判为崩溃。
        let open = {
            let mut rd = self.store.begin_read().await?;
            rd.tool_calls(conv.clone()).await?
        };
        let mut tx = self.store.begin_write().await?;
        for row in open {
            if row.status == "intent" || row.status == "dispatched" {
                tx.abandon_tool_call(row.call, reason).await?;
            }
        }
        if turn_no > 0 {
            tx.append(NewEvent {
                conv: conv.clone(),
                run: run.clone(),
                turn: None,
                kind: EventKind::TurnFinished,
                payload: serde_json::json!({ "status": reason }),
                dedupe_key: None,
            })
            .await?;
        }
        tx.append(NewEvent {
            conv: conv.clone(),
            run: run.clone(),
            turn: None,
            kind: EventKind::RunFinished,
            payload: serde_json::json!({ "status": reason }),
            dedupe_key: None,
        })
        .await?;
        tx.commit().await?;
        Ok(RunOutcome { text: String::new(), status: RunStatus::Cancelled, tools: Vec::new() })
    }
}

fn into_doc(w: DocWrite) -> Doc {
    Doc {
        id: w.id,
        kind: w.kind,
        scope: w.scope,
        payload: w.payload,
        version: w.version,
        valid_from: now_ms(),
        valid_to: None,
    }
}

/// 工具输出进上下文/账本的上限。
///
/// 这个值以前是 2000——模型**永远看不到**超过 2k 的工具输出，
/// 大 JSON 被切在半截，于是它以为"数据被截断了"，反复重试同一个接口。
/// 工具侧已经自己控制体积（结构摘要/正文预算），这里放宽到 12k 并在截断时明确说明。
const PREVIEW_CAP: usize = 12_000;

fn render_preview(blocks: &[crate::view::Block]) -> String {
    use crate::view::Block;
    let mut s = String::new();
    for b in blocks {
        match b {
            Block::Paragraph(t) => s.push_str(t),
            Block::Code { text, .. } => s.push_str(text),
            Block::Image { alt, .. } => s.push_str(&format!("[image {alt}]")),
            Block::Divider => {}
            Block::ToolCall { name, state, .. } => {
                s.push_str(&format!("[{name} {state:?}]"));
            }
        }
        s.push('\n');
    }
    if s.chars().count() > PREVIEW_CAP {
        let cut: String = s.chars().take(PREVIEW_CAP).collect();
        return format!(
            "{cut}\n\n…（工具输出超过 {PREVIEW_CAP} 字，已被截断；\
             大结果通常会落盘到 /ws/.tool-results/，用 exec 读取）"
        );
    }
    s.trim().to_owned()
}

/// 幂等键：名字 + 规范化参数。与 call id 解耦，跨重新规划仍有效。
fn idem_key(name: &str, args: &serde_json::Value) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes().iter().chain(args.to_string().as_bytes()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

#[allow(dead_code)]
fn _unused(_: Timestamp, _: EffectClass, _: ToolState, _: FinishReason) {}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::view::Block;

    #[test]
    fn long_tool_output_is_capped_with_a_marker() {
        let long = "x".repeat(PREVIEW_CAP + 500);
        let out = render_preview(&[Block::Paragraph(long)]);
        assert!(out.chars().count() > PREVIEW_CAP, "应保留 cap 内容再加说明");
        assert!(out.contains("已被截断"), "截断必须有明确标记");
        assert!(out.contains(".tool-results"), "要告诉模型去哪里找全文");
    }

    #[test]
    fn mid_size_output_is_not_cut() {
        let body = "y".repeat(6_000);
        let out = render_preview(&[Block::Paragraph(body)]);
        assert_eq!(out.chars().count(), 6_000, "6k 的输出不该被截");
    }
}
