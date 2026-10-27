//! `exec`：在沙盒里执行命令或脚本。

use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ExecOpts, ExecutionEnv, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

use crate::proxy::ProxyConfig;

/// 两条执行 lane 共用一份实现：
/// - `exec`：无网络（默认），沙盒内任意代码；
/// - `lan_exec`：有网卡，专门调试内网设备（打印机/网关/HA），自带联网环境。
pub struct ExecTool {
    lan: bool,
    proxy: ProxyConfig,
    /// 联网 lane 自带环境；普通 lane 用 `ctx.env`
    env: Option<Arc<dyn ExecutionEnv>>,
}

impl ExecTool {
    pub fn sandboxed() -> Self {
        Self { lan: false, proxy: ProxyConfig::default(), env: None }
    }

    pub fn lan(env: Arc<dyn ExecutionEnv>) -> Self {
        Self { lan: true, proxy: ProxyConfig::default(), env: Some(env) }
    }

    /// 代理策略：`proxy=true`（或 mode=always）时把 http_proxy/https_proxy/no_proxy 注入沙盒。
    pub fn with_proxy(mut self, proxy: ProxyConfig) -> Self {
        self.proxy = proxy;
        self
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
只有 `/ws` 可写。命令会逐条经主人确认。只用来做网络请求/探测，不要在命令行里写敏感内容。\
访问境外站点时传 `proxy=true`：本 lane 里 `127.0.0.1` 就是宿主，能直接连宿主上的代理。",
            )
        } else {
            (
                "exec",
                "在隔离沙盒里执行命令。**一次调用完成一整件事**：把多条命令写进一个脚本，\
不要为了每一步单独调用（每次启动沙盒约 30ms）。\
**没有网络**（自己的 netns，只有 lo），因此也**没有 proxy 参数**——代理在这个 lane 里连不上；\
要出网请用 `lan_exec`（可传 `proxy=true`）或 `web_fetch`。\
可见路径只有：`/ws`（工作区，可写）、`/inbox`（用户投递的文件，只读）。\
宿主机的 /tmp、/home、/var 一律不可见——如果用户提到这些路径下的文件，要说\"沙盒内不可见\"，\
不要说\"文件不存在\"。",
            )
        };
        let mut props = serde_json::json!({
            "argv": { "type": "array", "items": { "type": "string" } },
            "script": { "type": "string", "description": "shell 脚本，等价于 sh -lc" },
            "timeout_ms": { "type": "integer" },
        });
        // 代理只对联网 lane 有意义：离线 lane 的 netns 里没有网卡，注入 http_proxy
        // 只会让脚本去连一个不存在的端口（现场表现：exit=7 连不上代理）。
        if self.lan {
            props["proxy"] = serde_json::json!({
                "type": "boolean",
                "description": "走宿主代理（本 lane 里 127.0.0.1 即宿主）；境外站点用 true，国内直连更快"
            });
        }
        ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": props,
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

        // 只有联网 lane 才可能用到代理；离线 lane 注入了也只会误导脚本
        let env = if self.lan {
            let want_proxy = args.get("proxy").and_then(|v| v.as_bool()).unwrap_or(false);
            self.proxy.sandbox_env(want_proxy)
        } else {
            Vec::new()
        };
        let opts = ExecOpts {
            timeout_ms: args.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(15_000),
            cwd: None,
            env,
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
