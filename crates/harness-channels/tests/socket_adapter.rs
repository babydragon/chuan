//! D44 协议测试：核心监听 UDS，模拟一个适配器完成握手、入站、投递。

use std::sync::Arc;

use harness_channels::socket::{AdapterConfig, SocketChannel};
use harness_kernel::{Block, Caps, Channel, View};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn protocol_handshake_inbound_and_deliver() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("echo.sock");
    let media = dir.path().join("media");
    let ch = Arc::new(
        SocketChannel::bind(
            AdapterConfig::new("echo", sock.clone(), None, Some(media), 20).unwrap(),
            dir.path().join("inbox"),
        )
        .unwrap(),
    );

    let (tx, mut rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let run = {
        let ch = Arc::clone(&ch);
        let cancel = cancel.clone();
        tokio::spawn(async move { ch.run(tx, cancel).await })
    };

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    let caps = serde_json::to_value(Caps::default()).unwrap();
    let hello = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "chuan.hello",
        "params": {
            "protocol": 1,
            "channel": "echo",
            "adapter_version": "test",
            "caps": caps,
        }
    });
    w.write_all(format!("{hello}\n").as_bytes()).await.unwrap();
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    assert!(line.contains("\"ok\":true"), "handshake reply: {line}");

    let inbound = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "chuan.inbound",
        "params": {
            "conversation": "tui",
            "actor": "local",
            "kind": "text",
            "text": "hello",
            "dedupe_key": "d1",
            "attachments": []
        }
    });
    w.write_all(format!("{inbound}\n").as_bytes())
        .await
        .unwrap();

    let inb = rx.recv().await.unwrap();
    assert_eq!(inb.channel.0, "echo");
    assert_eq!(inb.actor, "local");
    assert_eq!(inb.text.as_deref(), Some("hello"));

    let deliver = {
        let ch = Arc::clone(&ch);
        tokio::spawn(async move {
            ch.deliver(
                "tui",
                View {
                    shard: 0,
                    is_last: true,
                    blocks: vec![Block::Paragraph("pong".into())],
                    status: None,
                    buttons: Vec::new(),
                },
            )
            .await
        })
    };

    let mut req = String::new();
    r.read_line(&mut req).await.unwrap();
    let req: serde_json::Value = serde_json::from_str(req.trim()).unwrap();
    assert_eq!(req["method"], "chuan.deliver");
    assert_eq!(req["params"]["view"]["blocks"][0]["value"], "pong");
    let id = req["id"].as_u64().unwrap();
    let reply = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"msg": {"channel": "echo", "id": "m1"}}
    });
    w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();

    let msg = deliver.await.unwrap().unwrap();
    assert_eq!(msg.0, "m1");

    cancel.cancel();
    let _ = run.await;
}
