//! 出网工具的集成测试：需要网络；拿不到就跳过。

use harness_capabilities::{NetLogTool, WebFetchTool};
use harness_kernel::{Tool, ToolCtx, Workspace};
use tokio_util::sync::CancellationToken;

fn has_dns() -> bool {
    std::process::Command::new("python3")
        .args(["-c", "import socket;socket.gethostbyname('example.com')"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ctx<'a>(ws: &'a Workspace, env: &'a harness_capabilities::NullEnv) -> ToolCtx<'a> {
    ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("call".into()),
        workspace: ws,
        env,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn fetch_public_page_and_write_log() {
    if !has_dns() {
        eprintln!("skip: no dns");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("net.jsonl");
    let tool = WebFetchTool::new(&log).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let out = tool
        .execute(serde_json::json!({"url":"https://example.com"}), &ctx(&ws, &env))
        .await
        .unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("HTTP 200"), "{text}");

    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("example.com"), "{logged}");
    assert!(logged.contains("\"note\":\"ok\""), "{logged}");
}

#[tokio::test]
async fn refuses_loopback_and_logs_the_denial() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("net.jsonl");
    let tool = WebFetchTool::new(&log).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let err = tool
        .execute(serde_json::json!({"url":"http://127.0.0.1:8080/admin"}), &ctx(&ws, &env))
        .await
        .unwrap_err();
    assert!(err.message.contains("内网"), "{}", err.message);
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("127.0.0.1"), "被拒绝的访问也要留痕: {logged}");
}

#[tokio::test]
async fn net_log_aggregates_by_host() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("net.jsonl");
    let now = harness_kernel::now_ms();
    let lines = [
        serde_json::json!({"at": now, "url":"https://a.example/x?q=1", "host":"a.example", "query_len":3, "status":200, "bytes":100, "ms":5, "note":"ok"}),
        serde_json::json!({"at": now, "url":"https://a.example/y", "host":"a.example", "query_len":0, "status":500, "bytes":0, "ms":9, "note":"HTTP 500"}),
    ];
    std::fs::write(
        &log,
        lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n") + "\n",
    )
    .unwrap();

    let tool = NetLogTool::new(&log);
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let out = tool.execute(serde_json::json!({"days":1}), &ctx(&ws, &env)).await.unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("a.example"), "{text}");
    assert!(text.contains("2 次访问"), "{text}");
}
