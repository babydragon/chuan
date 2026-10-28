//! Turn Loop 集成测试：三段式事务 + 工具执行 + 取消。
//! 放在 store crate，才能同时用 kernel 的 Runner 与真实的 EphemeralStore。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness_kernel::{
    Block, CallId, ConversationId, EffectClass, EngineError, EventSink, ExecutionEnv, ExecOpts,
    ExecOutput, FinishReason, Message, RunId, RunRequest, Runner, RunnerError, Step, Tool,
    ToolCallRequest, ToolCtx, ToolError, ToolOutput, ToolSpec, Turn, TurnDriver, TurnId, TurnSpec,
    UsageSnapshot, Workspace, Store,
};
use harness_store::EphemeralStore;
use tokio_util::sync::CancellationToken;

struct FakeDriver {
    steps: Arc<Mutex<VecDeque<Step>>>,
    seen: Mutex<Vec<Vec<Message>>>,
    systems: Mutex<Vec<Option<String>>>,
}

impl FakeDriver {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
            seen: Mutex::new(Vec::new()),
            systems: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl TurnDriver for FakeDriver {
    async fn begin(
        &self,
        spec: TurnSpec,
        history: &[Message],
        _tools: &[ToolSpec],
        _sink: EventSink,
    ) -> Result<Box<dyn Turn>, EngineError> {
        self.seen.lock().unwrap().push(history.to_vec());
        self.systems.lock().unwrap().push(spec.system_prompt.clone());
        Ok(Box::new(FakeTurn { steps: Arc::clone(&self.steps) }))
    }
}

struct FakeTurn {
    steps: Arc<Mutex<VecDeque<Step>>>,
}

#[async_trait]
impl Turn for FakeTurn {
    async fn step(&mut self) -> Result<Step, EngineError> {
        self.steps
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| EngineError::Provider("exhausted".into()))
    }
    async fn on_tool_result(&mut self, _c: CallId, _ok: bool, _p: String) -> Result<(), EngineError> {
        Ok(())
    }
    fn canceller(&self) -> CancellationToken {
        CancellationToken::new()
    }
    fn usage(&self) -> UsageSnapshot {
        UsageSnapshot::default()
    }
    fn turn_id(&self) -> TurnId {
        TurnId("t".into())
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "echo back".into(),
            parameters: serde_json::json!({"type":"object","properties":{"say":{"type":"string"}}}),
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
        Ok(ToolOutput {
            content: vec![Block::Paragraph(args["say"].as_str().unwrap_or_default().into())],
            doc_writes: vec![],
            artifacts: vec![],
        })
    }
}

struct NoopEnv;

#[async_trait]
impl ExecutionEnv for NoopEnv {
    async fn exec(
        &self,
        _argv: &[String],
        _opts: ExecOpts,
        _cancel: CancellationToken,
    ) -> Result<ExecOutput, harness_kernel::EnvError> {
        Ok(ExecOutput { code: Some(0), stdout: String::new(), stderr: String::new(), truncated: false })
    }
}

fn request() -> RunRequest {
    RunRequest {
        conv: ConversationId::from("c1"),
        actor: "42".into(),
        run: RunId::from("r1"),
        text: "说点什么".into(),
        model: "m".into(),
        max_turns: 8,
        wall_clock_ms: 5_000,
        workspace: Workspace { run: RunId::from("r1"), root: "/tmp/ws".into() },
        system_prompt: None,
        images: Vec::new(),
        context_budget_chars: 100_000,
    }
}

#[tokio::test]
async fn done_without_tools() {
    let store = EphemeralStore::new();
    let driver = FakeDriver::new(vec![Step::Done { text: "你好".into(), finish: FinishReason::Stop }]);
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let env = NoopEnv;

    let runner = Runner { store: &store, driver: &driver, tools: &registry, env: &env, gate: None, outbox: None, docs: None };
    let pair = EventSink::channel(32);
    let out = runner.run(request(), pair.sink, CancellationToken::new()).await.unwrap();

    assert_eq!(out.text, "你好");
    assert_eq!(out.status, harness_kernel::RunStatus::Done);

    let mut rd = store.begin_read().await.unwrap();
    let view = rd.view(ConversationId::from("c1")).await.unwrap();
    // user(run.started) + assistant
    assert_eq!(view.messages.len(), 2);
    assert!(rd.tool_calls(ConversationId::from("c1")).await.unwrap().is_empty());
}

#[tokio::test]
async fn tool_then_done_runs_three_phase_tx() {
    let store = EphemeralStore::new();
    let call = CallId::from("call_1");
    let driver = FakeDriver::new(vec![
        Step::Tools {
            // 中间轮说的话（板上实例：先列出的采购清单）不能被终稿刷掉
            text: "先查一下清单".into(),
            calls: vec![ToolCallRequest {
                call: call.clone(),
                name: "echo".into(),
                args: serde_json::json!({"say":"hi"}),
            }],
        },
        Step::Done { text: "完成".into(), finish: FinishReason::Stop },
    ]);
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let env = NoopEnv;

    let runner = Runner { store: &store, driver: &driver, tools: &registry, env: &env, gate: None, outbox: None, docs: None };
    let pair = EventSink::channel(32);
    let out = runner.run(request(), pair.sink, CancellationToken::new()).await.unwrap();
    assert_eq!(out.text, "先查一下清单\n\n完成", "中间轮正文要留在终稿里");

    // 工具调用账本：intent -> dispatched -> finished
    let rows = {
        let mut rd = store.begin_read().await.unwrap();
        rd.tool_calls(ConversationId::from("c1")).await.unwrap()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "finished");
    assert_eq!(rows[0].effect, EffectClass::Pure);

    // Runner 整个 run 只 begin 一次，后续 step 复用同一个 Turn（历史由 wire 维护）
    assert_eq!(driver.seen.lock().unwrap().len(), 1);

    // 消息流：user / assistant(带调用) / tool_result / assistant(终稿)
    let mut rd = store.begin_read().await.unwrap();
    let page = rd.messages(ConversationId::from("c1"), 0, 10).await.unwrap();
    let roles: Vec<_> = page.msgs.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        vec![
            harness_kernel::Role::User,
            harness_kernel::Role::Assistant,
            harness_kernel::Role::ToolResult,
            harness_kernel::Role::Assistant
        ]
    );
    let tool_msg = &page.msgs[2];
    assert_eq!(tool_msg.payload["preview"], "hi");
}

#[tokio::test]
async fn cancel_writes_run_finished() {
    let store = EphemeralStore::new();
    let driver = FakeDriver::new(vec![Step::Done { text: "不会跑到".into(), finish: FinishReason::Stop }]);
    let registry = harness_kernel::ToolRegistry::new();
    let env = NoopEnv;
    let runner = Runner { store: &store, driver: &driver, tools: &registry, env: &env, gate: None, outbox: None, docs: None };

    let cancel = CancellationToken::new();
    cancel.cancel();
    let pair = EventSink::channel(8);
    let out = runner.run(request(), pair.sink, cancel).await.unwrap();
    assert_eq!(out.status, harness_kernel::RunStatus::Cancelled);

    let mut rd = store.begin_read().await.unwrap();
    let view = rd.view(ConversationId::from("c1")).await.unwrap();
    assert_eq!(view.run, None, "run.finished 后不应再有进行中的 run");
}

struct SleepTool;

#[async_trait]
impl Tool for SleepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "sleep".into(), description: "sleep".into(), parameters: serde_json::json!({}) }
    }
    fn effect(&self) -> EffectClass {
        EffectClass::External
    }
    async fn execute(&self, _a: serde_json::Value, _c: &ToolCtx<'_>) -> Result<ToolOutput, ToolError> {
        // 故意不检查取消令牌，验证 Runner 的 select 兜底
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        Ok(ToolOutput::default())
    }
}

#[tokio::test]
async fn cancel_interrupts_running_tool_promptly() {
    let store = Arc::new(EphemeralStore::new());
    let driver = FakeDriver::new(vec![
        Step::Tools {
            text: String::new(),
            calls: vec![ToolCallRequest {
                call: CallId::from("call-slow"),
                name: "sleep".into(),
                args: serde_json::json!({}),
            }],
        },
        Step::Done { text: "不该到这里".into(), finish: FinishReason::Stop },
    ]);
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(SleepTool));
    let registry = Arc::new(registry);
    let env = Arc::new(NoopEnv);
    let driver = Arc::new(driver);
    let pair = EventSink::channel(32);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let store2 = Arc::clone(&store);

    let started = std::time::Instant::now();
    let task = tokio::spawn(async move {
        let runner = Runner {
            store: &*store2,
            driver: &*driver,
            tools: &registry,
            env: &*env,
            gate: None,
            outbox: None,
            docs: None,
        };
        runner
            .run(
                RunRequest {
                    conv: ConversationId::from("c1"),
        actor: "42".into(),
                    run: RunId::from("r1"),
                    text: "慢任务".into(),
                    model: "m".into(),
                    max_turns: 4,
                    wall_clock_ms: 60_000,
                    workspace: Workspace { run: RunId::from("r1"), root: "/tmp/ws".into() },
                    system_prompt: None,
                    images: Vec::new(),
                    context_budget_chars: 100_000,
                },
                pair.sink,
                c2,
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    cancel.cancel();

    let out = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .expect("取消应迅速生效")
        .unwrap()
        .unwrap();
    assert_eq!(out.status, harness_kernel::RunStatus::Cancelled);
    assert!(started.elapsed() < std::time::Duration::from_secs(3));

    let rows = {
        let mut rd = store.begin_read().await.unwrap();
        rd.tool_calls(ConversationId::from("c1")).await.unwrap()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "abandoned:cancelled", "被取消的工具应标记 abandoned");
}

#[tokio::test]
async fn unknown_tool_fails_loudly() {
    let store = EphemeralStore::new();
    let driver = FakeDriver::new(vec![Step::Tools {
        text: String::new(),
        calls: vec![ToolCallRequest {
            call: CallId::from("c9"),
            name: "nope".into(),
            args: serde_json::json!({}),
        }],
    }]);
    let registry = harness_kernel::ToolRegistry::new();
    let env = NoopEnv;
    let runner = Runner { store: &store, driver: &driver, tools: &registry, env: &env, gate: None, outbox: None, docs: None };
    let pair = EventSink::channel(8);
    let err = runner.run(request(), pair.sink, CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, RunnerError::ToolNotFound(_)));
}

// ---------------- ConversationActor 接线测试 ----------------

use harness_kernel::{
    ActorConfig, Caps, ChannelError, ConversationActor, Inbound, InboundKind, MsgRef, View,
};

#[derive(Default)]
struct FakeChannel {
    delivered: Mutex<Vec<String>>,
    cards: Mutex<Vec<(String, Vec<Vec<(String, String)>>)>>,
    acks: Mutex<Vec<String>>,
    buttons: Mutex<Vec<Vec<Vec<(String, String)>>>>,
    cleared: Mutex<Vec<String>>,
    /// 接下来 N 次 deliver 返回 429（用来验证限流后仍能送达）
    fail_deliver_times: Mutex<usize>,
}

fn view_text(v: &View) -> String {
    v.blocks
        .iter()
        .filter_map(|b| match b {
            Block::Paragraph(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl harness_kernel::Channel for FakeChannel {
    fn id(&self) -> harness_kernel::ChannelId {
        harness_kernel::ChannelId("fake".into())
    }
    fn caps(&self) -> Caps {
        Caps { edit: true, buttons: true, markdown: false, max_text: 4096, min_edit_interval_ms: 0 }
    }
    async fn run(
        &self,
        _tx: tokio::sync::mpsc::Sender<Inbound>,
        _cancel: CancellationToken,
    ) -> Result<(), ChannelError> {
        Ok(())
    }
    async fn deliver(&self, _c: &str, view: View) -> Result<MsgRef, ChannelError> {
        {
            let mut left = self.fail_deliver_times.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(ChannelError::RateLimited { retry_after_ms: 10 });
            }
        }
        self.delivered.lock().unwrap().push(view_text(&view));
        self.buttons.lock().unwrap().push(
            view.buttons
                .iter()
                .map(|row| row.iter().map(|b| (b.label.clone(), b.data.clone())).collect())
                .collect(),
        );
        Ok(MsgRef("m1".into()))
    }
    async fn update(&self, _c: &str, _m: &MsgRef, view: View) -> Result<harness_kernel::EditOutcome, ChannelError> {
        self.delivered.lock().unwrap().push(view_text(&view));
        self.buttons.lock().unwrap().push(
            view.buttons
                .iter()
                .map(|row| row.iter().map(|b| (b.label.clone(), b.data.clone())).collect())
                .collect(),
        );
        Ok(harness_kernel::EditOutcome::Edited)
    }
    async fn clear_buttons(&self, _c: &str, msg: &MsgRef) -> Result<(), ChannelError> {
        self.cleared.lock().unwrap().push(msg.0.clone());
        Ok(())
    }
    async fn ack(&self, _cb: &str, text: &str) -> Result<(), ChannelError> {
        self.acks.lock().unwrap().push(text.to_owned());
        Ok(())
    }
    async fn deliver_prompt(
        &self,
        _c: &str,
        prompt: harness_kernel::Prompt,
    ) -> Result<MsgRef, ChannelError> {
        let buttons: Vec<Vec<(String, String)>> = prompt
            .choices
            .iter()
            .map(|c| vec![(c.label.clone(), format!("appr:{}:{}", c.id, c.value))])
            .collect();
        self.cards.lock().unwrap().push((prompt.text, buttons));
        Ok(MsgRef("card-1".into()))
    }
}

#[tokio::test]
async fn actor_turns_inbound_into_reply() {
    let store = EphemeralStore::new();
    let driver = FakeDriver::new(vec![Step::Done { text: "你好".into(), finish: FinishReason::Stop }]);
    let registry = harness_kernel::ToolRegistry::new();
    let env = NoopEnv;
    let channel = Arc::new(FakeChannel::default());

    let store = Arc::new(store);
    let actor = ConversationActor::new(
        Arc::clone(&store),
        Arc::new(driver),
        Arc::new(registry),
        Arc::new(env),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 4,
            wall_clock_ms: 5_000,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    );

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let cancel = CancellationToken::new();
    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Text,
        text: Some("说点什么".into()),
        dedupe_key: "u1".into(),
        callback_id: None,
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();
    drop(tx);

    Arc::new(actor).run(rx, cancel).await.unwrap();

    let sent = channel.delivered.lock().unwrap();
    assert!(!sent.is_empty());
    assert!(sent.iter().any(|t| t.contains("你好")), "{sent:?}");
}

#[tokio::test]
async fn final_message_is_retried_after_rate_limit_window() {
    let store = Arc::new(EphemeralStore::new());
    let driver = FakeDriver::new(vec![Step::Done { text: "限流也要送到".into(), finish: FinishReason::Stop }]);
    let registry = harness_kernel::ToolRegistry::new();
    let env = NoopEnv;
    let channel = Arc::new(FakeChannel::default());
    // 3 次 = deliver_with_retry 的全部尝试次数：让它们统统失败，逼出"等窗口后重发"
    *channel.fail_deliver_times.lock().unwrap() = 3;

    let actor = ConversationActor::new(
        Arc::clone(&store),
        Arc::new(driver),
        Arc::new(registry),
        Arc::new(env),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 4,
            wall_clock_ms: 5_000,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    );

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Text,
        text: Some("说点什么".into()),
        dedupe_key: "u-rl".into(),
        callback_id: None,
        received_at: 0,
        attachments: Vec::new(),
    })
    .await
    .unwrap();
    drop(tx);
    Arc::new(actor).run(rx, CancellationToken::new()).await.unwrap();

    let sent = channel.delivered.lock().unwrap();
    assert!(
        sent.iter().any(|t| t.contains("限流也要送到")),
        "退避窗口结束后必须补发终稿: {sent:?}"
    );
}

#[tokio::test]
async fn actor_answers_slash_status() {
    let store = EphemeralStore::new();
    let driver = FakeDriver::new(vec![]);
    let registry = harness_kernel::ToolRegistry::new();
    let env = NoopEnv;
    let channel = Arc::new(FakeChannel::default());
    let store = Arc::new(store);
    let actor = ConversationActor::new(
        Arc::clone(&store),
        Arc::new(driver),
        Arc::new(registry),
        Arc::new(env),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 2,
            wall_clock_ms: 1_000,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    );
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Command,
        text: Some("/status".into()),
        dedupe_key: "u2".into(),
        callback_id: None,
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();
    drop(tx);
    Arc::new(actor).run(rx, CancellationToken::new()).await.unwrap();
    let msg = channel.delivered.lock().unwrap()[0].clone();
    assert!(msg.contains("空闲"), "{msg}");
    assert!(msg.contains("epoch"), "{msg}");
}


#[tokio::test]
async fn approval_gate_allows_after_callback() {
    let store = EphemeralStore::new();
    let driver = FakeDriver::new(vec![
        Step::Tools {
            text: String::new(),
            calls: vec![ToolCallRequest {
                call: CallId::from("call-appr"),
                name: "echo".into(),
                args: serde_json::json!({"say":"hi"}),
            }],
        },
        Step::Done { text: "完成".into(), finish: FinishReason::Stop },
    ]);
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let env = NoopEnv;
    let channel = Arc::new(FakeChannel::default());
    let store_arc = Arc::new(store);

    let actor = ConversationActor::new(
        Arc::clone(&store_arc),
        Arc::new(driver),
        Arc::new(registry),
        Arc::new(env),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 4,
            wall_clock_ms: 5_000,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy {
                extra_tools: vec!["echo".into()],
                ..Default::default()
            },
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    );

    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(async move { Arc::new(actor).run(rx, cancel).await });

    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Text,
        text: Some("跑一下".into()),
        dedupe_key: "u1".into(),
        callback_id: None,
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();

    // 等卡片出现
    let mut data = None;
    for _ in 0..100 {
        if let Some((_, buttons)) = channel.cards.lock().unwrap().first().cloned() {
            data = Some(buttons[0][0].1.clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let data = data.expect("应发出审批卡片");
    assert!(data.starts_with("appr:"), "{data}");

    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Callback,
        text: Some(data),
        dedupe_key: "u2".into(),
        callback_id: Some("cb1".into()),
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();

    // 等 run 收尾
    for _ in 0..100 {
        let rows = {
            let mut rd = store_arc.begin_read().await.unwrap();
            rd.tool_calls(harness_kernel::ConversationId("fake:1000:0".into()))
                .await
                .unwrap()
        };
        if rows.iter().any(|r| r.status == "finished") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let rows = {
        let mut rd = store_arc.begin_read().await.unwrap();
        rd.tool_calls(harness_kernel::ConversationId("fake:1000:0".into()))
            .await
            .unwrap()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "finished", "允许后应真正执行");
    assert!(channel.acks.lock().unwrap().iter().any(|a| a.contains("已允许")));

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

/// 跑一次会用 write 的 run：终稿应带「沉淀为 Skill」按钮。
#[tokio::test]
async fn done_run_with_writes_offers_publish_button() {
    let store = Arc::new(EphemeralStore::new());
    let driver = FakeDriver::new(vec![
        Step::Tools {
            text: String::new(),
            calls: vec![ToolCallRequest {
                call: CallId::from("call-w"),
                name: "write".into(),
                args: serde_json::json!({}),
            }],
        },
        Step::Done { text: "写好了".into(), finish: FinishReason::Stop },
    ]);
    // 用名为 write 的最小工具，触发"本轮动过文件"
    struct NamedTool(&'static str);
    #[async_trait]
    impl Tool for NamedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: "x".into(),
                parameters: serde_json::json!({"type":"object"}),
            }
        }
        fn effect(&self) -> EffectClass {
            EffectClass::Pure
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                content: vec![Block::Paragraph("ok".into())],
                doc_writes: vec![],
                artifacts: vec![],
            })
        }
    }
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(NamedTool("write")));

    let channel = Arc::new(FakeChannel::default());
    let actor = ConversationActor::new(
        Arc::clone(&store),
        Arc::new(driver),
        Arc::new(registry),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 4,
            wall_clock_ms: 5_000,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: "SOP".into(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    );

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Text,
        text: Some("写个脚本".into()),
        dedupe_key: "u1".into(),
        callback_id: None,
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();
    drop(tx);
    Arc::new(actor).run(rx, CancellationToken::new()).await.unwrap();

    let buttons = channel.buttons.lock().unwrap();
    let has_publish = buttons
        .iter()
        .flatten()
        .flatten()
        .any(|(_, data)| data.starts_with("skill:"));
    assert!(has_publish, "终稿应带沉淀按钮: {buttons:?}");
}

/// 点「沉淀」→ 用配置的 SOP 另起一次 run。
#[tokio::test]
async fn skill_button_starts_distill_run() {
    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![Step::Done {
        text: "已沉淀".into(),
        finish: FinishReason::Stop,
    }]));
    let channel = Arc::new(FakeChannel::default());
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(harness_kernel::ToolRegistry::new()),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 2,
            wall_clock_ms: 2_000,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: "SOP-标记".into(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    ));

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Callback,
        text: Some("skill:run-1".into()),
        dedupe_key: "cb1".into(),
        callback_id: Some("cb1".into()),
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();
    drop(tx);

    let runner = tokio::spawn(Arc::clone(&actor).run(rx, CancellationToken::new()));

    // 提炼 run 是 spawn 出去的，轮询等 driver 收到历史
    let mut seen = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        seen = driver.seen.lock().unwrap().clone();
        if !seen.is_empty() {
            break;
        }
    }
    assert!(!seen.is_empty(), "点击后应触发一次 run");
    let text = format!("{:?}", seen[0]);
    assert!(text.contains("SOP-标记"), "注入的应是配置的 SOP: {text}");
    assert!(text.contains("run-1"), "应带上来源 run: {text}");

    assert!(channel.acks.lock().unwrap().iter().any(|a| a.contains("沉淀")), "应回执");
    runner.abort();
}

/// 点过「沉淀」后，按钮要从消息上撤掉（对话里不留过期按钮）。
#[tokio::test]
async fn clicking_publish_button_removes_keyboard() {
    struct NamedTool(&'static str);
    #[async_trait]
    impl Tool for NamedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: "x".into(),
                parameters: serde_json::json!({"type":"object"}),
            }
        }
        fn effect(&self) -> EffectClass {
            EffectClass::Pure
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                content: vec![Block::Paragraph("ok".into())],
                doc_writes: vec![],
                artifacts: vec![],
            })
        }
    }

    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![
        Step::Tools {
            text: String::new(),
            calls: vec![ToolCallRequest {
                call: CallId::from("call-w"),
                name: "write".into(),
                args: serde_json::json!({}),
            }],
        },
        Step::Done { text: "写好了".into(), finish: FinishReason::Stop },
        Step::Done { text: "已沉淀".into(), finish: FinishReason::Stop },
    ]));
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(NamedTool("write")));

    let channel = Arc::new(FakeChannel::default());
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(registry),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 4,
            wall_clock_ms: 5_000,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: "SOP".into(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    ));

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let runner = tokio::spawn(Arc::clone(&actor).run(rx, CancellationToken::new()));

    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Text,
        text: Some("写个脚本".into()),
        dedupe_key: "u1".into(),
        callback_id: None,
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();

    // 等第一次 run 定稿（run id 可预测：fake:1000:0-u1）
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if channel.delivered.lock().unwrap().iter().any(|t| t.contains("写好了")) {
            break;
        }
    }
    assert!(channel.cleared.lock().unwrap().is_empty(), "还没点就不该撤");

    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Callback,
        text: Some("skill:fake:1000:0-u1".into()),
        dedupe_key: "cb1".into(),
        callback_id: Some("cb1".into()),
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if !channel.cleared.lock().unwrap().is_empty() {
            break;
        }
    }
    assert_eq!(
        channel.cleared.lock().unwrap().clone(),
        vec!["m1".to_owned()],
        "点击后应撤掉按钮所在的最后一条消息"
    );
    runner.abort();
}

/// 审批等待不该消耗 wall clock：否则走开 30 秒回来就超时。
#[tokio::test]
async fn approval_wait_does_not_burn_wall_clock() {
    struct SlowGate;
    #[async_trait]
    impl harness_kernel::runner::ApprovalGate for SlowGate {
        async fn ask(
            &self,
            _conv: &ConversationId,
            _call: &CallId,
            _tool: &str,
            _args: &serde_json::Value,
        ) -> Result<bool, String> {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok(true)
        }
    }

    let store = EphemeralStore::new();
    let driver = FakeDriver::new(vec![
        Step::Tools {
            text: String::new(),
            calls: vec![ToolCallRequest {
                call: CallId::from("call-slow-approval"),
                name: "echo".into(),
                args: serde_json::json!({"say":"hi"}),
            }],
        },
        Step::Done { text: "完成".into(), finish: FinishReason::Stop },
    ]);
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(EchoTool));

    let gate = SlowGate;
    let mut req = request();
    // 预算比审批等待还短：若把等待算进预算，这里必然 Budget
    req.wall_clock_ms = 150;
    let runner = Runner {
        store: &store,
        driver: &driver,
        tools: &registry,
        env: &NoopEnv,
        gate: Some(&gate),
        outbox: None,
        docs: None,
    };
    let pair = EventSink::channel(32);
    let out = runner.run(req, pair.sink, CancellationToken::new()).await.unwrap();
    assert_eq!(out.text, "完成");
    assert_eq!(out.tools, vec!["echo".to_owned()]);
}

struct DomainTool {
    name: &'static str,
    domain: harness_kernel::Domain,
}

#[async_trait]
impl Tool for DomainTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "x".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }
    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }
    fn domain(&self) -> harness_kernel::Domain {
        self.domain
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![Block::Paragraph("ran".into())],
            doc_writes: vec![],
            artifacts: vec![],
        })
    }
}

async fn run_one_tool_call(
    tool: DomainTool,
    call: &'static str,
) -> (Vec<String>, Vec<String>, Arc<EphemeralStore>) {
    let store = Arc::new(EphemeralStore::new());
    let driver = FakeDriver::new(vec![
        Step::Tools {
            text: String::new(),
            calls: vec![ToolCallRequest {
                call: CallId::from(call),
                name: tool.name.into(),
                args: serde_json::json!({}),
            }],
        },
        Step::Done { text: "完成".into(), finish: FinishReason::Stop },
    ]);
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(tool));
    let channel = Arc::new(FakeChannel::default());
    let actor = ConversationActor::new(
        Arc::clone(&store),
        Arc::new(driver),
        Arc::new(registry),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 4,
            wall_clock_ms: 0,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    );
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Text,
        text: Some("做".into()),
        dedupe_key: "u1".into(),
        callback_id: None,
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();
    drop(tx);
    let handle = tokio::spawn(Arc::new(actor).run(rx, CancellationToken::new()));
    // 等"弹了卡片"或"跑完"——改自身的域会一直等人点，不能干等
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if !channel.cards.lock().unwrap().is_empty()
            || channel.delivered.lock().unwrap().iter().any(|t| t.contains("完成"))
        {
            break;
        }
    }
    handle.abort();
    (
        channel.delivered.lock().unwrap().clone(),
        channel.cards.lock().unwrap().iter().map(|(t, _)| t.clone()).collect(),
        store,
    )
}

/// 沙盒内的工具不问：exec 里能做的事已经被边界限死，问也没用。
#[tokio::test]
async fn sandbox_tool_does_not_ask() {
    let (_sent, cards, _store) = run_one_tool_call(
        DomainTool { name: "exec_like", domain: harness_kernel::Domain::Sandbox },
        "call-sbx",
    )
    .await;
    assert!(cards.is_empty(), "沙盒域不应弹审批: {cards:?}");
}

/// 改自身的工具必须问，不需要有人记得把它写进配置名单。
#[tokio::test]
async fn self_modify_tool_asks_by_default() {
    let (_sent, cards, _store) = run_one_tool_call(
        DomainTool { name: "install_like", domain: harness_kernel::Domain::SelfModify },
        "call-self",
    )
    .await;
    assert_eq!(cards.len(), 1, "改自身应弹一次审批");
    assert!(cards[0].contains("install_like"), "{cards:?}");
}

/// 重建后的指令路径抽检：空记忆提示 + /help 内容。
#[tokio::test]
async fn slash_facts_empty_and_help_lists_commands() {
    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![]));
    let channel = Arc::new(FakeChannel::default());
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(harness_kernel::ToolRegistry::new()),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 2,
            wall_clock_ms: 0,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    for (i, text) in ["/facts", "/help"].iter().enumerate() {
        tx.send(Inbound {
            channel: harness_kernel::ChannelId("fake".into()),
            conversation: "1000".into(),
            actor: "42".into(),
            kind: InboundKind::Command,
            text: Some((*text).into()),
            dedupe_key: format!("c{i}"),
            callback_id: None,
            received_at: 0,
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    }
    drop(tx);
    Arc::clone(&actor).run(rx, CancellationToken::new()).await.unwrap();

    let sent = channel.delivered.lock().unwrap().join("\n");
    assert!(sent.contains("还没有记住任何事实"), "{sent}");
    assert!(sent.contains("/forget"), "{sent}");
    assert!(sent.contains("playbook"), "{sent}");
}

/// 到点的提醒会起一次 run，并把下一次排好（不会重复触发同一次）。
#[tokio::test]
async fn due_reminder_fires_once_and_rearms() {
    use harness_kernel::{Doc, DocId};

    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![Step::Done {
        text: "汇总完成".into(),
        finish: FinishReason::Stop,
    }]));
    let channel = Arc::new(FakeChannel::default());
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(harness_kernel::ToolRegistry::new()),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 2,
            wall_clock_ms: 0,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    ));

    let now = harness_kernel::now_ms();
    let mut tx = store.begin_write().await.unwrap();
    tx.upsert_doc(
        ConversationId::from("__reminders__"),
        RunId::from("__reminders__"),
        Doc {
            id: DocId("reminder-test".into()),
            kind: "reminder".into(),
            scope: "family".into(),
            payload: serde_json::json!({
                "text": "汇总 Hacker News 热榜前 10",
                "schedule": {"kind":"weekdays","time":"09:00"},
                "next_at": now - 1_000,
                "channel": "fake",
                "chat": "1000",
                "actor": "42",
            }),
            version: 1,
            valid_from: now,
            valid_to: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let fired = actor.tick_reminders().await;
    assert_eq!(fired, 1, "到期提醒应触发一次");

    // run 是 spawn 出去的，等它跑完
    let mut seen = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        seen = driver.seen.lock().unwrap().clone();
        if !seen.is_empty() {
            break;
        }
    }
    assert!(!seen.is_empty(), "应该起了一次 run");
    let text = format!("{:?}", seen[0]);
    assert!(text.contains("定时任务"), "{text}");
    assert!(text.contains("Hacker News"), "{text}");

    // 再 tick 一次不应重复触发
    assert_eq!(actor.tick_reminders().await, 0, "同一次不应重复触发");

    // 下一次时间已经推进到将来
    let mut rd = store.begin_read().await.unwrap();
    let docs = rd
        .list_docs(harness_kernel::DocQuery {
            scopes: Vec::new(),
            kinds: vec!["reminder".into()],
            limit: 10,
        })
        .await
        .unwrap();
    let next = docs[0].payload["next_at"].as_i64().unwrap_or(0);
    assert!(next > harness_kernel::now_ms(), "next_at 应推进: {next}");
    assert!(docs[0].version > 1, "应写入新版本");
}

/// 钉住脚本的定时任务：到点直接执行技能，把输出原样发回，不调模型。
#[tokio::test]
async fn pinned_reminder_runs_skill_without_model() {
    use harness_kernel::{Doc, DocId};

    // 只提供一个同名工具，验证"走 run_skill、不走 driver"
    struct SkillStub;
    #[async_trait]
    impl Tool for SkillStub {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "run_skill".into(),
                description: "stub".into(),
                parameters: serde_json::json!({"type":"object"}),
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
            let name = args["name"].as_str().unwrap_or("?");
            Ok(ToolOutput {
                content: vec![Block::Paragraph(format!("PYOUT:{name}:1. 头条 A\n2. 头条 B"))],
                doc_writes: vec![],
                artifacts: vec![],
            })
        }
    }

    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![]));
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(SkillStub));
    let channel = Arc::new(FakeChannel::default());
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(registry),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 2,
            wall_clock_ms: 0,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    ));

    let now = harness_kernel::now_ms();
    let mut tx = store.begin_write().await.unwrap();
    tx.upsert_doc(
        ConversationId::from("__reminders__"),
        RunId::from("__reminders__"),
        Doc {
            id: DocId("reminder-pinned".into()),
            kind: "reminder".into(),
            scope: "family".into(),
            payload: serde_json::json!({
                "text": "HN 热榜",
                "schedule": {"kind":"weekdays","time":"09:00"},
                "next_at": now - 500,
                "channel": "fake",
                "chat": "1000",
                "actor": "42",
                "skill": {"name": "hn-top", "args": {}}
            }),
            version: 1,
            valid_from: now,
            valid_to: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(actor.tick_reminders().await, 1);

    let mut delivered = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        delivered = channel.delivered.lock().unwrap().clone();
        if delivered.iter().any(|t| t.contains("PYOUT")) {
            break;
        }
    }
    let all = delivered.join("\n");
    assert!(all.contains("PYOUT:hn-top"), "应直接投递脚本输出: {all}");
    assert!(all.contains("定时任务"), "应带时间头: {all}");
    assert!(driver.seen.lock().unwrap().is_empty(), "不该调用模型");

    // 账本要留痕：pinned 任务不经过模型，但也必须能审计/恢复
    let mut rd = store.begin_read().await.unwrap();
    let calls = rd
        .tool_calls(ConversationId::from("job:1000"))
        .await
        .unwrap();
    assert_eq!(calls.len(), 1, "应记录一次技能调用: {calls:?}");
    assert_eq!(calls[0].name, "run_skill");
    assert_eq!(calls[0].status, "finished", "调用应已收尾");
    let usage = rd.usage_total(0).await.unwrap();
    assert_eq!(
        usage.prompt_tokens + usage.completion_tokens,
        0,
        "pinned 任务不该产生任何 token 用量"
    );
}

/// 静态提醒：到点只发文本，不调模型、不跑技能。
#[tokio::test]
async fn static_reminder_sends_text_only() {
    use harness_kernel::{Doc, DocId};

    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![]));
    let channel = Arc::new(FakeChannel::default());
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(harness_kernel::ToolRegistry::new()),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 2,
            wall_clock_ms: 0,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    ));

    let now = harness_kernel::now_ms();
    let mut tx = store.begin_write().await.unwrap();
    tx.upsert_doc(
        ConversationId::from("__reminders__"),
        RunId::from("__reminders__"),
        Doc {
            id: DocId("reminder-static".into()),
            kind: "reminder".into(),
            scope: "family".into(),
            payload: serde_json::json!({
                "text": "提醒穿校服",
                "message": "明天记得穿红色校服",
                "schedule": {"kind":"once","at":"2026-10-26T20:00"},
                "next_at": now - 500,
                "channel": "fake",
                "chat": "1000",
                "actor": "42"
            }),
            version: 1,
            valid_from: now,
            valid_to: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(actor.tick_reminders().await, 1);
    let sent = channel.delivered.lock().unwrap().clone();
    assert!(sent.iter().any(|t| t == "明天记得穿红色校服"), "应原样发送: {sent:?}");
    assert!(driver.seen.lock().unwrap().is_empty(), "静态提醒不该调模型");
}

/// 审批卡上要出现工具自己给的"人话"（例如解析后的触发时间），而不是只堆参数。
#[tokio::test]
async fn approval_card_uses_tool_hint() {
    struct HintTool;
    #[async_trait]
    impl Tool for HintTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "hintme".into(),
                description: "x".into(),
                parameters: serde_json::json!({"type":"object"}),
            }
        }
        fn effect(&self) -> EffectClass {
            EffectClass::Pure
        }
        fn domain(&self) -> harness_kernel::Domain {
            harness_kernel::Domain::SelfModify
        }
        fn approval_hint(&self, _args: &serde_json::Value) -> Option<String> {
            Some("HINT-SENTINEL · 2026-10-29 19:30（周四）".into())
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                content: vec![Block::Paragraph("done".into())],
                doc_writes: vec![],
                artifacts: vec![],
            })
        }
    }

    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![
        Step::Tools {
            text: String::new(),
            calls: vec![ToolCallRequest {
                call: CallId::from("call-hint"),
                name: "hintme".into(),
                args: serde_json::json!({}),
            }],
        },
        Step::Done {
            text: "ok".into(),
            finish: FinishReason::Stop,
        },
    ]));
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(HintTool));
    let channel = Arc::new(FakeChannel::default());
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(registry),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 3,
            wall_clock_ms: 0,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Text,
        text: Some("做".into()),
        dedupe_key: "u1".into(),
        callback_id: None,
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();
    let handle = tokio::spawn(Arc::clone(&actor).run(rx, CancellationToken::new()));

    let mut text = String::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if let Some((t, _)) = channel.cards.lock().unwrap().first().cloned() {
            text = t;
            break;
        }
    }
    handle.abort();
    assert!(text.contains("HINT-SENTINEL"), "审批卡应包含工具提示: {text}");
    assert!(text.contains("2026-10-29 19:30"), "{text}");
}

/// 提醒只属于创建它的人：别人的提醒不会出现在 /reminders 里。
#[tokio::test]
async fn reminders_are_per_user() {
    use harness_kernel::{Binding, Directory, Doc, DocId, User};

    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![]));
    let channel = Arc::new(FakeChannel::default());
    let dir = Directory {
        users: vec![
            User {
                id: "owner".into(),
                name: None,
                channels: vec![Binding::parse("fake:1000").unwrap()],
            },
            User {
                id: "kid".into(),
                name: None,
                channels: vec![Binding::parse("fake:2000").unwrap()],
            },
        ],
        default_user: None,
    };
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(harness_kernel::ToolRegistry::new()),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 2,
            wall_clock_ms: 0,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(dir),
        },
        None,
    ));

    let now = harness_kernel::now_ms();
    let mut tx = store.begin_write().await.unwrap();
    for (id, user, msg) in [
        ("rem-owner", "owner", "主人的提醒"),
        ("rem-kid", "kid", "孩子的提醒"),
    ] {
        tx.upsert_doc(
            ConversationId::from("__reminders__"),
            RunId::from("__reminders__"),
            Doc {
                id: DocId(id.into()),
                kind: "reminder".into(),
                scope: "family".into(),
                payload: serde_json::json!({
                    "text": msg,
                    "message": msg,
                    "user": user,
                    "schedule": {"kind":"weekdays","time":"09:00"},
                    "next_at": now + 86_400_000,
                    "channel": "fake",
                    "chat": if user == "owner" { "1000" } else { "2000" },
                    "actor": if user == "owner" { "1000" } else { "2000" },
                }),
                version: 1,
                valid_from: now,
                valid_to: None,
            },
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "1000".into(),
        kind: InboundKind::Command,
        text: Some("/reminders".into()),
        dedupe_key: "c1".into(),
        callback_id: None,
        received_at: 0,
            attachments: Vec::new(),
    })
    .await
    .unwrap();
    drop(tx);
    Arc::clone(&actor).run(rx, CancellationToken::new()).await.unwrap();

    let all = channel.delivered.lock().unwrap().join("\n");
    assert!(all.contains("主人的提醒"), "{all}");
    assert!(!all.contains("孩子的提醒"), "不应看到别人的提醒: {all}");
}

/// 脚本拉数据 + 模型加工：stdout 只进模型，不直接发给用户。
#[tokio::test]
async fn pinned_reminder_with_polish_passes_data_to_model() {
    use harness_kernel::{Doc, DocId};

    struct SkillStub;
    #[async_trait]
    impl Tool for SkillStub {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "run_skill".into(),
                description: "stub".into(),
                parameters: serde_json::json!({"type":"object"}),
            }
        }
        fn effect(&self) -> EffectClass {
            EffectClass::Pure
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                content: vec![Block::Paragraph("RAW-DATA: 1. Rust is fast".into())],
                doc_writes: vec![],
                artifacts: vec![],
            })
        }
    }

    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![Step::Done {
        text: "FORMATTED DIGEST".into(),
        finish: FinishReason::Stop,
    }]));
    let mut registry = harness_kernel::ToolRegistry::new();
    registry.register(Arc::new(SkillStub));
    let channel = Arc::new(FakeChannel::default());
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(registry),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 3,
            wall_clock_ms: 0,
            workspace_root: "/tmp/ws".into(),
            system_prompt: String::new(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            host_paths: None,
            vision: false,
            proxy_hint: None,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    ));

    let now = harness_kernel::now_ms();
    let mut tx = store.begin_write().await.unwrap();
    tx.upsert_doc(
        ConversationId::from("__reminders__"),
        RunId::from("__reminders__"),
        Doc {
            id: DocId("reminder-polish".into()),
            kind: "reminder".into(),
            scope: "family".into(),
            payload: serde_json::json!({
                "text": "HN 热榜",
                "schedule": {"kind":"weekdays","time":"09:00"},
                "next_at": now - 500,
                "channel": "fake",
                "chat": "1000",
                "actor": "42",
                "skill": {"name": "hn-top", "args": {}},
                "polish": "把标题翻译成中文并按领域分类"
            }),
            version: 1,
            valid_from: now,
            valid_to: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(actor.tick_reminders().await, 1);

    // 模型被调用，且拿到了脚本输出 + 加工指令
    let mut seen = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        seen = driver.seen.lock().unwrap().clone();
        if !seen.is_empty() {
            break;
        }
    }
    assert!(!seen.is_empty(), "应触发一次模型 run");
    let with_data = format!("{:?}", seen[0]);
    assert!(with_data.contains("RAW-DATA"), "数据要传给模型: {with_data}");
    assert!(with_data.contains("翻译"), "指令要传进去: {with_data}");

    let all = channel.delivered.lock().unwrap().join("\n");
    assert!(all.contains("FORMATTED DIGEST"), "应发模型的成品: {all}");
    assert!(!all.contains("RAW-DATA: 1. Rust is fast"), "原始数据不该直接发给用户: {all}");
}

/// 外部工具目录（MCP）要注入 system prompt，和技能目录并列。
#[tokio::test]
async fn tool_catalog_is_injected_into_system_prompt() {
    struct StubCatalog;
    impl harness_kernel::SkillSource for StubCatalog {
        fn catalog(&self) -> String {
            "MCP-TOOLS-MARKER: dida/add".into()
        }
    }

    let store = Arc::new(EphemeralStore::new());
    let driver = Arc::new(FakeDriver::new(vec![Step::Done {
        text: "ok".into(),
        finish: FinishReason::Stop,
    }]));
    let channel = Arc::new(FakeChannel::default());
    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::clone(&driver),
        Arc::new(harness_kernel::ToolRegistry::new()),
        Arc::new(NoopEnv),
        Arc::clone(&channel) as Arc<dyn harness_kernel::Channel>,
        ActorConfig {
            model: "m".into(),
            max_turns: 2,
            wall_clock_ms: 0,
            workspace_root: "/tmp/ws".into(),
            system_prompt: "BASE".into(),
            policy: harness_kernel::ApprovalPolicy::default(),
            context_budget_chars: 100_000,
            skill_source: None,
            tool_source: Some(std::sync::Arc::new(StubCatalog)),
            host_paths: Some("HOST-PATHS-MARKER: secrets/".into()),
            vision: false,
            proxy_hint: None,
            distill_prompt: String::new(),
            tz_offset_min: 480,
            identity: std::sync::Arc::new(harness_kernel::Directory::default()),
        },
        None,
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(Inbound {
        channel: harness_kernel::ChannelId("fake".into()),
        conversation: "1000".into(),
        actor: "42".into(),
        kind: InboundKind::Text,
        text: Some("hi".into()),
        dedupe_key: "u1".into(),
        callback_id: None,
        received_at: 0,
        attachments: Vec::new(),
    })
    .await
    .unwrap();
    drop(tx);
    Arc::clone(&actor).run(rx, CancellationToken::new()).await.unwrap();

    let systems = driver.systems.lock().unwrap();
    let s = systems.first().and_then(|x| x.clone()).unwrap_or_default();
    assert!(s.contains("BASE"), "{s}");
    assert!(s.contains("MCP-TOOLS-MARKER"), "外部工具目录没注入: {s}");
    assert!(s.contains("HOST-PATHS-MARKER"), "宿主路径块没注入: {s}");
}
