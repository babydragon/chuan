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
            ..Default::default()
        }],
        &log,
    )
    .unwrap();

    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ToolCtx {
        system_prompt: "",
        spawner: None,
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
            ..Default::default()
        }],
        dir.path().join("net.jsonl"),
    )
    .unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ToolCtx {
        system_prompt: "",
        spawner: None,
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
            ..Default::default()
        }],
        dir.path().join("net.jsonl"),
    )
    .unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ToolCtx {
        system_prompt: "",
        spawner: None,
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

/// 起一个把请求原样回显成响应体的服务（用来验证响应里回显 token 时会不会被抹掉）。
async fn echo_server() -> (String, Arc<Mutex<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(String::new()));
    let seen2 = Arc::clone(&seen);
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        *seen2.lock().unwrap() = req.clone();
        // 故意把整个请求（含 URL 与 body）回显出来
        let body = format!(r#"{{"echo":{}}}"#, serde_json::to_string(&req).unwrap());
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    });
    (format!("http://{addr}"), seen)
}

fn ctx_for<'a>(
    ws: &'a Workspace,
    env: &'a harness_capabilities::NullEnv,
) -> ToolCtx<'a> {
    ToolCtx {
        system_prompt: "",
        spawner: None,
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

/// 钉钉形态：access_token/timestamp/sign 都放 query，签名用毫秒 + 第 2 行 secret。
#[tokio::test]
async fn injects_query_and_hmac_signature() {
    use harness_capabilities::auth::{Encode, SignConfig};
    let (base, seen) = echo_server().await;
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("dingtalk");
    std::fs::write(&token_file, "acc-token-123\nSECsecret\n").unwrap();

    let mut svc = Service {
        name: "dingtalk".into(),
        base,
        token_file: Some(token_file),
        sign: Some(SignConfig {
            secret_key: None,
            canonical: "{ts_ms}\n{secret}".into(),
            encode: Encode::Base64,
        }),
        ..Default::default()
    };
    svc.query.insert("access_token".into(), "{token}".into());
    svc.query.insert("timestamp".into(), "{ts_ms}".into());
    svc.query.insert("sign".into(), "{sign}".into());

    let log = dir.path().join("net.jsonl");
    let tool = HttpServiceTool::new(vec![svc], &log).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    tool.execute(
        serde_json::json!({"service":"dingtalk","method":"POST","path":"/robot/send","body":{"msgtype":"text"}}),
        &ctx,
    )
    .await
    .unwrap();

    let req = seen.lock().unwrap().clone();
    assert!(req.contains("access_token=acc-token-123"), "{req}");
    assert!(req.contains("timestamp="), "缺少 timestamp: {req}");
    assert!(req.contains("sign="), "缺少 sign: {req}");
    assert!(req.contains("msgtype"), "body 丢了: {req}");

    // 审计日志里不能出现 token（query 会被打码）
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(!logged.contains("acc-token-123"), "日志泄漏了 token: {logged}");
    assert!(logged.contains("dingtalk:/robot/send"), "{logged}");
}

/// 飞书形态：hook id 在 path 里，timestamp/sign 放 body，用秒。
#[tokio::test]
async fn injects_path_token_and_body_signature() {
    use harness_capabilities::auth::{Encode, SignConfig};
    let (base, seen) = echo_server().await;
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("feishu");
    std::fs::write(&token_file, "hook-id-abc\nfeishu-secret\n").unwrap();

    let mut svc = Service {
        name: "feishu".into(),
        base,
        token_file: Some(token_file),
        sign: Some(SignConfig {
            secret_key: None,
            canonical: "{ts_s}\n{secret}".into(),
            encode: Encode::Base64,
        }),
        ..Default::default()
    };
    svc.body.insert("timestamp".into(), "{ts_s}".into());
    svc.body.insert("sign".into(), "{sign}".into());

    let log = dir.path().join("net.jsonl");
    let tool = HttpServiceTool::new(vec![svc], &log).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    tool.execute(
        serde_json::json!({
            "service":"feishu","method":"POST",
            "path":"/open-apis/bot/v2/hook/{token}",
            "body":{"msg_type":"text"}
        }),
        &ctx,
    )
    .await
    .unwrap();

    let req = seen.lock().unwrap().clone();
    assert!(req.contains("/open-apis/bot/v2/hook/hook-id-abc"), "path 没注入: {req}");
    assert!(req.contains("\"timestamp\""), "{req}");
    assert!(req.contains("\"sign\""), "{req}");

    // path 里带凭据 → 日志必须打码
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(!logged.contains("hook-id-abc"), "日志泄漏了 hook id: {logged}");
}

/// 上游把 token 回显在响应里时，工具输出必须是打码后的。
#[tokio::test]
async fn scrubs_echoed_token_from_response() {
    let (base, _seen) = echo_server().await;
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("t");
    std::fs::write(&token_file, "echo-token-abcdef\n").unwrap();

    let svc = Service {
        name: "echo".into(),
        base,
        token_file: Some(token_file),
        header: "Authorization: Bearer {token}".into(),
        ..Default::default()
    };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let out = tool
        .execute(serde_json::json!({"service":"echo","method":"GET","path":"/dump"}), &ctx)
        .await
        .unwrap();
    let text = match &out.content[0] {
        harness_kernel::Block::Paragraph(t) => t.clone(),
        other => panic!("unexpected block: {other:?}"),
    };
    assert!(!text.contains("echo-token-abcdef"), "响应回显的 token 没被抹掉: {text}");
    assert!(text.contains("***"), "{text}");
}

/// body_file 只能读 workspace 内的文件，越界要拒绝。
#[tokio::test]
async fn body_file_is_jailed_to_workspace() {
    let (base, seen) = echo_server().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("msg.json"), r#"{"hello":"world"}"#).unwrap();
    let svc = Service { name: "s".into(), base, ..Default::default() };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let err = tool
        .execute(serde_json::json!({"service":"s","method":"POST","path":"/x","body_file":"../etc/passwd"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.message.contains("相对路径") || err.message.contains("越出"), "{}", err.message);

    tool.execute(
        serde_json::json!({"service":"s","method":"POST","path":"/x","body_file":"msg.json"}),
        &ctx,
    )
    .await
    .unwrap();
    let req = seen.lock().unwrap().clone();
    assert!(req.contains(r#""hello":"world""#), "body_file 内容没发出去: {req}");
}

/// base 带路径 + path 又写一遍 → 明确报"路径重复"，而不是等 404。
#[tokio::test]
async fn duplicate_base_and_path_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let svc = Service {
        name: "dingtalk".into(),
        base: "https://oapi.dingtalk.com/robot/send".into(),
        ..Default::default()
    };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let err = tool
        .execute(
            serde_json::json!({"service":"dingtalk","method":"POST","path":"/robot/send","body":{}}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("路径重复"), "{}", err.message);
    assert!(err.message.contains("只到域名"), "{}", err.message);
}

/// 302 到白名单主机：宿主侧跳一次、流式落盘、返回 sha256，且签名 URL 不进结果。
#[tokio::test]
async fn save_to_follows_allowed_redirect_and_hashes() {
    let payload = b"%PDF-1.4\nfake textbook bytes\n";
    let (file_base, _seen) = binary_server(payload).await;
    let (redir_base, _seen2) = redirect_server(&format!("{file_base}/file?sign=SECRET-SIGN")).await;

    let dir = tempfile::tempdir().unwrap();
    let svc = Service {
        name: "storage".into(),
        base: redir_base,
        redirect_hosts: vec!["127.0.0.1".into()],
        ..Default::default()
    };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let out = tool
        .execute(
            serde_json::json!({"service":"storage","method":"GET","path":"/file/book.pdf","save_to":"book.pdf"}),
            &ctx,
        )
        .await
        .unwrap();
    let text = match &out.content[0] {
        harness_kernel::Block::Paragraph(t) => t.clone(),
        other => panic!("{other:?}"),
    };
    assert!(!text.contains("SECRET-SIGN"), "签名 URL 不能出现在结果里: {text}");
    let path = dir.path().join("book.pdf");
    assert_eq!(std::fs::read(&path).unwrap(), payload, "落盘内容必须与响应体一致");
    // sha256(payload) 前 8 位，防止只写了个空文件
    assert!(text.contains("sha256"), "{text}");
    assert!(!dir.path().join(".book.pdf.part").exists(), "临时文件必须被重命名掉");
}

/// 目标主机不在 `redirect_hosts` 里 → 拒绝，且不落盘。
#[tokio::test]
async fn save_to_refuses_unlisted_redirect_host() {
    let (redir_base, _seen) = redirect_server("http://127.0.0.1:1/x").await;
    let dir = tempfile::tempdir().unwrap();
    let svc = Service {
        name: "storage".into(),
        base: redir_base,
        redirect_hosts: vec!["*.aliyundrive.com".into()],
        ..Default::default()
    };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let err = tool
        .execute(
            serde_json::json!({"service":"storage","method":"GET","path":"/file/x","save_to":"x.pdf"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, harness_kernel::ErrorCode::NotPermitted, "{}", err.message);
    assert!(err.message.contains("redirect_hosts"), "{}", err.message);
    assert!(!dir.path().join("x.pdf").exists(), "被拒时不能留下文件");
}

/// 重定向补头**按目标主机**生效：命中才加，不命中一个都不加。
/// 这一条保证"百度的 UA"不会跟着同一个 alist 服务发给阿里云。
#[tokio::test]
async fn redirect_headers_are_scoped_to_matched_host() {
    let payload = b"%PDF-1.4\nbytes\n";
    let (file_base, seen) = binary_server(payload).await;
    let (redir_base, _seen2) = redirect_server(&format!("{file_base}/file?sign=SECRET-SIGN")).await;

    let dir = tempfile::tempdir().unwrap();
    let svc = Service {
        name: "alist".into(),
        base: redir_base,
        redirect_hosts: vec!["127.0.0.1".into(), "*.baidu.com".into()],
        redirect_rules: vec![
            harness_capabilities::services::RedirectRule {
                hosts: vec!["127.0.0.1".into()],
                headers: vec![("User-Agent".into(), "pan.baidu.com".into())],
            },
            harness_capabilities::services::RedirectRule {
                hosts: vec!["*.baidu.com".into()],
                headers: vec![("Referer".into(), "https://pan.baidu.com/".into())],
            },
        ],
        ..Default::default()
    };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    tool.execute(
        serde_json::json!({"service":"alist","method":"GET","path":"/d/x.pdf?sign=S","save_to":"x.pdf"}),
        &ctx,
    )
    .await
    .unwrap();

    let req = seen.lock().unwrap().to_lowercase();
    assert!(req.contains("user-agent: pan.baidu.com"), "命中的规则没生效: {req}");
    assert!(!req.contains("referer:"), "不命中的规则不该带上: {req}");
    assert_eq!(std::fs::read(dir.path().join("x.pdf")).unwrap(), payload);
}

/// 第二跳不允许带凭据：写在重定向规则里也拒绝（宁可报错，也不悄悄发出去）。
#[tokio::test]
async fn redirect_headers_refuse_credentials() {
    let (file_base, _seen) = binary_server(b"x").await;
    let (redir_base, _seen2) = redirect_server(&format!("{file_base}/file")).await;

    let dir = tempfile::tempdir().unwrap();
    let svc = Service {
        name: "alist".into(),
        base: redir_base,
        redirect_hosts: vec!["127.0.0.1".into()],
        redirect_rules: vec![harness_capabilities::services::RedirectRule {
            hosts: vec!["127.0.0.1".into()],
            headers: vec![("Authorization".into(), "Bearer leak".into())],
        }],
        ..Default::default()
    };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let err = tool
        .execute(
            serde_json::json!({"service":"alist","method":"GET","path":"/d/x.bin","save_to":"x.bin"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("不允许"), "{}", err.message);
    assert!(err.message.contains("Authorization"), "{}", err.message);
    assert!(!dir.path().join("x.bin").exists());
}

/// JSON 接口误加 `save_to` → 明确报错，不把接口结果悄悄写进文件。
#[tokio::test]
async fn save_to_rejects_json_response() {
    let (base, _seen) = echo_server().await;
    let dir = tempfile::tempdir().unwrap();
    let svc = Service { name: "demo".into(), base, ..Default::default() };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let err = tool
        .execute(
            serde_json::json!({"service":"demo","method":"GET","path":"/api","save_to":"x.json"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("不是文件"), "{}", err.message);
    // 2xx 也一样要把响应体带出来：模型要靠它判断"接口回了什么"
    assert!(err.message.contains("echo"), "响应体丢了: {}", err.message);
    assert!(!dir.path().join("x.json").exists());
}

/// 上游 4xx/5xx 常常也是 JSON：必须报 `Unavailable` 并带出响应体，
/// 不能翻译成"响应不是文件"——否则模型会去改路径/编码，而不是看上游说了什么。
#[tokio::test]
async fn save_to_surfaces_upstream_error_body() {
    let (base, _seen) = status_server(
        "401 Unauthorized",
        "application/json",
        r#"{"code":401,"message":"sign mismatch"}"#,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("net.jsonl");
    let svc = Service { name: "alist".into(), base, ..Default::default() };
    let tool = HttpServiceTool::new(vec![svc], &log).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let err = tool
        .execute(
            serde_json::json!({
                "service":"alist","method":"GET",
                "path":"/d/book.pdf?sign=SECRET-SIGN","save_to":"book.pdf"
            }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, harness_kernel::ErrorCode::Unavailable, "{}", err.message);
    assert!(err.message.contains("401"), "{}", err.message);
    assert!(err.message.contains("sign mismatch"), "上游原因必须带出来: {}", err.message);
    assert!(!dir.path().join("book.pdf").exists(), "失败时不能留下文件");
    assert!(!dir.path().join(".book.pdf.part").exists());

    // 下载路径上的签名能直接换到文件本体：审计日志只留键、不留值
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(!logged.contains("SECRET-SIGN"), "日志泄漏了签名: {logged}");
    assert!(logged.contains("alist:/d/book.pdf?…"), "{logged}");
}

/// 下载不设总超时：同样的慢响应，API 调用会被总超时掐掉，`save_to` 要能下完。
/// 判据是"有没有新数据"，不是"总共花了多久"——512MB 上限配 30 秒总时限是自相矛盾的。
#[tokio::test]
async fn save_to_ignores_api_total_timeout() {
    let base = slow_server(400, std::time::Duration::from_millis(80)).await;
    let dir = tempfile::tempdir().unwrap();
    let svc = Service { name: "slow".into(), base, ..Default::default() };
    let tool = HttpServiceTool::with_limits(
        vec![svc],
        dir.path().join("net.jsonl"),
        &harness_capabilities::proxy::ProxyConfig::default(),
        harness_capabilities::Timeouts {
            api_total: std::time::Duration::from_millis(200),
            download_idle: Some(std::time::Duration::from_secs(5)),
        },
    )
    .unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    // 普通 API 调用：200ms 总超时应当把它掐掉
    let err = tool
        .execute(
            serde_json::json!({"service":"slow","method":"GET","path":"/slow"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, harness_kernel::ErrorCode::Unavailable, "{}", err.message);

    // 同一个响应、同样的耗时：走 save_to 必须下完
    tool.execute(
        serde_json::json!({"service":"slow","method":"GET","path":"/slow","save_to":"slow.bin"}),
        &ctx,
    )
    .await
    .unwrap();
    let got = std::fs::read(dir.path().join("slow.bin")).unwrap();
    assert_eq!(got.len(), 400, "下载被总超时截断了");
}

/// 分 4 段、段间停顿的慢服务：总耗时 ≈ 4×gap，用来区分"总超时"与"空闲超时"。
async fn slow_server(total: usize, gap: std::time::Duration) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // 两个请求（一次普通、一次 save_to），逐条受理
        for _ in 0..2 {
            let (mut sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n"
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let step = total / 4;
                let body = vec![b'x'; step];
                for _ in 0..4 {
                    let _ = sock.write_all(&body).await;
                    let _ = sock.flush().await;
                    tokio::time::sleep(gap).await;
                }
            });
        }
    });
    format!("http://{addr}")
}

/// `upload_from`：把 /ws 下的文件本体流式发出去，Content-Length 由宿主写死。
#[tokio::test]
async fn upload_streams_workspace_file_with_headers() {
    let dir = tempfile::tempdir().unwrap();
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.path().join("blob.bin"), &payload).unwrap();

    let (base, seen) = upload_server().await;
    let svc = Service { name: "alist".into(), base, ..Default::default() };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let out = tool
        .execute(
            serde_json::json!({
                "service":"alist","method":"PUT","path":"/api/fs/put",
                "upload_from":"blob.bin",
                "headers":{"File-Path":"/%E4%B8%AD/x.pdf","Content-Type":"application/pdf"}
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = match &out.content[0] {
        harness_kernel::Block::Paragraph(t) => t.clone(),
        other => panic!("{other:?}"),
    };
    assert!(text.contains("已发出 /ws/blob.bin"), "{text}");

    let cap = seen.lock().unwrap().clone().expect("没收到请求");
    let head = cap.head.to_ascii_lowercase();
    assert!(head.contains("file-path: /%e4%b8%ad/x.pdf"), "{}", cap.head);
    assert!(head.contains("content-type: application/pdf"), "{}", cap.head);
    assert!(
        head.contains(&format!("content-length: {}", payload.len())),
        "长度没写死：{}",
        cap.head
    );
    assert_eq!(cap.body, payload, "上传的字节与文件不一致");
}

/// 上传时不允许模型改这些头：鉴权与长度由宿主决定。
#[tokio::test]
async fn upload_rejects_credential_headers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.bin"), b"x").unwrap();
    let svc = Service { name: "alist".into(), base: "http://127.0.0.1:1".into(), ..Default::default() };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    for bad in ["Authorization", "Cookie", "Content-Length", "Host"] {
        let err = tool
            .execute(
                serde_json::json!({
                    "service":"alist","method":"PUT","path":"/api/fs/put",
                    "upload_from":"x.bin","headers":{ bad: "v" }
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("不允许"), "{bad}: {}", err.message);
    }
}

/// `upload_from` 与 `save_to` / body 互斥，且不能越出 /ws。
#[tokio::test]
async fn upload_rejects_conflicts_and_escapes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.bin"), b"x").unwrap();
    let svc = Service { name: "alist".into(), base: "http://127.0.0.1:1".into(), ..Default::default() };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    for (args, needle) in [
        (
            serde_json::json!({"service":"alist","method":"PUT","path":"/p","upload_from":"x.bin","save_to":"y.bin"}),
            "不能同时用",
        ),
        (
            serde_json::json!({"service":"alist","method":"PUT","path":"/p","upload_from":"x.bin","body":{"a":1}}),
            "不能同时用",
        ),
        (
            serde_json::json!({"service":"alist","method":"GET","path":"/p","upload_from":"x.bin"}),
            "只支持 PUT / POST",
        ),
        (
            serde_json::json!({"service":"alist","method":"PUT","path":"/p","upload_from":"../etc/passwd"}),
            "不能包含", // check_save_path 的文案："不能以 / 开头，不能含 .."
        ),
    ] {
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(
            err.message.contains(needle) || err.message.contains("相对路径"),
            "{}",
            err.message
        );
    }
}

/// 收一次请求：读出头部与按 Content-Length 指定的完整请求体。
#[derive(Clone)]
struct Captured {
    head: String,
    body: Vec<u8>,
}

async fn upload_server() -> (String, Arc<Mutex<Option<Captured>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(None));
    let seen2 = Arc::clone(&seen);
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 8192];
        let mut head_end = None;
        loop {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if head_end.is_none() {
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    head_end = Some(p + 4);
                }
            }
            if let Some(he) = head_end {
                let head = String::from_utf8_lossy(&buf[..he]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= he + len {
                    *seen2.lock().unwrap() = Some(Captured {
                        head: String::from_utf8_lossy(&buf[..he]).to_string(),
                        body: buf[he..he + len].to_vec(),
                    });
                    break;
                }
            }
        }
        let body = r#"{"code":200,"message":"success"}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    });
    (format!("http://{addr}"), seen)
}

/// 只应答一次、指定状态与响应体的服务。
async fn status_server(
    status: &'static str,
    ctype: &'static str,
    body: &'static str,
) -> (String, Arc<Mutex<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(String::new()));
    let seen2 = Arc::clone(&seen);
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        *seen2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
        let resp = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    });
    (format!("http://{addr}"), seen)
}

/// 只应答一次、返回固定字节的服务。
async fn binary_server(payload: &'static [u8]) -> (String, Arc<Mutex<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(String::new()));
    let seen2 = Arc::clone(&seen);
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        *seen2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/pdf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            payload.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.write_all(payload).await;
    });
    (format!("http://{addr}"), seen)
}

/// 只应答一次、回 302 的服务。
async fn redirect_server(location: &str) -> (String, Arc<Mutex<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(String::new()));
    let seen2 = Arc::clone(&seen);
    let location = location.to_owned();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        *seen2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
        let resp = format!(
            "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    });
    (format!("http://{addr}"), seen)
}

/// `save_to` 只配 GET：POST 加它会把接口调用变成写文件，难排查。
#[tokio::test]
async fn save_to_requires_get() {
    let dir = tempfile::tempdir().unwrap();
    let svc = Service { name: "demo".into(), base: "http://127.0.0.1:1".into(), ..Default::default() };
    let tool = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ctx_for(&ws, &env);

    let err = tool
        .execute(
            serde_json::json!({"service":"demo","method":"POST","path":"/x","save_to":"x.bin"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("只支持 GET"), "{}", err.message);
}
