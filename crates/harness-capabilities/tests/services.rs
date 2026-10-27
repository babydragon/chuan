//! 命名 HTTP 服务：token 只在宿主注入、不落日志。

use std::sync::{Arc, Mutex};

use harness_capabilities::{HttpServiceTool, Service};
use harness_kernel::{Tool, ToolCtx, Workspace};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// 起一个只应答一次的最小 HTTP 服务，把收到的请求头回传给测试。
async fn one_shot_server() -> (String, Arc<Mutex<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(String::new()));
    let seen2 = Arc::clone(&seen);
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        *seen2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
        let body = r#"{"ok":true,"value":42}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn injects_token_and_hides_it() {
    let (base, seen) = one_shot_server().await;
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, "s3cret-token\n").unwrap();
    let log = dir.path().join("net.jsonl");

    let tool = HttpServiceTool::new(
        vec![Service {
            name: "demo".into(),
            base,
            token_file: Some(token_file),
            header: "Authorization: Bearer {token}".into(),
            extra_headers: vec![],
        }],
        &log,
    )
    .unwrap();

    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
        outbox: &harness_kernel::NullOutbox,
        docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("c".into()),
        workspace: &ws,
        env: &env,
        cancel: CancellationToken::new(),
    };

    let out = tool
        .execute(serde_json::json!({"service":"demo","method":"GET","path":"/items?page=1"}), &ctx)
        .await
        .unwrap();
    let text = match &out.content[0] {
        harness_kernel::Block::Paragraph(t) => t.clone(),
        other => panic!("unexpected block: {other:?}"),
    };
    assert!(text.contains("\"value\":42"), "{text}");

    // 服务端确实收到了注入的 header
    let req = seen.lock().unwrap().to_lowercase();
    assert!(req.contains("authorization: bearer s3cret-token"), "{req}");
    assert!(req.starts_with("get /items?page=1"), "{req}");

    // 日志里不能出现 token
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("demo:/items?page=1"), "{logged}");
    assert!(!logged.contains("s3cret"), "日志泄漏了 token: {logged}");
}

#[tokio::test]
async fn rejects_bad_paths_and_unknown_services() {
    let dir = tempfile::tempdir().unwrap();
    let tool = HttpServiceTool::new(
        vec![Service {
            name: "demo".into(),
            base: "https://example.com".into(),
            token_file: None,
            header: String::new(),
            extra_headers: vec![],
        }],
        dir.path().join("net.jsonl"),
    )
    .unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
        outbox: &harness_kernel::NullOutbox,
        docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("c".into()),
        workspace: &ws,
        env: &env,
        cancel: CancellationToken::new(),
    };

    let err = tool
        .execute(serde_json::json!({"service":"nope","method":"GET","path":"/x"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.message.contains("没有这个服务"), "{}", err.message);

    let err = tool
        .execute(serde_json::json!({"service":"demo","method":"GET","path":"x"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.message.contains("以 / 开头"), "{}", err.message);

    let err = tool
        .execute(
            serde_json::json!({"service":"demo","method":"GET","path":"https://evil.com/x"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("协议"), "{}", err.message);
}

/// 大 JSON 回包只给结构摘要，不把整段塞进上下文。
#[tokio::test]
async fn large_json_is_summarized() {
    let big = format!(
        r#"{{"books":[{}],"total":51}}"#,
        (0..300)
            .map(|i| format!(r#"{{"bookId":"b{i}","title":"书{i}","deepLink":"weread://x"}}"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = big.clone();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 2048];
        let _ = sock.read(&mut buf).await;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let tool = HttpServiceTool::new(
        vec![Service {
            name: "demo".into(),
            base: format!("http://{addr}"),
            token_file: None,
            header: String::new(),
            extra_headers: vec![],
        }],
        dir.path().join("net.jsonl"),
    )
    .unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
        outbox: &harness_kernel::NullOutbox,
        docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("c".into()),
        workspace: &ws,
        env: &env,
        cancel: CancellationToken::new(),
    };
    let out = tool
        .execute(serde_json::json!({"service":"demo","method":"GET","path":"/x"}), &ctx)
        .await
        .unwrap();
    let text = match &out.content[0] {
        harness_kernel::Block::Paragraph(t) => t.clone(),
        other => panic!("{other:?}"),
    };
    assert!(text.contains("books: array(300)"), "{text}");
    assert!(text.contains("首项键"), "{text}");
    assert!(text.chars().count() < 3500, "摘要应远小于原文: {}", text.chars().count());
    assert!(!text.contains("书299"), "不应把整段内容塞进上下文");
    assert!(text.contains("/ws/.tool-results/"), "应给出落盘路径: {text}");

    // 完整内容确实写到了工作区，能被 exec 读到
    let dumped = std::fs::read_dir(dir.path().join(".tool-results"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .next()
        .expect("应有落盘文件");
    let saved = std::fs::read_to_string(&dumped).unwrap();
    assert!(saved.contains("书299"), "落盘文件应是完整响应");
    assert!(saved.len() > 15_000);
}
