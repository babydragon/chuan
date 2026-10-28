//! `task` 工具：父 agent 能查、能停自己派出去的后台任务。

use std::sync::Mutex;

use async_trait::async_trait;
use harness_capabilities::TaskTool;
use harness_kernel::{
    AgentError, AgentSpawner, AgentSpec, Conclusion, ErrorCode, ExecutionEnv, ExecOpts, ExecOutput,
    NullDocReader, NullOutbox, TaskInfo, Tool, ToolCtx, Workspace,
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

struct Stub {
    cancelled: Mutex<Vec<String>>,
}

#[async_trait]
impl AgentSpawner for Stub {
    async fn spawn(
        &self,
        _spec: AgentSpec,
        _cancel: CancellationToken,
    ) -> Result<Conclusion, AgentError> {
        Err(AgentError::Other("unused".into()))
    }

    async fn tasks(&self) -> Result<Vec<TaskInfo>, AgentError> {
        Ok(vec![
            TaskInfo {
                id: "task-1".into(),
                status: "pending".into(),
                text: "把 /ws/refactor-demo 的重复函数抽到 common.py".into(),
                created_ms: 0,
                started_ms: None,
                child_conv: Some("sub:c#1".into()),
                result: None,
            },
            TaskInfo {
                id: "task-2".into(),
                status: "done".into(),
                text: "查价格".into(),
                created_ms: 0,
                started_ms: Some(1),
                child_conv: Some("sub:c#2".into()),
                result: Some("查到了 3 个型号".into()),
            },
        ])
    }

    async fn cancel_task(&self, id: &str) -> Result<String, AgentError> {
        self.cancelled.lock().unwrap().push(id.to_owned());
        Ok(format!("已取消 {id}"))
    }
}

fn ctx<'a>(ws: &'a Workspace, env: &'a NullEnv, sp: Option<&'a dyn AgentSpawner>) -> ToolCtx<'a> {
    ToolCtx {
        conv: harness_kernel::ConversationId("fake:1:0".into()),
        actor: "42",
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("c".into()),
        workspace: ws,
        env,
        outbox: &NullOutbox,
        docs: &NullDocReader,
        system_prompt: "",
        spawner: sp,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn lists_and_shows_status() {
    let env = NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp/ws".into() };
    let sp = Stub { cancelled: Mutex::new(vec![]) };
    let c = ctx(&ws, &env, Some(&sp));

    let out = TaskTool.execute(serde_json::json!({"action":"list"}), &c).await.unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("task-1"), "{text}");
    assert!(text.contains("task-2"), "{text}");
    assert!(text.contains("common.py"), "{text}");

    let out = TaskTool
        .execute(serde_json::json!({"action":"status","id":"task-2"}), &c)
        .await
        .unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("查到了 3 个型号"), "应带结论: {text}");
    assert!(text.contains("sub:c#2"), "应指出完整过程在哪: {text}");

    let err = TaskTool
        .execute(serde_json::json!({"action":"status","id":"nope"}), &c)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn cancels_by_id() {
    let env = NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp/ws".into() };
    let sp = Stub { cancelled: Mutex::new(vec![]) };
    let c = ctx(&ws, &env, Some(&sp));

    let out = TaskTool
        .execute(serde_json::json!({"action":"cancel","id":"task-1"}), &c)
        .await
        .unwrap();
    assert!(format!("{:?}", out.content).contains("已取消 task-1"));
    assert_eq!(sp.cancelled.lock().unwrap().as_slice(), ["task-1"]);

    let err = TaskTool
        .execute(serde_json::json!({"action":"cancel"}), &c)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgs);
}

#[tokio::test]
async fn without_spawner_is_not_permitted() {
    let env = NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp/ws".into() };
    let c = ctx(&ws, &env, None);
    let err = TaskTool
        .execute(serde_json::json!({"action":"list"}), &c)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotPermitted);
}
