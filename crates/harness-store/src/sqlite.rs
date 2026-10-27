//! SQLite 实现：单写者线程 + 读连接。
//!
//! - 写：一个专用线程持有唯一写连接；`Tx` 是远端句柄，每个操作同步回执。
//! - 读：每次 `begin_read` 开一个只读连接（WAL 下并发安全）。
//! - `seq` 在写事务内用 `MAX(seq)+1` 分配。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use harness_kernel::{
    ApprovalId, CallId, ConversationId, ConversationView, Doc, DocId, DocOp, DocQuery, EventKind,
    NewApproval, Resolve,
    Message, MessagePage, NewEvent, ReadTx, Role, RunId, Seq, Store, StoreError, TaskId,
    TaskRecord, Timestamp, ToolCallRecord, TurnId, Tx, UsageRecord, UsageTotal, now_ms,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use tokio::sync::{mpsc, oneshot};

use crate::schema::{MIGRATIONS, SCHEMA_VERSION};

pub struct SqliteStore {
    path: PathBuf,
    cmd: mpsc::Sender<Cmd>,
    version: u32,
}

enum Cmd {
    Begin(oneshot::Sender<Result<(), StoreError>>),
    Op(Op, oneshot::Sender<Result<OpResult, StoreError>>),
    Commit(oneshot::Sender<Result<(), StoreError>>),
    Rollback,
    Rebuild(oneshot::Sender<Result<(), StoreError>>),
    Recover(oneshot::Sender<Result<u32, StoreError>>),
    Backup(std::path::PathBuf, oneshot::Sender<Result<(), StoreError>>),
}

enum Op {
    Append(NewEvent),
    OpenToolCalls(Vec<ToolCallRecord>, ConversationId, RunId, Option<TurnId>),
    MarkDispatched(CallId),
    CloseToolCall(CallId, String, String),
    AbandonToolCall(CallId, String),
    UpsertDoc(ConversationId, RunId, Doc),
    RecordUsage(ConversationId, RunId, UsageRecord),
    KvSet(String, String),
    CreateApproval(NewApproval),
    AttachCard(ApprovalId, String, String),
    ResolveApproval(ApprovalId, bool, String),
    ExpireApprovals(i64),
    DeleteDoc(DocId),
    PutTask(TaskRecord, String),
    OpenRuns,
    ClaimDue(i64, u32),
    ListTasks(String),
    Rearm(TaskId, i64, String),
}

enum OpResult {
    Seq(Seq),
    Unit,
    Approval(ApprovalId),
    Resolved(Resolve),
    Count(u32),
    Tasks(Vec<TaskRecord>),
    Runs(Vec<(ConversationId, RunId)>),
}

fn map_err(e: rusqlite::Error) -> StoreError {
    StoreError::Unavailable(e.to_string())
}

impl SqliteStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path).map_err(map_err)?;
        configure(&conn)?;
        let before: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(map_err)?;
        if before > 0 && before < SCHEMA_VERSION as i64 {
            let bak = path.with_extension(format!("pre-migrate-v{before}.db"));
            conn.execute("VACUUM INTO ?1", params![bak.to_string_lossy()])
                .map_err(map_err)?;
            eprintln!("pre-migrate backup: {}", bak.display());
        }
        let version = migrate(&conn)?;
        let (cmd, rx) = mpsc::channel(64);
        let mut writer = conn;
        std::thread::Builder::new()
            .name("chuan-store-writer".into())
            .spawn(move || writer_loop(&mut writer, rx))
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        Ok(Self { path, cmd, version })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 备份到指定路径（`VACUUM INTO`，一致性快照）。
    pub async fn backup_to(&self, path: std::path::PathBuf) -> Result<(), StoreError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Backup(path, tx))
            .await
            .map_err(|_| StoreError::Unavailable("writer gone".into()))?;
        rx.await.map_err(|_| StoreError::Unavailable("writer gone".into()))?
    }

    /// 从 journal 重建全部投影表（验收用，不是日常路径）。
    pub async fn rebuild(&self) -> Result<(), StoreError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Rebuild(tx))
            .await
            .map_err(|_| StoreError::Unavailable("writer gone".into()))?;
        rx.await.map_err(|_| StoreError::Unavailable("writer gone".into()))?
    }

    async fn op(&self, op: Op) -> Result<OpResult, StoreError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Op(op, tx))
            .await
            .map_err(|_| StoreError::Unavailable("writer gone".into()))?;
        rx.await.map_err(|_| StoreError::Unavailable("writer gone".into()))?
    }
}

fn configure(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;
         PRAGMA cache_size=-8000;
         PRAGMA journal_size_limit=33554432;
         PRAGMA foreign_keys=ON;",
    )
    .map_err(map_err)
}

fn migrate(conn: &Connection) -> Result<u32, StoreError> {
    let mut current: i64 =
        conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(map_err)?;
    for (version, sql) in MIGRATIONS {
        if *version > current {
            conn.execute_batch(sql).map_err(map_err)?;
            conn.execute_batch(&format!("PRAGMA user_version={version}")).map_err(map_err)?;
            current = *version;
        }
    }
    Ok(current as u32)
}

fn writer_loop(conn: &mut Connection, mut rx: mpsc::Receiver<Cmd>) {
    let mut in_tx = false;
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Cmd::Begin(reply) => {
                let res = if in_tx {
                    Err(StoreError::Busy { retry_after_ms: 50 })
                } else {
                    match conn.execute_batch("BEGIN IMMEDIATE") {
                        Ok(()) => {
                            in_tx = true;
                            Ok(())
                        }
                        Err(e) => Err(map_err(e)),
                    }
                };
                let _ = reply.send(res);
            }
            Cmd::Op(op, reply) => {
                // 事务内：由调用方 commit；事务外（Store 级便捷方法）：自动开/提交
                let res = if in_tx {
                    apply(conn, op)
                } else {
                    match conn.execute_batch("BEGIN IMMEDIATE") {
                        Err(e) => Err(map_err(e)),
                        Ok(()) => {
                            let r = apply(conn, op);
                            if r.is_ok() {
                                let _ = conn.execute_batch("COMMIT");
                            } else {
                                let _ = conn.execute_batch("ROLLBACK");
                            }
                            r
                        }
                    }
                };
                let _ = reply.send(res);
            }
            Cmd::Commit(reply) => {
                let res = if in_tx {
                    in_tx = false;
                    conn.execute_batch("COMMIT").map_err(map_err)
                } else {
                    Err(StoreError::Conflict)
                };
                let _ = reply.send(res);
            }
            Cmd::Rollback => {
                if in_tx {
                    let _ = conn.execute_batch("ROLLBACK");
                    in_tx = false;
                }
            }
            Cmd::Rebuild(reply) => {
                let _ = reply.send(rebuild(conn));
            }
            Cmd::Recover(reply) => {
                let _ = reply.send(recover_open_calls(conn));
            }
            Cmd::Backup(path, reply) => {
                let res = conn
                    .execute("VACUUM INTO ?1", params![path.to_string_lossy()])
                    .map(|_| ())
                    .map_err(map_err);
                let _ = reply.send(res);
            }
        }
    }
}

fn apply(conn: &Connection, op: Op) -> Result<OpResult, StoreError> {
    match op {
        Op::Append(ev) => Ok(OpResult::Seq(append(conn, &ev)?)),
        Op::OpenToolCalls(calls, conv, run, turn) => {
            for c in calls {
                conn.execute(
                    "INSERT OR REPLACE INTO tool_calls
                     (call_id, conv_id, run_id, name, args, effect, idem_key, status)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,'intent')",
                    params![
                        c.call.0,
                        conv.0,
                        run.0,
                        c.name,
                        c.args.to_string(),
                        effect_str(&c.effect),
                        c.idem_key
                    ],
                )
                .map_err(map_err)?;
                append(
                    conn,
                    &NewEvent {
                        conv: conv.clone(),
                        run: run.clone(),
                        turn: turn.clone(),
                        kind: EventKind::ToolCallStarted,
                        payload: serde_json::json!({
                            "call_id": c.call.0, "name": c.name, "args": c.args,
                            "effect": effect_str(&c.effect), "idem_key": c.idem_key,
                        }),
                        dedupe_key: None,
                    },
                )?;
            }
            Ok(OpResult::Unit)
        }
        Op::MarkDispatched(call) => {
            conn.execute(
                "UPDATE tool_calls SET status='dispatched', dispatched_at=?2 WHERE call_id=?1",
                params![call.0, now_ms()],
            )
            .map_err(map_err)?;
            append_event_by_call(conn, &call, EventKind::ToolCallDispatched, serde_json::json!({}))?;
            Ok(OpResult::Unit)
        }
        Op::CloseToolCall(call, status, preview) => {
            conn.execute(
                "UPDATE tool_calls SET status='finished', finished_at=?2, preview=?3 WHERE call_id=?1",
                params![call.0, now_ms(), preview],
            )
            .map_err(map_err)?;
            append_event_by_call(
                conn,
                &call,
                EventKind::ToolCallFinished,
                serde_json::json!({ "status": status, "preview": preview }),
            )?;
            Ok(OpResult::Unit)
        }
        Op::AbandonToolCall(call, reason) => {
            conn.execute(
                "UPDATE tool_calls SET status='abandoned', finished_at=?2 WHERE call_id=?1",
                params![call.0, now_ms()],
            )
            .map_err(map_err)?;
            append_event_by_call(
                conn,
                &call,
                EventKind::ToolCallAbandoned,
                serde_json::json!({ "reason": reason }),
            )?;
            Ok(OpResult::Unit)
        }
        Op::UpsertDoc(conv, run, doc) => {
            conn.execute(
                "INSERT INTO docs (id, kind, scope, payload, version, valid_from, valid_to)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(id) DO UPDATE SET
                   kind=excluded.kind, scope=excluded.scope, payload=excluded.payload,
                   version=excluded.version, valid_from=excluded.valid_from,
                   valid_to=excluded.valid_to",
                params![
                    doc.id.0,
                    doc.kind,
                    doc.scope,
                    doc.payload.to_string(),
                    doc.version,
                    doc.valid_from,
                    doc.valid_to
                ],
            )
            .map_err(map_err)?;
            let fts_text = doc_fts_text(&doc);
            conn.execute("DELETE FROM docs_fts WHERE doc_id=?1", params![doc.id.0])
                .map_err(map_err)?;
            conn.execute(
                "INSERT INTO docs_fts (doc_id, scope, kind, text) VALUES (?1,?2,?3,?4)",
                params![doc.id.0, doc.scope, doc.kind, fts_text],
            )
            .map_err(map_err)?;
            append(
                conn,
                &NewEvent {
                    conv,
                    run,
                    turn: None,
                    kind: EventKind::DocUpserted,
                    payload: serde_json::json!({
                        "id": doc.id.0, "kind": doc.kind, "scope": doc.scope,
                        "payload": doc.payload, "version": doc.version,
                        "valid_from": doc.valid_from, "valid_to": doc.valid_to,
                        "op": "upsert"
                    }),
                    dedupe_key: None,
                },
            )?;
            Ok(OpResult::Unit)
        }
        Op::CreateApproval(a) => {
            let id = format!("appr-{}-{}", now_ms(), rand_suffix());
            let expires = now_ms() + (a.ttl_s as i64) * 1000;
            conn.execute(
                "INSERT INTO approvals
                 (id, conv_id, call_id, action, scope, status, expires_at, created_at)
                 VALUES (?1,?2,?3,?4,?5,'pending',?6,?7)",
                params![id, a.conv.0, a.call.0, a.action, a.scope, expires, now_ms()],
            )
            .map_err(map_err)?;
            let run = run_of_call(conn, &a.call).unwrap_or_else(|| RunId("approval".into()));
            append(
                conn,
                &NewEvent {
                    conv: a.conv.clone(),
                    run,
                    turn: None,
                    kind: EventKind::ApprovalRequested,
                    payload: serde_json::json!({
                        "approval_id": id, "call_id": a.call.0,
                        "action": a.action, "scope": a.scope, "ttl_s": a.ttl_s,
                    }),
                    dedupe_key: None,
                },
            )?;
            Ok(OpResult::Approval(ApprovalId(id)))
        }
        Op::AttachCard(id, chat, msg) => {
            conn.execute(
                "UPDATE approvals SET card_chat=?2, card_msg=?3 WHERE id=?1",
                params![id.0, chat, msg],
            )
            .map_err(map_err)?;
            Ok(OpResult::Unit)
        }
        Op::ResolveApproval(id, approved, by) => {
            let row: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT status, conv_id, action FROM approvals WHERE id=?1",
                    params![id.0],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(map_err)?;
            let Some((status, conv, action)) = row else {
                return Ok(OpResult::Resolved(Resolve::NotFound));
            };
            if status != "pending" {
                return Ok(OpResult::Resolved(Resolve::AlreadyResolved));
            }
            let new_status = if approved { "approved" } else { "denied" };
            conn.execute(
                "UPDATE approvals SET status=?2, resolved_at=?3 WHERE id=?1",
                params![id.0, new_status, now_ms()],
            )
            .map_err(map_err)?;
            append(
                conn,
                &NewEvent {
                    conv: ConversationId(conv),
                    run: RunId("approval".into()),
                    turn: None,
                    kind: EventKind::ApprovalResolved,
                    payload: serde_json::json!({
                        "approval_id": id.0, "decision": new_status, "by": by, "action": action,
                    }),
                    dedupe_key: None,
                },
            )?;
            Ok(OpResult::Resolved(Resolve::Applied))
        }
        Op::DeleteDoc(id) => {
            let n = conn
                .execute("DELETE FROM docs WHERE id=?1", params![id.0])
                .map_err(map_err)?;
            conn.execute("DELETE FROM docs_fts WHERE doc_id=?1", params![id.0])
                .map_err(map_err)?;
            if n > 0 {
                append(
                    conn,
                    &NewEvent {
                        conv: ConversationId("__docs__".into()),
                        run: RunId("__docs__".into()),
                        turn: None,
                        kind: EventKind::DocUpserted,
                        payload: serde_json::json!({ "id": id.0, "op": "expire" }),
                        dedupe_key: None,
                    },
                )?;
            }
            Ok(OpResult::Count(n as u32))
        }
        Op::ExpireApprovals(now) => {
            let n = conn
                .execute(
                    "UPDATE approvals SET status='expired', resolved_at=?1
                     WHERE status='pending' AND expires_at<=?1",
                    params![now],
                )
                .map_err(map_err)?;
            Ok(OpResult::Count(n as u32))
        }
        Op::OpenRuns => {
            let mut stmt = conn
                .prepare(
                    "SELECT conv_id, run_id FROM journal
                     WHERE kind='run.started'
                       AND run_id NOT IN (SELECT run_id FROM journal WHERE kind='run.finished')
                     GROUP BY run_id",
                )
                .map_err(map_err)?;
            let rows = stmt
                .query_map([], |r| Ok((ConversationId(r.get(0)?), RunId(r.get(1)?))))
                .map_err(map_err)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(map_err)?);
            }
            Ok(OpResult::Runs(out))
        }
        Op::PutTask(t, status) => {
            conn.execute(
                "INSERT INTO tasks (id, kind, due_at, payload, status) VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(id) DO UPDATE SET kind=excluded.kind, due_at=excluded.due_at,
                   payload=excluded.payload, status=excluded.status",
                params![t.id.0, t.kind, t.due_at, t.payload.to_string(), status],
            )
            .map_err(map_err)?;
            Ok(OpResult::Unit)
        }
        Op::ClaimDue(now, limit) => {
            let mut rows: Vec<TaskRecord> = Vec::new();
            {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, kind, due_at, payload FROM tasks
                         WHERE status='pending' AND due_at<=?1 ORDER BY due_at LIMIT ?2",
                    )
                    .map_err(map_err)?;
                let iter = stmt
                    .query_map(params![now, limit], |r| {
                        let payload: String = r.get(3)?;
                        Ok(TaskRecord {
                            id: TaskId(r.get(0)?),
                            kind: r.get(1)?,
                            due_at: r.get(2)?,
                            payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                        })
                    })
                    .map_err(map_err)?;
                for row in iter {
                    rows.push(row.map_err(map_err)?);
                }
            }
            for t in &rows {
                conn.execute("UPDATE tasks SET status='running' WHERE id=?1", params![t.id.0])
                    .map_err(map_err)?;
            }
            Ok(OpResult::Tasks(rows))
        }
        Op::ListTasks(kind) => {
            let mut stmt = conn
                .prepare(
                    "SELECT id, kind, due_at, payload FROM tasks
                     WHERE kind=?1 AND status='pending' ORDER BY due_at",
                )
                .map_err(map_err)?;
            let iter = stmt
                .query_map(params![kind], |r| {
                    let payload: String = r.get(3)?;
                    Ok(TaskRecord {
                        id: TaskId(r.get(0)?),
                        kind: r.get(1)?,
                        due_at: r.get(2)?,
                        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                    })
                })
                .map_err(map_err)?;
            let mut out = Vec::new();
            for row in iter {
                out.push(row.map_err(map_err)?);
            }
            Ok(OpResult::Tasks(out))
        }
        Op::Rearm(id, due_at, status) => {
            conn.execute(
                "UPDATE tasks SET due_at=?2, status=?3 WHERE id=?1",
                params![id.0, due_at, status],
            )
            .map_err(map_err)?;
            Ok(OpResult::Unit)
        }
        Op::KvSet(k, v) => {
            conn.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![k, v],
            )
            .map_err(map_err)?;
            Ok(OpResult::Unit)
        }
        Op::RecordUsage(conv, run, u) => {
            conn.execute(
                "INSERT INTO usage_rollup (at, model, prompt_tokens, completion_tokens, cost_est)
                 VALUES (?1,?2,?3,?4,?5)",
                params![now_ms(), u.model, u.prompt_tokens, u.completion_tokens, u.cost_est],
            )
            .map_err(map_err)?;
            // 账本必须同源，否则 rebuild 归零。
            append(
                conn,
                &NewEvent {
                    conv,
                    run,
                    turn: None,
                    kind: EventKind::UsageRecorded,
                    payload: serde_json::json!({
                        "model": u.model,
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                        "cost_est": u.cost_est,
                    }),
                    dedupe_key: None,
                },
            )?;
            Ok(OpResult::Unit)
        }
    }
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn run_of_call(conn: &Connection, call: &CallId) -> Option<RunId> {
    conn.query_row(
        "SELECT run_id FROM tool_calls WHERE call_id=?1",
        params![call.0],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(RunId)
}

fn doc_fts_text(doc: &Doc) -> String {
    let mut parts = Vec::new();
    for key in ["subject", "predicate", "value", "title", "text"] {
        if let Some(v) = doc.payload.get(key).and_then(|v| v.as_str()) {
            parts.push(v.to_owned());
        }
    }
    if parts.is_empty() {
        parts.push(doc.payload.to_string());
    }
    format!("{} {} {}", doc.kind, doc.scope, parts.join(" "))
}

fn effect_str(e: &harness_kernel::EffectClass) -> String {
    match e {
        harness_kernel::EffectClass::Pure => "pure".into(),
        harness_kernel::EffectClass::Idempotent { .. } => "idempotent".into(),
        harness_kernel::EffectClass::External => "external".into(),
    }
}

fn append(conn: &Connection, ev: &NewEvent) -> Result<Seq, StoreError> {
    let conv = ev.conv.0.clone();
    if let Some(k) = &ev.dedupe_key {
        if let Some(seq) = conn
            .query_row(
                "SELECT seq FROM journal WHERE conv_id=?1 AND dedupe_key=?2",
                params![conv, k],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_err)?
        {
            return Ok(seq);
        }
    }
    let seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM journal WHERE conv_id=?1",
            params![conv],
            |r| r.get(0),
        )
        .map_err(map_err)?;
    conn.execute(
        "INSERT INTO journal
           (conv_id, seq, run_id, turn_id, kind, payload, meta, dedupe_key, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,NULL,?7,?8)",
        params![
            conv,
            seq,
            ev.run.0,
            ev.turn.as_ref().map(|t| t.0.clone()),
            ev.kind.as_str(),
            ev.payload.to_string(),
            ev.dedupe_key,
            now_ms()
        ],
    )
    .map_err(map_err)?;
    Ok(seq)
}

fn append_event_by_call(
    conn: &Connection,
    call: &CallId,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<(), StoreError> {
    // 事件必须自带 call_id，否则 rebuild 无法把它折叠回投影。
    let mut payload = payload;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("call_id".into(), serde_json::json!(call.0));
    }
    let row = conn
        .query_row(
            "SELECT conv_id, run_id, name FROM tool_calls WHERE call_id=?1",
            params![call.0],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()
        .map_err(map_err)?;
    if let Some((conv, run, _name)) = row {
        append(
            conn,
            &NewEvent {
                conv: ConversationId(conv),
                run: RunId(run),
                turn: None,
                kind,
                payload,
                dedupe_key: None,
            },
        )?;
    }
    Ok(())
}

/// 启动恢复：把遗留的 intent/dispatched 调用标为 abandoned，并给对应 run 收尾。
fn recover_open_calls(conn: &Connection) -> Result<u32, StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(map_err)?;
    let mut runs: Vec<(String, String)> = Vec::new();
    let mut n = 0u32;
    {
        let mut stmt = conn
            .prepare(
                "SELECT call_id, conv_id, run_id FROM tool_calls
                 WHERE status IN ('intent','dispatched')",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;

        for (call, conv, run) in rows {
            conn.execute(
                "UPDATE tool_calls SET status='abandoned', finished_at=?2 WHERE call_id=?1",
                params![call, now_ms()],
            )
            .map_err(map_err)?;
            append(
                conn,
                &NewEvent {
                    conv: ConversationId(conv.clone()),
                    run: RunId(run.clone()),
                    turn: None,
                    kind: EventKind::ToolCallAbandoned,
                    payload: serde_json::json!({ "call_id": call, "reason": "restart" }),
                    dedupe_key: None,
                },
            )?;
            if !runs.iter().any(|(c, r)| c == &conv && r == &run) {
                runs.push((conv, run));
            }
            n += 1;
        }
    }
    for (conv, run) in runs {
        append(
            conn,
            &NewEvent {
                conv: ConversationId(conv),
                run: RunId(run.clone()),
                turn: None,
                kind: EventKind::RunFinished,
                payload: serde_json::json!({ "status": "interrupted" }),
                dedupe_key: Some(format!("recover-{run}")),
            },
        )?;
    }
    conn.execute_batch("COMMIT").map_err(map_err)?;
    Ok(n)
}

fn rebuild(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch("DELETE FROM tool_calls; DELETE FROM docs; DELETE FROM usage_rollup;")
        .map_err(map_err)?;
    let mut stmt = conn
        .prepare("SELECT kind, payload, created_at, conv_id, run_id FROM journal ORDER BY created_at, conv_id, seq")
        .map_err(map_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(map_err)?;
    for row in rows {
        let (kind, payload, at, conv_id, run_id) = row.map_err(map_err)?;
        let Some(kind) = EventKind::parse(&kind) else { continue };
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
        match kind {
            EventKind::ToolCallStarted => {
                conn.execute(
                    "INSERT OR REPLACE INTO tool_calls
                     (call_id, conv_id, run_id, name, args, effect, idem_key, status)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,'intent')",
                    params![
                        v["call_id"].as_str().unwrap_or_default(),
                        conv_id,
                        run_id,
                        v["name"].as_str().unwrap_or_default(),
                        v["args"].to_string(),
                        v["effect"].as_str().unwrap_or("pure"),
                        v["idem_key"].as_str().unwrap_or_default()
                    ],
                )
                .map_err(map_err)?;
            }
            EventKind::ToolCallDispatched => {
                conn.execute(
                    "UPDATE tool_calls SET status='dispatched', dispatched_at=?2 WHERE call_id=?1",
                    params![v["call_id"].as_str().unwrap_or_default(), at],
                )
                .map_err(map_err)?;
            }
            EventKind::ToolCallFinished => {
                conn.execute(
                    "UPDATE tool_calls SET status='finished', finished_at=?2 WHERE call_id=?1",
                    params![v["call_id"].as_str().unwrap_or_default(), at],
                )
                .map_err(map_err)?;
            }
            EventKind::ToolCallAbandoned => {
                conn.execute(
                    "UPDATE tool_calls SET status='abandoned', finished_at=?2 WHERE call_id=?1",
                    params![v["call_id"].as_str().unwrap_or_default(), at],
                )
                .map_err(map_err)?;
            }
            EventKind::DocUpserted => {
                if v["op"].as_str() == Some("expire") {
                    conn.execute(
                        "DELETE FROM docs WHERE id=?1",
                        params![v["id"].as_str().unwrap_or_default()],
                    )
                    .map_err(map_err)?;
                    continue;
                }
                conn.execute(
                    "INSERT INTO docs (id, kind, scope, payload, version, valid_from, valid_to)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(id) DO UPDATE SET payload=excluded.payload, version=excluded.version",
                    params![
                        v["id"].as_str().unwrap_or_default(),
                        v["kind"].as_str().unwrap_or_default(),
                        v["scope"].as_str().unwrap_or_default(),
                        v["payload"].to_string(),
                        v["version"].as_u64().unwrap_or(1) as i64,
                        v["valid_from"].as_i64().unwrap_or(at),
                        v["valid_to"].as_i64()
                    ],
                )
                .map_err(map_err)?;
            }
            EventKind::UsageRecorded => {
                conn.execute(
                    "INSERT INTO usage_rollup (at, model, prompt_tokens, completion_tokens, cost_est)
                     VALUES (?1,?2,?3,?4,?5)",
                    params![
                        at,
                        v["model"].as_str().unwrap_or("unknown"),
                        v["prompt_tokens"].as_u64().unwrap_or(0) as i64,
                        v["completion_tokens"].as_u64().unwrap_or(0) as i64,
                        v["cost_est"].as_f64().unwrap_or(0.0)
                    ],
                )
                .map_err(map_err)?;
            }
            _ => {}
        }
    }
    // 重建 FTS 投影
    conn.execute("DELETE FROM docs_fts", []).map_err(map_err)?;
    conn.execute(
        "INSERT INTO docs_fts (doc_id, scope, kind, text)
         SELECT id, scope, kind, payload FROM docs",
        [],
    )
    .map_err(map_err)?;
    Ok(())
}

#[async_trait]
impl Store for SqliteStore {
    async fn begin_write(&self) -> Result<Box<dyn Tx>, StoreError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Begin(tx))
            .await
            .map_err(|_| StoreError::Unavailable("writer gone".into()))?;
        rx.await.map_err(|_| StoreError::Unavailable("writer gone".into()))??;
        Ok(Box::new(SqliteTx { cmd: self.cmd.clone(), open: true }))
    }

    async fn begin_read(&self) -> Result<Box<dyn ReadTx>, StoreError> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_err)?;
        conn.execute_batch("PRAGMA busy_timeout=5000;").map_err(map_err)?;
        Ok(Box::new(SqliteRead { conn }))
    }

    async fn migrate(&self) -> Result<u32, StoreError> {
        Ok(self.version)
    }

    async fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_err)?;
        conn.query_row("SELECT value FROM kv WHERE key=?1", params![key], |r| r.get(0))
            .optional()
            .map_err(map_err)
    }

    async fn kv_set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        tx.kv_set(key, value).await?;
        tx.commit().await
    }

    async fn open_runs(&self) -> Result<Vec<(ConversationId, RunId)>, StoreError> {
        match self.op(Op::OpenRuns).await? {
            OpResult::Runs(v) => Ok(v),
            _ => Ok(Vec::new()),
        }
    }

    async fn put_task(&self, t: TaskRecord, status: &str) -> Result<(), StoreError> {
        self.op(Op::PutTask(t, status.to_owned())).await.map(|_| ())
    }

    async fn claim_due(&self, now: Timestamp, limit: u32) -> Result<Vec<TaskRecord>, StoreError> {
        match self.op(Op::ClaimDue(now, limit)).await? {
            OpResult::Tasks(t) => Ok(t),
            _ => Ok(Vec::new()),
        }
    }

    async fn list_tasks(&self, kind: &str) -> Result<Vec<TaskRecord>, StoreError> {
        match self.op(Op::ListTasks(kind.to_owned())).await? {
            OpResult::Tasks(t) => Ok(t),
            _ => Ok(Vec::new()),
        }
    }

    async fn rearm_task(
        &self,
        id: &TaskId,
        due_at: Timestamp,
        status: &str,
    ) -> Result<(), StoreError> {
        self.op(Op::Rearm(id.clone(), due_at, status.to_owned())).await.map(|_| ())
    }

    async fn delete_doc(&self, id: &DocId) -> Result<bool, StoreError> {
        match self.op(Op::DeleteDoc(id.clone())).await? {
            OpResult::Count(n) => Ok(n > 0),
            _ => Ok(false),
        }
    }

    async fn create_approval(&self, a: NewApproval) -> Result<ApprovalId, StoreError> {
        match self.op(Op::CreateApproval(a)).await? {
            OpResult::Approval(id) => Ok(id),
            _ => Err(StoreError::Conflict),
        }
    }

    async fn attach_approval_card(
        &self,
        id: &ApprovalId,
        chat: &str,
        msg: &harness_kernel::MsgRef,
    ) -> Result<(), StoreError> {
        self.op(Op::AttachCard(id.clone(), chat.to_owned(), msg.0.clone())).await?;
        Ok(())
    }

    async fn resolve_approval(
        &self,
        id: &ApprovalId,
        approved: bool,
        by: &str,
    ) -> Result<Resolve, StoreError> {
        match self.op(Op::ResolveApproval(id.clone(), approved, by.to_owned())).await? {
            OpResult::Resolved(r) => Ok(r),
            _ => Err(StoreError::Conflict),
        }
    }

    async fn approval_card(
        &self,
        id: &ApprovalId,
    ) -> Result<Option<(String, harness_kernel::MsgRef)>, StoreError> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_err)?;
        let row: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT card_chat, card_msg FROM approvals WHERE id=?1",
                params![id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(map_err)?;
        Ok(row.and_then(|(c, m)| match (c, m) {
            (Some(c), Some(m)) => Some((c, harness_kernel::MsgRef(m))),
            _ => None,
        }))
    }

    async fn expire_approvals(&self, now: i64) -> Result<u32, StoreError> {
        match self.op(Op::ExpireApprovals(now)).await? {
            OpResult::Count(n) => Ok(n),
            _ => Err(StoreError::Conflict),
        }
    }

    async fn recover(&self) -> Result<u32, StoreError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Recover(tx))
            .await
            .map_err(|_| StoreError::Unavailable("writer gone".into()))?;
        rx.await.map_err(|_| StoreError::Unavailable("writer gone".into()))?
    }
}

struct SqliteTx {
    cmd: mpsc::Sender<Cmd>,
    open: bool,
}

impl Drop for SqliteTx {
    fn drop(&mut self) {
        if self.open {
            let _ = self.cmd.try_send(Cmd::Rollback);
        }
    }
}

#[async_trait]
impl Tx for SqliteTx {
    async fn append(&mut self, ev: NewEvent) -> Result<Seq, StoreError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Op(Op::Append(ev), tx))
            .await
            .map_err(|_| StoreError::Unavailable("writer gone".into()))?;
        match rx.await.map_err(|_| StoreError::Unavailable("writer gone".into()))?? {
            OpResult::Seq(s) => Ok(s),
            OpResult::Unit
            | OpResult::Approval(_)
            | OpResult::Resolved(_)
            | OpResult::Count(_)
            | OpResult::Tasks(_)
            | OpResult::Runs(_) => Err(StoreError::Conflict),
        }
    }

    async fn append_many(&mut self, evs: Vec<NewEvent>) -> Result<Vec<Seq>, StoreError> {
        let mut out = Vec::with_capacity(evs.len());
        for ev in evs {
            out.push(self.append(ev).await?);
        }
        Ok(out)
    }

    async fn open_tool_calls(
        &mut self,
        conv: ConversationId,
        run: RunId,
        turn: Option<TurnId>,
        calls: Vec<ToolCallRecord>,
    ) -> Result<(), StoreError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Op(Op::OpenToolCalls(calls, conv, run, turn), tx))
            .await
            .map_err(|_| StoreError::Unavailable("writer gone".into()))?;
        rx.await.map_err(|_| StoreError::Unavailable("writer gone".into()))??;
        Ok(())
    }

    async fn mark_dispatched(&mut self, call: CallId) -> Result<(), StoreError> {
        self.unit(Op::MarkDispatched(call)).await
    }

    async fn close_tool_call(
        &mut self,
        call: CallId,
        status: &str,
        preview: String,
    ) -> Result<(), StoreError> {
        self.unit(Op::CloseToolCall(call, status.into(), preview)).await
    }

    async fn abandon_tool_call(&mut self, call: CallId, reason: &str) -> Result<(), StoreError> {
        self.unit(Op::AbandonToolCall(call, reason.into())).await
    }

    async fn upsert_doc(&mut self, conv: ConversationId, run: RunId, doc: Doc) -> Result<(), StoreError> {
        self.unit(Op::UpsertDoc(conv, run, doc)).await
    }

    async fn record_usage(&mut self, conv: ConversationId, run: RunId, u: UsageRecord) -> Result<(), StoreError> {
        self.unit(Op::RecordUsage(conv, run, u)).await
    }

    async fn kv_set(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.unit(Op::KvSet(key.to_owned(), value.to_owned())).await
    }

    async fn commit(mut self: Box<Self>) -> Result<(), StoreError> {
        let (tx, rx) = oneshot::channel();
        self.open = false;
        self.cmd
            .send(Cmd::Commit(tx))
            .await
            .map_err(|_| StoreError::Unavailable("writer gone".into()))?;
        rx.await.map_err(|_| StoreError::Unavailable("writer gone".into()))?
    }
}

impl SqliteTx {
    async fn unit(&mut self, op: Op) -> Result<(), StoreError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Op(op, tx))
            .await
            .map_err(|_| StoreError::Unavailable("writer gone".into()))?;
        rx.await.map_err(|_| StoreError::Unavailable("writer gone".into()))??;
        Ok(())
    }
}

struct SqliteRead {
    conn: Connection,
}

#[async_trait]
impl ReadTx for SqliteRead {
    async fn view(&mut self, conv: ConversationId) -> Result<ConversationView, StoreError> {
        let page = self.messages(conv.clone(), 0, 50).await?;
        let last: Option<(Seq, String, String)> = self
            .conn
            .query_row(
                "SELECT seq, run_id, kind FROM journal WHERE conv_id=?1 ORDER BY seq DESC LIMIT 1",
                params![conv.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(map_err)?;
        Ok(ConversationView {
            conv,
            run: last.as_ref().and_then(|(_, run, kind)| {
                if kind == "run.finished" { None } else { Some(RunId(run.clone())) }
            }),
            messages: page.msgs,
            last_seq: last.map(|(s, _, _)| s).unwrap_or(0),
            summary: None,
        })
    }

    async fn messages(
        &mut self,
        conv: ConversationId,
        from: Seq,
        max_turns: u16,
    ) -> Result<MessagePage, StoreError> {
        let anchor: Seq = self
            .conn
            .query_row(
                "SELECT COALESCE(MIN(seq), ?2) FROM journal
                 WHERE conv_id=?1 AND kind IN ('run.started','turn.started') AND seq>=?2",
                params![conv.0, from],
                |r| r.get(0),
            )
            .map_err(map_err)?;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq FROM journal WHERE conv_id=?1 AND kind='turn.started' AND seq>=?2
                 ORDER BY seq LIMIT ?3",
            )
            .map_err(map_err)?;
        let starts: Vec<Seq> = stmt
            .query_map(params![conv.0, anchor, (max_turns as i64) + 1], |r| r.get(0))
            .map_err(map_err)?
            .collect::<Result<_, _>>()
            .map_err(map_err)?;
        let cut = if starts.len() > max_turns as usize { Some(starts[max_turns as usize]) } else { None };

        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, turn_id, kind, payload FROM journal
                 WHERE conv_id=?1 AND seq>=?2 AND seq<?3
                   AND kind IN ('run.started','msg.assistant','msg.tool_result','msg.synthetic')
                 ORDER BY seq",
            )
            .map_err(map_err)?;
        let msgs: Vec<Message> = stmt
            .query_map(params![conv.0, anchor, cut.unwrap_or(i64::MAX)], |r| {
                let kind: String = r.get(2)?;
                let role = match kind.as_str() {
                    "run.started" => Role::User,
                    "msg.assistant" => Role::Assistant,
                    "msg.tool_result" => Role::ToolResult,
                    _ => Role::Synthetic,
                };
                let payload: String = r.get(3)?;
                Ok(Message {
                    seq: r.get(0)?,
                    turn: r.get::<_, Option<String>>(1)?.map(TurnId),
                    role,
                    payload: serde_json::from_str(&payload).unwrap_or_default(),
                })
            })
            .map_err(map_err)?
            .collect::<Result<_, _>>()
            .map_err(map_err)?;

        Ok(MessagePage { msgs, next: cut, truncated: cut.is_some() })
    }

    async fn get_doc(&mut self, id: DocId) -> Result<Option<Doc>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, kind, scope, payload, version, valid_from, valid_to FROM docs WHERE id=?1",
                params![id.0],
                |r| {
                    Ok(Doc {
                        id: DocId(r.get(0)?),
                        kind: r.get(1)?,
                        scope: r.get(2)?,
                        payload: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                        version: r.get::<_, i64>(4)? as u64,
                        valid_from: r.get(5)?,
                        valid_to: r.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(map_err)?;
        Ok(row)
    }

    async fn list_docs(&mut self, q: DocQuery) -> Result<Vec<Doc>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, scope, payload, version, valid_from, valid_to FROM docs
                 WHERE (?1='' OR scope=?1) AND (?2='' OR kind=?2)
                 ORDER BY valid_from DESC LIMIT ?3",
            )
            .map_err(map_err)?;
        let scope = q.scopes.first().cloned().unwrap_or_default();
        let kind = q.kinds.first().cloned().unwrap_or_default();
        let limit = if q.limit == 0 { -1 } else { q.limit as i64 };
        let docs = stmt
            .query_map(params![scope, kind, limit], |r| {
                Ok(Doc {
                    id: DocId(r.get(0)?),
                    kind: r.get(1)?,
                    scope: r.get(2)?,
                    payload: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    version: r.get::<_, i64>(4)? as u64,
                    valid_from: r.get(5)?,
                    valid_to: r.get(6)?,
                })
            })
            .map_err(map_err)?
            .collect::<Result<_, _>>()
            .map_err(map_err)?;
        Ok(docs)
    }

    async fn usage_total(&mut self, since: Timestamp) -> Result<UsageTotal, StoreError> {
        let (p, c, cost): (i64, i64, f64) = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0),
                        COALESCE(SUM(cost_est),0) FROM usage_rollup WHERE at>=?1",
                params![since],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(map_err)?;
        Ok(UsageTotal {
            prompt_tokens: p as u64,
            completion_tokens: c as u64,
            cost_est: cost,
        })
    }

    async fn tool_calls(&mut self, conv: ConversationId) -> Result<Vec<harness_kernel::ToolCallRow>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT call_id, name, effect, idem_key, status, preview FROM tool_calls
                 WHERE conv_id=?1 ORDER BY call_id",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![conv.0], |r| {
                let effect = match r.get::<_, String>(2)?.as_str() {
                    "external" => harness_kernel::EffectClass::External,
                    "idempotent" => harness_kernel::EffectClass::Idempotent { key: String::new() },
                    _ => harness_kernel::EffectClass::Pure,
                };
                Ok(harness_kernel::ToolCallRow {
                    call: CallId(r.get(0)?),
                    name: r.get(1)?,
                    effect,
                    idem_key: r.get(3)?,
                    status: r.get(4)?,
                    preview: r.get(5)?,
                })
            })
            .map_err(map_err)?
            .collect::<Result<_, _>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    async fn task(&mut self, id: TaskId) -> Result<Option<TaskRecord>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, kind, due_at, payload FROM tasks WHERE id=?1",
                params![id.0],
                |r| {
                    Ok(TaskRecord {
                        id: TaskId(r.get(0)?),
                        kind: r.get(1)?,
                        due_at: r.get(2)?,
                        payload: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    })
                },
            )
            .optional()
            .map_err(map_err)?;
        Ok(row)
    }
}

#[allow(dead_code)]
fn _unused(_: ApprovalId, _: DocOp) {}
