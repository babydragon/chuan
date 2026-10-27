//! 崩溃恢复演练：run 模式跑到工具执行中，由外部 kill -9；check 模式验证恢复。
//!
//!   crash_drill run   <db>
//!   crash_drill check <db>

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness_capabilities::{BwrapEnv, ExecTool};
use harness_kernel::{
    CallId, ConversationId, EngineError, EventSink, Message, RunId, RunRequest, Runner, Step,
    ToolCallRequest, ToolRegistry, ToolSpec, Turn, TurnDriver, TurnId, TurnSpec, UsageSnapshot,
    Workspace, Store,
};
use harness_store::SqliteStore;
use tokio_util::sync::CancellationToken;

struct OneShotDriver;

#[async_trait]
impl TurnDriver for OneShotDriver {
    async fn begin(
        &self,
        _spec: TurnSpec,
        _history: &[Message],
        _tools: &[ToolSpec],
        _sink: EventSink,
    ) -> Result<Box<dyn Turn>, EngineError> {
        Ok(Box::new(OneShotTurn {
            steps: Mutex::new(VecDeque::from(vec![Step::Tools {
                text: String::new(),
                calls: vec![ToolCallRequest {
                    call: CallId("drill-call".into()),
                    name: "exec".into(),
                    args: serde_json::json!({ "script": "sleep 30", "timeout_ms": 60000 }),
                }],
            }])),
        }))
    }
}

struct OneShotTurn {
    steps: Mutex<VecDeque<Step>>,
}

#[async_trait]
impl Turn for OneShotTurn {
    async fn step(&mut self) -> Result<Step, EngineError> {
        self.steps
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| EngineError::Provider("done".into()))
    }
    async fn on_tool_result(&mut self, _c: CallId, _o: bool, _p: String) -> Result<(), EngineError> {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let db = std::env::args().nth(2).unwrap_or_else(|| "/tmp/opencode/drill.db".into());
    let store = Arc::new(SqliteStore::open(&db).await?);

    match mode.as_str() {
        "run" => {
            let ws = std::env::temp_dir().join("chuan-drill-ws");
            std::fs::create_dir_all(&ws)?;
            let env = BwrapEnv::new(&ws);
            let mut tools = ToolRegistry::new();
            tools.register(Arc::new(ExecTool::sandboxed()));
            let runner = Runner {
                store: &*store,
                driver: &OneShotDriver,
                tools: &tools,
                env: &env,
                gate: None,
                outbox: None,
                docs: None,
            };
            let pair = EventSink::channel(32);
            let req = RunRequest {
                conv: ConversationId("drill".into()),
                run: RunId("drill-1".into()),
                text: "drill".into(),
                model: "drill".into(),
                max_turns: 4,
                wall_clock_ms: 60_000,
                workspace: Workspace { run: RunId("drill-1".into()), root: ws.to_string_lossy().into() },
                system_prompt: None,
                images: Vec::new(),
                actor: "drill".into(),
                context_budget_chars: 100_000,
            };
            println!("drill running; kill -9 me now");
            let _ = runner.run(req, pair.sink, CancellationToken::new()).await;
        }
        "check" => {
            let n = store.recover().await?;
            println!("recovered={n}");
            let mut rd = store.begin_read().await?;
            for row in rd.tool_calls(ConversationId("drill".into())).await? {
                println!("tool {} status={}", row.call.0, row.status);
            }
            let view = rd.view(ConversationId("drill".into())).await?;
            println!("run_active={:?} last_seq={}", view.run.map(|r| r.0), view.last_seq);
        }
        _ => eprintln!("用法: crash_drill run|check <db>"),
    }
    Ok(())
}
