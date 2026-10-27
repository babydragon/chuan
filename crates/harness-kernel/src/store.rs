use async_trait::async_trait;

use crate::error::StoreError;
use crate::event::NewEvent;
use crate::ids::*;
use crate::record::*;

/// 存储端口。实现（SQLite/PG/内存）放在 harness-store。
#[async_trait]
pub trait Store: Send + Sync + 'static {
    async fn begin_write(&self) -> Result<Box<dyn Tx>, StoreError>;
    async fn begin_read(&self) -> Result<Box<dyn ReadTx>, StoreError>;
    async fn migrate(&self) -> Result<u32, StoreError>;

    /// 小键值：offset、会话 epoch、UI 状态。不走 Tx，实现自行保证原子性。
    async fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError>;
    async fn kv_set(&self, key: &str, value: &str) -> Result<(), StoreError>;

    /// 删除一条文档（`/forget` 用）。
    async fn delete_doc(&self, id: &DocId) -> Result<bool, StoreError>;

    // ---- 审批 ----
    async fn create_approval(&self, a: NewApproval) -> Result<ApprovalId, StoreError>;
    async fn attach_approval_card(
        &self,
        id: &ApprovalId,
        chat: &str,
        msg: &MsgRef,
    ) -> Result<(), StoreError>;
    /// CAS：只有 pending 能被 resolve；返回 Applied / AlreadyResolved / NotFound。
    async fn resolve_approval(
        &self,
        id: &ApprovalId,
        approved: bool,
        by: &str,
    ) -> Result<Resolve, StoreError>;
    /// 读审批卡片位置（审批结束后清理用）。
    async fn approval_card(
        &self,
        id: &ApprovalId,
    ) -> Result<Option<(String, MsgRef)>, StoreError>;

    /// 把过期未决的审批标记为 expired，返回条数。
    async fn expire_approvals(&self, now: Timestamp) -> Result<u32, StoreError>;

    /// 有 `run.started` 但没有 `run.finished` 的 run（进程被杀留下的），
    /// 启动时补记 `interrupted`，让账本可读。
    async fn open_runs(&self) -> Result<Vec<(ConversationId, RunId)>, StoreError>;

    // ---- 定时任务 ----
    /// 新建或覆盖一条任务（`status` = pending/done/running）。
    async fn put_task(&self, t: TaskRecord, status: &str) -> Result<(), StoreError>;
    /// 原子领取到期任务：pending 且 due_at<=now 的置为 running 并返回。
    /// 领取在单写者内完成，所以重启/并发不会重复触发。
    async fn claim_due(&self, now: Timestamp, limit: u32) -> Result<Vec<TaskRecord>, StoreError>;
    /// 按 kind 列出未完成任务（`/reminders` 用）。
    async fn list_tasks(&self, kind: &str) -> Result<Vec<TaskRecord>, StoreError>;
    /// 重排下一次到期时间（或标记 done）。
    async fn rearm_task(
        &self,
        id: &TaskId,
        due_at: Timestamp,
        status: &str,
    ) -> Result<(), StoreError>;

    /// 启动恢复：把上次进程遗留的未完成调用标记为 abandoned，返回处理条数。
    /// 返回后账本里不应再有 intent/dispatched 状态。
    async fn recover(&self) -> Result<u32, StoreError>;
}

#[async_trait]
pub trait ReadTx: Send {
    async fn view(&mut self, conv: ConversationId) -> Result<ConversationView, StoreError>;
    async fn messages(
        &mut self,
        conv: ConversationId,
        from: Seq,
        max_turns: u16,
    ) -> Result<MessagePage, StoreError>;
    async fn get_doc(&mut self, id: DocId) -> Result<Option<Doc>, StoreError>;
    async fn list_docs(&mut self, q: DocQuery) -> Result<Vec<Doc>, StoreError>;
    async fn usage_total(&mut self, since: Timestamp) -> Result<UsageTotal, StoreError>;
    async fn task(&mut self, id: TaskId) -> Result<Option<TaskRecord>, StoreError>;
    /// 恢复用：该会话里尚未 finished/abandoned 的调用。
    async fn tool_calls(&mut self, conv: ConversationId) -> Result<Vec<ToolCallRow>, StoreError>;
}

#[async_trait]
pub trait Tx: Send {
    async fn append(&mut self, ev: NewEvent) -> Result<Seq, StoreError>;
    async fn append_many(&mut self, evs: Vec<NewEvent>) -> Result<Vec<Seq>, StoreError>;

    async fn open_tool_calls(
        &mut self,
        conv: ConversationId,
        run: RunId,
        turn: Option<TurnId>,
        calls: Vec<ToolCallRecord>,
    ) -> Result<(), StoreError>;
    async fn mark_dispatched(&mut self, call: CallId) -> Result<(), StoreError>;
    async fn close_tool_call(
        &mut self,
        call: CallId,
        status: &str,
        preview: String,
    ) -> Result<(), StoreError>;
    async fn abandon_tool_call(&mut self, call: CallId, reason: &str) -> Result<(), StoreError>;

    async fn upsert_doc(&mut self, conv: ConversationId, run: RunId, doc: Doc) -> Result<(), StoreError>;
    async fn record_usage(&mut self, conv: ConversationId, run: RunId, u: UsageRecord) -> Result<(), StoreError>;

    /// 键值写入（走同一写事务，保证与其它状态一致）。
    async fn kv_set(&mut self, key: &str, value: &str) -> Result<(), StoreError>;

    async fn commit(self: Box<Self>) -> Result<(), StoreError>;
}
