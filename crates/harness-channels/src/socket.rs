//! 外部通道适配器协议（D44）：核心监听 UDS，适配器作为客户端连入。
//!
//! 协议是 JSON-RPC 2.0 的极小子集：适配器发 `chuan.hello` 握手、`chuan.inbound`
//! 通知；核心发 `chuan.deliver` / `chuan.update` / `chuan.ack` / `chuan.send_file` /
//! `chuan.prompt` / `chuan.clear_buttons` 请求。
//!
//! 安全边界：
//! - 核心创建并监听 socket（适配器不能抢注）；
//! - socket 0600，配置了 `uid` 时用 `SO_PEERCRED` 校验对端；
//! - 附件只接受适配器 media 目录下的路径，重新计算大小与摘要后拷进核心的 media 目录；
//! - 适配器没有 Journal/DB/工具访问权，只搬 `Inbound` / `View`。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use harness_kernel::{
    Attachment, Caps, Channel, ChannelError, ChannelId, EditOutcome, Inbound, InboundKind, MsgRef,
    Prompt, View,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const PROTOCOL: u32 = 1;
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_LINE: usize = 1024 * 1024;

/// 一个适配器的启动配置。
#[derive(Clone, Debug)]
pub struct AdapterConfig {
    pub id: String,
    pub socket: PathBuf,
    /// 期望的对端 uid；`None` = 不校验（socket 仍为 0600）
    pub expected_uid: Option<u32>,
    /// 期望对端的主 gid；用于把 socket 设成 `chuan:<gid> 0660`
    pub expected_gid: Option<u32>,
    /// 适配器写入附件的目录；`None` = 不接受附件
    pub media_dir: Option<PathBuf>,
    pub max_media_bytes: u64,
}

impl AdapterConfig {
    /// 从配置文本构造；`uid` 支持数字或 `/etc/passwd` 里的用户名。
    pub fn new(
        id: impl Into<String>,
        socket: PathBuf,
        uid: Option<&str>,
        media_dir: Option<PathBuf>,
        max_media_mb: u64,
    ) -> Result<Self, String> {
        let id = id.into();
        if id.is_empty() {
            return Err("适配器 id 不能为空".into());
        }
        let (expected_uid, expected_gid) = match uid.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => {
                let (u, g) = resolve_uid(name)?;
                (Some(u), Some(g))
            }
            None => (None, None),
        };
        Ok(Self {
            id,
            socket,
            expected_uid,
            expected_gid,
            media_dir,
            max_media_bytes: max_media_mb.saturating_mul(1024 * 1024),
        })
    }
}

fn resolve_uid(name: &str) -> Result<(u32, u32), String> {
    let passwd = std::fs::read_to_string("/etc/passwd")
        .map_err(|e| format!("读取 /etc/passwd 失败：{e}"))?;
    let numeric = name.parse::<u32>().ok();
    for line in passwd.lines() {
        let mut f = line.split(':');
        let user = f.next().unwrap_or_default();
        let _pw = f.next();
        let uid = f.next().unwrap_or_default();
        let gid = f.next().unwrap_or_default();
        let matches = match numeric {
            Some(n) => uid.parse::<u32>().ok() == Some(n),
            None => user == name,
        };
        if matches {
            let uid = uid
                .parse::<u32>()
                .map_err(|e| format!("用户 {name} 的 uid 不合法：{e}"))?;
            let gid = gid
                .parse::<u32>()
                .map_err(|e| format!("用户 {name} 的 gid 不合法：{e}"))?;
            return Ok((uid, gid));
        }
    }
    match numeric {
        // 纯数字且 /etc/passwd 里没有：只能假设 gid 同 uid
        Some(n) => Ok((n, n)),
        None => Err(format!(
            "找不到用户 {name}（uid 可以写数字，或写系统用户名）"
        )),
    }
}

#[derive(Debug, Clone)]
struct RpcError {
    code: i64,
    message: String,
}

struct Conn {
    out: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>,
}

/// 核心侧的 socket 通道：一个适配器 = 一条通道。
pub struct SocketChannel {
    id: ChannelId,
    listener: UnixListener,
    expected_uid: Option<u32>,
    conn: Mutex<Option<Arc<Conn>>>,
    caps: RwLock<Caps>,
    media_root: Option<PathBuf>,
    inbox_dir: PathBuf,
    max_media_bytes: u64,
    next_id: AtomicU64,
}

impl SocketChannel {
    /// 绑定 socket。路径已存在且能连上时拒绝启动，避免两个核心抢同一个适配器。
    pub fn bind(cfg: AdapterConfig, inbox_dir: PathBuf) -> Result<Self, String> {
        if let Some(dir) = cfg.socket.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
            let _ =
                std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o755));
        }
        if cfg.socket.exists() {
            match std::os::unix::net::UnixStream::connect(&cfg.socket) {
                Ok(_) => {
                    return Err(format!(
                        "socket {} 已被占用（另一个 chuan 或适配器在跑？）",
                        cfg.socket.display()
                    ));
                }
                Err(_) => {
                    std::fs::remove_file(&cfg.socket)
                        .map_err(|e| format!("清理旧 socket {} 失败：{e}", cfg.socket.display()))?;
                }
            }
        }
        let listener = UnixListener::bind(&cfg.socket)
            .map_err(|e| format!("绑定 {} 失败：{e}", cfg.socket.display()))?;
        // 同 uid：0600；独立 uid：把组改成适配器的主组，0660，核心仍是 owner。
        let mode = if cfg.expected_gid.is_some() {
            0o660
        } else {
            0o600
        };
        std::fs::set_permissions(
            &cfg.socket,
            std::os::unix::fs::PermissionsExt::from_mode(mode),
        )
        .map_err(|e| format!("设置 socket 权限失败：{e}"))?;
        if let Some(gid) = cfg.expected_gid {
            std::os::unix::fs::chown(&cfg.socket, None, Some(gid))
                .map_err(|e| format!("设置 socket 所属组失败：{e}"))?;
        }
        if let Some(dir) = &cfg.media_dir {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
        }
        Ok(Self {
            id: ChannelId(cfg.id),
            listener,
            expected_uid: cfg.expected_uid,
            conn: Mutex::new(None),
            // 握手前的保守默认：只发终稿、不做原地编辑
            caps: RwLock::new(Caps {
                edit: false,
                buttons: true,
                markdown: false,
                max_text: 4000,
                min_edit_interval_ms: 0,
            }),
            media_root: cfg.media_dir,
            inbox_dir,
            max_media_bytes: cfg.max_media_bytes,
            next_id: AtomicU64::new(1),
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ChannelError> {
        let conn = {
            let g = self.conn.lock().unwrap();
            g.as_ref().cloned()
        }
        .ok_or(ChannelError::Closed)?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        conn.pending.lock().unwrap().insert(id, tx);
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string()
            + "\n";
        if conn.out.send(line).await.is_err() {
            conn.pending.lock().unwrap().remove(&id);
            return Err(ChannelError::Closed);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(map_rpc_error(e)),
            Ok(Err(_)) => Err(ChannelError::Closed),
            Err(_) => {
                conn.pending.lock().unwrap().remove(&id);
                Err(ChannelError::Transport("适配器响应超时".into()))
            }
        }
    }

    async fn serve(
        &self,
        stream: UnixStream,
        tx: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if let Some(uid) = self.expected_uid {
            let cred = stream
                .peer_cred()
                .map_err(|e| format!("读取对端凭据失败：{e}"))?;
            if cred.uid() != uid {
                return Err(format!(
                    "对端 uid {} 与配置的 uid {} 不一致",
                    cred.uid(),
                    uid
                ));
            }
        }

        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        // 握手：5s 内必须发 `chuan.hello`
        let mut line = String::new();
        let n = tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.read_line(&mut line))
            .await
            .map_err(|_| "握手超时".to_owned())?
            .map_err(|e| format!("读取握手失败：{e}"))?;
        if n == 0 {
            return Err("连接在握手前关闭".into());
        }
        let hello: Value =
            serde_json::from_str(line.trim()).map_err(|e| format!("握手不是合法 JSON：{e}"))?;
        let id = hello
            .get("id")
            .and_then(Value::as_u64)
            .ok_or("握手缺少 id")?;
        let params = hello.get("params").cloned().unwrap_or(Value::Null);
        let channel = params.get("channel").and_then(Value::as_str).unwrap_or("");
        let protocol = params.get("protocol").and_then(Value::as_u64).unwrap_or(0);
        if protocol != PROTOCOL as u64 {
            let _ = write_rpc_error(
                &mut w,
                id,
                -32001,
                &format!("不支持的协议版本 {protocol}（本机支持 {PROTOCOL}）"),
            )
            .await;
            return Err(format!("协议版本不匹配：{protocol}"));
        }
        if channel != self.id.0 {
            let _ = write_rpc_error(
                &mut w,
                id,
                -32002,
                &format!("channel 不匹配：握手为 {channel}，socket 为 {}", self.id.0),
            )
            .await;
            return Err("channel 不匹配".into());
        }
        let caps: Caps =
            serde_json::from_value(params.get("caps").cloned().ok_or("握手缺少 caps")?)
                .map_err(|e| format!("caps 不合法：{e}"))?;
        *self.caps.write().unwrap() = caps;

        let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
        let writer = tokio::spawn(async move {
            while let Some(s) = out_rx.recv().await {
                if w.write_all(s.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let conn = Arc::new(Conn {
            out: out_tx.clone(),
            pending: Arc::new(Mutex::new(HashMap::new())),
        });
        *self.conn.lock().unwrap() = Some(Arc::clone(&conn));
        let _ = out_tx
            .send(
                json!({"jsonrpc":"2.0","id":id,"result":{"protocol":PROTOCOL,"ok":true}})
                    .to_string()
                    + "\n",
            )
            .await;

        let result = self.read_loop(&mut reader, &tx, &conn, &cancel).await;
        // 清理：断线后所有等待中的请求立即失败，适配器重连后重新开始
        self.conn.lock().unwrap().take();
        {
            let mut pending = conn.pending.lock().unwrap();
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(RpcError {
                    code: -32000,
                    message: "适配器连接已断开".into(),
                }));
            }
        }
        writer.abort();
        result
    }

    async fn read_loop(
        &self,
        reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        tx: &mpsc::Sender<Inbound>,
        conn: &Arc<Conn>,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let mut buf = String::new();
        loop {
            buf.clear();
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                n = reader.read_line(&mut buf) => {
                    let n = n.map_err(|e| format!("读取失败：{e}"))?;
                    if n == 0 {
                        return Ok(());
                    }
                    if n > MAX_LINE {
                        return Err("单行超过 1MB".into());
                    }
                    let msg: Value = match serde_json::from_str(buf.trim()) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(channel = %self.id.0, error = %e, "适配器发来非法 JSON");
                            continue;
                        }
                    };
                    if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                        if msg.get("method").is_some() {
                            let _ = conn.out.send(
                                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}})
                                    .to_string() + "\n"
                            ).await;
                            continue;
                        }
                        let pending = conn.pending.lock().unwrap().remove(&id);
                        if let Some(tx) = pending {
                            let res = match msg.get("error") {
                                Some(err) => Err(RpcError {
                                    code: err.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                                    message: err
                                        .get("message")
                                        .and_then(Value::as_str)
                                        .unwrap_or("adapter error")
                                        .to_owned(),
                                }),
                                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = tx.send(res);
                        }
                        continue;
                    }
                    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                    if method != "chuan.inbound" {
                        continue;
                    }
                    let params = msg.get("params").cloned().unwrap_or(Value::Null);
                    match self.parse_inbound(params).await {
                        Ok(inb) => {
                            if tx.send(inb).await.is_err() {
                                return Ok(());
                            }
                        }
                        Err(e) => {
                            tracing::warn!(channel = %self.id.0, error = %e, "拒绝入站消息");
                        }
                    }
                }
            }
        }
    }

    async fn parse_inbound(&self, params: Value) -> Result<Inbound, String> {
        #[derive(Deserialize)]
        struct WireInbound {
            conversation: String,
            actor: String,
            kind: String,
            #[serde(default)]
            text: Option<String>,
            dedupe_key: String,
            #[serde(default)]
            callback_id: Option<String>,
            #[serde(default)]
            msg: Option<String>,
            #[serde(default)]
            attachments: Vec<AttachmentInput>,
        }

        let w: WireInbound =
            serde_json::from_value(params).map_err(|e| format!("inbound 参数不合法：{e}"))?;
        if w.dedupe_key.trim().is_empty() {
            return Err("dedupe_key 不能为空".into());
        }
        let kind = match w.kind.as_str() {
            "text" => InboundKind::Text,
            "command" => InboundKind::Command,
            "media" => InboundKind::Media,
            "callback" => InboundKind::Callback,
            other => return Err(format!("不支持的 kind：{other}")),
        };
        let attachments = self.ingest(w.attachments).await?;
        Ok(Inbound {
            channel: self.id(),
            conversation: w.conversation,
            actor: w.actor,
            kind,
            text: w.text,
            dedupe_key: w.dedupe_key,
            callback_id: w.callback_id,
            msg: w.msg.map(MsgRef),
            received_at: harness_kernel::now_ms(),
            attachments,
        })
    }

    async fn ingest(&self, items: Vec<AttachmentInput>) -> Result<Vec<Attachment>, String> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let Some(root) = &self.media_root else {
            return Err("该适配器未配置 media_dir，不能接收附件".into());
        };
        let root = tokio::fs::canonicalize(root)
            .await
            .map_err(|e| format!("media_dir 不可用：{e}"))?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let path = tokio::fs::canonicalize(&item.path)
                .await
                .map_err(|e| format!("附件不可读：{}（{e}）", item.path))?;
            if !path.starts_with(&root) {
                return Err(format!("附件不在适配器 media 目录内：{}", item.path));
            }
            let meta = tokio::fs::metadata(&path)
                .await
                .map_err(|e| format!("读取附件元数据失败：{e}"))?;
            if !meta.is_file() {
                return Err(format!("附件不是普通文件：{}", item.path));
            }
            let bytes = meta.len();
            if bytes > self.max_media_bytes {
                return Err(format!(
                    "附件 {} 有 {bytes} 字节，超过上限 {}",
                    item.path, self.max_media_bytes
                ));
            }
            let data = tokio::fs::read(&path)
                .await
                .map_err(|e| format!("读取附件失败：{e}"))?;
            let name = sanitize_name(&item.name, &path);
            // 与内置通道共用同一套落盘（摘要命名、扩展名、去重）
            let att = crate::media::save_media(&self.inbox_dir, &data, &name, &item.mime)
                .map_err(|e| format!("写入附件失败：{e}"))?;
            out.push(att);
        }
        Ok(out)
    }
}

/// `ingest` 的输入（从 wire 解出来）。
#[derive(Deserialize)]
struct AttachmentInput {
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    mime: String,
}

fn sanitize_name(name: &str, path: &Path) -> String {
    let raw = if name.trim().is_empty() {
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into())
    } else {
        name.to_owned()
    };
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', ' ']).to_owned();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

fn map_rpc_error(e: RpcError) -> ChannelError {
    match e.code {
        -32001 | -32002 | -32600 | -32602 => ChannelError::BadRequest(e.message),
        _ => ChannelError::Transport(format!("{} ({})", e.message, e.code)),
    }
}

async fn write_rpc_error(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    id: u64,
    code: i64,
    message: &str,
) -> std::io::Result<()> {
    let line = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
    .to_string()
        + "\n";
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

#[async_trait]
impl Channel for SocketChannel {
    fn id(&self) -> ChannelId {
        self.id.clone()
    }

    fn caps(&self) -> Caps {
        self.caps.read().unwrap().clone()
    }

    async fn run(
        &self,
        tx: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> Result<(), ChannelError> {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted
                        .map_err(|e| ChannelError::Transport(format!("accept 失败：{e}")))?;
                    if let Err(e) = self.serve(stream, tx.clone(), cancel.clone()).await {
                        tracing::warn!(channel = %self.id.0, error = %e, "适配器连接结束");
                    }
                    if tx.is_closed() {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn deliver(&self, conversation: &str, view: View) -> Result<MsgRef, ChannelError> {
        let v = self
            .request(
                "chuan.deliver",
                json!({"conversation": conversation, "view": view}),
            )
            .await?;
        msg_ref_of(&v)
    }

    async fn update(
        &self,
        conversation: &str,
        msg: &MsgRef,
        view: View,
    ) -> Result<EditOutcome, ChannelError> {
        let v = self
            .request(
                "chuan.update",
                json!({"conversation": conversation, "msg": msg, "view": view}),
            )
            .await?;
        match v.get("outcome").and_then(Value::as_str).unwrap_or("") {
            "edited" => Ok(EditOutcome::Edited),
            "not_modified" => Ok(EditOutcome::NotModified),
            "too_old" => Ok(EditOutcome::TooOld),
            "rejected" => Ok(EditOutcome::Rejected {
                retry_after_ms: v
                    .get("retry_after_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(1000) as u32,
            }),
            other => Err(ChannelError::BadRequest(format!("未知 outcome：{other}"))),
        }
    }

    async fn ack(&self, callback: &str, text: &str) -> Result<(), ChannelError> {
        self.request("chuan.ack", json!({"callback_id": callback, "text": text}))
            .await
            .map(|_| ())
    }

    async fn clear_buttons(&self, conversation: &str, msg: &MsgRef) -> Result<(), ChannelError> {
        self.request(
            "chuan.clear_buttons",
            json!({"conversation": conversation, "msg": msg}),
        )
        .await
        .map(|_| ())
    }

    async fn send_file(
        &self,
        conversation: &str,
        path: &str,
        name: &str,
        mime: &str,
        caption: &str,
    ) -> Result<MsgRef, ChannelError> {
        let v = self
            .request(
                "chuan.send_file",
                json!({
                    "conversation": conversation,
                    "path": path,
                    "name": name,
                    "mime": mime,
                    "caption": caption,
                }),
            )
            .await?;
        msg_ref_of(&v)
    }

    async fn deliver_prompt(
        &self,
        conversation: &str,
        prompt: Prompt,
    ) -> Result<MsgRef, ChannelError> {
        let v = self
            .request(
                "chuan.prompt",
                json!({"conversation": conversation, "prompt": prompt}),
            )
            .await?;
        msg_ref_of(&v)
    }
}

fn msg_ref_of(v: &Value) -> Result<MsgRef, ChannelError> {
    v.get("msg")
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| MsgRef(s.to_owned()))
        .ok_or_else(|| ChannelError::BadRequest("适配器没有返回 msg.id".into()))
}
