//! 通道循环与渲染落地的离线测试。

use std::sync::Arc;
use std::time::Duration;


use harness_channels::telegram::api::{Chat, Message, Update, User};
use harness_channels::telegram::fake::FakeApi;
use harness_channels::telegram::{MemoryOffsets, OffsetStore, TelegramChannel, TelegramConfig};
use harness_kernel::{
    Block, Caps, Channel, Inbound, InboundKind, RenderAction, Renderer, StreamEvent, TurnRenderer,
    View,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn update(id: i64, user: i64, text: &str, age_s: i64) -> Update {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    Update {
        update_id: id,
        callback_query: None,
        message: Some(Message {
            message_id: id * 10,
            date: now - age_s,
            chat: Chat { id: 1000 },
            from: Some(User { id: user, username: Some("u".into()) }),
            text: Some(text.into()),
            photo: None,
            document: None,
            caption: None,
        }),
    }
}

async fn run_collect(api: Arc<FakeApi>, cfg: TelegramConfig, expect: usize) -> Vec<Inbound> {
    let offsets = Arc::new(MemoryOffsets::default());
    let ch = TelegramChannel::new(Arc::clone(&api), Arc::clone(&offsets) as _, cfg);
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
async fn filters_whitelist_and_stale() {
    let api = Arc::new(FakeApi::new());
    api.push_update(update(1, 42, "你好", 0));       // 允许、新鲜
    api.push_update(update(2, 999, "坏人", 0));      // 非白名单
    api.push_update(update(3, 42, "旧消息", 3600));  // 过期
    api.push_update(update(4, 42, "/status", 0));    // 命令

    let cfg = TelegramConfig {
        allowed_users: vec![42],
        media_dir: std::env::temp_dir().join("chuan-test-media"),
        stale_after_s: 300,
    };
    let got = run_collect(api, cfg, 2).await;

    assert_eq!(got.len(), 2);
    assert_eq!(got[0].text.as_deref(), Some("你好"));
    assert_eq!(got[0].kind, InboundKind::Text);
    assert_eq!(got[1].kind, InboundKind::Command);
    assert!(got.iter().all(|i| i.conversation == "1000"));
}

#[tokio::test]
async fn persists_offset() {
    let api = Arc::new(FakeApi::new());
    api.push_update(update(7, 42, "a", 0));
    let offsets = Arc::new(MemoryOffsets::default());
    let ch = TelegramChannel::new(
        Arc::clone(&api),
        Arc::clone(&offsets) as _,
        TelegramConfig {
            allowed_users: vec![42],
            media_dir: std::env::temp_dir().join("chuan-test-media"),
            stale_after_s: 300,
        },
    );
    let (tx, mut rx) = mpsc::channel(4);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let h = tokio::spawn(async move { ch.run(tx, c2).await });
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), h).await;

    assert_eq!(offsets.get("tg.offset").await, Some(8));
}

#[tokio::test]
async fn survives_rate_limit() {
    let api = Arc::new(FakeApi::new());
    *api.rate_limit_once.lock().unwrap() = Some(10);
    api.push_update(update(1, 42, "after-429", 0));
    let cfg = TelegramConfig {
        allowed_users: vec![42],
        media_dir: std::env::temp_dir().join("chuan-test-media"),
        stale_after_s: 300,
    };
    let got = run_collect(Arc::clone(&api), cfg, 1).await;
    assert_eq!(got.len(), 1, "429 之后应继续轮询");
    assert_eq!(got[0].text.as_deref(), Some("after-429"));
}

#[tokio::test]
async fn delivers_html_and_edits_in_place() {
    let api = Arc::new(FakeApi::new());
    let offsets = Arc::new(MemoryOffsets::default());
    let ch = TelegramChannel::new(Arc::clone(&api), offsets as _, TelegramConfig::default());

    let mut renderer = TurnRenderer::new(Caps {
        edit: true,
        buttons: true,
        markdown: true,
        max_text: 4096,
        min_edit_interval_ms: 1000,
    });

    renderer.push(harness_kernel::RenderEvent::Status("思考中…".into()));
    renderer.push(harness_kernel::RenderEvent::Stream(StreamEvent::TextDelta(
        "**粗** 与 `code`".into(),
    )));
    let action = renderer.flush(0);
    assert!(matches!(action, RenderAction::Send(_)));
    harness_kernel::apply_action(&ch, "1000", &mut renderer, action).await.unwrap();

    {
        let sent = api.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].1.contains("<b>粗</b>"), "应转成 HTML: {}", sent[0].1);
        assert!(sent[0].1.contains("<code>code</code>"));
        assert!(sent[0].1.contains("生成中") || sent[0].1.contains("思考中"));
    }

    renderer.push(harness_kernel::RenderEvent::Stream(StreamEvent::TextDelta("!" .into())));
    let action = renderer.flush(2000);
    assert!(matches!(action, RenderAction::Edit(_)));
    harness_kernel::apply_action(&ch, "1000", &mut renderer, action).await.unwrap();
    assert_eq!(api.edited.lock().unwrap().len(), 1);

    // TooOld → 下一次重新发送
    *api.deny_edit.lock().unwrap() = true;
    renderer.push(harness_kernel::RenderEvent::Stream(StreamEvent::TextDelta("?" .into())));
    let action = renderer.flush(4000);
    harness_kernel::apply_action(&ch, "1000", &mut renderer, action).await.unwrap();
    renderer.push(harness_kernel::RenderEvent::Stream(StreamEvent::TextDelta("x" .into())));
    let action = renderer.flush(8000);
    assert!(matches!(action, RenderAction::Send(_)), "TooOld 后应发新消息");
}

#[tokio::test]
async fn long_final_text_is_split() {
    let api = Arc::new(FakeApi::new());
    let offsets = Arc::new(MemoryOffsets::default());
    let ch = TelegramChannel::new(Arc::clone(&api), offsets as _, TelegramConfig::default());
    let mut renderer = TurnRenderer::new(Caps { max_text: 600, ..Caps::default() });

    renderer.push(harness_kernel::RenderEvent::Stream(StreamEvent::TextDelta(
        "行\n".repeat(400),
    )));
    let actions = renderer.finish(harness_kernel::RunStatus::Done);
    assert!(actions.len() > 1);
    for a in actions {
        harness_kernel::apply_action(&ch, "1000", &mut renderer, a).await.unwrap();
    }
    let sent = api.sent.lock().unwrap();
    assert!(sent.len() > 1);
    assert!(sent.iter().all(|(_, t)| t.chars().count() <= 4096));
}

#[allow(dead_code)]
fn _view(_: View, _: Block) {}

/// 发文件：图片走 sendPhoto，其它走 sendDocument。
#[tokio::test]
async fn sends_files_and_images() {
    use harness_kernel::Channel;
    let api = std::sync::Arc::new(FakeApi::new());
    let ch = TelegramChannel::new(
        std::sync::Arc::clone(&api),
        std::sync::Arc::new(MemoryOffsets::default()),
        TelegramConfig {
            allowed_users: vec![42],
            media_dir: std::env::temp_dir().join("chuan-test-media"),
            stale_after_s: 300,
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("报告.pdf");
    let png = dir.path().join("预览.png");
    std::fs::write(&pdf, b"%PDF-1.4").unwrap();
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();

    ch.send_file("42", &pdf.to_string_lossy(), "报告.pdf", "application/pdf", "看看")
        .await
        .unwrap();
    ch.send_file("42", &png.to_string_lossy(), "预览.png", "image/png", "")
        .await
        .unwrap();

    let files = api.files.lock().unwrap().clone();
    assert_eq!(files, vec!["报告.pdf".to_string(), "预览.png".to_string()]);
}
