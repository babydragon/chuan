//! 微信通道的离线测试：白名单/新鲜度/非文本跳过、分片、无令牌、会话失效。

use std::sync::Arc;
use std::time::Duration;

use harness_channels::weixin::api::Item;
use harness_channels::weixin::fake::FakeApi;
use harness_channels::weixin::{Account, WeixinChannel, WeixinConfig};
use harness_kernel::{Block, Channel, Inbound, InboundKind, MsgRef, View};
use harness_channels::offsets::{MemoryOffsets, OffsetStore};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn channel(api: Arc<FakeApi>) -> Arc<WeixinChannel<FakeApi>> {
    channel_with(vec![("u1@im.wechat", "b1@im.bot", api)], Arc::new(MemoryOffsets::default()))
}

fn channel_with(
    accounts: Vec<(&str, &str, Arc<FakeApi>)>,
    offsets: Arc<MemoryOffsets>,
) -> Arc<WeixinChannel<FakeApi>> {
    let accs = accounts
        .into_iter()
        .map(|(user, bot, api)| Account { user_id: user.into(), bot_id: bot.into(), api })
        .collect();
    Arc::new(WeixinChannel::new(
        accs,
        offsets,
        WeixinConfig { stale_after_s: 300, cursor_key: "wx.buf".into() },
    ))
}

async fn collect(ch: Arc<WeixinChannel<FakeApi>>, expect: usize) -> Vec<Inbound> {
    let (tx, mut rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let handle = tokio::spawn(async move { ch.run(tx, c2).await });
    let mut out = Vec::new();
    for _ in 0..expect {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(i)) => out.push(i),
            _ => break,
        }
    }
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    out
}

#[tokio::test]
async fn filters_stale_and_non_text() {
    let api = Arc::new(FakeApi::new());
    let now = now_ms();
    api.push_text(1, "u1@im.wechat", "你好", "ctx-1", now);
    api.push_text(3, "u1@im.wechat", "旧消息", "ctx-old", now - 3_600_000);
    api.push_text(4, "u1@im.wechat", "/status", "ctx-cmd", now);
    // 纯图片：本步跳过
    api.push_items(5, "u1@im.wechat", vec![Item { kind: 2, ..Default::default() }], "ctx-img", now);

    let got = collect(channel(api), 2).await;

    assert_eq!(got.len(), 2, "只该有两条：文本 + 命令");
    assert_eq!(got[0].text.as_deref(), Some("你好"));
    assert_eq!(got[0].kind, InboundKind::Text);
    assert_eq!(got[0].conversation, "u1@im.wechat");
    assert_eq!(got[0].dedupe_key, "1");
    assert_eq!(got[0].msg, Some(MsgRef("1".into())), "入站消息句柄要带上（状态反应用）");
    assert_eq!(got[1].kind, InboundKind::Command);
    assert_eq!(got[1].text.as_deref(), Some("/status"));
}

#[tokio::test]
async fn deliver_chunks_long_text_and_requires_context() {
    let api = Arc::new(FakeApi::new());
    let ch = channel(Arc::clone(&api));

    // 还没有会话令牌：发不出去
    let view = || View {
        shard: 0,
        is_last: true,
        blocks: vec![Block::Paragraph("先说话".into())],
        status: None,
        buttons: vec![],
    };
    assert!(ch.deliver("u1@im.wechat", view()).await.is_err(), "没有 context_token 应报错");

    // 收一条消息拿到令牌
    api.push_text(1, "u1@im.wechat", "在吗", "ctx-1", now_ms());
    let got = collect(Arc::clone(&ch), 1).await;
    assert_eq!(got.len(), 1);

    // 9000 字 → 4000/4000/1000 三条
    let long = View {
        shard: 0,
        is_last: true,
        blocks: vec![Block::Paragraph("字".repeat(9_000))],
        status: None,
        buttons: vec![],
    };
    let last = ch.deliver("u1@im.wechat", long).await.unwrap();
    assert!(!last.0.is_empty(), "应返回最后一条的 message_id");

    let sent = api.sent_texts();
    assert_eq!(sent.len(), 3, "9000 字应分成三条：{sent:?}");
    assert_eq!(sent[0].chars().count(), 4000);
    assert_eq!(sent[1].chars().count(), 4000);
    assert_eq!(sent[2].chars().count(), 1_000);
}

#[tokio::test]
async fn session_expired_stops_the_loop() {
    let api = Arc::new(FakeApi::new());
    *api.fail.lock().unwrap() = Some("expired");
    let ch = channel(Arc::clone(&api));

    let (tx, _rx) = mpsc::channel(4);
    let err = tokio::time::timeout(Duration::from_secs(2), ch.run(tx, CancellationToken::new()))
        .await
        .expect("应该立刻返回而不是继续转圈");
    assert!(err.is_err(), "-14 应作为错误上抛，交给上层提示重新登录");
}

#[tokio::test]
async fn caps_match_weixin_reality() {
    let ch = channel(Arc::new(FakeApi::new()));
    let caps = ch.caps();
    assert!(!caps.edit, "协议不能编辑消息");
    assert!(!caps.buttons, "协议没有按钮");
    assert!(!caps.markdown, "微信不解析 Markdown");
    assert_eq!(caps.max_text, 4000);
}

/// 两个 bot（两个微信号）：各收各的、各发各的，游标也各存各的。
#[tokio::test]
async fn two_accounts_do_not_cross_talk() {
    let api_a = Arc::new(FakeApi::new());
    let api_b = Arc::new(FakeApi::new());
    let now = now_ms();
    api_a.push_text(11, "alice@im.wechat", "我是 A", "ctx-a", now);
    api_b.push_text(22, "bob@im.wechat", "我是 B", "ctx-b", now);

    let offsets = Arc::new(MemoryOffsets::default());
    let ch = channel_with(
        vec![
            ("alice@im.wechat", "botA@im.bot", Arc::clone(&api_a)),
            ("bob@im.wechat", "botB@im.bot", Arc::clone(&api_b)),
        ],
        Arc::clone(&offsets),
    );

    let got = collect(Arc::clone(&ch), 2).await;
    assert_eq!(got.len(), 2, "两个账号的消息都要收到");
    assert!(got.iter().all(|i| i.channel.0 == "weixin"), "通道 id 仍是 weixin");

    // 回复走各自的 token
    let view = || View {
        shard: 0,
        is_last: true,
        blocks: vec![Block::Paragraph("回你".into())],
        status: None,
        buttons: vec![],
    };
    ch.deliver("alice@im.wechat", view()).await.unwrap();
    ch.deliver("bob@im.wechat", view()).await.unwrap();

    assert_eq!(api_a.sent_texts(), vec!["回你".to_string()], "A 的回复只能走 A 的 api");
    assert_eq!(api_b.sent_texts(), vec!["回你".to_string()], "B 的回复只能走 B 的 api");

    // 游标按 bot 分开
    assert!(offsets.get("wx.buf:botA@im.bot").await.is_some(), "A 的游标");
    assert!(offsets.get("wx.buf:botB@im.bot").await.is_some(), "B 的游标");
}

/// 会话在账号簿里找不到 → 明确报错，而不是随便挑一个 bot 发出去。
#[tokio::test]
async fn unknown_conversation_is_rejected() {
    let api = Arc::new(FakeApi::new());
    let ch = channel(api);
    let view = View {
        shard: 0,
        is_last: true,
        blocks: vec![Block::Paragraph("x".into())],
        status: None,
        buttons: vec![],
    };
    let err = ch.deliver("stranger@im.wechat", view).await.unwrap_err();
    assert!(
        format!("{err}").contains("没有绑定这个微信号的 bot"),
        "应明确说没这个账号：{err}"
    );
}

/// "正在输入"：发给对应账号，收尾时显式取消。
#[tokio::test]
async fn typing_goes_to_the_right_account() {
    let api_a = Arc::new(FakeApi::new());
    let api_b = Arc::new(FakeApi::new());
    let ch = channel_with(
        vec![
            ("alice@im.wechat", "botA@im.bot", Arc::clone(&api_a)),
            ("bob@im.wechat", "botB@im.bot", Arc::clone(&api_b)),
        ],
        Arc::new(MemoryOffsets::default()),
    );

    ch.typing("alice@im.wechat").await.unwrap();
    ch.typing_stop("alice@im.wechat").await.unwrap();

    assert_eq!(
        api_a.typings.lock().unwrap().clone(),
        vec![("alice@im.wechat".to_string(), true), ("alice@im.wechat".to_string(), false)],
        "A 的 typing 只走 A 的 api，且收尾要显式关掉"
    );
    assert!(api_b.typings.lock().unwrap().is_empty(), "B 不该收到 A 的 typing");

    // 未知会话：静默跳过，不报错（best-effort）
    ch.typing("stranger@im.wechat").await.unwrap();
}
