//! `chuan hostd`：唯一的提权通道（D22 的 L2）。
//!
//! 它**不懂领域语义**：不做 `pkg.install` / `unit.start` 这种动词，只做四件事——
//! 校验对端、按 argv 前缀匹配授权表、`env -i` 执行、写审计。加一种新命令只改 TOML。
//!
//! 安全前提（每一条都不能省）：
//! - socket `0660 root:chuan`，`SO_PEERCRED` 再校一次 uid（组权限只是第一道）；
//! - `env -i`，只给 PATH 与 `DEBIAN_FRONTEND`；不挂凭据目录、不继承任何 `*_KEY`；
//! - 只接受 argv 数组，不走 shell；
//! - 授权表解析失败 → 全部拒绝（fail-closed），不回退上一版。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::grants::{ArgvGrant, Grants, GrantsCache};

/// 单次执行的输出上限（与工具侧一致，避免把整块日志灌进上下文）
const OUTPUT_CAP: usize = 64 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 300_000;

#[derive(Debug, Clone)]
pub struct HostdConfig {
    pub socket: PathBuf,
    pub config: PathBuf,
    pub audit: PathBuf,
    /// 只接受这个 uid 的连接（install.sh 传 `id -u chuan`）
    pub allow_uid: u32,
    /// socket 的组（= 服务账号的主组），让 chuan 能在文件系统层连进来
    pub allow_gid: u32,
    /// `snapshot = "dpkg"` 的条目在执行前把包清单存到这里
    pub snapshot_dir: PathBuf,
}

#[derive(serde::Deserialize)]
struct Request {
    argv: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct Response {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
    /// 命中的授权条目（从 1 开始），审计与工具栏对账用
    grant_index: usize,
    error: Option<String>,
    /// 执行前做的快照文件（没有就是 None）
    snapshot: Option<String>,
}

/// argv 前缀匹配：逐项相等，不做 glob、不看参数个数。
///
/// 这是这套设计公认的弱点（`apt-get install` 一条等于放行它全部参数），
/// 兜底靠"逐次审批 + `-s` 预览 + dpkg 快照"，见 D22「代价」。
pub fn match_grant<'a>(grants: &'a Grants, argv: &[String]) -> Option<(usize, &'a ArgvGrant)> {
    if grants.is_locked() || argv.is_empty() {
        return None;
    }
    grants
        .elevated
        .iter()
        .enumerate()
        .find(|(_, g)| !g.argv.is_empty() && argv.starts_with(&g.argv))
        .map(|(i, g)| (i + 1, g))
}

/// 变更前把已装包清单落盘。失败不阻断（宁可真跑失败也能回滚知道原状？不——失败就说明
/// 状态不可知，仍然继续执行但记 WARN：apt 自己有事务性，回滚信息缺失是可见的代价）。
fn take_dpkg_snapshot(dir: &Path) -> Option<String> {
    let _ = std::fs::create_dir_all(dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("{ts}.txt"));
    match std::process::Command::new("/usr/bin/dpkg")
        .arg("--get-selections")
        .output()
    {
        Ok(o) if o.status.success() => {
            if let Err(e) = std::fs::write(&path, &o.stdout) {
                tracing::warn!(error = %e, "hostd: 写 dpkg 快照失败");
                return None;
            }
            tracing::info!(path = %path.display(), "hostd: 已存 dpkg 快照");
            Some(path.display().to_string())
        }
        Ok(o) => {
            tracing::warn!(code = ?o.status.code(), "hostd: dpkg --get-selections 失败");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "hostd: 跑不了 dpkg，跳过快照");
            None
        }
    }
}

fn truncate(s: &mut String, truncated: &mut bool) {
    if s.len() > OUTPUT_CAP {
        let mut end = OUTPUT_CAP;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        *truncated = true;
    }
}

/// 审计：一行一条 JSONL。写失败只记日志，不影响执行结果（审计不能反过来阻断运维）。
fn audit_log(path: &Path, grant_index: usize, argv: &[String], code: Option<i32>, ms: u128) {
    let line = serde_json::json!({
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "grant_index": grant_index,
        "argv": argv,
        "code": code,
        "ms": ms as u64,
    });
    let old = std::fs::metadata(path).ok().map(|m| m.len()).unwrap_or(0);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    if let Ok(mut f) = opts.open(path) {
        use std::io::Write;
        if old == 0 {
            // 首次创建：收紧成 0600（命令里可能带敏感参数，只给属主看）
            let _ = std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
        }
        let _ = writeln!(f, "{line}");
    } else {
        tracing::warn!(path = %path.display(), "写审计失败");
    }
}

async fn handle(mut stream: UnixStream, cfg: &HostdConfig, grants: &GrantsCache) -> std::io::Result<()> {
    // 1. 对端身份：组权限之外的第二次校验
    let peer = stream.peer_cred()?;
    if peer.uid() != cfg.allow_uid {
        tracing::warn!(uid = peer.uid(), "hostd: 拒绝非服务账号的连接");
        return Ok(());
    }

    let (rd, mut wr) = stream.split();
    let mut line = String::new();
    if BufReader::new(rd).read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let started = Instant::now();

    let resp = match serde_json::from_str::<Request>(&line) {
        Err(e) => Response {
            ok: false,
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            grant_index: 0,
            error: Some(format!("请求不是合法 JSON：{e}")),
            snapshot: None,
        },
        Ok(req) if req.argv.is_empty() => Response {
            ok: false,
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            grant_index: 0,
            error: Some("argv 为空".into()),
            snapshot: None,
        },
        Ok(req) => {
            let grants_now = grants.current();
            match match_grant(&grants_now, &req.argv) {
                None => {
                    tracing::info!(argv = ?req.argv, "hostd: 未命中授权表");
                    Response {
                        ok: false,
                        code: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        truncated: false,
                        grant_index: 0,
                        snapshot: None,
                        error: Some(match &grants_now.locked {
                            Some(r) => format!("授权表不可用（{r}）——修好配置再试，不会回退旧授权"),
                            None => format!(
                                "命令不在 [[host_elevated.grant]] 里：{}。加一行业主授权后再试",
                                req.argv.join(" ")
                            ),
                        }),
                    }
                }
                Some((idx, g)) => {
                    // 快照不算提权：它由 hostd 自己决定，不需要主人再授权一条 argv。
                    let snapshot = match g.snapshot.as_deref() {
                        Some("dpkg") => take_dpkg_snapshot(&cfg.snapshot_dir),
                        Some(other) => {
                            tracing::warn!(other, "hostd: 未知的 snapshot 类型，跳过");
                            None
                        }
                        None => None,
                    };
                    let mut cmd = tokio::process::Command::new(&req.argv[0]);
                    cmd.args(&req.argv[1..])
                        .env_clear()
                        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
                        .env("LANG", "C.UTF-8")
                        .env("DEBIAN_FRONTEND", "noninteractive")
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);
                    let out = tokio::time::timeout(
                        Duration::from_millis(DEFAULT_TIMEOUT_MS),
                        cmd.output(),
                    )
                    .await;
                    let mut r = match out {
                        Ok(Ok(o)) => {
                            let mut stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                            let mut stderr = String::from_utf8_lossy(&o.stderr).into_owned();
                            let mut truncated = false;
                            truncate(&mut stdout, &mut truncated);
                            truncate(&mut stderr, &mut truncated);
                            Response {
                                ok: o.status.success(),
                                code: o.status.code(),
                                stdout,
                                stderr,
                                truncated,
                                grant_index: idx,
                                error: None,
                                snapshot: snapshot.clone(),
                            }
                        }
                        Ok(Err(e)) => Response {
                            ok: false,
                            code: None,
                            stdout: String::new(),
                            stderr: String::new(),
                            truncated: false,
                            grant_index: idx,
                            error: Some(format!("启动失败：{e}")),
                            snapshot: snapshot.clone(),
                        },
                        Err(_) => Response {
                            ok: false,
                            code: None,
                            stdout: String::new(),
                            stderr: String::new(),
                            truncated: false,
                            grant_index: idx,
                            error: Some(format!("超时（{}ms）", DEFAULT_TIMEOUT_MS)),
                            snapshot: snapshot.clone(),
                        },
                    };
                    audit_log(&cfg.audit, idx, &req.argv, r.code, started.elapsed().as_millis());
                    r
                }
            }
        }
    };

    let body = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    wr.write_all(&body).await?;
    wr.write_all(b"\n").await?;
    wr.flush().await?;
    Ok(())
}

/// 入口：`chuan hostd --socket <sock> --config <toml> --audit <jsonl> --allow-uid <uid>`
pub async fn serve(cfg: HostdConfig) -> Result<(), String> {
    if let Some(dir) = cfg.socket.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建 socket 目录：{e}"))?;
    }
    // 旧的 socket 文件会让 bind 失败；先删（同目录同属主，删错了也只是自己）
    let _ = std::fs::remove_file(&cfg.socket);
    let listener = UnixListener::bind(&cfg.socket)
        .map_err(|e| format!("bind {}：{e}", cfg.socket.display()))?;
    // 0660 + root:服务组：组权限是第一道闸，SO_PEERCRED 是第二道。
    // 不 chown 的话 socket 属主是 root:root，chuan 连文件系统层都进不来。
    if cfg.allow_gid != 0 {
        let _ = std::os::unix::fs::chown(&cfg.socket, Some(0), Some(cfg.allow_gid));
    }
    let _ = std::fs::set_permissions(
        &cfg.socket,
        std::os::unix::fs::PermissionsExt::from_mode(0o660),
    );

    let grants = Arc::new(GrantsCache::new(&cfg.config));
    tracing::info!(
        socket = %cfg.socket.display(),
        config = %cfg.config.display(),
        allow_uid = cfg.allow_uid,
        "hostd 已启动"
    );

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "accept 失败");
                continue;
            }
        };
        let cfg = cfg.clone();
        let grants = Arc::clone(&grants);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &cfg, &grants).await {
                tracing::warn!(error = %e, "hostd 连接处理失败");
            }
        });
    }
}

/// 从命令行参数解析配置（`hostd` 之后的参数）
pub fn config_from_args(args: &[String]) -> Result<HostdConfig, String> {
    let mut cfg = HostdConfig {
        socket: PathBuf::from("/run/chuan/host.sock"),
        config: PathBuf::from("/etc/chuan/chuan.toml"),
        audit: PathBuf::from("/var/lib/chuan/host-access.jsonl"),
        allow_uid: 0,
        allow_gid: 0,
        snapshot_dir: PathBuf::from("/var/lib/chuan/pkg-snapshots"),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut take = |name: &str| -> Result<String, String> {
            it.next().cloned().ok_or_else(|| format!("{name} 缺少参数"))
        };
        match a.as_str() {
            "--socket" => cfg.socket = PathBuf::from(take("--socket")?),
            "--config" => cfg.config = PathBuf::from(take("--config")?),
            "--audit" => cfg.audit = PathBuf::from(take("--audit")?),
            "--allow-gid" => {
                cfg.allow_gid = take("--allow-gid")?
                    .parse()
                    .map_err(|_| "--allow-gid 需要数字".to_string())?
            }
            "--snapshot-dir" => cfg.snapshot_dir = PathBuf::from(take("--snapshot-dir")?),
            "--allow-uid" => {
                cfg.allow_uid = take("--allow-uid")?
                    .parse()
                    .map_err(|_| "--allow-uid 需要数字".to_string())?
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }
    if cfg.allow_uid == 0 {
        return Err("必须显式指定 --allow-uid（不允许 0）".into());
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(toml: &str) -> Grants {
        crate::grants::from_toml(toml).unwrap()
    }

    const SAMPLE: &str = r#"
[[host_elevated.grant]]
argv = ["/usr/bin/apt-get", "install"]
approval = "once"
snapshot = "dpkg"

[[host_elevated.grant]]
argv = ["/usr/bin/systemctl", "restart"]
approval = "once"
"#;

    #[test]
    fn prefix_match_is_exact_per_item() {
        let g = grants(SAMPLE);
        let hit = match_grant(&g, &["/usr/bin/apt-get".into(), "install".into(), "-y".into()]);
        assert_eq!(hit.map(|(i, _)| i), Some(1));

        // 不是前缀就不匹配：remove 不能借用 install 的授权
        assert!(match_grant(&g, &["/usr/bin/apt-get".into(), "remove".into()]).is_none());
        // 只用第一个词也不匹配（argv 前缀要逐项相等）
        assert!(match_grant(&g, &["/usr/bin/apt-get".into()]).is_none());
    }

    #[test]
    fn locked_table_allows_nothing() {
        let g = Grants::locked("配置写坏了");
        assert!(match_grant(&g, &["/usr/bin/apt-get".into(), "install".into()]).is_none());
    }

    #[test]
    fn args_require_non_root_uid() {
        let a: Vec<String> = vec![
            "--socket".into(),
            "/tmp/s".into(),
            "--allow-uid".into(),
            "996".into(),
        ];
        let cfg = config_from_args(&a).unwrap();
        assert_eq!(cfg.allow_uid, 996);
        assert_eq!(cfg.socket, PathBuf::from("/tmp/s"));

        let bad: Vec<String> = vec!["--allow-uid".into(), "0".into()];
        assert!(config_from_args(&bad).is_err(), "uid 0 必须被拒");
    }

    #[test]
    fn output_truncation_respects_char_boundary() {
        let mut s = "汉".repeat(OUTPUT_CAP); // 3 字节/字，保证会截断
        let mut t = false;
        truncate(&mut s, &mut t);
        assert!(t);
        assert!(s.len() <= OUTPUT_CAP);
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());
    }
}
