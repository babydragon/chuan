//! 同一套一致性测试跑两个实现。新增 backend 必须跑通这里。

use harness_kernel::{
    CallId, ConversationId, Doc, DocId, DocQuery, EffectClass, EventKind, NewEvent, RunId, Store,
    ToolCallRecord, TurnId, UsageRecord, now_ms,
};

use crate::{EphemeralStore, SqliteStore};

fn ev(
    conv: &ConversationId,
    run: &RunId,
    turn: Option<&TurnId>,
    kind: EventKind,
    payload: serde_json::Value,
    dedupe: Option<&str>,
) -> NewEvent {
    NewEvent {
        conv: conv.clone(),
        run: run.clone(),
        turn: turn.cloned(),
        kind,
        payload,
        dedupe_key: dedupe.map(str::to_owned),
    }
}

pub async fn suite<S: Store>(store: &S) {
    let conv = ConversationId::from("c1");
    let run = RunId::from("r1");

    // --- 幂等：同一 dedupe_key 只产生一条 ---
    let mut tx = store.begin_write().await.unwrap();
    let a = tx.append(ev(&conv, &run, None, EventKind::RunStarted, serde_json::json!({"text":"hi"}), Some("u1"))).await.unwrap();
    let b = tx.append(ev(&conv, &run, None, EventKind::RunStarted, serde_json::json!({"text":"hi"}), Some("u1"))).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(a, b, "dedupe_key 必须幂等");

    // --- seq 会话内单调 ---
    let mut tx = store.begin_write().await.unwrap();
    let mut seqs = vec![a];
    for i in 0..3 {
        let turn = TurnId(format!("t{i}"));
        seqs.push(tx.append(ev(&conv, &run, Some(&turn), EventKind::TurnStarted, serde_json::json!({"turn_no":i}), None)).await.unwrap());
        seqs.push(tx.append(ev(&conv, &run, Some(&turn), EventKind::MsgAssistant, serde_json::json!({"text":format!("a{i}")}), None)).await.unwrap());
    }
    tx.commit().await.unwrap();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq 必须严格递增: {seqs:?}");

    // --- 分页不切半轮 ---
    let mut rd = store.begin_read().await.unwrap();
    let p1 = rd.messages(conv.clone(), 0, 2).await.unwrap();
    assert_eq!(p1.msgs.len(), 3, "第一页含用户消息 + 2 轮 assistant");
    assert!(p1.next.is_some(), "还有下一页");
    let p2 = rd.messages(conv.clone(), p1.next.unwrap(), 2).await.unwrap();
    assert_eq!(p2.msgs.len(), 1, "第二页剩 1 轮");
    assert!(p2.next.is_none());

    // --- 工具调用生命周期 + 恢复查询 ---
    let call = CallId::from("call-1");
    let mut tx = store.begin_write().await.unwrap();
    tx.open_tool_calls(
        conv.clone(),
        run.clone(),
        Some(TurnId("t0".into())),
        vec![ToolCallRecord {
            call: call.clone(),
            name: "exec".into(),
            args: serde_json::json!({"argv":["true"]}),
            effect: EffectClass::External,
            idem_key: "k1".into(),
        }],
    )
    .await
    .unwrap();
    tx.mark_dispatched(call.clone()).await.unwrap();
    tx.commit().await.unwrap();

    let mut rd = store.begin_read().await.unwrap();
    let rows = rd.tool_calls(conv.clone()).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "dispatched");

    let mut tx = store.begin_write().await.unwrap();
    tx.close_tool_call(call.clone(), "ok", "done".into()).await.unwrap();
    tx.commit().await.unwrap();

    let mut rd = store.begin_read().await.unwrap();
    assert_eq!(rd.tool_calls(conv.clone()).await.unwrap()[0].status, "finished");

    // --- 审批：CAS + 过期 ---
    let appr = store
        .create_approval(harness_kernel::NewApproval {
            conv: conv.clone(),
            call: call.clone(),
            action: "exec".into(),
            scope: "workspace".into(),
            ttl_s: 60,
        })
        .await
        .unwrap();
    store
        .attach_approval_card(&appr, "chat-1", &harness_kernel::MsgRef("m1".into()))
        .await
        .unwrap();
    assert_eq!(
        store.resolve_approval(&appr, true, "owner").await.unwrap(),
        harness_kernel::Resolve::Applied
    );
    assert_eq!(
        store.resolve_approval(&appr, true, "owner").await.unwrap(),
        harness_kernel::Resolve::AlreadyResolved
    );
    let expiring = store
        .create_approval(harness_kernel::NewApproval {
            conv: conv.clone(),
            call: call.clone(),
            action: "exec".into(),
            scope: "workspace".into(),
            ttl_s: 0,
        })
        .await
        .unwrap();
    let n = store.expire_approvals(now_ms() + 5_000).await.unwrap();
    assert!(n >= 1, "应至少过期 1 条");
    assert_eq!(
        store.resolve_approval(&expiring, true, "owner").await.unwrap(),
        harness_kernel::Resolve::AlreadyResolved
    );

    // --- 文档与计量 ---
    let doc = Doc {
        id: DocId::from("d1"),
        kind: "fact".into(),
        scope: "family".into(),
        payload: serde_json::json!({"printer":"HP"}),
        version: 1,
        valid_from: now_ms(),
        valid_to: None,
    };
    let mut tx = store.begin_write().await.unwrap();
    tx.upsert_doc(conv.clone(), run.clone(), doc).await.unwrap();
    tx.record_usage(
        conv.clone(),
        run.clone(),
        UsageRecord { model: "m".into(), prompt_tokens: 10, completion_tokens: 5, cost_est: 0.001 },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut rd = store.begin_read().await.unwrap();
    let got = rd.get_doc(DocId::from("d1")).await.unwrap().expect("doc 应可读");
    assert_eq!(got.payload["printer"], "HP");
    let total = rd.usage_total(0).await.unwrap();
    assert_eq!(total.prompt_tokens, 10);
    assert!((total.cost_est - 0.001).abs() < 1e-9);
    let listed = rd.list_docs(DocQuery { scopes: vec!["family".into()], kinds: vec![], limit: 10 }).await.unwrap();
    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn ephemeral_conformance() {
    let store = EphemeralStore::new();
    suite(&store).await;
}

#[tokio::test]
async fn sqlite_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("t.db")).await.unwrap();
    suite(&store).await;
}

#[tokio::test]
async fn sqlite_rebuild_matches_live_state() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("t.db")).await.unwrap();
    suite(&store).await;

    let conv = ConversationId::from("c1");
    let before: Vec<(String, String)> = {
        let mut rd = store.begin_read().await.unwrap();
        rd.tool_calls(conv.clone()).await.unwrap().into_iter().map(|r| (r.call.0, r.status)).collect()
    };
    let usage_before = {
        let mut rd = store.begin_read().await.unwrap();
        rd.usage_total(0).await.unwrap().prompt_tokens
    };

    store.rebuild().await.unwrap();

    let after: Vec<(String, String)> = {
        let mut rd = store.begin_read().await.unwrap();
        rd.tool_calls(conv.clone()).await.unwrap().into_iter().map(|r| (r.call.0, r.status)).collect()
    };
    let usage_after = {
        let mut rd = store.begin_read().await.unwrap();
        rd.usage_total(0).await.unwrap().prompt_tokens
    };

    assert_eq!(before, after, "rebuild 后 tool_calls 必须一致");
    assert_eq!(usage_before, usage_after, "rebuild 后 usage 必须一致");
}

#[tokio::test]
async fn fts_finds_facts_and_respects_delete() {
    use crate::{Fts5Retriever, SqliteStore};
    use harness_kernel::{Doc, DocId, Query, Retriever};

    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(SqliteStore::open(dir.path().join("t.db")).await.unwrap());
    let conv = ConversationId::from("c1");
    let run = RunId::from("r1");

    let mut tx = store.begin_write().await.unwrap();
    for (id, subject, pred, value, scope) in [
        ("fact-1", "打印机", "型号", "HP M15", "family"),
        ("fact-2", "女儿", "班级", "三年级二班", "family"),
    ] {
        tx.upsert_doc(
            conv.clone(),
            run.clone(),
            Doc {
                id: DocId(id.into()),
                kind: "fact".into(),
                scope: scope.into(),
                payload: serde_json::json!({"subject":subject,"predicate":pred,"value":value}),
                version: 1,
                valid_from: 0,
                valid_to: None,
            },
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let r = Fts5Retriever::new(std::sync::Arc::clone(&store));
    let hits = r
        .search(Query {
            text: "打印机".into(),
            scopes: vec!["family".into()],
            kinds: vec!["fact".into()],
            k: 5,
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].doc_id.0, "fact-1");

    store.delete_doc(&DocId("fact-1".into())).await.unwrap();
    let hits = r
        .search(Query {
            text: "打印机".into(),
            scopes: vec!["family".into()],
            kinds: vec!["fact".into()],
            k: 5,
        })
        .await
        .unwrap();
    assert!(hits.is_empty(), "删除后不应再命中: {hits:?}");
}
