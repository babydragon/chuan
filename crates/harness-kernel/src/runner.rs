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
use crate::lanes::{Lane, Lanes};
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
    /// 整轮上下文的 token 预算（system + 工具 + 历史 + 当前消息）
    pub context_budget_tokens: usize,
    /// 发起者的主体角色（D30）。审批门据此决定"发卡"还是"直接 Blocked"。
    pub role: crate::identity::ActorRole,
    /// 这一轮是定时任务触发的（D30 §7）：**没有人在场**，所以不能发卡等人点。
    pub scheduled: bool,
    /// token 预算（prompt+completion 累计）。None = 不限。
    /// 子 agent 用它兜住"跑飞"（D20）。
    pub token_budget: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub text: String,
    pub status: RunStatus,
    /// 本轮实际发起过的工具名（按调用顺序）。用于判断"这轮有没有产出"，
    /// 比看渲染事件可靠：渲染只是展示，这里才是账本事实。
    pub tools: Vec<String>,
}

/// 门的对外结果（D30）。`Ask` 是门的内部中间态：它自己发卡、自己等，
/// 不会以 `Ask` 的形式交回 runner。
#[derive(Clone, Debug)]
pub enum GateOutcome {
    Allowed,
    /// 这条路不存在（域关闭 / 参数非法）：记 `denied`，run 继续。
    Denied { reason: String },
    /// 终态：结束该 run（D30 §5）。
    Blocked { reason: String, hint: String },
}

/// 执行工具前的审批门。`Allowed` 才真正执行。
#[async_trait::async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn ask(
        &self,
        conv: &ConversationId,
        call: &crate::ids::CallId,
        tool: &str,
        args: &serde_json::Value,
        role: crate::identity::ActorRole,
        scheduled: bool,
    ) -> Result<GateOutcome, String>;
}

/// 去掉内核保留的元字段（目前只有 `_approval`），工具看到的仍是它自己的参数。
fn strip_meta(args: &serde_json::Value) -> serde_json::Value {
    let mut v = args.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("_approval");
    }
    v
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
    /// 被治理挡住（D30）：不是出错，是这条路本身不存在。
    #[error("blocked: {reason}")]
    Blocked { reason: String, hint: String },
}

pub struct Runner<'a, S: Store, D: TurnDriver> {
    pub store: &'a S,
    pub driver: &'a D,
    pub tools: &'a ToolRegistry,
    pub env: &'a dyn ExecutionEnv,
    /// 并发车道（D11）。所有 run 共享同一组信号量。
    pub lanes: &'a Lanes,
    /// 子 agent 派生入口。`None` = 本 run 不能派生（子 agent 自身 / 定时任务）。
    pub spawner: Option<&'a dyn crate::agent::AgentSpawner>,
    pub gate: Option<&'a dyn ApprovalGate>,
    pub outbox: Option<&'a dyn crate::tool::Outbox>,
    pub docs: Option<&'a dyn crate::tool::DocReader>,
}

/// 任务句柄随作用域结束一起中止（后台协程不该比 run 活得久）。
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 把各轮正文拼成终稿：空段落丢掉，段落之间空一行。
fn join_said(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
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
        // 工具执行阶段还要发事件（审批通过后重新起算耗时），先留一份
        let tool_sink = sink.clone();
        // 每轮正文累积：中间轮说的话（比如查完数据先列出的清单）也是用户看到的内容，
        // 终稿若只取最后一步，会把已经展示过的内容刷掉。
        let mut said: Vec<String> = Vec::new();
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
        let (history, trimmed) = crate::context::select(&history, req.context_budget_tokens);
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

        let system_prompt: &str = req.system_prompt.as_deref().unwrap_or("");
        // `/cancel` 必须能打断**正在飞**的模型请求，而不只是下一个轮次边界。
        // 之前只在 loop 顶部检查 token，于是一次 60s 的思考要等它自然结束才停，
        // 用户看到的就是"一直输入中、取消没反应"。
        let _turn_link = AbortOnDrop(tokio::spawn({
            let run = cancel.clone();
            let turn_token = turn.canceller();
            async move {
                run.cancelled().await;
                turn_token.cancel();
            }
        }));

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
            // 模型请求走 external 车道：这是 provider 侧限流的第一道闸，
            // 也保证一个长编译不会和三个 run 的推理抢同一张网卡。
            let step = {
                let permit = self.lanes.acquire(Lane::External, &cancel).await;
                if permit.is_err() {
                    return self.finish_interrupted(&conv, &run, turn_no, "cancelled").await;
                }
                turn.step().await
            };
            self.record_usage(&mut *turn, &conv, &run, &mut recorded_usage).await?;
            if let Some(limit) = req.token_budget {
                let used: u64 = turn
                    .usage()
                    .records
                    .iter()
                    .map(|u| u.prompt_tokens as u64 + u.completion_tokens as u64)
                    .sum();
                if used > limit {
                    self.finish_interrupted(&conv, &run, turn_no, "token_budget").await?;
                    return Err(RunnerError::Budget(format!("tokens {used}/{limit}")));
                }
            }

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
                        payload: serde_json::json!({
                            "stop_reason": format!("{finish:?}"),
                            "attempts": turn.attempts(),
                        }),
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
                    if !text.trim().is_empty() {
                        said.push(text);
                    }
                    let text = join_said(&said);
                    return Ok(RunOutcome { text, status: RunStatus::Done, tools: used });
                }
                Ok(Step::Tools { text, calls }) => {
                    if !text.trim().is_empty() {
                        said.push(text.clone());
                    }
                    self.persist_assistant_and_intents(&conv, &run, &turn_id, &text, &calls)
                        .await?;

                    for call in &calls {
                        used.push(call.name.clone());
                        if cancel.is_cancelled() {
                            return self.finish_interrupted(&conv, &run, turn_no, "cancelled").await;
                        }
                        let exec = self
                            .execute_one(
                                &mut *turn,
                                &conv,
                                &run,
                                &turn_id,
                                call,
                                &req.workspace,
                                &cancel,
                                &mut deadline,
                                &req.actor,
                                req.role,
                                req.scheduled,
                                system_prompt,
                                &tool_sink,
                            )
                            .await;
                        match exec {
                            Ok(()) => {}
                            // 被治理挡住：这不是出错，而是"没有合法路径"。
                            // 结束 run 并把原因交给上层回给人（D30 §5）。
                            Err(RunnerError::Blocked { reason, hint }) => {
                                tracing::info!(tool = %call.name, reason, "blocked by policy");
                                return self
                                    .finish_with(
                                        &conv,
                                        &run,
                                        turn_no,
                                        RunStatus::Blocked,
                                        "blocked",
                                        hint,
                                    )
                                    .await;
                            }
                            Err(e) => return Err(e),
                        }
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
        role: crate::identity::ActorRole,
        scheduled: bool,
        system_prompt: &str,
        sink: &crate::driver::EventSink,
    ) -> Result<(), RunnerError> {
        let tool = self.tools.get(&call.name).ok_or_else(|| RunnerError::ToolNotFound(call.name.clone()))?;

        if let Some(gate) = self.gate {
            let asked_at = Instant::now();
            let decision = gate
                .ask(conv, &call.call, &call.name, &call.args, role, scheduled)
                .await
                .map_err(RunnerError::Approval)?;
            // 等人确认的时间不算进 wall clock：预算约束的是模型/工具的耗时，
            // 不是人回消息的速度（否则走开一会儿回来就超时）。
            if let Some(d) = deadline.as_mut() {
                *d += Instant::now().saturating_duration_since(asked_at);
            }
            match decision {
                GateOutcome::Allowed => {
                    // 审批通过：告诉渲染层"现在才真正开跑"，把等人确认的时间从耗时里扣掉。
                    sink.emit(crate::driver::StreamEvent::ToolStarted {
                        call: call.call.clone(),
                        name: call.name.clone(),
                        // 有 approval_hint 就用它（人话），否则留空。delegate 用它显示任务首行，
                        // 否则前台委托期间用户只看到"第 N 轮 · 执行 delegate"，不知道在跑什么。
                        preview: tool.approval_hint(&call.args).unwrap_or_default(),
                    });
                }
                GateOutcome::Denied { reason } => {
                    let preview = format!("已拒绝执行：{reason}");
                    let mut tx = self.store.begin_write().await?;
                    tx.close_tool_call(call.call.clone(), "denied", reason).await?;
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
                    turn.on_tool_result(call.call.clone(), false, preview).await?;
                    return Ok(());
                }
                GateOutcome::Blocked { reason, hint } => {
                    // 终态：把这次调用记成 blocked（不是 denied——它不是"这次不批"，
                    // 而是"这条路径不存在"），再由上层结束 run（D30 §5）。
                    let mut tx = self.store.begin_write().await?;
                    tx.close_tool_call(call.call.clone(), "blocked", hint.clone()).await?;
                    tx.append(NewEvent {
                        conv: conv.clone(),
                        run: run.clone(),
                        turn: Some(turn_id.clone()),
                        kind: EventKind::MsgToolResult,
                        payload: serde_json::json!({
                            "call_id": call.call.0, "ok": false, "preview": hint, "blocked": true,
                        }),
                        dedupe_key: None,
                    })
                    .await?;
                    tx.commit().await?;
                    return Err(RunnerError::Blocked { reason, hint });
                }
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
            system_prompt,
            spawner: self.spawner,
            cancel: run_cancel.clone(),
        };
        // 工具若不理会取消令牌，这里也要能立刻退出。
        // 车道许可同理：排队期间被取消，token 已触发，直接走下面的取消分支。
        let _lane_permit = self.lanes.acquire(tool.lane(), run_cancel).await.ok();
        let result = tokio::select! {
            // `_approval` 是给审批卡用的保留字段，不传给工具
            r = tool.execute(strip_meta(&call.args), &ctx) => r,
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
                // 值不落地：工具输出在进上下文与账本之前，把像凭据的片段遮掉。
                // **先打码再落盘**——否则 spill 会把凭据原样写到 /ws。
                let (clean, hits) = crate::leak::redact(&render_text(&out.content));
                let (preview, spilled) =
                    preview_or_spill(clean, &workspace.root, &call.call.0);
                if !hits.is_empty() {
                    tracing::warn!(kinds = ?hits, tool = %call.name, "工具输出里发现凭据形态，已打码");
                }
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
                        "spill": spilled,
                    }),
                    dedupe_key: None,
                })
                .await?;
                tx.commit().await?;
                (true, preview)
            }
            Err(e) => {
                let (clean, hits) = crate::leak::redact(&e.message);
                let (preview, _spilled) = preview_or_spill(clean, &workspace.root, &call.call.0);
                if !hits.is_empty() {
                    tracing::warn!(kinds = ?hits, tool = %call.name, "工具错误里发现凭据形态，已打码");
                }
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
        self.finish_with(conv, run, turn_no, RunStatus::Cancelled, reason, String::new()).await
    }

    /// 以指定终态收尾（取消 / 预算 / 阻塞共用）。`reason` 同时写进 `run.finished.status`。
    async fn finish_with(
        &self,
        conv: &ConversationId,
        run: &RunId,
        turn_no: u32,
        status: RunStatus,
        reason: &str,
        text: String,
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
        Ok(RunOutcome { text, status, tools: Vec::new() })
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

/// 工具输出进上下文/账本的上限（按**字符**算，与上下文预算同一口径）。
///
/// 超过就落盘：以前这里直接截断并只给一个目录名，模型既看不到尾部、也不知道去读哪个
/// 文件（2026-11-03 板上真实事故：`ipptool` 的 marker 字段在尾部，被切掉后模型只能
/// 去 `/ws/.tool-results/` 里猜文件名，最后又用 `lan_exec` 重查一遍并触发审批）。
const PREVIEW_CAP: usize = 12_000;
/// 落盘后留多少：**首尾都要**。只留头部的话，排在后面的字段（比如 marker）永远看不到。
const SPILL_HEAD: usize = 6_000;
const SPILL_TAIL: usize = 2_000;

fn render_text(blocks: &[crate::view::Block]) -> String {
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
    s.trim_end().to_owned()
}

/// 超过上限就写 `<workspace>/.tool-results/<call_id>.txt`，只把「首部 + 尾部 + 路径」交给模型。
///
/// 与 `web_fetch` 的 payload 落盘同一落点、同一 GC；区别是这里对**所有工具**生效，
/// 所以新工具不写任何代码就自动有"大输出落盘"的行为。
fn preview_or_spill(text: String, workspace_root: &str, call_id: &str) -> (String, Option<String>) {
    let chars = text.chars().count();
    if chars <= PREVIEW_CAP {
        return (text, None);
    }
    let safe: String = call_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let dir = std::path::Path::new(workspace_root).join(".tool-results");
    let name = format!("{safe}.txt");
    let sandbox_path = format!("/ws/.tool-results/{name}");
    let write = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(&name), &text));
    let head: String = text.chars().take(SPILL_HEAD).collect();
    let tail: String = text.chars().skip(chars - SPILL_TAIL).collect();
    match write {
        Ok(()) => (
            format!(
                "{head}\n\n…（中间省略；完整输出 {chars} 字 / {} 字节，已写入 {sandbox_path}。\
                 用 `read` 直接读它，或用 `exec` 处理）\n\n{tail}",
                text.len()
            ),
            Some(sandbox_path),
        ),
        Err(e) => (
            format!("{head}\n\n…（工具输出 {chars} 字超过 {PREVIEW_CAP}，且落盘失败：{e}）"),
            None,
        ),
    }
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
    fn long_tool_output_spills_and_keeps_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let head = "HEAD-MARKER ";
        let tail = "TAIL-MARKER";
        let filler = "x".repeat(PREVIEW_CAP + 500);
        let text = format!("{head}{filler}{tail}");
        let (out, spill) = preview_or_spill(text.clone(), &dir.path().to_string_lossy(), "call_9");
        let path = spill.expect("超过上限必须落盘");
        assert_eq!(path, "/ws/.tool-results/call_9.txt");
        // 头部和尾部都要在，否则"排在后面的字段"永远看不到
        assert!(out.contains("HEAD-MARKER"), "首部丢了");
        assert!(out.contains("TAIL-MARKER"), "尾部丢了");
        assert!(out.contains(&path), "要给出确切路径，不能只给目录");
        assert!(out.chars().count() < PREVIEW_CAP + 400, "内联部分要明显变小");
        // 落盘的是完整原文
        let on_disk = std::fs::read_to_string(dir.path().join(".tool-results/call_9.txt")).unwrap();
        assert_eq!(on_disk, text);
    }

    #[test]
    fn mid_size_output_is_not_cut() {
        let body = "y".repeat(6_000);
        let (out, spill) = preview_or_spill(body.clone(), "/tmp/ws", "call_1");
        assert_eq!(out.chars().count(), 6_000, "6k 的输出不该被截");
        assert!(spill.is_none());
    }

    #[test]
    fn spill_failure_still_marks_truncation() {
        // 指向一个不存在的只读路径：写不进去时不能假装成功
        let (out, spill) = preview_or_spill("z".repeat(PREVIEW_CAP + 10), "/proc/nope", "c");
        assert!(spill.is_none());
        assert!(out.contains("落盘失败"), "{out}");
    }

    #[test]
    fn render_text_keeps_all_blocks() {
        let out = render_text(&[
            Block::Paragraph("p".into()),
            Block::Code { lang: None, text: "c".into() },
        ]);
        assert_eq!(out, "p\nc");
    }
}

