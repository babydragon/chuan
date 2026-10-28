//! `delegate` 的门槛：没有 spawner 必须直接拒绝；有 spawner 时只回结论。

use std::sync::Mutex;

use async_trait::async_trait;
use harness_capabilities::DelegateTool;
use harness_kernel::{
    AgentError, AgentProfile, AgentSpec, AgentSpawner, AgentStatus, Conclusion, ErrorCode,
    ExecutionEnv, ExecOpts, ExecOutput, NullDocReader, NullOutbox, Tool, ToolCtx, UsageSnapshot,
    Workspace,
};
use tokio_util::sync::CancellationToken;

struct NullEnv;

#[async_trait]
impl ExecutionEnv for NullEnv {
    async fn exec(
        &self,
        _argv: &[String],
        _opts: ExecOpts,
        _cancel: CancellationToken,
    ) -> Result<ExecOutput, harness_kernel::EnvError> {
        Ok(ExecOutput { code: Some(0), stdout: String::new(), stderr: String::new(), truncated: false })
    }
}

#[derive(Default)]
struct Seen {
    spec: Option<AgentSpec>,
}

struct StubSpawner {
    seen: Mutex<Seen>,
}

#[async_trait]
impl AgentSpawner for StubSpawner {
    async fn spawn(
        &self,
        spec: AgentSpec,
        _cancel: CancellationToken,
    ) -> Result<Conclusion, AgentError> {
        self.seen.lock().unwrap().spec = Some(spec);
        Ok(Conclusion {
            status: AgentStatus::Done,
            summary: "查到了 42".into(),
            child_conv: "sub:c#1".into(),
            child_run: "r1".into(),
            artifacts: vec!["/ws/x.txt".into()],
            pending_actions: vec![],
            usage: UsageSnapshot::default(),
        })
    }
}

fn ctx<'a>(
    ws: &'a Workspace,
    env: &'a NullEnv,
    spawner: Option<&'a dyn AgentSpawner>,
) -> ToolCtx<'a> {
    ToolCtx {
        conv: harness_kernel::ConversationId("fake:1:0".into()),
        actor: "42",
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("c".into()),
        workspace: ws,
        env,
        outbox: &NullOutbox,
        docs: &NullDocReader,
        system_prompt: "SYS-PROMPT",
        spawner,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn without_spawner_it_is_not_permitted() {
    let env = NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp/ws".into() };
    let c = ctx(&ws, &env, None);
    let err = DelegateTool
        .execute(serde_json::json!({"task":"查点东西"}), &c)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotPermitted);
    assert!(err.message.contains("不能派生"), "{}", err.message);
}

#[tokio::test]
async fn empty_task_is_rejected() {
    let env = NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp/ws".into() };
    let spawner = StubSpawner { seen: Mutex::new(Seen::default()) };
    let c = ctx(&ws, &env, Some(&spawner));
    let err = DelegateTool
        .execute(serde_json::json!({"task":"   "}), &c)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgs);
}

#[tokio::test]
async fn passes_parent_prompt_and_depth_one() {
    let env = NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp/ws".into() };
    let spawner = StubSpawner { seen: Mutex::new(Seen::default()) };
    let c = ctx(&ws, &env, Some(&spawner));

    let out = DelegateTool
        .execute(
            serde_json::json!({
                "task": "读 /ws/a.md 并给出三条结论",
                "inputs": ["/ws/a.md", "/etc/passwd"],
                "role": "fast"
            }),
            &c,
        )
        .await
        .unwrap();

    let spec = spawner.seen.lock().unwrap().spec.clone().expect("应派发一次");
    assert_eq!(spec.depth, 1, "拓扑单层：depth 恒为 1");
    assert_eq!(spec.parent_conv, "fake:1:0");
    assert_eq!(spec.system_prompt.as_deref(), Some("SYS-PROMPT"), "子 agent 复用父前缀");
    assert_eq!(spec.inputs, vec!["/ws/a.md".to_owned()], "inputs 只收 /ws 下的路径");
    assert_eq!(spec.profile, AgentProfile { name: "fast".into(), ..Default::default() });

    let text = format!("{:?}", out.content);
    assert!(text.contains("查到了 42"), "{text}");
    assert!(text.contains("sub:c#1"), "结论应指出完整过程在哪: {text}");

    // 前台委托会跑几十秒：工具轨迹要显示"在跑什么"
    let hint = DelegateTool
        .approval_hint(&serde_json::json!({"task":"读 /ws/a.md 并给出三条结论"}))
        .unwrap();
    assert!(hint.contains("跑子任务"), "{hint}");
    assert!(hint.contains("读 /ws/a.md"), "{hint}");
}
