//! 失败提示的补发：通道恢复后能自己把"刚才没送到"的几句话送给用户。
//!
//! 背景：通道挂掉时，连"出错了"的提示也发不出去（走同一条发送路径）。
//! 2026-11-26 的 502 事故里，用户不翻日志就完全不知道那一轮发生了什么。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness_kernel::{
    notice, Block, Caps, Channel, ChannelError, ChannelId, EditOutcome, Inbound, MsgRef, View,
};
use harness_kernel::ids::now_ms;
use harness_store::EphemeralStore;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Recorder {
    sent: Mutex<Vec<String>>,
    down: Mutex<bool>,
}

fn text_of(view: &View) -> String {
    view.blocks
        .iter()
        .filter_map(|b| match b {
            Block::Paragraph(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Channel for Recorder {
    fn id(&self) -> ChannelId {
        ChannelId("fake".into())
    }
    fn caps(&self) -> Caps {
        Caps { edit: false, buttons: false, markdown: false, max_text: 4096, min_edit_interval_ms: 0 }
    }
    async fn run(
        &self,
        _tx: tokio::sync::mpsc::Sender<Inbound>,
        _cancel: CancellationToken,
    ) -> Result<(), ChannelError> {
        Ok(())
    }
    async fn deliver(&self, _c: &str, view: View) -> Result<MsgRef, ChannelError> {
        if *self.down.lock().unwrap() {
            return Err(ChannelError::Transient("Bad Gateway".into()));
        }
        self.sent.lock().unwrap().push(text_of(&view));
        Ok(MsgRef("m1".into()))
    }
    async fn update(&self, _c: &str, _m: &MsgRef, _v: View) -> Result<EditOutcome, ChannelError> {
        Ok(EditOutcome::Edited)
    }
    async fn ack(&self, _cb: &str, _text: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

fn channels(ch: Arc<Recorder>) -> HashMap<String, Arc<dyn Channel>> {
    let mut m: HashMap<String, Arc<dyn Channel>> = HashMap::new();
    m.insert("fake".into(), ch);
    m
}

#[tokio::test]
async fn notice_is_delivered_once_then_removed() {
    let store = EphemeralStore::new();
    assert!(notice::enqueue(&store, "fake", "1000", "没处理完").await);

    let ch = Arc::new(Recorder::default());
    let n = notice::drain(&store, &channels(Arc::clone(&ch)), None, now_ms()).await;
    assert_eq!(n, 1);
    assert_eq!(ch.sent.lock().unwrap().as_slice(), ["没处理完"]);
    assert!(notice::list(&store).await.is_empty(), "送到后必须删除，避免重复补发");
}

#[tokio::test]
async fn notice_survives_failure_and_counts_attempts() {
    let store = EphemeralStore::new();
    notice::enqueue(&store, "fake", "1000", "没处理完").await;

    let ch = Arc::new(Recorder::default());
    *ch.down.lock().unwrap() = true;
    let n = notice::drain(&store, &channels(Arc::clone(&ch)), None, now_ms()).await;
    assert_eq!(n, 0);

    let left = notice::list(&store).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].attempts, 1, "失败要留下次数，供过期判断");

    *ch.down.lock().unwrap() = false;
    assert_eq!(notice::drain(&store, &channels(ch), None, now_ms()).await, 1);
    assert!(notice::list(&store).await.is_empty());
}

#[tokio::test]
async fn duplicate_enqueue_keeps_only_the_latest() {
    let store = EphemeralStore::new();
    notice::enqueue(&store, "fake", "1000", "旧的").await;
    notice::enqueue(&store, "fake", "1000", "新的").await;

    let items = notice::list(&store).await;
    assert_eq!(items.len(), 1, "同一会话只留最新一条，避免刷屏");
    assert_eq!(items[0].text, "新的");
}

#[tokio::test]
async fn drain_can_be_scoped_to_one_conversation() {
    let store = EphemeralStore::new();
    notice::enqueue(&store, "fake", "1000", "A").await;
    notice::enqueue(&store, "fake", "2000", "B").await;

    let ch = Arc::new(Recorder::default());
    let n = notice::drain(&store, &channels(Arc::clone(&ch)), Some(("fake", "1000")), now_ms()).await;
    assert_eq!(n, 1);
    assert_eq!(ch.sent.lock().unwrap().as_slice(), ["A"]);
    assert_eq!(notice::list(&store).await.len(), 1, "别的会话不该被顺手发掉");
}

#[tokio::test]
async fn restart_notice_uses_the_conversation_from_the_run_id() {
    let store = EphemeralStore::new();
    notice::enqueue_restart(&store, &harness_kernel::ids::ConversationId("fake:1000:3".into())).await;

    let items = notice::list(&store).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].channel, "fake");
    assert_eq!(items[0].conversation, "1000");
    assert_eq!(items[0].text, harness_kernel::copy::RESTART_INTERRUPTED);
}

#[tokio::test]
async fn unparseable_conversation_is_skipped() {
    let store = EphemeralStore::new();
    notice::enqueue_restart(&store, &harness_kernel::ids::ConversationId("__reminders__".into())).await;
    assert!(notice::list(&store).await.is_empty(), "认不出的会话宁可少发，也不能发错人");
}
