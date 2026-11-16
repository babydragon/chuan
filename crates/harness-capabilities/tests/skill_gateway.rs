//! 技能服务网关（D41）的端到端验证：
//! 脚本 → unix socket → 宿主注入凭据 → 上游 HTTP → 原路返回。
//!
//! 关键断言（任何一条挂了都说明边界破了）：
//! 1. 上游收到的鉴权头来自配置，脚本没有、也设不了；
//! 2. 脚本只能设白名单里的头，值也必须命中清单；
//! 3. method / path 越界直接拒绝，不会打到上游；
//! 4. 白名单外的服务名在启动时就失败（fail closed）。

use std::collections::BTreeMap;
use std::path::Path;

use harness_capabilities::services::Service;
use harness_capabilities::{GatewaySession, HttpServiceTool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};

/// 一个只认 `POST /JNAP/` 的假路由器：回显收到的 action 与鉴权头。
async fn mock_router() -> (
    String,
    std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let seen_task = std::sync::Arc::clone(&seen);
    let h = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let seen = std::sync::Arc::clone(&seen_task);
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buf = [0u8; 2048];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw);
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let need = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if body.len() >= need {
                            let action = head
                                .lines()
                                .find(|l| l.to_ascii_lowercase().starts_with("x-jnap-action:"))
                                .map(str::to_owned)
                                .unwrap_or_default();
                            let auth = head
                                .lines()
                                .find(|l| {
                                    l.to_ascii_lowercase()
                                        .starts_with("x-jnap-authorization:")
                                })
                                .map(str::to_owned)
                                .unwrap_or_default();
                            seen.lock().await.push(format!("{action}|{auth}"));
                            break;
                        }
                    }
                }
                let body = r#"{"result":"OK","output":{"ok":true}}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), seen, h)
}

fn velop(base: &str, token: &Path) -> Service {
    Service {
        name: "velop".into(),
        base: base.into(),
        token_file: Some(token.to_path_buf()),
        header: "X-JNAP-Authorization: Basic {token}".into(),
        extra_headers: vec![("Content-Type".into(), "text/plain; charset=UTF-8".into())],
        expose_to_skills: true,
        allow_methods: vec!["POST".into()],
        allow_paths: vec!["/JNAP/".into()],
        header_allow: BTreeMap::from([(
            "X-JNAP-Action".to_owned(),
            vec!["GetDeviceInfo".to_owned(), "GetNodesInfo".to_owned()],
        )]),
        ..Default::default()
    }
}

async fn call(sock: &Path, req: &serde_json::Value) -> serde_json::Value {
    let mut s = UnixStream::connect(sock).await.unwrap();
    let mut line = serde_json::to_vec(req).unwrap();
    line.push(b'\n');
    s.write_all(&line).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    serde_json::from_str(out.trim()).unwrap()
}

#[tokio::test]
async fn injects_auth_and_enforces_allowlists() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("velop_auth");
    std::fs::write(&secret, "YWRtaW46c2VjcmV0\n").unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let (base, seen_log, server) = mock_router().await;
    let http = HttpServiceTool::new(vec![velop(&base, &secret)], dir.path().join("net.jsonl"))
        .unwrap();
    let gw = GatewaySession::start(http, &["velop".into()], ws.to_str().unwrap())
        .await
        .unwrap();
    let sandbox_path = gw.endpoints()["velop"].clone();
    let sock = ws.join(sandbox_path.trim_start_matches("/ws/"));

    // 1) 正常请求：宿主注入 Basic 头 + 脚本给的动作头
    let ok = call(
        &sock,
        &serde_json::json!({
            "method": "POST",
            "path": "/JNAP/",
            "headers": { "X-JNAP-Action": "GetDeviceInfo" },
            "body": {}
        }),
    )
    .await;
    assert_eq!(ok["status"], 200, "{ok}");
    assert!(ok["body"].as_str().unwrap().contains("\"ok\":true"));

    // 2) 动作不在白名单：拒绝，且不会打到上游
    let bad_action = call(
        &sock,
        &serde_json::json!({
            "method": "POST", "path": "/JNAP/",
            "headers": { "X-JNAP-Action": "SetWiFiSettings" }, "body": {}
        }),
    )
    .await;
    assert!(bad_action["error"].as_str().unwrap().contains("取值不在允许清单"));

    // 3) 路径越界
    let bad_path = call(
        &sock,
        &serde_json::json!({ "method": "POST", "path": "/other", "body": {} }),
    )
    .await;
    assert!(bad_path["error"].as_str().unwrap().contains("没有对技能开放路径"));

    // 4) 脚本想自己带鉴权头：拒绝
    let bad_auth = call(
        &sock,
        &serde_json::json!({
            "method": "POST", "path": "/JNAP/",
            "headers": { "Authorization": "Basic eA==" }, "body": {}
        }),
    )
    .await;
    assert!(bad_auth["error"].as_str().unwrap().contains("不能由技能设置"));

    // 5) 方法越界
    let bad_method = call(
        &sock,
        &serde_json::json!({ "method": "DELETE", "path": "/JNAP/", "body": {} }),
    )
    .await;
    assert!(bad_method["error"].as_str().unwrap().contains("没有对技能开放方法"));

    drop(gw);
    server.abort();
    let seen = seen_log.lock().await.clone();
    assert_eq!(seen.len(), 1, "只有第 1 条应当到达上游：{seen:?}");
    assert!(seen[0].contains("GetDeviceInfo"), "{}", seen[0]);
    assert!(
        seen[0].contains("Basic YWRtaW46c2VjcmV0"),
        "宿主必须注入 Basic 头：{}",
        seen[0]
    );
}

#[tokio::test]
async fn refuses_service_not_exposed() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("s");
    std::fs::write(&secret, "x\n").unwrap();
    let mut svc = velop("http://127.0.0.1:1", &secret);
    svc.expose_to_skills = false;
    let http = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let err = GatewaySession::start(http, &["velop".into()], dir.path().to_str().unwrap())
        .await
        .err()
        .expect("必须拒绝未开放的服务");
    assert!(err.contains("expose_to_skills"), "{err}");
}

#[tokio::test]
async fn refuses_empty_allowlists() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("s");
    std::fs::write(&secret, "x\n").unwrap();
    let mut svc = velop("http://127.0.0.1:1", &secret);
    svc.allow_paths.clear();
    let http = HttpServiceTool::new(vec![svc], dir.path().join("net.jsonl")).unwrap();
    let err = GatewaySession::start(http, &["velop".into()], dir.path().to_str().unwrap())
        .await
        .err()
        .expect("空白名单不能当成全放开");
    assert!(err.contains("allow_methods / allow_paths"), "{err}");
}
