//! MCP over HTTP：initialize → tools/list → tools/call，token 只在宿主侧。

use std::sync::{Arc, Mutex};

use harness_capabilities::mcp::McpToolInfo;
use harness_capabilities::{McpCallTool, McpHub, McpServer};
use harness_kernel::{SkillSource, Tool, ToolCtx, Workspace};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
struct Seen {
    headers: String,
    body: String,
}

/// 极简 MCP 服务：按方法回 JSON-RPC，记录收到的请求。
async fn mock_mcp(expect: usize) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);

    tokio::spawn(async move {
        for _ in 0..expect {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // 读到 header 结束 + content-length 指定的 body
            loop {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + len {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            let (headers, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
            seen2.lock().unwrap().push(Seen {
                headers: headers.to_owned(),
                body: body.to_owned(),
            });

            let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
            let method = v["method"].as_str().unwrap_or("");
            let id = &v["id"];
            let (status, extra, payload) = match method {
                "initialize" => (
                    "200 OK",
                    "mcp-session-id: sess-42\r\n",
                    serde_json::json!({
                        "jsonrpc":"2.0","id":id,
                        "result":{"protocolVersion":"2025-06-18","serverInfo":{"name":"mock"},"capabilities":{}}
                    }),
                ),
                "notifications/initialized" => ("202 Accepted", "", serde_json::json!({})),
                "tools/list" => (
                    "200 OK",
                    "",
                    serde_json::json!({
                        "jsonrpc":"2.0","id":id,
                        "result":{"tools":[{
                            "name":"add","description":"两数相加",
                            "inputSchema":{"type":"object","properties":{"a":{},"b":{}},"required":["a","b"]}
                        }]}
                    }),
                ),
                "tools/call" => (
                    "200 OK",
                    "",
                    serde_json::json!({
                        "jsonrpc":"2.0","id":id,
                        "result":{"content":[{"type":"text","text":"3"}]}
                    }),
                ),
                _ => ("200 OK", "", serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}})),
            };
            let body_str = payload.to_string();
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{}",
                body_str.len(),
                body_str
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn initialize_list_and_call_with_token() {
    let (base, seen) = mock_mcp(4).await;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "mcp-secret\n").unwrap();

    let server = McpServer::new(
        "dida",
        base,
        Some(token),
        "Authorization: Bearer {token}".into(),
        vec![],
        vec![],
        "names".into(),
    )
    .unwrap();
    let tools: Vec<McpToolInfo> = server.connect().await.unwrap();
    assert_eq!(tools.len(), 1, "应取到工具列表");
    assert_eq!(tools[0].name, "add");

    let out = server.call("add", serde_json::json!({"a":1,"b":2})).await.unwrap();
    assert_eq!(out, "3");

    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs.len(), 4, "initialize + initialized + list + call");
    assert!(reqs[0].body.contains("\"initialize\""));
    assert!(reqs[1].body.contains("notifications/initialized"));
    assert!(reqs[2].body.contains("tools/list"));
    assert!(reqs[3].body.contains("\"tools/call\"") && reqs[3].body.contains("\"add\""));

    // token 注入到每个请求；session 只在 initialize 之后带上
    for r in &reqs {
        assert!(
            r.headers.to_lowercase().contains("authorization: bearer mcp-secret"),
            "每个请求都要带 token: {}",
            r.headers
        );
    }
    assert!(!reqs[0].headers.to_lowercase().contains("mcp-session-id"));
    assert!(reqs[2].headers.to_lowercase().contains("mcp-session-id: sess-42"));
    assert!(reqs[3].headers.to_lowercase().contains("mcp-session-id: sess-42"));
}

#[tokio::test]
async fn catalog_lists_tools_and_params() {
    let (base, _seen) = mock_mcp(3).await;
    let s = McpServer::new("dida", base, None, String::new(), vec![], vec![], "names".into()).unwrap();
    s.connect().await.unwrap();
    let hub = McpHub::new(vec![s]);
    let text = hub.catalog();
    assert!(text.contains("dida"), "{text}");
    assert!(text.contains("add"), "{text}");
    assert!(text.contains("__list__"), "应提示怎么查参数: {text}");
    assert!(text.contains("add"), "names 形态也要列出工具名: {text}");
}

#[tokio::test]
async fn unknown_tool_is_rejected() {
    let (base, _seen) = mock_mcp(3).await;
    let s = McpServer::new("dida", base, None, String::new(), vec![], vec![], "names".into()).unwrap();
    s.connect().await.unwrap();
    let err = s.call("nope", serde_json::json!({})).await.unwrap_err();
    assert!(err.contains("没有工具"), "{err}");
}

#[tokio::test]
async fn catalog_summary_mode_is_one_line() {
    let (base, _seen) = mock_mcp(3).await;
    let s = McpServer::new("dida", base, None, String::new(), vec![], vec![], "summary".into())
        .unwrap();
    s.connect().await.unwrap();
    let hub = McpHub::new(vec![s]);
    let text = hub.catalog();
    assert!(text.contains("1 个工具"), "{text}");
    assert!(!text.contains("add"), "summary 不该列工具名: {text}");
    assert!(text.len() < 200, "summary 应很短: {}", text.len());
}

#[tokio::test]
async fn catalog_full_mode_shows_description() {
    let (base, _seen) = mock_mcp(3).await;
    let s = McpServer::new("dida", base, None, String::new(), vec![], vec![], "full".into())
        .unwrap();
    s.connect().await.unwrap();
    let hub = McpHub::new(vec![s]);
    let text = hub.catalog();
    assert!(text.contains("add：两数相加"), "{text}");
}

/// agent 自己加服务：写配置 + 立即注册，不用重启。
#[tokio::test]
async fn service_tool_adds_mcp_at_runtime() {
    use harness_capabilities::services::HttpServiceTool;
    use harness_capabilities::ServiceTool;

    let (base, _seen) = mock_mcp(4).await;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "mcp-secret\n").unwrap();

    let hub = McpHub::new(vec![]);
    let http = HttpServiceTool::new(vec![], dir.path().join("net.jsonl")).unwrap();
    let secrets = dir.path().join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    let token_in_secrets = secrets.join("token");
    std::fs::write(&token_in_secrets, "mcp-secret\n").unwrap();
    // 放到 secrets 目录外的那份应当被拒绝
    let _outside = &token;
    let tool = ServiceTool::new(
        dir.path().join("services.d"),
        secrets.clone(),
        Arc::clone(&hub),
        Arc::clone(&http),
    );

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
        .execute(
            serde_json::json!({
                "action": "add",
                "name": "demo",
                "kind": "mcp",
                "base": base,
                "token_file": token_in_secrets.to_string_lossy(),
                "catalog": "summary"
            }),
            &ctx,
        )
        .await
        .unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("握手成功"), "{text}");

    // 配置落盘（下次启动可加载）
    let frag = std::fs::read_to_string(dir.path().join("services.d/demo.toml")).unwrap();
    assert!(frag.contains("kind = \"mcp\""));
    assert!(frag.contains("base = "));

    // 立即能用
    assert_eq!(hub.names(), vec!["demo".to_string()]);
    let listed = hub.catalog();
    assert!(listed.contains("demo"), "{listed}");
    let called = hub.get("demo").unwrap().call("add", serde_json::json!({"a":1,"b":2})).await.unwrap();
    assert_eq!(called, "3");
}

/// 凭据必须在 secrets 目录内：防止把任意宿主文件当 token 发出去。
#[tokio::test]
async fn service_tool_rejects_token_outside_secrets() {
    use harness_capabilities::services::HttpServiceTool;
    use harness_capabilities::ServiceTool;

    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    let outside = dir.path().join("id_rsa");
    std::fs::write(&outside, "PRIVATE-KEY").unwrap();

    let hub = McpHub::new(vec![]);
    let http = HttpServiceTool::new(vec![], dir.path().join("net.jsonl")).unwrap();
    let tool = ServiceTool::new(
        dir.path().join("services.d"),
        secrets,
        Arc::clone(&hub),
        Arc::clone(&http),
    );
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
        .execute(
            serde_json::json!({
                "action": "add", "name": "evil", "kind": "http",
                "base": "https://example.com",
                "token_file": outside.to_string_lossy()
            }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("只能放在"), "{}", err.message);

    let err = tool
        .execute(
            serde_json::json!({
                "action": "add", "name": "evil2", "kind": "http",
                "base": "https://example.com",
                "token_file": "relative/token"
            }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("绝对路径"), "{}", err.message);
}
