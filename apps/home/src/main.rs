//! chuan 入口。配置优先级：CLI > 环境变量 > TOML > 内置默认。
//!
//! 用法：
//!   chuan [--config <toml>] [--provider <p>] [--model <m>] [--db <path>]
//!   chuan backup [...]

mod any_driver;
mod backup;
mod config;
mod offsets;
mod system_prompt;
mod workspace_gc;

use std::sync::Arc;

use harness_capabilities::{
    BwrapEnv, ExecTool, FilesTool, HttpServiceTool, InstallTool, KeepTool, McpCallTool, McpHub,
    NetLogTool, PlaybookTool, PublishSkillTool, ReadTool, RememberTool, RemindTool,
    RollbackSkillTool, RunSkillTool, SendFileTool, ServiceTool, SkillCatalog, WebFetchTool,
    WriteTool,
};
use harness_channels::telegram::http::HttpApi;
use harness_channels::telegram::{TelegramChannel, TelegramConfig};
use harness_kernel::{ActorConfig, Channel, ConversationActor, Store, ToolRegistry};
use harness_store::SqliteStore;
use offsets::StoreOffsets;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 之前没装 subscriber：所有 tracing::* 都被丢掉了，卡住时无迹可查。
    // 默认 info，可用 RUST_LOG 覆盖；stderr 会被 systemd 收进 journald。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let cli = match config::Cli::parse(std::env::args()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let cfg = match config::load(&cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置错误: {e}");
            std::process::exit(2);
        }
    };

    if let Some(parent) = std::path::Path::new(&cfg.db).parent() {
        std::fs::create_dir_all(parent)?;
    }

    if cli.backup {
        return cmd_backup(&cfg).await;
    }
    daemon(&cfg).await
}

async fn cmd_backup(cfg: &config::Config) -> Result<(), Box<dyn std::error::Error>> {
    let store = SqliteStore::open(&cfg.db).await?;
    let policy = backup::BackupPolicy {
        dir: cfg.backup_dir.clone().into(),
        keep_days: cfg.backup_keep_days,
        keep_min: cfg.backup_keep_min,
        max_total_mb: cfg.backup_max_mb,
    };
    std::fs::create_dir_all(&policy.dir)?;
    let target = backup::next_path(&policy.dir);
    store.backup_to(target.clone()).await?;
    let removed = backup::prune(&policy)?;
    println!(
        "backup: {} · pruned {removed} · keep_days={} keep_min={} max={}MB",
        target.display(),
        policy.keep_days,
        policy.keep_min,
        policy.max_total_mb
    );
    Ok(())
}

async fn daemon(cfg: &config::Config) -> Result<(), Box<dyn std::error::Error>> {
    // 单实例
    if let Some(parent) = std::path::Path::new(&cfg.lock).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_file = std::fs::OpenOptions::new().create(true).write(true).open(&cfg.lock)?;
    {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            eprintln!("another instance is running (lock {})", cfg.lock);
            std::process::exit(1);
        }
    }

    let store = Arc::new(SqliteStore::open(&cfg.db).await?);
    let recovered = store.recover().await?;
    if recovered > 0 {
        println!("recovered {recovered} unfinished tool call(s)");
    }
    // 被杀掉的 run 补一条 interrupted，账本里不留"永远不结束"的 run
    if let Ok(open) = store.open_runs().await {
        for (conv, run) in open {
            if let Ok(mut tx) = store.begin_write().await {
                let _ = tx
                    .append(harness_kernel::NewEvent {
                        conv,
                        run: run.clone(),
                        turn: None,
                        kind: harness_kernel::EventKind::RunFinished,
                        payload: serde_json::json!({ "status": "interrupted" }),
                        dedupe_key: Some(format!("interrupted-{}", run.0)),
                    })
                    .await;
                let _ = tx.commit().await;
            }
        }
    }

    // Telegram：CHUAN_PROXY 优先（兼容旧用法），否则用 [proxy] url
    let tg_proxy = std::env::var("CHUAN_PROXY")
        .ok()
        .or_else(|| cfg.proxy.url.clone());
    let api = Arc::new(HttpApi::with_proxy(cfg.token.clone(), tg_proxy.as_deref())?);
    let offsets = Arc::new(StoreOffsets::new(Arc::clone(&store), "tg.offset"));
    let channel: Arc<dyn Channel> = Arc::new(TelegramChannel::new(
        api,
        offsets as Arc<dyn harness_channels::telegram::OffsetStore>,
        TelegramConfig {
            allowed_users: cfg.allowed_users.clone(),
            media_dir: std::path::PathBuf::from(&cfg.media_dir),
            stale_after_s: cfg.stale_after_s,
        },
    ));

    std::fs::create_dir_all(&cfg.workspace)?;
    std::fs::create_dir_all(&cfg.media_dir)?;
    std::fs::create_dir_all(&cfg.library_dir)?;
    let gc = workspace_gc::sweep_now(
        std::path::Path::new(&cfg.workspace),
        cfg.workspace_keep_days,
        cfg.workspace_max_mb,
    );
    let mgc = workspace_gc::sweep_now(
        std::path::Path::new(&cfg.media_dir),
        cfg.media_keep_days,
        0,
    );
    if mgc.removed_files > 0 {
        println!("media gc: 删除 {} 个附件", mgc.removed_files);
    }
    if gc.removed_files > 0 {
        println!(
            "workspace gc: 删除 {} 个文件 / {} KB，保留 {} 个 / {} KB",
            gc.removed_files,
            gc.removed_bytes / 1024,
            gc.kept_files,
            gc.kept_bytes / 1024
        );
    }
    std::fs::create_dir_all(&cfg.inbox)?;
    std::fs::create_dir_all(&cfg.skills_dir)?;
    std::fs::create_dir_all(&cfg.playbooks_dir)?;

    // 运行时目录：venv 一次建好，之后只读挂进沙盒
    let rt = std::path::PathBuf::from(&cfg.runtime_dir);
    std::fs::create_dir_all(rt.join("py"))?;
    std::fs::create_dir_all(rt.join("node"))?;

    // 只判断 `bin/python3` 在不在会漏掉半成品：`ensurepip` 缺失时 venv 会创建出
    // 一个能跑但没有 pip 的目录，之后永远走"已存在"分支，install_deps 永久不可用。
    // 所以探针换成真的跑一次 `python3 -m pip --version`。
    let py = rt.join("py");
    match venv_probe(&py) {
        Ok(()) => allow_system_site_packages(&py),
        Err(reason) => {
            if py.join("bin/python3").exists() {
                println!("runtime: venv 不可用（{reason}），重建 {}", py.display());
            } else {
                println!("runtime: 创建 venv {}", py.display());
            }
            // --system-site-packages：apt 装在系统里的包（numpy/Pillow 这类大件）也能直接用，
            // 不必为它们在 riscv64 上现场编译。pip 装的包仍在 /py 里，优先命中。
            // 对已存在的目录重跑是安全的：会重写 bin/ 并重新执行 ensurepip，pip 包保留。
            let ok = std::process::Command::new("python3")
                .arg("-m")
                .arg("venv")
                .arg("--system-site-packages")
                .arg(&py)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                eprintln!("runtime: venv 创建失败，install_deps(pip) 将不可用");
                eprintln!(
                    "  → 多半缺 ensurepip：apt install python3-venv（Debian 系也可能是 python3.13-venv）"
                );
            } else if let Err(e) = venv_probe(&py) {
                eprintln!("runtime: venv 建好了但仍不可用（{e}）；install_deps(pip) 将不可用");
            } else {
                allow_system_site_packages(&py);
            }
        }
    }
    let skills = SkillCatalog::new(&cfg.skills_dir);
    let env = BwrapEnv::new(&cfg.workspace)
        .with_inbox(&cfg.inbox)
        .with_skills(&cfg.skills_dir)
        .with_media(&cfg.media_dir)
        .with_mounts(cfg.sandbox_mounts.clone())
        .with_runtime(&cfg.runtime_dir, false);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ReadTool));
    tools.register(Arc::new(WriteTool));
    tools.register(Arc::new(ExecTool::sandboxed().with_proxy(cfg.proxy.clone())));
    // 内网调试 lane：有网卡，逐条审批；看不到宿主文件
    tools.register(Arc::new(ExecTool::lan(Arc::new(
        BwrapEnv::new(&cfg.workspace)
            .with_network(true)
            .with_mounts(cfg.sandbox_mounts.clone()),
    ))
    .with_proxy(cfg.proxy.clone())));
    // 装包 lane：有网络，只把运行时目录挂成可写（/rt）
    tools.register(Arc::new(InstallTool::new(
        Arc::new(
            BwrapEnv::new(&cfg.workspace)
                .with_network(true)
                .with_runtime(&cfg.runtime_dir, true),
        ),
        cfg.pip_index.clone(),
        cfg.npm_registry.clone(),
    )
    .with_proxy(cfg.proxy.clone())));
    tools.register(Arc::new(RememberTool));
    tools.register(Arc::new(SendFileTool));
    tools.register(KeepTool::new(&cfg.library_dir));
    tools.register(FilesTool::new(&cfg.library_dir));
    tools.register(Arc::new(
        RemindTool::new(cfg.tz_offset_min)
            .with_skills(Arc::clone(&skills))
            .with_identity(Arc::clone(&cfg.identity)),
    ));
    tools.register(WebFetchTool::with_proxy(&cfg.net_log, &cfg.proxy)?);
    // http / mcp / service 三个工具常驻：agent 才能在运行时自己接新服务
    let mcp = McpHub::new(cfg.mcp_servers.clone());
    if !mcp.is_empty() {
        let names: Vec<&str> = cfg.mcp_servers.iter().map(|s| s.name.as_str()).collect();
        println!("mcp servers: {}", names.join(", "));
        mcp.connect_all().await;
    }
    tools.register(McpCallTool::new(Arc::clone(&mcp), &cfg.net_log));
    if !cfg.services.is_empty() {
        let names: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
        println!("http services: {}", names.join(", "));
    }
    let http_svc = HttpServiceTool::with_proxy(cfg.services.clone(), &cfg.net_log, &cfg.proxy)?;
    tools.register(Arc::clone(&http_svc) as Arc<dyn harness_kernel::Tool>);
    tools.register(
        ServiceTool::new(
            cfg.config_dir.join("services.d"),
            cfg.config_dir.join("secrets"),
            Arc::clone(&mcp),
            http_svc,
        )
        .with_proxy(cfg.proxy.clone()),
    );
    tools.register(NetLogTool::new(&cfg.net_log));
    tools.register(PlaybookTool::new(
        &cfg.playbooks_dir,
        vec![
            ("import-skill", crate::system_prompt::IMPORT),
            ("distill", crate::system_prompt::DISTILL),
            ("schedule-job", crate::system_prompt::SCHEDULE_JOB),
        ],
    ));
    tools.register(Arc::new(
        RunSkillTool::new(Arc::clone(&skills)).with_network(Arc::new(
            BwrapEnv::new(&cfg.workspace)
                .with_inbox(&cfg.inbox)
                .with_skills(&cfg.skills_dir)
                .with_runtime(&cfg.runtime_dir, false)
                .with_mounts(cfg.sandbox_mounts.clone())
                .with_network(true),
        )),
    ));
    tools.register(Arc::new(
        PublishSkillTool::new(Arc::clone(&skills)).with_network(Arc::new(
            BwrapEnv::new(&cfg.workspace)
                .with_inbox(&cfg.inbox)
                .with_skills(&cfg.skills_dir)
                .with_runtime(&cfg.runtime_dir, false)
                .with_mounts(cfg.sandbox_mounts.clone())
                .with_network(true),
        )),
    ));
    tools.register(Arc::new(RollbackSkillTool::new(Arc::clone(&skills))));

    let driver = any_driver::build(
        &cfg.provider,
        cfg.base_url.as_deref(),
        cfg.model.as_deref(),
        &cfg.api_key,
        cfg.llm_proxy.as_deref(),
        cfg.max_retries,
        cfg.retry_backoff_ms,
    )?;

    println!(
        "chuan {} · app={} · tz={} · db={} · provider={} · allowed={:?}",
        env!("CARGO_PKG_VERSION"),
        cfg.app_name,
        cfg.timezone,
        cfg.db,
        cfg.provider,
        cfg.allowed_users
    );
    println!(
        "sandbox: /ws(可写) /inbox(只读) /skills(只读) /media(只读) ← {} · skills={} · media={}",
        cfg.inbox, cfg.skills_dir, cfg.media_dir
    );
    if let Some(url) = &cfg.proxy.url {
        println!(
            "proxy: {}（沙盒={:?} web_fetch={:?} 服务={:?}）",
            url, cfg.proxy.sandbox, cfg.proxy.web_fetch, cfg.proxy.services
        );
    }
    println!(
        "vision: {}",
        if cfg.vision {
            "on（图片会作为多模态内容发给模型厂商，≤4 张/轮）"
        } else {
            "off（只给图片路径）"
        }
    );
    if !cfg.sandbox_mounts.is_empty() {
        let list: Vec<String> = cfg
            .sandbox_mounts
            .iter()
            .map(|m| format!("{}→{}{}", m.src.display(), m.dst.display(), if m.ro { "(ro)" } else { "(rw)" }))
            .collect();
        println!("sandbox mounts: {}", list.join(", "));
    }

    let actor = Arc::new(ConversationActor::new(
        Arc::clone(&store),
        Arc::new(driver),
        Arc::new(tools),
        Arc::new(env),
        Arc::clone(&channel),
        ActorConfig {
            model: cfg.model.clone().unwrap_or_else(|| cfg.provider.clone()),
            max_turns: cfg.max_turns,
            wall_clock_ms: cfg.wall_clock_ms,
            workspace_root: cfg.workspace.clone(),
            system_prompt: cfg.system_prompt.clone(),
            policy: cfg.policy.clone(),
            context_budget_chars: cfg.context_budget_chars,
            skill_source: Some(Arc::clone(&skills) as Arc<dyn harness_kernel::SkillSource>),
            tool_source: if mcp.is_empty() {
                None
            } else {
                Some(Arc::clone(&mcp) as Arc<dyn harness_kernel::SkillSource>)
            },
            distill_prompt: cfg.distill_prompt.clone(),
            tz_offset_min: cfg.tz_offset_min,
            identity: Arc::clone(&cfg.identity),
            vision: cfg.vision,
            proxy_hint: cfg.proxy.url.as_ref().map(|url| {
                format!(
                    "宿主机代理：{url}（联网 lane 里 127.0.0.1 就是宿主）。访问境外站点时用 `exec(proxy=true)` 或脚本里 `export https_proxy={url}`；\n\
                     国内站点直连更快，不要默认开代理；`web_fetch` 有 `proxy` 参数，不传则直连失败后自动重试代理。\n\
                     代理地址不要写进技能或记忆，以本段为准。"
                )
            }),
            host_paths: Some(format!(
                "宿主路径（你在沙盒里看不到，只用于回答“该放哪/在哪改”；不要试图读取，也永远不要索取凭据内容）：\n\
                 - 凭据目录：{secrets}/（每个服务一个文件，权限 0600，由主人创建）\n\
                 - 配置文件：{config}/chuan.toml；运行期新增的服务：{config}/services.d/<name>.toml\n\
                 - playbook：{config}/playbooks/<topic>.md（用 playbook 工具取）\n\
                 - 技能：{skills}（用 publish_skill / rollback_skill 管理）\n\
                 - 文件库：{files}；聊天附件：{media}\n\
                 - 工作区（沙盒内 /ws）：{ws}",
                secrets = cfg.config_dir.join("secrets").display(),
                config = cfg.config_dir.display(),
                skills = cfg.skills_dir,
                files = cfg.library_dir,
                media = cfg.media_dir,
                ws = cfg.workspace,
            )),
        },
        Some(Arc::new(harness_store::Fts5Retriever::new(Arc::clone(&store)))),
    ));

    if let Err(e) = channel.register_commands().await {
        eprintln!("register commands failed: {e}");
    }

    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();

    // workspace 定期清理（/ws 是草稿区，不是档案库）
    {
        let dir = std::path::PathBuf::from(&cfg.workspace);
        let media = std::path::PathBuf::from(&cfg.media_dir);
        let media_keep = cfg.media_keep_days;
        let keep_days = cfg.workspace_keep_days;
        let max_mb = cfg.workspace_max_mb;
        let stop = cancel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
            tick.tick().await; // 第一次立刻返回，启动时已清过，跳过
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let m = workspace_gc::sweep_now(&media, media_keep, 0);
                        if m.removed_files > 0 {
                            tracing::info!(files = m.removed_files, "media gc");
                        }
                        let r = workspace_gc::sweep_now(&dir, keep_days, max_mb);
                        if r.removed_files > 0 {
                            tracing::info!(
                                files = r.removed_files,
                                bytes = r.removed_bytes,
                                kept = r.kept_files,
                                "workspace gc"
                            );
                        }
                    }
                    _ = stop.cancelled() => break,
                }
            }
        });
    }
    let poll_cancel = cancel.clone();
    let ch = Arc::clone(&channel);
    let poller = tokio::spawn(async move {
        if let Err(e) = ch.run(tx, poll_cancel).await {
            eprintln!("poll loop ended: {e}");
        }
    });

    Arc::clone(&actor).spawn_scheduler(cancel.clone());
    println!("polling started");
    tokio::select! {
        r = actor.run(rx, cancel.clone()) => { r?; }
        _ = tokio::signal::ctrl_c() => { cancel.cancel(); }
    }
    let _ = poller.await;
    Ok(())
}

/// venv 探针：能跑 `python3 -m pip --version` 才算可用（只跑 `import sys` 不够——
/// ensurepip 缺失时 python 可执行、pip 却不存在）。失败时返回可读原因，直接进日志。
fn venv_probe(py: &std::path::Path) -> Result<(), String> {
    let bin = py.join("bin/python3");
    if !bin.exists() {
        return Err("缺少 bin/python3".into());
    }
    let out = std::process::Command::new(&bin)
        .args(["-m", "pip", "--version"])
        .output()
        .map_err(|e| format!("无法执行 {}: {e}", bin.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.is_empty() {
        return Err(format!("pip 探针退出码 {:?}", out.status.code()));
    }
    // 只留一行，避免把整段 traceback 灌进 journal
    Err(first.chars().take(160).collect())
}

/// 老 venv（没带 --system-site-packages）就地改标志，不用重建。
fn allow_system_site_packages(py: &std::path::Path) {
    let cfg = py.join("pyvenv.cfg");
    if let Ok(text) = std::fs::read_to_string(&cfg) {
        if text.contains("include-system-site-packages = false") {
            let _ = std::fs::write(
                &cfg,
                text.replace(
                    "include-system-site-packages = false",
                    "include-system-site-packages = true",
                ),
            );
            println!("runtime: venv 已允许读取系统包（include-system-site-packages=true）");
        }
    }
}

#[cfg(test)]
mod venv_tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("chuan-venv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn probe_rejects_missing_bin() {
        let d = tmp("nobin");
        let err = venv_probe(&d).unwrap_err();
        assert!(err.contains("缺少"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn probe_rejects_python_without_pip() {
        // 复现板上那种半成品：bin/python3 能执行，但没有 pip
        let d = tmp("nopip");
        std::fs::create_dir_all(d.join("bin")).unwrap();
        let bin = d.join("bin/python3");
        std::fs::write(&bin, "#!/bin/sh\necho 'No module named pip' >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let err = venv_probe(&d).unwrap_err();
        assert!(err.contains("No module named pip"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
