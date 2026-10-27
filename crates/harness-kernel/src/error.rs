use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("stream interrupted")]
    Interrupted,
    #[error("cancelled")]
    Cancelled,
    #[error("invalid tool call: {0}")]
    InvalidToolCall(String),
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),
    /// 鉴权失败（key 无效/过期）
    #[error("auth failed: {0}")]
    Auth(String),
    /// 额度用尽（5 小时/每周/每月配额、余额不足）——绝不自动重试
    #[error("quota exhausted: {message}")]
    Quota { message: String, retry_after_s: Option<u64> },
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("busy, retry after {retry_after_ms}ms")]
    Busy { retry_after_ms: u32 },
    #[error("conflict")]
    Conflict,
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("not found")]
    NotFound,
}

#[derive(Debug, Error)]
pub enum EnvError {
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
    #[error("timeout after {0}ms")]
    Timeout(u64),
    #[error("io error: {0}")]
    Io(String),
    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidArgs,
    NotPermitted,
    NotFound,
    Timeout,
    Unavailable,
    Internal,
}

#[derive(Debug, Error)]
#[error("{code:?}: {message}")]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ToolError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self { code, message: message.into(), retryable }
    }
}
