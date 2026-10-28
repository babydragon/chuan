//! bwrap 执行环境与工具的集成测试。没有 bwrap 的环境自动跳过。

use std::time::{Duration, Instant};

use harness_capabilities::{BwrapEnv, ReadTool, WriteTool};
use harness_kernel::{ExecOpts, ExecutionEnv, Tool, ToolCtx, Workspace};
use tokio_util::sync::CancellationToken;

/// 真的能起一个沙盒才算"有 bwrap"：CI 上 userns 可能被 AppArmor 禁掉，
/// 只看 `--version` 会让测试假失败。
fn has_bwrap() -> bool {
    std::process::Command::new("bwrap")
        .args(["--unshare-all", "--ro-bind", "/usr", "/usr", "--proc", "/proc",
               "--dev", "/dev", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn opts(ms: u64) -> ExecOpts {
    ExecOpts { timeout_ms: ms, cwd: None, env: vec![], stdin: None }
}

#[tokio::test]
async fn runs_command_in_sandbox() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = BwrapEnv::new(dir.path());
    let out = env
        .exec(
            &["sh".into(), "-lc".into(), "echo hi; pwd".into()],
            opts(5000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.code, Some(0));
    assert!(out.stdout.contains("hi"));
    assert!(out.stdout.contains("/ws"), "应在 workspace 内: {}", out.stdout);
}

#[tokio::test]
async fn hides_home_and_network() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = BwrapEnv::new(dir.path());
    let out = env
        .exec(
            &[
                "sh".into(),
                "-lc".into(),
                "test -d /home && echo HOME_VISIBLE || echo HOME_HIDDEN; \
                 (curl -s --max-time 2 https://example.com >/dev/null && echo NET) || echo NONET"
                    .into(),
            ],
            opts(8000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.stdout.contains("HOME_HIDDEN"), "{}", out.stdout);
    assert!(out.stdout.contains("NONET"), "{}", out.stdout);
}

#[tokio::test]
async fn timeout_kills_process_group() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = BwrapEnv::new(dir.path());
    let start = Instant::now();
    let err = env
        .exec(
            &["sh".into(), "-lc".into(), "sleep 10".into()],
            opts(300),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, harness_kernel::EnvError::Timeout(_)), "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(5), "超时后应快速返回");
}

#[tokio::test]
async fn truncates_huge_output() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = BwrapEnv::new(dir.path());
    let out = env
        .exec(
            &["sh".into(), "-lc".into(), "yes x | head -c 200000".into()],
            opts(8000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.truncated, "应标记截断");
    assert!(out.stdout.len() <= 64 * 1024 + 16, "len={}", out.stdout.len());
}

/// 联网 lane 要能解析 `.local`：需要宿主 avahi 的 socket（nss-mdns）。
/// 家里大量设备用 mDNS 发布（打印机 `dnssd://`、Home Assistant、智能插座）。
#[tokio::test]
async fn network_lane_sees_avahi_socket() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    if !std::path::Path::new("/run/avahi-daemon/socket").exists() {
        eprintln!("skip: 宿主没跑 avahi-daemon");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    let lan = BwrapEnv::new(ws.path()).with_network(true);
    let out = lan
        .exec(
            &["sh".into(), "-lc".into(),
              "test -S /run/avahi-daemon/socket && echo HAVE-SOCKET || echo NO-SOCKET".into()],
            opts(8000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.stdout.contains("HAVE-SOCKET"), "联网 lane 应能看到 avahi socket: {}", out.stdout);

    // 离线 lane 不该有：那边没有网络，解析 .local 也没意义
    let offline = BwrapEnv::new(ws.path());
    let out = offline
        .exec(
            &["sh".into(), "-lc".into(),
              "test -S /run/avahi-daemon/socket && echo HAVE-SOCKET || echo NO-SOCKET".into()],
            opts(8000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.stdout.contains("NO-SOCKET"), "离线 lane 不该看到 avahi: {}", out.stdout);
}

/// 凭据目录必须被遮掉：/etc 是整块挂进来的，daemon 与沙盒同 uid，
/// 权限位挡不住，只能挂空 tmpfs 盖住。
#[tokio::test]
async fn masked_dir_is_empty_in_sandbox() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    // 造一个"凭据目录"挂进沙盒，再遮掉它
    let secrets = tempfile::tempdir().unwrap();
    std::fs::write(secrets.path().join("token"), b"super-secret").unwrap();
    let env = BwrapEnv::new(ws.path())
        .with_mounts(vec![harness_capabilities::Mount {
            src: secrets.path().to_path_buf(),
            dst: "/opt/secretbox".into(),
            ro: true,
        }])
        .with_masked_dir("/opt/secretbox");

    let out = env
        .exec(
            &["sh".into(), "-lc".into(),
              "ls -A /opt/secretbox | wc -l; cat /opt/secretbox/token 2>/dev/null || echo READ-FAILED".into()],
            opts(8000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.stdout.contains("0"), "遮掉后应为空目录: {}", out.stdout);
    assert!(out.stdout.contains("READ-FAILED"), "不该读到内容: {}", out.stdout);

    // 也不能往里写：否则看起来像个"可写的空目录"，会误导模型
    let out = env
        .exec(
            &["sh".into(), "-lc".into(),
              "touch /opt/secretbox/x 2>/dev/null && echo WRITABLE || echo READONLY".into()],
            opts(8000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.stdout.contains("READONLY"), "遮蔽目录应只读: {}", out.stdout);
}

/// `/tmp` 是 tmpfs，必须限大小：不限制时它会长到内存的一半，脚本写满会把
/// 整个服务（MemoryMax=1G）拖进 OOM。
#[tokio::test]
async fn tmp_has_size_cap() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let env = BwrapEnv::new(ws.path());
    let out = env
        .exec(
            &[
                "sh".into(),
                "-lc".into(),
                "df -k /tmp | awk 'NR==2{print $2}'".into(),
            ],
            opts(8000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let kb: u64 = out.stdout.trim().parse().unwrap_or(u64::MAX);
    assert!(kb <= 512 * 1024, "/tmp 上限过大（{kb}KB），应当有 --size 限制");
}

async fn inbox_is_readonly_and_tmp_is_isolated() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let inbox = tempfile::tempdir().unwrap();
    std::fs::write(inbox.path().join("a.pdf"), b"x").unwrap();

    let env = BwrapEnv::new(ws.path()).with_inbox(inbox.path());
    let out = env
        .exec(
            &[
                "sh".into(),
                "-lc".into(),
                "ls /inbox; (touch /inbox/x 2>/dev/null && echo WRITABLE) || echo READONLY; \
                 echo tmp_count=$(ls -A /tmp | wc -l)"
                    .into(),
            ],
            opts(8000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.stdout.contains("a.pdf"), "{}", out.stdout);
    assert!(out.stdout.contains("READONLY"), "{}", out.stdout);
    assert!(out.stdout.contains("tmp_count=0"), "宿主 /tmp 不应可见: {}", out.stdout);
}

#[tokio::test]
async fn write_tool_jails_paths() {
    let dir = tempfile::tempdir().unwrap();
    let env = harness_capabilities::NullEnv;
    let ws = Workspace { run: harness_kernel::RunId("r".into()), root: dir.path().to_string_lossy().into() };
    let ctx = ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("call".into()),
        workspace: &ws,
        env: &env,
        cancel: CancellationToken::new(),
    };

    WriteTool
        .execute(serde_json::json!({"path":"a/b.txt","content":"hello"}), &ctx)
        .await
        .unwrap();
    let out = ReadTool
        .execute(serde_json::json!({"path":"a/b.txt"}), &ctx)
        .await
        .unwrap();
    assert!(format!("{out:?}").contains("hello"));

    let err = WriteTool
        .execute(serde_json::json!({"path":"../escape.txt","content":"x"}), &ctx)
        .await
        .unwrap_err();
    assert_eq!(err.code, harness_kernel::ErrorCode::NotPermitted);
}

/// 联网 lane：不带 `--unshare-all` 时仍要能正常跑命令（参数组合别写错）。
#[tokio::test]
async fn network_lane_runs_and_sees_a_nic() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = BwrapEnv::new(dir.path()).with_network(true);
    let out = env
        .exec(
            &["sh".into(), "-lc".into(), "echo net-ok; cat /proc/net/dev | tail -n +3 | wc -l".into()],
            opts(5000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.code, Some(0), "{:?}", out);
    assert!(out.stdout.contains("net-ok"), "{}", out.stdout);
    let ifaces: usize = out
        .stdout
        .trim()
        .lines()
        .last()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    assert!(ifaces > 1, "联网 lane 应能看到 lo 以外的接口: {}", out.stdout);
}

/// 默认 lane 必须仍然没有网络：/proc/net/dev 只有 lo。
#[tokio::test]
async fn offline_lane_has_no_external_nic() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = BwrapEnv::new(dir.path());
    let out = env
        .exec(
            &["sh".into(), "-lc".into(), "cat /proc/net/dev | tail -n +3 | wc -l".into()],
            opts(5000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // unshare-all 的 netns 里只有 lo
    assert_eq!(out.stdout.trim(), "1", "离线 lane 不应有外网接口: {}", out.stdout);
}

/// 运行时目录只读挂进沙盒后，技能能直接用 venv 的 python。
#[tokio::test]
async fn runtime_venv_is_visible_in_sandbox() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let rt = tempfile::tempdir().unwrap();
    // 用一个最小 venv；没有 python3-venv 就跳过
    let ok = std::process::Command::new("python3")
        .args(["-m", "venv", "--system-site-packages"])
        .arg(rt.path().join("py"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skip: no python3-venv");
        return;
    }
    let env = BwrapEnv::new(ws.path()).with_runtime(rt.path(), false);
    let out = env
        .exec(
            &["/py/bin/python3".into(), "-c".into(), "import sys;print(sys.prefix)".into()],
            opts(5000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.code, Some(0), "{:?} {}", out.code, out.stderr);
    assert!(out.stdout.contains("/py"), "{}", out.stdout);
    // 系统包可见：apt 装的 site-packages 在 /usr 下，仍应出现在 sys.path
    let out = env
        .exec(
            &["/py/bin/python3".into(), "-c".into(), "import sys;print(len(sys.path))".into()],
            opts(5000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.stdout.trim().parse::<usize>().unwrap_or(0) >= 4, "{}", out.stdout);
}

/// 联网 lane 的 DNS 必须能用：/etc/resolv.conf 常指向 /run，默认不可见。
#[tokio::test]
async fn network_lane_resolves_dns() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    // 宿主自己都解析不了就跳过（离线环境）
    let host_ok = std::process::Command::new("python3")
        .args(["-c", "import socket;socket.gethostbyname('example.com')"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !host_ok {
        eprintln!("skip: 宿主无 DNS");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let env = BwrapEnv::new(dir.path()).with_network(true);
    let out = env
        .exec(
            &[
                "python3".into(),
                "-c".into(),
                "import socket;print(socket.gethostbyname('example.com'))".into(),
            ],
            opts(15000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.trim().split('.').count() == 4,
        "应解析出 IPv4: {}",
        out.stdout
    );
}

/// 已经能 import 的包不该再联网安装（apt 装的、stdlib 都算）。
#[tokio::test]
async fn install_skips_when_already_importable() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    use harness_capabilities::InstallTool;
    let ws = tempfile::tempdir().unwrap();
    let rt = tempfile::tempdir().unwrap();
    let ok = std::process::Command::new("python3")
        .args(["-m", "venv", "--system-site-packages"])
        .arg(rt.path().join("py"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skip: no python3-venv");
        return;
    }
    let env = std::sync::Arc::new(
        BwrapEnv::new(ws.path()).with_network(true).with_runtime(rt.path(), true),
    );
    let tool = InstallTool::new(std::sync::Arc::clone(&env), None, None);
    let wsx = Workspace { run: harness_kernel::RunId("r".into()), root: ws.path().to_string_lossy().into() };
    let ctx = ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("call".into()),
        workspace: &wsx,
        env: &*env,
        cancel: CancellationToken::new(),
    };
    // json 是标准库：探测应命中，直接返回，不联网
    let out = tool
        .execute(serde_json::json!({"ecosystem":"pip","packages":["json"]}), &ctx)
        .await
        .unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("已经可用"), "{text}");
}

/// 声明 network=true 的技能真的能出网；同一份技能在离线 lane 里必须失败。
#[tokio::test]
async fn network_skill_gets_network_and_offline_skill_does_not() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let dns = std::process::Command::new("python3")
        .args(["-c", "import socket;socket.gethostbyname('example.com')"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !dns {
        eprintln!("skip: no dns");
        return;
    }

    let skills = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    for (name, net) in [("netprobe", true), ("offprobe", false)] {
        let d = skills.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("skill.json"),
            serde_json::json!({
                "name": name,
                "description": "探测网络",
                "params": {"type":"object","properties":{},"required":[]},
                "network": net
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            d.join("run.py"),
            "import urllib.request, sys\ntry:\n    print('NET:%d' % urllib.request.urlopen('https://example.com', timeout=10).status)\nexcept Exception as e:\n    print('NONET:%s' % type(e).__name__)\n",
        )
        .unwrap();
    }

    let cat = harness_capabilities::SkillCatalog::new(skills.path());
    let net_env = crate::BwrapEnv::new(ws.path()).with_network(true).with_skills(skills.path());
    let offline = crate::BwrapEnv::new(ws.path()).with_skills(skills.path());
    let tool = harness_capabilities::RunSkillTool::new(std::sync::Arc::clone(&cat))
        .with_network(std::sync::Arc::new(net_env));
    let wsx = Workspace { run: harness_kernel::RunId("r".into()), root: ws.path().to_string_lossy().into() };
    let ctx = ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("c".into()),
        workspace: &wsx,
        env: &offline,
        cancel: CancellationToken::new(),
    };

    let out = tool
        .execute(serde_json::json!({"name":"netprobe","args":{}}), &ctx)
        .await
        .unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("NET:200"), "联网技能应能取到 200: {text}");

    let out = tool
        .execute(serde_json::json!({"name":"offprobe","args":{}}), &ctx)
        .await
        .unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("NONET"), "离线技能不该有网: {text}");
}

/// 附件目录只读挂到 /media：模型能读，不能改。
#[tokio::test]
async fn media_is_mounted_readonly() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    std::fs::write(media.path().join("a.txt"), "hello").unwrap();

    let env = BwrapEnv::new(ws.path()).with_media(media.path());
    let out = env
        .exec(&["cat".into(), "/media/a.txt".into()], opts(5000), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout.trim(), "hello");

    let out = env
        .exec(
            &["sh".into(), "-c".into(), "echo x > /media/a.txt".into()],
            opts(5000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_ne!(out.code, Some(0), "不应可写: {:?}", out);
}

/// 主人授权的额外挂载：只读可见，不能写；保留挂载点不允许被覆盖。
#[tokio::test]
async fn extra_mounts_are_visible_and_readonly() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    use harness_capabilities::Mount;
    let ws = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    std::fs::write(host.path().join("state.txt"), "printer: idle").unwrap();

    let env = BwrapEnv::new(ws.path()).with_mounts(vec![Mount {
        src: host.path().to_path_buf(),
        dst: "/run/extra".into(),
        ro: true,
    }]);
    let out = env
        .exec(
            &["cat".into(), "/run/extra/state.txt".into()],
            opts(5000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout.trim(), "printer: idle");

    let out = env
        .exec(
            &["sh".into(), "-c".into(), "echo x > /run/extra/state.txt".into()],
            opts(5000),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_ne!(out.code, Some(0), "只读挂载不该可写");
}

#[test]
fn reserved_mount_points_are_rejected() {
    use harness_capabilities::{validate_mount, Mount};
    let src = std::env::temp_dir();
    for dst in ["/", "/ws", "/skills", "/etc", "/proc", "/usr"] {
        let m = Mount { src: src.clone(), dst: dst.into(), ro: true };
        assert!(validate_mount(&m).is_err(), "{dst} 应被拒绝");
    }
    let ok = Mount { src, dst: "/run/cups".into(), ro: true };
    assert!(validate_mount(&ok).is_ok());
}

/// send_file：只发工作区内的文件，按类型交给 outbox。
#[tokio::test]
async fn send_file_respects_workspace_and_mime() {
    use harness_capabilities::SendFileTool;
    use harness_kernel::{Outbox, Tool};
    use std::sync::Mutex;

    struct Rec(Mutex<Vec<(String, String, String)>>);
    #[async_trait::async_trait]
    impl Outbox for Rec {
        async fn send_file(
            &self,
            path: &str,
            name: &str,
            mime: &str,
            _caption: &str,
        ) -> Result<(), String> {
            self.0.lock().unwrap().push((path.to_owned(), name.to_owned(), mime.to_owned()));
            Ok(())
        }
    }

    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join("out")).unwrap();
    std::fs::write(ws.path().join("out/report.pdf"), b"%PDF-1.4 test").unwrap();
    let rec = Rec(Mutex::new(Vec::new()));
    let wsx = Workspace { run: harness_kernel::RunId("r".into()), root: ws.path().to_string_lossy().into() };
    let env = harness_capabilities::NullEnv;
    let ctx = ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
        outbox: &rec,
            docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("c".into()),
        workspace: &wsx,
        env: &env,
        cancel: CancellationToken::new(),
    };
    let tool = SendFileTool;

    tool.execute(serde_json::json!({"path":"/ws/out/report.pdf","caption":"预览"}), &ctx)
        .await
        .unwrap();
    let got = rec.0.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, "report.pdf");
    assert_eq!(got[0].2, "application/pdf");

    // 越界路径必须拒绝
    let err = tool
        .execute(serde_json::json!({"path":"../../etc/passwd"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.message.contains("越界"), "{}", err.message);
    assert_eq!(rec.0.lock().unwrap().len(), 1, "拒绝时不应调用 outbox");
}

/// 代理只在 `proxy=true`（或 always）时注入沙盒环境，默认不带。
#[tokio::test]
async fn proxy_env_only_exists_in_network_lane() {
    if !has_bwrap() {
        eprintln!("skip: no bwrap");
        return;
    }
    use harness_capabilities::{ExecTool, ProxyConfig, ProxyMode};

    let ws = tempfile::tempdir().unwrap();
    let proxy = ProxyConfig {
        url: Some("http://127.0.0.1:7890".into()),
        sandbox: ProxyMode::Auto,
        ..Default::default()
    };
    let wsx = Workspace { run: harness_kernel::RunId("r".into()), root: ws.path().to_string_lossy().into() };

    // 离线 lane：没有网卡，注入了也连不上，所以**永远不注入**（exec 也不提供 proxy 参数）
    let offline_env = BwrapEnv::new(ws.path());
    let offline = ExecTool::sandboxed().with_proxy(proxy.clone());
    let ctx = ToolCtx {
        conv: harness_kernel::ConversationId("c".into()),
        actor: "42",
        outbox: &harness_kernel::NullOutbox,
        docs: &harness_kernel::NullDocReader,
        run: harness_kernel::RunId("r".into()),
        call: harness_kernel::CallId("c".into()),
        workspace: &wsx,
        env: &offline_env,
        cancel: CancellationToken::new(),
    };

    // 模型就算照旧习惯传了 proxy=true，也必须一个变量都不注入
    let out = offline
        .execute(
            serde_json::json!({"script":"echo \"[${https_proxy:-none}]\"","timeout_ms":5000,"proxy":true}),
            &ctx,
        )
        .await
        .unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("[none]"), "离线 lane 不该带代理: {text}");

    // 离线 lane 的 spec 里不该出现 proxy 参数（不给模型这个选项）
    assert!(
        offline.spec().parameters["properties"].get("proxy").is_none(),
        "离线 exec 不应暴露 proxy 参数: {}",
        offline.spec().parameters
    );

    // 联网 lane：127.0.0.1 即宿主，proxy=true 时注入
    let lan_env = std::sync::Arc::new(BwrapEnv::new(ws.path()).with_network(true));
    let lan = ExecTool::lan(lan_env.clone()).with_proxy(proxy);
    assert!(
        lan.spec().parameters["properties"].get("proxy").is_some(),
        "lan_exec 应提供 proxy 参数"
    );
    let ctx_lan = ToolCtx { env: &*lan_env, ..ctx };
    let out = lan
        .execute(
            serde_json::json!({"script":"echo \"[$https_proxy]\"","timeout_ms":5000,"proxy":true}),
            &ctx_lan,
        )
        .await
        .unwrap();
    let text = format!("{:?}", out.content);
    assert!(text.contains("[http://127.0.0.1:7890]"), "{text}");
}
