//! L1 宿主 lane 的行为测试（需要 bwrap；没有就跳过）。
//!
//! 断言的是**边界**而不是命令输出：
//! - `/proc` 是宿主视图（能看到 1 号进程），说明没换 pid namespace；
//! - 未授权的路径根本不出现（不是"权限不足"，是"不存在"）；
//! - 授权表解锁的路径立刻可见；
//! - 未授权的命令给的是 `NotGranted`，而不是一个空白的 exit=127。

use std::sync::Arc;

use harness_capabilities::{GrantsCache, HostEnv};
use harness_kernel::{EnvError, ExecOpts, ExecutionEnv};
use tokio_util::sync::CancellationToken;

fn have_bwrap() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn opts() -> ExecOpts {
    ExecOpts { timeout_ms: 10_000, cwd: None, env: Vec::new(), stdin: None, nice: false }
}

fn env_with(dir: &std::path::Path, toml: &str) -> HostEnv {
    let cfg = dir.join("chuan.toml");
    std::fs::write(&cfg, toml).unwrap();
    let cache = Arc::new(GrantsCache::new(&cfg));
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    HostEnv::new(dir.join("ws"), cache)
}

#[tokio::test]
async fn sees_host_pid_view_and_not_its_own() {
    if !have_bwrap() {
        eprintln!("skip: 没有 bwrap");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = env_with(
        dir.path(),
        r#"
[host.path]
"/usr" = { mode = "ro" }
"/etc" = { mode = "ro" }
"#,
    );
    let out = env
        .exec(
            &["sh".into(), "-c".into(), "cat /proc/1/comm; echo ws=$(ls -d /ws); echo xdg=$XDG_RUNTIME_DIR".into()],
            opts(),
            CancellationToken::new(),
        )
        .await
        .expect("宿主 lane 应该能跑");
    assert_eq!(out.code, Some(0), "stderr={}", out.stderr);
    // 宿主 1 号进程能读到 → 没换 pid namespace
    assert!(!out.stdout.trim().is_empty());
    assert!(out.stdout.contains("/ws"));
    // `systemctl --user` 靠它找到 /run/user/<uid>；不设会报 "$XDG_RUNTIME_DIR not defined"
    assert!(out.stdout.contains("xdg=/run/user/"), "stdout={}", out.stdout);
}

#[tokio::test]
async fn ungranted_paths_are_absent() {
    if !have_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = env_with(
        dir.path(),
        r#"
[host.path]
"/usr" = { mode = "ro" }
"#,
    );
    // 只给了 /usr：/etc 与 /home 都不该出现
    let out = env
        .exec(
            &["sh".into(), "-c".into(), "ls /etc >/dev/null 2>&1; echo etc=$?; ls /home >/dev/null 2>&1; echo home=$?".into()],
            opts(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.stdout.contains("etc=2"), "stdout={}", out.stdout);
    assert!(out.stdout.contains("home=2"), "stdout={}", out.stdout);
}

#[tokio::test]
async fn executable_outside_grants_is_structured_error() {
    if !have_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = env_with(
        dir.path(),
        r#"
[host.path]
"/usr" = { mode = "ro" }
"#,
    );
    let err = env
        .exec(&["/opt/nope/bin".into()], opts(), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        EnvError::NotGranted { item, hint, .. } => {
            assert_eq!(item, "/opt/nope/bin");
            assert!(hint.contains("不在 [host.path] 里"), "{hint}");
        }
        other => panic!("应给 NotGranted，得到 {other:?}"),
    }
}
