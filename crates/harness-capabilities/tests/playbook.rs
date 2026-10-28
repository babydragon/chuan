//! playbook：内置规范要能被列出来、能取到内容，且文件可覆盖。

use harness_capabilities::PlaybookTool;
use harness_kernel::{Tool, ToolCtx, Workspace};
use tokio_util::sync::CancellationToken;

fn ctx<'a>(ws: &'a Workspace, env: &'a harness_capabilities::NullEnv) -> ToolCtx<'a> {
    ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
        outbox: &harness_kernel::NullOutbox,
        docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("c".into()),
        workspace: ws,
        env,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn builtin_webhook_playbook_is_listed_and_readable() {
    let dir = tempfile::tempdir().unwrap();
    let tool = PlaybookTool::new(
        dir.path(),
        vec![("webhook", "钉钉：1) 加签 2) 放凭据 3) service add 4) 验证")],
    );
    let spec = tool.spec();
    assert!(spec.description.contains("webhook"), "{}", spec.description);

    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let out = tool
        .execute(serde_json::json!({"topic":"webhook"}), &ctx(&ws, &env))
        .await
        .unwrap();
    let text = match &out.content[0] {
        harness_kernel::Block::Paragraph(t) => t.clone(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(text.contains("放凭据"), "{text}");
}

#[tokio::test]
async fn file_overrides_builtin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("webhook.md"), "本地覆盖版：先看群设置").unwrap();
    let tool = PlaybookTool::new(dir.path(), vec![("webhook", "内置版")]);
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let out = tool
        .execute(serde_json::json!({"topic":"webhook"}), &ctx(&ws, &env))
        .await
        .unwrap();
    let text = match &out.content[0] {
        harness_kernel::Block::Paragraph(t) => t.clone(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(text.contains("本地覆盖版"), "{text}");
}
