//! `host`：L1 宿主执行（D22）。命令由模型选，可见范围由授权表定。
//!
//! 与 `exec` 的区别不是"有没有网"，而是**执行位置**：这条 lane 跑在宿主上，
//! 能看到宿主的进程、服务与授权目录；代价是必须逐条授权、默认要主人点头。

use std::sync::Arc;

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
}

impl HostTool {
    pub fn new(env: Arc<HostEnv>, cache: Arc<GrantsCache>) -> Self {
        Self { env, cache, default_timeout_ms: 20_000 }
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
注意：**一次调用做一整件事**（把多条命令写进一个 script），每次调用都会新建容器。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
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
        !self.cache.current().is_locked()
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

        let out = self
            .env
            .exec_with(&argv, opts, &merged, ctx.cancel.clone())
            .await
            .map_err(|e| match e {
                harness_kernel::EnvError::NotGranted { kind, item, hint } => ToolError::new(
                    ErrorCode::NotPermitted,
                    format!("permission denied ({kind}): {item} — {hint}"),
                    false,
                ),
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
        (dir, HostTool::new(env, cache))
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
