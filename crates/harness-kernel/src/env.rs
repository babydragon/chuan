use async_trait::async_trait;

use crate::error::EnvError;

#[derive(Clone, Debug, Default)]
pub struct ExecOpts {
    pub timeout_ms: u64,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// 传给子进程的 stdin（技能用 JSON 参数）
    pub stdin: Option<String>,
    /// 以 nice 19 启动（编译/装包类，D11）。沙盒实现里用 setpriority。
    pub nice: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ExecOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 工具执行的抽象；bwrap 实现放在 capabilities crate。
#[async_trait]
pub trait ExecutionEnv: Send + Sync {
    async fn exec(
        &self,
        argv: &[String],
        opts: ExecOpts,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ExecOutput, EnvError>;
}
