//! 内存桩：用于一致性测试与本地演练。语义必须与 SQLite 实现一致。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness_kernel::{
    ApprovalId, NewApproval, Resolve,
    CallId, ConversationId, ConversationView, Doc, DocId, DocQuery, Event, EventKind, Message,
    MessagePage, NewEvent, ReadTx, Role, RunId, Seq, Store, StoreError, TaskId, TaskRecord,
    Timestamp,
    ToolCallRecord, Tx, UsageRecord, UsageTotal, now_ms,
};

#[derive(Clone, Debug)]
struct EApproval {
    conv: String,
    call: String,
    action: String,
    status: String,
    expires_at: i64,
    card: Option<(String, String)>,
}

#[derive(Clone, Default)]
struct Inner {
    journal: Vec<Event>,
    tool_calls: HashMap<String, harness_kernel::ToolCallRow>,
    docs: HashMap<String, Doc>,
    usage: Vec<(Timestamp, UsageRecord)>,
    tasks: HashMap<String, TaskRecord>,
    task_status: HashMap<String, String>,
    kv: HashMap<String, String>,
    approvals: HashMap<String, EApproval>,
    approval_seq: u64,
}

impl Inner {
    fn next_seq(&self, conv: &str) -> Seq {
        self.journal.iter().filter(|e| e.conv.0 == conv).map(|e| e.seq).max().unwrap_or(0) + 1
    }

    fn push(&mut self, ev: NewEvent) -> Seq {
        if let Some(k) = &ev.dedupe_key {
            if let Some(e) = self
                .journal
                .iter()
                .find(|e| e.conv == ev.conv && e.dedupe_key.as_deref() == Some(k.as_str()))
            {
                return e.seq;
            }
        }
        let seq = self.next_seq(&ev.conv.0);
        self.journal.push(Event {
            conv: ev.conv,
            seq,
            run: ev.run,
            turn: ev.turn,
            kind: ev.kind,
            payload: ev.payload,
            meta: None,
            dedupe_key: ev.dedupe_key,
            created_at: now_ms(),
        });
        seq
    }
}

pub struct EphemeralStore {
    inner: Arc<Mutex<Inner>>,
}

impl Default for EphemeralStore {
    fn default() -> Self {
        Self { inner: Arc::new(Mutex::new(Inner::default())) }
    }
}

impl EphemeralStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for EphemeralStore {
    async fn begin_write(&self) -> Result<Box<dyn Tx>, StoreError> {
        Ok(Box::new(EphemeralTx {
            shared: Arc::clone(&self.inner),
            state: self.inner.lock().unwrap().clone(),
        }))
    }

    async fn begin_read(&self) -> Result<Box<dyn ReadTx>, StoreError> {
        Ok(Box::new(EphemeralRead { state: self.inner.lock().unwrap().clone() }))
    }

    async fn migrate(&self) -> Result<u32, StoreError> {
        Ok(crate::SCHEMA_VERSION)
    }

    async fn open_runs(&self) -> Result<Vec<(ConversationId, RunId)>, StoreError> {
        let s = self.inner.lock().unwrap();
        let mut started: Vec<(ConversationId, RunId)> = Vec::new();
        for e in &s.journal {
            if e.kind == harness_kernel::EventKind::RunStarted
                && !started.iter().any(|(_, r)| r == &e.run)
            {
                started.push((e.conv.clone(), e.run.clone()));
            }
        }
        let finished: Vec<&RunId> = s
            .journal
            .iter()
            .filter(|e| e.kind == harness_kernel::EventKind::RunFinished)
            .map(|e| &e.run)
            .collect();
        Ok(started.into_iter().filter(|(_, r)| !finished.contains(&r)).collect())
    }

    async fn put_task(&self, t: TaskRecord, status: &str) -> Result<(), StoreError> {
        let mut s = self.inner.lock().unwrap();
        s.task_status.insert(t.id.0.clone(), status.to_owned());
        s.tasks.insert(t.id.0.clone(), t);
        Ok(())
    }

    async fn claim_due(&self, now: Timestamp, limit: u32) -> Result<Vec<TaskRecord>, StoreError> {
        let mut s = self.inner.lock().unwrap();
        let mut ids: Vec<String> = s
            .tasks
            .values()
            .filter(|t| {
                s.task_status.get(&t.id.0).map(|st| st == "pending").unwrap_or(false)
                    && t.due_at <= now
            })
            .map(|t| t.id.0.clone())
            .collect();
        ids.sort_by_key(|id| s.tasks.get(id).map(|t| t.due_at).unwrap_or(0));
        ids.truncate(limit as usize);
        for id in &ids {
            s.task_status.insert(id.clone(), "running".to_owned());
        }
        Ok(ids.iter().filter_map(|id| s.tasks.get(id).cloned()).collect())
    }

    async fn list_tasks(&self, kind: &str) -> Result<Vec<TaskRecord>, StoreError> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<TaskRecord> = s
            .tasks
            .values()
            .filter(|t| {
                t.kind == kind
                    && s.task_status.get(&t.id.0).map(|st| st == "pending").unwrap_or(false)
            })
            .cloned()
            .collect();
        out.sort_by_key(|t| t.due_at);
        Ok(out)
    }

    async fn rearm_task(
        &self,
        id: &TaskId,
        due_at: Timestamp,
        status: &str,
    ) -> Result<(), StoreError> {
        let mut s = self.inner.lock().unwrap();
        if let Some(t) = s.tasks.get_mut(&id.0) {
            t.due_at = due_at;
        }
        s.task_status.insert(id.0.clone(), status.to_owned());
        Ok(())
    }

    async fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.inner.lock().unwrap().kv.get(key).cloned())
    }

    async fn kv_set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        tx.kv_set(key, value).await?;
        tx.commit().await
    }

    async fn delete_doc(&self, id: &DocId) -> Result<bool, StoreError> {
        Ok(self.inner.lock().unwrap().docs.remove(&id.0).is_some())
    }

    async fn create_approval(&self, a: NewApproval) -> Result<ApprovalId, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.approval_seq += 1;
        let id = format!("appr-{}-{}", now_ms(), inner.approval_seq);
        inner.approvals.insert(
            id.clone(),
            EApproval {
                conv: a.conv.0.clone(),
                call: a.call.0.clone(),
                action: a.action,
                status: "pending".into(),
                expires_at: now_ms() + (a.ttl_s as i64) * 1000,
                card: None,
            },
        );
        Ok(ApprovalId(id))
    }

    async fn attach_approval_card(
        &self,
        id: &ApprovalId,
        chat: &str,
        msg: &harness_kernel::MsgRef,
    ) -> Result<(), StoreError> {
        if let Some(a) = self.inner.lock().unwrap().approvals.get_mut(&id.0) {
            a.card = Some((chat.to_owned(), msg.0.clone()));
        }
        Ok(())
    }

    async fn resolve_approval(
        &self,
        id: &ApprovalId,
        approved: bool,
        _by: &str,
    ) -> Result<Resolve, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.approvals.get_mut(&id.0) {
            None => Ok(Resolve::NotFound),
            Some(a) if a.status != "pending" => Ok(Resolve::AlreadyResolved),
            Some(a) => {
                a.status = if approved { "approved".into() } else { "denied".into() };
                Ok(Resolve::Applied)
            }
        }
    }

    async fn approval_card(
        &self,
        id: &ApprovalId,
    ) -> Result<Option<(String, harness_kernel::MsgRef)>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .approvals
            .get(&id.0)
            .and_then(|a| a.card.clone())
            .map(|(c, m)| (c, harness_kernel::MsgRef(m))))
    }

    async fn expire_approvals(&self, now: i64) -> Result<u32, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let mut n = 0;
        for a in inner.approvals.values_mut() {
            if a.status == "pending" && a.expires_at <= now {
                a.status = "expired".into();
                n += 1;
            }
        }
        Ok(n)
    }

    async fn recover(&self) -> Result<u32, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let mut n = 0;
        for row in inner.tool_calls.values_mut() {
            if row.status == "intent" || row.status == "dispatched" {
                row.status = "abandoned:restart".into();
                n += 1;
            }
        }
        Ok(n)
    }
}

struct EphemeralTx {
    shared: Arc<Mutex<Inner>>,
    state: Inner,
}

#[async_trait]
impl Tx for EphemeralTx {
    async fn append(&mut self, ev: NewEvent) -> Result<Seq, StoreError> {
        Ok(self.state.push(ev))
    }

    async fn append_many(&mut self, evs: Vec<NewEvent>) -> Result<Vec<Seq>, StoreError> {
        Ok(evs.into_iter().map(|e| self.state.push(e)).collect())
    }

    async fn open_tool_calls(
        &mut self,
        conv: ConversationId,
        run: harness_kernel::RunId,
        turn: Option<harness_kernel::TurnId>,
        calls: Vec<ToolCallRecord>,
    ) -> Result<(), StoreError> {
        for c in calls {
            self.state.tool_calls.insert(
                c.call.0.clone(),
                harness_kernel::ToolCallRow {
                    call: c.call.clone(), name: c.name.clone(), effect: c.effect.clone(),
                    idem_key: c.idem_key.clone(), status: "intent".into(), preview: None,
                },
            );
            self.state.push(NewEvent {
                conv: conv.clone(),
                run: run.clone(),
                turn: turn.clone(),
                kind: EventKind::ToolCallStarted,
                payload: serde_json::json!({
                    "call_id": c.call.0, "name": c.name, "args": c.args,
                    "effect": format!("{:?}", c.effect), "idem_key": c.idem_key,
                }),
                dedupe_key: None,
            });
        }
        Ok(())
    }

    async fn mark_dispatched(&mut self, call: CallId) -> Result<(), StoreError> {
        if let Some(r) = self.state.tool_calls.get_mut(&call.0) { r.status = "dispatched".into(); }
        Ok(())
    }

    async fn close_tool_call(
        &mut self,
        call: CallId,
        status: &str,
        _preview: String,
    ) -> Result<(), StoreError> {
        if let Some(r) = self.state.tool_calls.get_mut(&call.0) {
            r.status = "finished".into();
            r.preview = Some(status.to_string());
        }
        Ok(())
    }

    async fn abandon_tool_call(&mut self, call: CallId, reason: &str) -> Result<(), StoreError> {
        if let Some(r) = self.state.tool_calls.get_mut(&call.0) {
            r.status = format!("abandoned:{reason}");
        }
        Ok(())
    }

    async fn upsert_doc(&mut self, _conv: ConversationId, _run: harness_kernel::RunId, doc: Doc) -> Result<(), StoreError> {
        self.state.docs.insert(doc.id.0.clone(), doc);
        Ok(())
    }

    async fn record_usage(&mut self, _conv: ConversationId, _run: harness_kernel::RunId, u: UsageRecord) -> Result<(), StoreError> {
        self.state.usage.push((now_ms(), u));
        Ok(())
    }

    async fn kv_set(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.state.kv.insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        let mut guard = self.shared.lock().unwrap();
        *guard = self.state;
        Ok(())
    }
}

struct EphemeralRead {
    state: Inner,
}

fn to_message(e: &Event) -> Option<Message> {
    let role = match e.kind {
        EventKind::RunStarted => Role::User,
        EventKind::MsgAssistant => Role::Assistant,
        EventKind::MsgToolResult => Role::ToolResult,
        EventKind::MsgSynthetic => Role::Synthetic,
        _ => return None,
    };
    Some(Message { seq: e.seq, turn: e.turn.clone(), role, payload: e.payload.clone() })
}

#[async_trait]
impl ReadTx for EphemeralRead {
    async fn view(&mut self, conv: ConversationId) -> Result<ConversationView, StoreError> {
        let mut msgs: Vec<&Event> = self.state.journal.iter().filter(|e| e.conv == conv).collect();
        msgs.sort_by_key(|e| e.seq);
        Ok(ConversationView {
            conv,
            run: msgs.last().and_then(|e| {
                if e.kind == EventKind::RunFinished { None } else { Some(e.run.clone()) }
            }),
            messages: msgs.iter().filter_map(|e| to_message(e)).collect(),
            last_seq: msgs.last().map(|e| e.seq).unwrap_or(0),
            summary: None,
        })
    }

    async fn messages(
        &mut self,
        conv: ConversationId,
        from: Seq,
        max_turns: u16,
    ) -> Result<MessagePage, StoreError> {
        let is_boundary = |k: &EventKind| matches!(k, EventKind::RunStarted | EventKind::TurnStarted);
        let anchor = self
            .state
            .journal
            .iter()
            .filter(|e| e.conv == conv && is_boundary(&e.kind) && e.seq >= from)
            .map(|e| e.seq)
            .min()
            .unwrap_or(from);
        let mut starts: Vec<Seq> = self
            .state
            .journal
            .iter()
            .filter(|e| e.conv == conv && e.kind == EventKind::TurnStarted && e.seq >= anchor)
            .map(|e| e.seq)
            .collect();
        starts.sort_unstable();
        let cut = if starts.len() > max_turns as usize {
            Some(starts[max_turns as usize])
        } else {
            None
        };
        let mut msgs: Vec<Message> = self
            .state
            .journal
            .iter()
            .filter(|e| e.conv == conv && e.seq >= anchor)
            .filter(|e| matches!(e.kind, EventKind::RunStarted | EventKind::MsgAssistant | EventKind::MsgToolResult | EventKind::MsgSynthetic))
            .filter(|e| cut.map(|c| e.seq < c).unwrap_or(true))
            .filter_map(to_message)
            .collect();
        msgs.sort_by_key(|m| m.seq);
        Ok(MessagePage { msgs, next: cut, truncated: cut.is_some() })
    }

    async fn get_doc(&mut self, id: DocId) -> Result<Option<Doc>, StoreError> {
        Ok(self.state.docs.get(&id.0).cloned())
    }

    async fn list_docs(&mut self, q: DocQuery) -> Result<Vec<Doc>, StoreError> {
        let limit = if q.limit == 0 { usize::MAX } else { q.limit };
        Ok(self
            .state
            .docs
            .values()
            .filter(|d| q.scopes.is_empty() || q.scopes.contains(&d.scope))
            .filter(|d| q.kinds.is_empty() || q.kinds.contains(&d.kind))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn usage_total(&mut self, since: Timestamp) -> Result<UsageTotal, StoreError> {
        let mut t = UsageTotal::default();
        for (at, u) in &self.state.usage {
            if *at >= since {
                t.prompt_tokens += u.prompt_tokens as u64;
                t.completion_tokens += u.completion_tokens as u64;
                t.cost_est += u.cost_est;
            }
        }
        Ok(t)
    }

    async fn task(&mut self, id: TaskId) -> Result<Option<TaskRecord>, StoreError> {
        Ok(self.state.tasks.get(&id.0).cloned())
    }

    async fn tool_calls(&mut self, _conv: ConversationId) -> Result<Vec<harness_kernel::ToolCallRow>, StoreError> {
        Ok(self.state.tool_calls.values().cloned().collect())
    }
}
