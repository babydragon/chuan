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

/// 越界时缺的是哪一类授权（D22）。结构化拒绝要能让模型直接抄进申请单，
/// 所以这里不用自由文本，而是给枚举 + 精确条目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// 路径不在 `[host.path]` 里（或模式不够：要写却只给了 ro）
    Path,
    /// argv 前缀不在 `[[host_elevated.grant]]` 里
    Argv,
    /// 需要 root，但没有对应的提权授权
    Root,
}

impl GrantKind {
    /// 申请单里 `kind=` 的取值，也是日志字段
    pub fn as_str(&self) -> &'static str {
        match self {
            GrantKind::Path => "host_path",
            GrantKind::Argv => "host_argv",
            GrantKind::Root => "host_root",
        }
    }
}

impl std::fmt::Display for GrantKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// 结构化拒绝：缺什么、缺到什么程度、下一步该做什么。
    /// `hint` 由调用方拼成给人看的句子（含"已授权清单"与 `capability(...)` 调用示例），
    /// 但要保证 `item` 是精确的绝对路径或 argv，模型才能真正写出一份可审的申请单。
    #[error("permission denied ({kind}): {item} — {hint}")]
    NotGranted {
        kind: GrantKind,
        item: String,
        hint: String,
    },
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
