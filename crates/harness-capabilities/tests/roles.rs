//! 角色目录：agent 写进去的东西，下一次就真能用。

use harness_capabilities::roles::{DiskRoles, RoleTool};
use harness_kernel::{
    ErrorCode, NullDocReader, NullOutbox, RoleSource, Tool, ToolCtx, Workspace,
};
use tokio_util::sync::CancellationToken;

struct NullEnv;

#[async_trait::async_trait]
impl harness_kernel::ExecutionEnv for NullEnv {
    async fn exec(
        &self,
        _argv: &[String],
        _opts: harness_kernel::ExecOpts,
        _cancel: CancellationToken,
    ) -> Result<harness_kernel::ExecOutput, harness_kernel::EnvError> {
        Ok(harness_kernel::ExecOutput {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        })
    }
}

fn ctx<'a>(ws: &'a Workspace, env: &'a NullEnv) -> ToolCtx<'a> {
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
        spawner: None,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn add_list_remove_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let roles_dir = dir.path().join("roles");
    let tool = RoleTool::new(&roles_dir);
    let env = NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp/ws".into() };
    let c = ctx(&ws, &env);

    let out = tool
        .execute(
            serde_json::json!({
                "action": "add",
                "name": "coder",
                "description": "写代码",
                "prompt": "你是写代码的子 agent，先读再改。",
                "tools": ["read", "write", "exec"],
                "max_turns": 10
            }),
            &c,
        )
        .await
        .unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("已注册角色"), "{text}");

    // 文件真的落了盘，且能被读回来
    assert!(roles_dir.join("coder.toml").exists());
    assert!(roles_dir.join("coder.md").exists());
    let all = DiskRoles::new(&roles_dir).all();
    assert_eq!(all.len(), 1);
    let (name, spec) = &all[0];
    assert_eq!(name, "coder");
    assert_eq!(spec.description.as_deref(), Some("写代码"));
    assert!(spec.system_prompt.as_deref().unwrap().contains("先读再改"));
    assert_eq!(spec.tools.as_ref().unwrap(), &vec!["read".to_owned(), "write".to_owned(), "exec".to_owned()]);
    assert_eq!(spec.max_turns, 10);

    // list 里能看到它
    let listed = tool.execute(serde_json::json!({"action":"list"}), &c).await.unwrap();
    let text = format!("{:?}", listed.content);
    assert!(text.contains("coder"), "{text}");
    assert!(text.contains("写代码"), "{text}");

    // 删除
    tool.execute(serde_json::json!({"action":"remove","name":"coder"}), &c).await.unwrap();
    assert!(!roles_dir.join("coder.toml").exists());
    assert!(DiskRoles::new(&roles_dir).all().is_empty());
}

#[tokio::test]
async fn rejects_bad_names_and_empty_roles() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RoleTool::new(dir.path().join("roles"));
    let env = NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp/ws".into() };
    let c = ctx(&ws, &env);

    let err = tool
        .execute(serde_json::json!({"action":"add","name":"../evil","prompt":"x"}), &c)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgs);

    let err = tool
        .execute(serde_json::json!({"action":"add","name":"coder"}), &c)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgs, "没说明也没提示词的角色没意义");

    let err = tool
        .execute(serde_json::json!({"action":"remove","name":"nope"}), &c)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn prompt_is_written_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let roles_dir = dir.path().join("roles");
    let tool = RoleTool::new(&roles_dir);
    let env = NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: "/tmp/ws".into() };
    let c = ctx(&ws, &env);

    let prompt = "第一行\n第二行 \"引号\" 与 \\反斜杠";
    tool.execute(
        serde_json::json!({"action":"add","name":"tutor","prompt": prompt}),
        &c,
    )
    .await
    .unwrap();

    let spec = DiskRoles::new(&roles_dir).all().pop().unwrap().1;
    assert_eq!(spec.system_prompt.as_deref(), Some(prompt), "提示词不需要 TOML 转义，必须原样");
}
