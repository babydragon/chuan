//! 微信通道：ClawBot / iLink 长轮询。
//!
//! **本步只做文本**：收文本、发文本。图片/文件/语音先跳过并记一条 debug 日志——
//! 语音的取舍见 `docs/VOICE.md`，媒体通道见 `docs/WEIXIN.md`。
//!
//! 通道能力的硬事实（决定了这里为什么这么写）：
//! - **不能编辑消息**（F6）→ `Caps.edit = false`，渲染器只在定稿发一次
//! - **没有按钮**（F14）→ 审批走默认降级：文本 + `/approve <id>`
//! - **出站额度 10 条/入站**（F10）→ 回复必须分片合并，不刷进度

pub mod api;
pub mod fake;
pub mod login;
pub mod http;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness_kernel::{
    Caps, Channel, ChannelError, ChannelId, EditOutcome, Inbound, InboundKind, MsgRef, View,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::offsets::OffsetStore;
use crate::view::render_plain;
use api::{Api, ApiError};

/// 扫码登录得到的凭据（宿主侧 0600 文件；不进沙盒，D18）。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Credential {
    pub bot_token: String,
    /// 服务端分配的 bot 身份（`xxx@im.bot`）
    pub bot_id: String,
    /// 扫码人自己的 id（`xxx@im.wechat`）
    pub user_id: String,
    pub base_url: String,
}

impl Credential {
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct WeixinConfig {
    /// 超过该秒数的入站消息直接丢弃（断网恢复后不追更）
    pub stale_after_s: i64,
    /// 长轮询游标在 store 里的 key
    pub cursor_key: String,
}

impl Default for WeixinConfig {
    fn default() -> Self {
        Self { stale_after_s: 300, cursor_key: "wx.buf".into() }
    }
}

pub struct WeixinChannel<A: Api> {
    api: Arc<A>,
    offsets: Arc<dyn OffsetStore>,
    stale_after_s: i64,
    cursor_key: String,
    caps: Caps,
    /// conversation → 最近一条入站消息的 `context_token`；回复必须原样带回（WEIXIN §3.2）
    ctx: Mutex<HashMap<String, String>>,
}

impl<A: Api> WeixinChannel<A> {
    pub fn new(api: Arc<A>, offsets: Arc<dyn OffsetStore>, cfg: WeixinConfig) -> Self {
        Self {
            api,
            offsets,
            stale_after_s: cfg.stale_after_s,
            cursor_key: cfg.cursor_key,
            caps: Caps {
                // 协议没有编辑接口（F6）：改了也只是"又被丢弃"，不如不发
                edit: false,
                // 没有按钮（F14）；内核的 deliver_prompt 默认降级成文本 + /approve
                buttons: false,
                // 微信不解析 Markdown，发出去是原样字符
                markdown: false,
                // 实测 5000 可过、20000 被拒；取保守值，超出由 deliver 分片
                max_text: 4000,
                // 不发中间态，这个值实际用不到
                min_edit_interval_ms: 5_000,
            },
            ctx: Mutex::new(HashMap::new()),
        }
    }

    fn ctx_of(&self, conversation: &str) -> String {
        self.ctx
            .lock()
            .unwrap()
            .get(conversation)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl<A: Api + 'static> Channel for WeixinChannel<A> {
    fn id(&self) -> ChannelId {
        ChannelId("weixin".into())
    }

    fn caps(&self) -> Caps {
        self.caps.clone()
    }

    async fn run(
        &self,
        tx: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> Result<(), ChannelError> {
        let mut cursor = self.offsets.get(&self.cursor_key).await.unwrap_or_default();
        let mut longpoll_ms = 35_000i64;

        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            match self.api.get_updates(&cursor, longpoll_ms).await {
                Ok(u) => {
                    if !u.get_updates_buf.is_empty() {
                        cursor = u.get_updates_buf.clone();
                        self.offsets.set(&self.cursor_key, &cursor).await;
                    }
                    if u.longpolling_timeout_ms > 0 {
                        longpoll_ms = u.longpolling_timeout_ms.min(60_000);
                    }
                    for m in u.msgs {
                        let Some(text) = m.text() else {
                            // 本步只做文本：图片/文件/语音/视频一律跳过并留痕
                            let kinds: Vec<&str> =
                                m.item_list.iter().map(|i| i.kind_name()).collect();
                            tracing::debug!(id = m.message_id, ?kinds, "微信消息非文本，跳过");
                            continue;
                        };
                        let from = m.from_user_id.clone();
                        if from.is_empty() {
                            continue;
                        }
                        // 授权在 actor 统一做（D26）：这里必须先把 context_token 记下来，
                        // 否则未绑定的用户连"未授权"都收不到。
                        // 新鲜度：断网恢复后不补发旧消息
                        if self.stale_after_s > 0
                            && m.create_time_ms > 0
                            && now_ms() - m.create_time_ms > self.stale_after_s * 1000
                        {
                            continue;
                        }
                        if !m.context_token.is_empty() {
                            self.ctx
                                .lock()
                                .unwrap()
                                .insert(from.clone(), m.context_token.clone());
                        }
                        let kind = if text.starts_with('/') {
                            InboundKind::Command
                        } else {
                            InboundKind::Text
                        };
                        let inbound = Inbound {
                            channel: self.id(),
                            conversation: from.clone(),
                            actor: from.clone(),
                            kind,
                            text: Some(text),
                            dedupe_key: m.message_id.to_string(),
                            callback_id: None,
                            msg: Some(MsgRef(m.message_id.to_string())),
                            received_at: if m.create_time_ms > 0 {
                                m.create_time_ms
                            } else {
                                now_ms()
                            },
                            attachments: Vec::new(),
                        };
                        if tx.send(inbound).await.is_err() {
                            return Err(ChannelError::Closed);
                        }
                    }
                }
                Err(ApiError::SessionExpired) => {
                    // -14 只能重扫（WEIXIN F17 之外的已知状态）；不要静默重试，否则永远转圈
                    tracing::error!("微信会话失效（-14），需要重新执行 weixin-login");
                    return Err(ChannelError::BadRequest("weixin session expired".into()));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "微信轮询失败，2s 后重试");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }

    async fn deliver(&self, conversation: &str, view: View) -> Result<MsgRef, ChannelError> {
        let ctx = self.ctx_of(conversation);
        if ctx.is_empty() {
            // 没有入站就没有 context_token（协议硬约束），只能等用户开口
            return Err(ChannelError::BadRequest(
                "缺少会话令牌：需要用户先发一条消息".into(),
            ));
        }
        let body = render_plain(&view);
        if body.trim().is_empty() {
            return Ok(MsgRef::default());
        }
        let mut last = MsgRef::default();
        for (i, part) in chunk(&body, self.caps.max_text).into_iter().enumerate() {
            let client_id = format!("chuan-{}-{i}", now_ms());
            let id = self
                .api
                .send_text(conversation, &ctx, &part, &client_id)
                .await
                .map_err(map_err)?;
            last = MsgRef(id);
        }
        Ok(last)
    }

    async fn update(
        &self,
        _conversation: &str,
        _msg: &MsgRef,
        _view: View,
    ) -> Result<EditOutcome, ChannelError> {
        // 协议没有编辑接口。`Caps.edit=false` 时渲染器不会走到这里；
        // 这里返回 NotModified 是为了让 `clear_prompt` 的默认实现不至于报错。
        Ok(EditOutcome::NotModified)
    }

    async fn ack(&self, _callback: &str, _text: &str) -> Result<(), ChannelError> {
        // 没有回调按钮
        Ok(())
    }

    async fn typing(&self, _conversation: &str) -> Result<(), ChannelError> {
        // TODO(下一步)：getconfig 拿 typing_ticket + sendtyping。
        // 现在不做：typing 只用几秒就自动消失，而 trait 还没有"停止"钩子。
        Ok(())
    }
}

fn map_err(e: ApiError) -> ChannelError {
    match e {
        ApiError::Transport(t) => ChannelError::Transport(t),
        ApiError::BadRequest(b) => ChannelError::BadRequest(b),
        ApiError::SessionExpired => ChannelError::BadRequest("session expired".into()),
    }
}

/// 按字符数硬切。微信单条上限在 5000–20000 之间（未精确二分），4000 是保守值。
fn chunk(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if cur.chars().count() >= max {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::chunk;

    #[test]
    fn chunk_splits_by_chars() {
        let t = "字".repeat(10);
        let parts = chunk(&t, 4);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].chars().count(), 4);
        assert_eq!(parts.concat(), t);
    }

    #[test]
    fn chunk_keeps_short_text_whole() {
        assert_eq!(chunk("你好", 4000), vec!["你好".to_string()]);
    }
}
