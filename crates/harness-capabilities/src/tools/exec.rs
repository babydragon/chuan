//! `exec`：在沙盒里执行命令或脚本。

use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ExecOpts, ExecutionEnv, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

/// 两条执行 lane 共用一份实现：
/// - `exec`：无网络（默认），沙盒内任意代码；
/// - `lan_exec`：有网卡，专门调试内网设备（打印机/网关/HA），自带联网环境。
pub struct ExecTool {
    lan: bool,
    /// 联网 lane 自带环境；普通 lane 用 `ctx.env`
    env: Option<Arc<dyn ExecutionEnv>>,
}

impl ExecTool {
    pub fn sandboxed() -> Self {
        Self { lan: false, env: None }
    }

    pub fn lan(env: Arc<dyn ExecutionEnv>) -> Self {
        Self { lan: true, env: Some(env) }
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn spec(&self) -> ToolSpec {
        let (name, description) = if self.lan {
            (
                "lan_exec",
                "执行需要访问网络的命令，用于调试内网设备（打印机 631、路由器管理页、Home Assistant 等）。\
**注意**：这条 lane 保留网卡，所以局域网和公网都可达；看不到宿主的 /home、/var 与凭据，\
只有 `/ws` 可写。命令会逐条经主人确认。只用来做网络请求/探测，不要在命令行里写敏感内容。",
            )
        } else {
            (
                "exec",
                "在隔离沙盒里执行命令。**一次调用完成一整件事**：把多条命令写进一个脚本，\
不要为了每一步单独调用（每次启动沙盒约 30ms）。默认无网络。\
可见路径只有：`/ws`（工作区，可写）、`/inbox`（用户投递的文件，只读）。\
宿主机的 /tmp、/home、/var 一律不可见——如果用户提到这些路径下的文件，要说\"沙盒内不可见\"，\
不要说\"文件不存在\"。",
            )
        };
        ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "argv": { "type": "array", "items": { "type": "string" } },
                    "script": { "type": "string", "description": "shell 脚本，等价于 sh -lc" },
                    "timeout_ms": { "type": "integer" }
                },
                "oneOf": [{ "required": ["argv"] }, { "required": ["script"] }]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 任意命令：默认按外部副作用处理，崩溃后不自动重跑。
        EffectClass::External
    }

    fn domain(&self) -> harness_kernel::Domain {
        if self.lan {
            harness_kernel::Domain::Lan
        } else {
            harness_kernel::Domain::Sandbox
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let argv: Vec<String> = if let Some(script) = args.get("script").and_then(|v| v.as_str()) {
            vec!["sh".into(), "-lc".into(), script.to_owned()]
        } else if let Some(arr) = args.get("argv").and_then(|v| v.as_array()) {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        } else {
            return Err(ToolError::new(
                harness_kernel::ErrorCode::InvalidArgs,
                "需要 argv 或 script",
                false,
            ));
        };
        if argv.is_empty() {
            return Err(ToolError::new(
                harness_kernel::ErrorCode::InvalidArgs,
                "argv 为空",
                false,
            ));
        }

        let opts = ExecOpts {
            timeout_ms: args.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(15_000),
            cwd: None,
            env: Vec::new(),
            stdin: None,
        };

        let env: &dyn ExecutionEnv = match &self.env {
            Some(e) => &**e,
            None => ctx.env,
        };
        let out = env
            .exec(&argv, opts, ctx.cancel.clone())
            .await
            .map_err(|e| {
                let retryable = matches!(e, harness_kernel::EnvError::Timeout(_));
                ToolError::new(harness_kernel::ErrorCode::Unavailable, e.to_string(), retryable)
            })?;

        let mut text = String::new();
        text.push_str(&format!("exit={}\n", out.code.map(|c| c.to_string()).unwrap_or("signal".into())));
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
