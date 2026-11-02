use serde::{Deserialize, Serialize};

use crate::effect::EffectClass;
use crate::ids::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    ToolResult,
    Synthetic,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub seq: Seq,
    pub turn: Option<TurnId>,
    pub role: Role,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct MessagePage {
    pub msgs: Vec<Message>,
    pub next: Option<Seq>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ConversationView {
    pub conv: ConversationId,
    pub run: Option<RunId>,
    pub messages: Vec<Message>,
    pub last_seq: Seq,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocOp {
    Upsert,
    Supersede,
    Expire,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Doc {
    pub id: DocId,
    pub kind: String,
    pub scope: String,
    pub payload: serde_json::Value,
    pub version: u64,
    pub valid_from: Timestamp,
    pub valid_to: Option<Timestamp>,
}

#[derive(Clone, Debug)]
pub struct DocWrite {
    pub id: DocId,
    pub kind: String,
    pub scope: String,
    pub payload: serde_json::Value,
    pub version: u64,
    pub target: Option<DocId>,
    pub op: DocOp,
}

#[derive(Clone, Debug, Default)]
pub struct DocQuery {
    pub scopes: Vec<String>,
    pub kinds: Vec<String>,
    pub limit: usize,
}

/// 审批分两类（`DECISIONS.md` D30 §2）：只有主人能批，区别在"批准后发生什么"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalKind {
    /// 只改数据（命令条目表），不执行动作；请求方不等（run 已 `Blocked`）
    Permission,
    /// 执行那一次动作；短 TTL
    Confirm,
}

impl ApprovalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalKind::Permission => "permission",
            ApprovalKind::Confirm => "confirm",
        }
    }
    pub fn parse(s: &str) -> Self {
        // 旧行没有 kind → 那时确实只有"单次确认"
        if s == "permission" { ApprovalKind::Permission } else { ApprovalKind::Confirm }
    }
}

#[derive(Clone, Debug)]
pub struct NewApproval {
    pub conv: ConversationId,
    pub call: CallId,
    pub action: String,
    pub scope: String,
    pub ttl_s: u64,
    pub kind: ApprovalKind,
    /// 发起者（user id）。审批只对 owner，这个字段用于审计"谁提的"。
    pub requester: String,
    /// `Permission` 提案：命令条目名与正文指纹
    pub entry_name: Option<String>,
    pub entry_hash: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ApprovalInfo {
    pub kind: ApprovalKind,
    pub requester: String,
    pub entry_name: Option<String>,
    pub entry_hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolve {
    Applied,
    AlreadyResolved,
    NotFound,
}

#[derive(Clone, Debug)]
pub struct ApprovalRow {
    pub id: ApprovalId,
    pub conv: ConversationId,
    pub call: CallId,
    pub action: String,
    pub status: String,
    pub expires_at: Timestamp,
}

#[derive(Clone, Debug)]
pub struct ToolCallRow {
    pub call: CallId,
    pub name: String,
    pub effect: EffectClass,
    pub idem_key: String,
    /// intent | dispatched | finished | abandoned
    pub status: String,
    pub preview: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ToolCallRecord {
    pub call: CallId,
    pub name: String,
    pub args: serde_json::Value,
    pub effect: EffectClass,
    pub idem_key: String,
}

#[derive(Clone, Debug)]
pub struct UsageRecord {
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cost_est: f64,
}

#[derive(Clone, Debug, Default)]
pub struct UsageTotal {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_est: f64,
}

impl UsageTotal {
    pub fn tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// 用量按主体拆分：主会话 vs 子 agent（D20）。
#[derive(Clone, Debug, Default)]
pub struct UsageSplit {
    pub main: UsageTotal,
    pub agent: UsageTotal,
}

#[derive(Clone, Debug)]
pub struct TaskRecord {
    pub id: TaskId,
    pub kind: String,
    pub due_at: Timestamp,
    pub payload: serde_json::Value,
}
