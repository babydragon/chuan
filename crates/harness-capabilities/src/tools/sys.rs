//! `sys`：宿主只读观测（D22 的 L3）。
//!
//! 两条实现路径，刻意分开：
//! - **进程内读文件**：`/proc`、`/sys`。零子进程、零 lane 占用。
//! - **固定命令直连**：`systemctl show` / `journalctl`。参数只有"单元名 + 行数"，
//!   由这一层校验后才拼进 argv——**不经 bwrap**（板上实测 bwrap 内 sd_booted() 失败，
//!   `systemctl` 直接报 Host is down，见 `OPS.md` 已知限制）。
//!
//! 这里**没有** start/stop/restart：那是 `host`（逐条审批）与 `hostd`（argv 授权表）的事。

use std::fmt::Write as _;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use harness_kernel::{
    Block, Domain, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

/// 单元名白名单校验：systemd 允许字母数字与 `@ . _ : -`，长度上限 128。
/// 不做正则（少一个依赖），逐字符判断；**拒绝 `/`**，防止把路径拼进 argv。
pub fn valid_unit(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | ':' | '-'))
}

/// `/proc/loadavg` → (1m, 5m, 15m, runnable/total)
pub fn parse_loadavg(s: &str) -> Option<(f32, f32, f32, String)> {
    let mut it = s.split_whitespace();
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.parse().ok()?;
    let procs = it.next().unwrap_or("-").to_owned();
    Some((a, b, c, procs))
}

/// `/proc/meminfo` → 关心的几项（kB）
pub fn parse_meminfo(s: &str) -> Vec<(String, u64)> {
    const KEYS: [&str; 6] = [
        "MemTotal",
        "MemFree",
        "MemAvailable",
        "SwapTotal",
        "SwapFree",
        "Cached",
    ];
    s.lines()
        .filter_map(|l| {
            let (k, rest) = l.split_once(':')?;
            if !KEYS.contains(&k) {
                return None;
            }
            let v: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some((k.to_owned(), v))
        })
        .collect()
}

/// 从 `/proc/self/cgroup` 里取 cgroup v2 的路径（形如 `0::/system.slice/chuan.service`）
pub fn cgroup_v2_path(s: &str) -> Option<String> {
    s.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().trim_start_matches('/').to_owned())
}

pub struct SysTool;

impl SysTool {
    fn spec_text() -> &'static str {
        "只读观测本机状态。`metric` 取值：\
load（1/5/15 分钟负载）、mem（内存与 swap）、stat（CPU 时间片与运行/阻塞进程）、\
psi（资源压力，需内核支持）、cgroup（本服务的 memory/pids 用量与上限）、\
units（某个 systemd 单元的状态，需 `unit`）、journal（某个单元的最近日志，需 `unit`，可选 `lines`，上限 500）。\
这些都是**只读**：`sys` 不能启动/停止/重启任何东西——那要用 `host`（要主人确认）。\
读不到别的用户的进程内存时，会说「看不到」，不会说「进程不存在」。\
**数据来自宿主，不是沙盒**：`/proc` `/sys` 是宿主视图；`units` / `journal` 由内核直连\
`systemctl show` / `journalctl`（不经 bwrap），沙盒里看不到这些不影响本工具。"
    }

    async fn run_fixed(&self, argv: &[String], timeout_ms: u64) -> Result<String, ToolError> {
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| {
            ToolError::new(ErrorCode::Unavailable, format!("启动 {} 失败：{e}", argv[0]), true)
        })?;
        let out = match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(ToolError::new(ErrorCode::Unavailable, e.to_string(), true));
            }
            Err(_) => {
                return Err(ToolError::new(
                    ErrorCode::Timeout,
                    format!("{} 超过 {timeout_ms}ms", argv[0]),
                    true,
                ));
            }
        };
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            text.push_str(&format!("\n[{} 退出码 {:?}] {}", argv[0], out.status.code(), err.trim()));
        }
        Ok(text)
    }
}

#[async_trait]
impl Tool for SysTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sys".into(),
            description: Self::spec_text().into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "metric": {
                        "type": "string",
                        "enum": ["load", "mem", "stat", "psi", "cgroup", "units", "journal"]
                    },
                    "unit": { "type": "string", "description": "systemd 单元名，例如 chuan.service / chuan-backup.timer" },
                    "lines": { "type": "integer", "description": "journal 行数，1–500，默认 50" }
                },
                "required": ["metric"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    fn domain(&self) -> Domain {
        Domain::HostObserve
    }

    fn tier(&self) -> u8 {
        1
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let metric = args.get("metric").and_then(|v| v.as_str()).unwrap_or("");
        let unit = args.get("unit").and_then(|v| v.as_str()).unwrap_or("");
        let lines = args
            .get("lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 500);

        let text = match metric {
            "load" => {
                let raw = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
                match parse_loadavg(&raw) {
                    Some((a, b, c, p)) => {
                        format!("load 1m={a} 5m={b} 15m={c}\n进程（运行中/总数）={p}")
                    }
                    None => "[读不到 /proc/loadavg]".into(),
                }
            }
            "mem" => {
                let raw = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
                let items = parse_meminfo(&raw);
                if items.is_empty() {
                    "[读不到 /proc/meminfo]".into()
                } else {
                    let mut s = String::new();
                    for (k, kb) in items {
                        let _ = writeln!(s, "{k}: {} MB", kb / 1024);
                    }
                    s
                }
            }
            "stat" => {
                let raw = std::fs::read_to_string("/proc/stat").unwrap_or_default();
                raw.lines()
                    .filter(|l| l.starts_with("cpu ") || l.starts_with("procs_"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "psi" => {
                let mut s = String::new();
                for f in ["cpu", "memory", "io"] {
                    match std::fs::read_to_string(format!("/proc/pressure/{f}")) {
                        Ok(t) => {
                            let _ = writeln!(s, "[{f}]\n{}", t.trim_end());
                        }
                        Err(_) => {
                            let _ = writeln!(s, "[{f}] 不可用（内核未开 CONFIG_PSI）");
                        }
                    }
                }
                s
            }
            "cgroup" => {
                let raw = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
                let base = cgroup_v2_path(&raw)
                    .map(|p| format!("/sys/fs/cgroup/{p}"))
                    .unwrap_or_else(|| "/sys/fs/cgroup".into());
                let mut s = String::new();
                for f in ["memory.current", "memory.max", "pids.current", "pids.max"] {
                    let v = std::fs::read_to_string(format!("{base}/{f}"))
                        .map(|v| v.trim().to_owned())
                        .unwrap_or_else(|_| "不可读".into());
                    let _ = writeln!(s, "{f}: {v}");
                }
                s
            }
            "units" => {
                if !valid_unit(unit) {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        "unit 需要合法的单元名（如 chuan.service），不接受路径或空格",
                        false,
                    ));
                }
                let argv: Vec<String> = [
                    "systemctl",
                    "show",
                    unit,
                    "-p",
                    "Id,Description,ActiveState,SubState,UnitFileState,MainPID,MemoryCurrent,ActiveEnterTimestamp",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                self.run_fixed(&argv, 5_000).await?
            }
            "journal" => {
                if !valid_unit(unit) {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        "unit 需要合法的单元名（如 chuan.service），不接受路径或空格",
                        false,
                    ));
                }
                let argv: Vec<String> = vec![
                    "journalctl".into(),
                    "-n".into(),
                    lines.to_string(),
                    "-u".into(),
                    unit.to_owned(),
                    "--no-pager".into(),
                    "-o".into(),
                    "short-iso".into(),
                ];
                self.run_fixed(&argv, 10_000).await?
            }
            other => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("未知 metric：{other}"),
                    false,
                ));
            }
        };

        Ok(ToolOutput {
            content: vec![Block::Code { lang: None, text }],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names_are_whitelisted() {
        for good in ["chuan.service", "chuan-backup.timer", "user@1000.service", "a_b:c-d.e"] {
            assert!(valid_unit(good), "{good} 应通过");
        }
        for bad in ["", "a/b", "../etc/passwd", "foo service", "a\nb"] {
            assert!(!valid_unit(bad), "{bad} 应被拒");
        }
        assert!(!valid_unit(&"x".repeat(129)));
    }

    #[test]
    fn loadavg_parses() {
        let (a, b, c, p) = parse_loadavg("0.52 0.41 0.38 2/183 12345\n").unwrap();
        assert!((a - 0.52).abs() < 0.001);
        assert!((b - 0.41).abs() < 0.001);
        assert!((c - 0.38).abs() < 0.001);
        assert_eq!(p, "2/183");
        assert!(parse_loadavg("garbage").is_none());
    }

    #[test]
    fn meminfo_keeps_only_known_keys() {
        let raw = "MemTotal:        8000000 kB\nMemFree:          100000 kB\nVmallocTotal: 1 kB\n";
        let items = parse_meminfo(raw);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], ("MemTotal".to_string(), 8_000_000));
    }

    #[test]
    fn cgroup_path_parses() {
        assert_eq!(
            cgroup_v2_path("0::/system.slice/chuan.service\n").as_deref(),
            Some("system.slice/chuan.service")
        );
        assert_eq!(cgroup_v2_path("1:name=openrc:/\n").as_deref(), None);
    }
}
