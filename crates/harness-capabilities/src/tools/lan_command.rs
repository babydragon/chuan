//! `lan_command`：执行一条**命令条目**（`DECISIONS.md` D31）。
//!
//! 与 `lan_exec` 的区别是结构性的：
//! - 只能跑 `commands.d/` 里写好的 argv，**参数域封闭**，不经 shell；
//! - 命中即免审批（条目本身就是主人的一次性批准），参数越界直接 `Blocked`；
//! - 每次执行写一行审计（条目名 + 解析后 argv + 指纹 + exit + 耗时）。
//!
//! 它不做领域语义：打印/查墨量是条目里的数据，不是这个工具里的分支。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    ActorRole, Block, EffectClass, EntryCheck, ErrorCode, ExecOpts, ExecutionEnv, Tool, ToolCtx,
    ToolError, ToolOutput, ToolSpec,
};

use crate::commands::{now_ms, CommandCache, Who};

pub struct LanCommandTool {
    cache: Arc<CommandCache>,
    env: Arc<dyn ExecutionEnv>,
    /// 宿主侧工作区根：只用于校验 `file` 参数，传给沙盒的仍是 `/ws/...`
    ws: PathBuf,
    /// `lan-access.jsonl`
    log: PathBuf,
}

impl LanCommandTool {
    pub fn new(
        cache: Arc<CommandCache>,
        env: Arc<dyn ExecutionEnv>,
        workspace: impl Into<PathBuf>,
        log: impl Into<PathBuf>,
    ) -> Self {
        Self { cache, env, ws: workspace.into(), log: log.into() }
    }

    /// `(name, args)`；格式不对时给出可读原因（会被转成 `Blocked`）。
    fn split(args: &serde_json::Value) -> Result<(String, serde_json::Value), String> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "缺少 name（要跑哪一条命令条目）".to_owned())?;
        let a = args.get("args").cloned().unwrap_or(serde_json::json!({}));
        if !a.is_object() {
            return Err("args 必须是对象（例如 {\"media\":\"A4\",\"file\":\"/ws/a.pdf\"}）".into());
        }
        Ok((name.to_owned(), a))
    }

    /// 审计：只记"跑了哪条、解析成什么、结果如何"。失败不影响执行结果。
    fn audit(&self, actor: &str, r: &crate::commands::ResolvedCommand, code: Option<i32>, ms: i64) {
        if let Some(dir) = self.log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let line = serde_json::json!({
            "at": now_ms(),
            "entry": r.name,
            "env": "lan",
            "argv": r.argv,
            "hash": r.hash,
            "code": code,
            "ms": ms,
            "actor": actor,
        });
        use std::io::Write;
        match std::fs::OpenOptions::new().create(true).append(true).open(&self.log) {
            Ok(mut f) => {
                let _ = writeln!(f, "{line}");
            }
            Err(e) => tracing::warn!(error = %e, path = %self.log.display(), "命令条目审计写入失败"),
        }
    }
}

#[async_trait]
impl Tool for LanCommandTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lan_command".into(),
            description: "执行一条**已授权**的内网命令条目（打印、查状态、查墨量这类）。\
系统指令里会列出当前可用条目（名字 + 参数域）——**能命中条目的动作一律用它，不要用 `lan_exec`**。\n\
条目由主人在 `commands.d/` 里写死：固定 argv + 封闭参数域，**不经 shell**，命中即免审批。\
参数必须严格落在条目声明的取值里（枚举 / `/ws` 下的文件 / 区间整数 / 封闭字符）。\
**表外名字或参数越界会被直接挡住并结束本次任务**——不要改用 `lan_exec` 重试，\
也不要换写法猜；缺少的动作用 `capability(action=\"propose_command\")` 提一条提案，\
等批准后再生效。\
注意：这里只能看到 `/ws`（工作区）；要打聊天里的图片，先用 `exec` 把它 `cp` 到 `/ws`。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "条目名，例如 print_file / printer_status" },
                    "args": {
                        "type": "object",
                        "description": "条目声明的参数；没有参数就留空对象",
                        "additionalProperties": true
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 条目里可能有打印这类外部副作用：崩溃后不自动重跑。
        EffectClass::External
    }

    fn domain(&self) -> harness_kernel::Domain {
        harness_kernel::Domain::Lan
    }

    fn tier(&self) -> u8 {
        // 子 agent 不该直接对外设发指令（要打什么由父会话决定）。
        1
    }

    fn command_entry(&self, args: &serde_json::Value, role: ActorRole) -> EntryCheck {
        let (name, a) = match Self::split(args) {
            Ok(v) => v,
            Err(reason) => return EntryCheck::Denied(reason),
        };
        let table = self.cache.current();
        // 这一段会原样出现在用户看到的消息里：只说"做不了"和"哪个动作"，
        // 不写文件名/工具名/申请流程（那是模型该从工具描述里知道的）。
        let Some(entry) = table.get(&name) else {
            return EntryCheck::Denied(format!("「{name}」不在我能做的范围里。"));
        };
        if entry.who == Who::Owner && role != ActorRole::Owner {
            return EntryCheck::Denied(format!("「{name}」目前只有部分账号能用。"));
        }
        match table.resolve(&name, &a, &self.ws) {
            Ok(_) => EntryCheck::Allowed,
            // 具体是哪个参数越界只进日志；给用户一句人话。
            Err(reason) => {
                tracing::info!(entry = %name, %reason, "命令条目参数越界");
                EntryCheck::Denied(format!("「{name}」这次的参数不在允许的范围内。"))
            }
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let (name, a) = Self::split(&args).map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
        // 重新解析一次（表可能刚被改过；门的判定与执行之间不再共享可变状态，
        // 所以以这里解析出的 argv 为准，并把指纹写进审计）。
        let table = self.cache.current();
        let resolved = table
            .resolve(&name, &a, &self.ws)
            .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;

        let opts = ExecOpts {
            timeout_ms: resolved.timeout_ms,
            cwd: None,
            env: Vec::new(),
            stdin: None,
            nice: false,
        };
        let started = std::time::Instant::now();
        let out = self
            .env
            .exec(&resolved.argv, opts, ctx.cancel.clone())
            .await
            .map_err(|e| {
                let retryable = matches!(e, harness_kernel::EnvError::Timeout(_));
                self.audit(ctx.actor, &resolved, None, started.elapsed().as_millis() as i64);
                ToolError::new(ErrorCode::Unavailable, e.to_string(), retryable)
            })?;
        self.audit(ctx.actor, &resolved, out.code, started.elapsed().as_millis() as i64);

        let mut text = format!(
            "条目：{}\n指纹：{}\nexit={}\n",
            resolved.name,
            resolved.hash,
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
