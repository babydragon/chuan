//! `chuan hostd` 的端到端测试：真起一个 unix socket，走一遍授权/拒绝/审计。
//!
//! 不需要 root：`allow_uid` 设成当前 uid，socket 建在临时目录里。

use std::path::PathBuf;
use std::time::Duration;

use harness_capabilities::hostd::{self, HostdConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn cfg_at(dir: &std::path::Path) -> HostdConfig {
    let cfg = dir.join("chuan.toml");
    std::fs::write(
        &cfg,
        r#"
[[host_elevated.grant]]
argv = ["/bin/echo", "hi"]
approval = "once"

[[host_elevated.grant]]
argv = ["/usr/bin/true"]
"#,
    )
    .unwrap();
    HostdConfig {
        socket: dir.join("host.sock"),
        config: cfg,
        audit: dir.join("host-access.jsonl"),
        allow_uid: unsafe { libc::getuid() },
        allow_gid: unsafe { libc::getgid() },
        snapshot_dir: dir.join("snapshots"),
    }
}

async fn wait_for_socket(p: &std::path::Path) {
    for _ in 0..100 {
        if p.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("socket {} 没出现", p.display());
}

async fn call(sock: &std::path::Path, body: &str) -> serde_json::Value {
    let s = UnixStream::connect(sock).await.expect("应能连上 socket");
    let (r, mut w) = s.into_split();
    w.write_all(body.as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).expect("响应应是 JSON")
}

#[tokio::test]
async fn granted_command_runs_and_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_at(dir.path());
    let sock = cfg.socket.clone();
    let task = tokio::spawn(hostd::serve(cfg));
    wait_for_socket(&sock).await;

    let v = call(&sock, r#"{"argv":["/bin/echo","hi","extra"]}"#).await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["grant_index"], 1);
    assert!(v["stdout"].as_str().unwrap().contains("hi"), "{v}");

    // 审计：一行，含命中条目与退出码
    let audit = std::fs::read_to_string(dir.path().join("host-access.jsonl")).unwrap();
    assert_eq!(audit.lines().count(), 1, "{audit}");
    assert!(audit.contains("\"grant_index\":1"), "{audit}");

    task.abort();
}

#[tokio::test]
async fn ungranted_command_is_refused_and_not_audited() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_at(dir.path());
    let sock = cfg.socket.clone();
    let task = tokio::spawn(hostd::serve(cfg));
    wait_for_socket(&sock).await;

    // 前缀不匹配：`apt-get remove` 那种"借用 install 授权"的写法必须失败
    let v = call(&sock, r#"{"argv":["/bin/echo","bye"]}"#).await;
    assert_eq!(v["ok"], false, "{v}");
    assert!(v["error"].as_str().unwrap().contains("不在"), "{v}");
    // 拒绝不该写审计（只记 INFO 日志）——审计文件可以不存在或为空
    let p = dir.path().join("host-access.jsonl");
    assert!(!p.exists() || std::fs::read_to_string(&p).unwrap().trim().is_empty());

    task.abort();
}

#[tokio::test]
async fn broken_table_refuses_everything() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = cfg_at(dir.path());
    std::fs::write(&cfg.config, "[host_elevated.grant\n").unwrap();
    let sock = cfg.socket.clone();
    let task = tokio::spawn(hostd::serve(cfg));
    wait_for_socket(&sock).await;

    let v = call(&sock, r#"{"argv":["/bin/echo","hi"]}"#).await;
    assert_eq!(v["ok"], false, "{v}");
    assert!(v["error"].as_str().unwrap().contains("不可用"), "{v}");

    task.abort();
}

#[tokio::test]
async fn wrong_json_is_rejected_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_at(dir.path());
    let sock = cfg.socket.clone();
    let task = tokio::spawn(hostd::serve(cfg));
    wait_for_socket(&sock).await;

    let v = call(&sock, "not json").await;
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("JSON"), "{v}");

    task.abort();
}
