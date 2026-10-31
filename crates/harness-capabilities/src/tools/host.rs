//! `host`：L1 宿主执行（D22）。命令由模型选，可见范围由授权表定。
//!
//! 与 `exec` 的区别不是"有没有网"，而是**执行位置**：这条 lane 跑在宿主上，
//! 能看到宿主的进程、服务与授权目录；代价是必须逐条授权、默认要主人点头。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use harness_kernel::{
    Block, Domain, EffectClass, ErrorCode, ExecOpts, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

use crate::grants::GrantsCache;
use crate::host::{needs_approval, HostEnv};

pub struct HostTool {
    env: Arc<HostEnv>,
    cache: Arc<GrantsCache>,
    default_timeout_ms: u64,
    /// 逐次审计（与 hostd 写同一个文件）：argv、命中的授权、退出码、耗时。
    /// 没有它，"它半夜自己跑了什么"只能靠 journald 里的碎片拼。
    audit: PathBuf,
}

impl HostTool {
    pub fn new(env: Arc<HostEnv>, cache: Arc<GrantsCache>, audit: impl Into<PathBuf>) -> Self {
        Self { env, cache, default_timeout_ms: 20_000, audit: audit.into() }
    }

    fn argv_of(args: &serde_json::Value) -> Result<Vec<String>, ToolError> {
        let argv: Vec<String> = if let Some(script) = args.get("script").and_then(|v| v.as_str()) {
            vec!["sh".into(), "-lc".into(), script.to_owned()]
        } else if let Some(arr) = args.get("argv").and_then(|v| v.as_array()) {
            arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()
        } else {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "需要 argv 或 script", false));
        };
        if argv.is_empty() {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "argv 为空", false));
        }
        Ok(argv)
    }
}

/// 一条审计记录。字段刻意与 hostd 侧对齐（`grant_index` 在这里是每条挂载的来源），
/// 这样 `jq` 一个文件就能看全"谁在什么时候动了宿主"。
pub(crate) fn audit_call(
    path: &std::path::Path,
    argv: &[String],
    grants: &[String],
    ephemeral: bool,
    error: Option<String>,
    code: Option<i32>,
    ms: u128,
) {
    let rec = serde_json::json!({
        "at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "via": "host",
        "argv": argv,
        "grants": grants,
        "ephemeral": ephemeral,
        "code": code,
        "ms": ms as u64,
        "error": error,
    });
    crate::net::append_log(path, &rec.to_string());
}

#[async_trait]
impl Tool for HostTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "host".into(),
            description: "在**宿主机上**执行命令（L1，以服务账号身份，共享宿主网络）。\
和 `exec` 的区别：`exec` 在隔离沙盒里（看不见宿主、没有网络），`host` 能看到宿主的进程、服务与「授权表现已给定的目录」。\
**默认只挂 `approval = always` 的目录**：需要临时读别的路径（比如 `/home/<你>/xxx`）就在 `grant` 里点名，\
只读、一次一格，会弹审批卡——`grant` 只能只读，**要写必须让主人写进授权表**。\
被拒时不要换写法重试：按返回的 `permission denied (host_path ...)` 用 `capability` 提交申请。\
常见用法：`systemctl --user`（user timer / 用户级服务）、`journalctl -u`、读授权目录里的文件。\
注意：**一次调用做一整件事**（把多条命令写进一个 script），每次调用都会新建容器。\
**路径对照**：这里的 `/ws` 与沙盒的 `/ws` 是**同一个工作区**，一边写另一边立刻可见；其它路径就是宿主上的真实路径，没有映射层。\
`/etc/chuan/secrets`、`/etc/chuan/services.d`、`/etc/chuan/chuan.toml` 被强制遮蔽（空目录 / 0 字节）。\
**纯计算、画图、处理 /ws 里的文件不要用 host**：沙盒里 `/py/bin/python3`（venv 含系统 site-packages）够用，而且不需要审批。host 只用来做「必须看宿主」的事。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "_approval": {
                        "type": "object",
                        "description": "审批卡上给非程序员看的解释；这次调用会触发审批时必填",
                        "properties": {
                            "why": { "type": "string", "description": "一句人话：你想做什么、为什么需要" },
                            "impact": { "type": "string", "description": "影响面：只读 / 写文件 / 装软件 / 向设备发指令 / 改自身配置" }
                        },
                        "required": ["why", "impact"]
                    },
                    "argv": { "type": "array", "items": { "type": "string" } },
                    "script": { "type": "string", "description": "shell 脚本，等价于 sh -lc" },
                    "timeout_ms": { "type": "integer" },
                    "grant": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "本次额外只读挂载的绝对路径（≤8 条）。只能读；要写请改授权表。"
                    }
                },
                "oneOf": [{ "required": ["argv"] }, { "required": ["script"] }]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 任意宿主命令：按外部副作用处理，崩溃后不自动重跑。
        EffectClass::External
    }

    fn domain(&self) -> Domain {
        Domain::Host
    }

    fn tier(&self) -> u8 {
        1
    }

    /// 只用到 `always` 授权（没有临时挂载）时不打扰人——这正是 `always` 的意义。
    /// 策略仍可把整条 lane 设成 `deny`，那条天花板不会被这里越过。
    fn skip_approval(&self, args: &serde_json::Value) -> bool {
        let extra = args
            .get("grant")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        if extra > 0 {
            return false;
        }
        self.cache.current().paths_ok()
    }

    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(list) = args.get("grant").and_then(|v| v.as_array()) {
            let items: Vec<String> = list
                .iter()
                .filter_map(|v| v.as_str())
                .map(|p| match std::fs::metadata(p) {
                    Ok(m) if m.is_dir() => format!("{p}/（目录）"),
                    Ok(m) => format!("{p}（{} B）", m.len()),
                    Err(_) => format!("{p}（不存在）"),
                })
                .collect();
            if !items.is_empty() {
                parts.push(format!("临时只读：{}", items.join("、")));
            }
        }
        let cmd = match (args.get("argv"), args.get("script")) {
            (Some(a), _) => a
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            (None, Some(s)) => s.as_str().unwrap_or("").to_owned(),
            _ => String::new(),
        };
        let cmd: String = cmd.chars().take(160).collect();
        parts.push(format!("命令：{cmd}"));
        Some(parts.join("；"))
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let argv = Self::argv_of(&args)?;
        let extra: Vec<String> = args
            .get("grant")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();

        let grants = self.cache.current();
        let merged = grants.with_ephemeral(&extra).map_err(|e| {
            ToolError::new(ErrorCode::InvalidArgs, e, false)
        })?;
        debug_assert_eq!(
            needs_approval(&merged),
            !extra.is_empty(),
            "临时挂载与审批判断必须一致"
        );

        let opts = ExecOpts {
            nice: false,
            timeout_ms: args
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(self.default_timeout_ms),
            cwd: None,
            // 宿主 lane 不需要代理变量：它直接共享宿主 netns
            env: Vec::new(),
            stdin: None,
        };

        let grants_desc: Vec<String> = crate::host::mounts_for(&merged)
            .iter()
            .map(|m| format!("{}:{}", m.src.display(), if m.ro { "ro" } else { "rw" }))
            .collect();
        let started = Instant::now();
        let result = self
            .env
            .exec_with(&argv, opts, &merged, ctx.cancel.clone())
            .await;
        audit_call(
            &self.audit,
            &argv,
            &grants_desc,
            !merged.ephemeral.is_empty(),
            result.as_ref().err().map(|e| e.to_string()),
            result.as_ref().ok().and_then(|o| o.code),
            started.elapsed().as_millis(),
        );

        let out = result.map_err(|e| match e {
                harness_kernel::EnvError::NotGranted { kind, item, hint } => {
                    tracing::info!(%kind, %item, "host: 越界被拒");
                    ToolError::new(
                        ErrorCode::NotPermitted,
                        format!("permission denied ({kind}): {item} — {hint}"),
                        false,
                    )
                }
                harness_kernel::EnvError::Timeout(ms) => {
                    ToolError::new(ErrorCode::Timeout, format!("超时 {ms}ms"), true)
                }
                other => ToolError::new(ErrorCode::Unavailable, other.to_string(), true),
            })?;

        let mut text = format!(
            "exit={}\n",
            out.code.map(|c| c.to_string()).unwrap_or("signal".into())
        );
        if !out.stdout.is_empty() {
            text.push_str("--- stdout ---\n");
            text.push_str(&out.stdout);
        }
        if !out.stderr.is_empty() {
            text.push_str("\n--- stderr ---\n");
            text.push_str(&out.stderr);
        }
        if out.truncated {
            text.push_str("\n[输出已截断]");
        }

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

    fn tool(toml: &str) -> (tempfile::TempDir, HostTool) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("chuan.toml");
        std::fs::write(&cfg, toml).unwrap();
        let cache = Arc::new(GrantsCache::new(&cfg));
        let env = Arc::new(HostEnv::new(dir.path().join("ws"), Arc::clone(&cache)));
        let audit = dir.path().join("host-access.jsonl");
        (dir, HostTool::new(env, cache, audit))
    }

    #[test]
    fn only_always_grants_skip_approval() {
        let (_d, t) = tool("[host.path]\n\"/usr\" = { mode = \"ro\" }\n");
        // 没有点名 → 不用打扰人（这正是 always 的意义）
        assert!(t.skip_approval(&serde_json::json!({"argv": ["true"]})));
        // 点名临时挂载 → 必须问
        assert!(!t.skip_approval(
            &serde_json::json!({"argv": ["true"], "grant": ["/home"]})
        ));
        // 空数组也算没点名
        assert!(t.skip_approval(
            &serde_json::json!({"argv": ["true"], "grant": []})
        ));
    }

    #[test]
    fn broken_table_never_skips() {
        let (_d, t) = tool("[host.path\n");
        assert!(!t.skip_approval(&serde_json::json!({"argv": ["true"]})));
    }

    #[test]
    fn audit_line_is_one_json_object() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("host-access.jsonl");
        audit_call(
            &p,
            &["systemctl".into(), "--user".into(), "list-timers".into()],
            &["/usr:ro".into()],
            false,
            None,
            Some(0),
            42,
        );
        audit_call(&p, &["cat".into()], &[], true, Some("permission denied".into()), None, 7);

        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["via"], "host");
        assert_eq!(v["argv"][0], "systemctl");
        assert_eq!(v["code"], 0);
        assert_eq!(v["ephemeral"], false);
        let v2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v2["ephemeral"], true);
        assert_eq!(v2["error"], "permission denied");
    }

    #[test]
    fn approval_hint_mentions_temp_paths_and_command() {
        let (_d, t) = tool("[host.path]\n\"/usr\" = { mode = \"ro\" }\n");
        let hint = t
            .approval_hint(&serde_json::json!({
                "script": "systemctl --user list-timers",
                "grant": ["/home/alice/report.csv"]
            }))
            .unwrap();
        assert!(hint.contains("临时只读"), "{hint}");
        assert!(hint.contains("systemctl --user"), "{hint}");
    }
}
