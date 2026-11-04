//! 自改事务的一致性测试：装 → 改 → 回滚 → 删（回收站），每一步都要落账。
//!
//! 这里只覆盖 service / role；skill / playbook 的同等断言在各自模块的单测里。

use std::sync::Arc;

use harness_capabilities::services::HttpServiceTool;
use harness_capabilities::{McpHub, RoleTool, ServiceTool};
use harness_kernel::{CallId, ConversationId, RunId, Tool, ToolCtx, Workspace};
use tokio_util::sync::CancellationToken;

struct Ctx<'a> {
    ws: Workspace,
    env: harness_capabilities::NullEnv,
    _dir: &'a tempfile::TempDir,
}

impl<'a> Ctx<'a> {
    fn new(dir: &'a tempfile::TempDir) -> Self {
        let ws_root = dir.path().join("ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        Self {
            ws: Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into_owned() },
            env: harness_capabilities::NullEnv,
            _dir: dir,
        }
    }

    fn ctx(&self) -> ToolCtx<'_> {
        ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: ConversationId("c".into()),
            actor: "owner",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
            run: RunId("r".into()),
            call: CallId("call".into()),
            workspace: &self.ws,
            env: &self.env,
            cancel: CancellationToken::new(),
        }
    }
}

fn service_tool(dir: &tempfile::TempDir) -> Arc<ServiceTool> {
    let secrets = dir.path().join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    ServiceTool::new(
        dir.path().join("services.d"),
        secrets,
        McpHub::new(vec![]),
        HttpServiceTool::new(vec![], dir.path().join("net.jsonl")).unwrap(),
    )
}

#[tokio::test]
async fn service_add_update_rollback_remove() {
    let dir = tempfile::tempdir().unwrap();
    let c = Ctx::new(&dir);
    let tool = service_tool(&dir);
    let cfg = dir.path().join("services.d");

    let out = tool
        .execute(
            serde_json::json!({"action":"add","name":"demo","base":"http://127.0.0.1:1/v1"}),
            &c.ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out.doc_writes.len(), 1, "add 必须落一条账");
    assert_eq!(out.doc_writes[0].kind, "service");
    assert_eq!(out.doc_writes[0].payload["action"], "add");
    let v1 = std::fs::read_to_string(cfg.join("demo.toml")).unwrap();
    assert!(v1.contains("127.0.0.1:1/v1"));

    tool.execute(
        serde_json::json!({"action":"add","name":"demo","base":"http://127.0.0.1:2/v2"}),
        &c.ctx(),
    )
    .await
    .unwrap();
    let backups = dir.path().join("services.d/.backup");
    assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 1, "覆盖前必须备份");

    let out = tool
        .execute(serde_json::json!({"action":"rollback","name":"demo"}), &c.ctx())
        .await
        .unwrap();
    assert_eq!(out.doc_writes[0].payload["action"], "rollback");
    let restored = std::fs::read_to_string(cfg.join("demo.toml")).unwrap();
    assert!(restored.contains("127.0.0.1:1/v1"), "回滚应恢复旧版：{restored}");

    tool.execute(serde_json::json!({"action":"remove","name":"demo"}), &c.ctx())
        .await
        .unwrap();
    assert!(!cfg.join("demo.toml").exists());
    let trash = dir.path().join("services.d/.trash");
    assert_eq!(std::fs::read_dir(&trash).unwrap().count(), 1, "删除进回收站");
}

#[tokio::test]
async fn service_rollback_without_history_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let c = Ctx::new(&dir);
    let tool = service_tool(&dir);
    let err = tool
        .execute(serde_json::json!({"action":"rollback","name":"nope"}), &c.ctx())
        .await
        .unwrap_err();
    assert_eq!(err.code, harness_kernel::ErrorCode::NotFound);
}

#[tokio::test]
async fn role_add_update_rollback_remove() {
    let dir = tempfile::tempdir().unwrap();
    let c = Ctx::new(&dir);
    let tool = RoleTool::new(dir.path().join("roles")).with_allowed(vec!["read".into()]);

    let out = tool
        .execute(
            serde_json::json!({
                "action":"add","name":"coder","description":"写代码",
                "prompt":"你只管改代码。","tools":["read"],"model":"m1"
            }),
            &c.ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out.doc_writes.len(), 1);
    assert_eq!(out.doc_writes[0].payload["action"], "add");

    tool.execute(
        serde_json::json!({"action":"add","name":"coder","model":"m2"}),
        &c.ctx(),
    )
    .await
    .unwrap();
    let toml = std::fs::read_to_string(dir.path().join("roles/coder.toml")).unwrap();
    assert!(toml.contains("m2"));

    let out = tool
        .execute(serde_json::json!({"action":"rollback","name":"coder"}), &c.ctx())
        .await
        .unwrap();
    assert_eq!(out.doc_writes[0].payload["action"], "rollback");
    let restored = std::fs::read_to_string(dir.path().join("roles/coder.toml")).unwrap();
    assert!(restored.contains("m1"), "回滚应恢复旧模型：{restored}");
    // 提示词一起回滚（第一版有写 prompt）
    assert!(dir.path().join("roles/coder.md").exists());

    tool.execute(serde_json::json!({"action":"remove","name":"coder"}), &c.ctx())
        .await
        .unwrap();
    assert!(!dir.path().join("roles/coder.toml").exists());
    assert_eq!(
        std::fs::read_dir(dir.path().join("roles/.trash")).unwrap().count(),
        2,
        "toml 与 md 都进回收站"
    );
}
