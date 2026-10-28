//! Telegram 通道：长轮询、去重、新鲜度过滤、429 退避。

pub mod api;
pub mod fake;
pub mod http;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use harness_kernel::{
    Caps, Channel, ChannelError, ChannelId, EditOutcome, Inbound, InboundKind, MsgRef,
    Timestamp, View,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::html::to_html;
use crate::view::render_plain;
use api::{Api, ApiError};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// 允许的 Telegram user id；为空表示不限制（仅用于本地调试）。
    pub allowed_users: Vec<i64>,
    /// 附件落盘目录（宿主路径，沙盒里以 /media 只读可见）
    pub media_dir: std::path::PathBuf,
    /// 超过该秒数的入站消息直接丢弃（断网恢复后不追更）。
    pub stale_after_s: i64,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            allowed_users: Vec::new(),
            media_dir: std::env::temp_dir().join("chuan-media"),
            stale_after_s: 300,
        }
    }
}

/// offset 持久化：接口在通道层，持久化实现由应用层提供
/// （`apps/home/src/offsets.rs` 写 store 的 kv；测试/独立通道可用内存实现）。
#[async_trait]
pub trait OffsetStore: Send + Sync {
    async fn get(&self, key: &str) -> Option<i64>;
    async fn set(&self, key: &str, value: i64);
}

#[derive(Default)]
pub struct MemoryOffsets {
    value: std::sync::Mutex<Option<i64>>,
}

#[async_trait]
impl OffsetStore for MemoryOffsets {
    async fn get(&self, _key: &str) -> Option<i64> {
        *self.value.lock().unwrap()
    }
    async fn set(&self, _key: &str, value: i64) {
        *self.value.lock().unwrap() = Some(value);
    }
}

pub struct TelegramChannel<A: Api> {
    api: Arc<A>,
    media_dir: std::path::PathBuf,
    allowed: HashSet<i64>,
    offsets: Arc<dyn OffsetStore>,
    stale_after_s: i64,
    caps: Caps,
}

impl<A: Api> TelegramChannel<A> {
    pub fn new(api: Arc<A>, offsets: Arc<dyn OffsetStore>, cfg: TelegramConfig) -> Self {
        Self {
            api,
            media_dir: cfg.media_dir,
            allowed: cfg.allowed_users.into_iter().collect(),
            offsets,
            stale_after_s: cfg.stale_after_s,
            caps: Caps {
                edit: true,
                buttons: true,
                markdown: true,
                max_text: 4096,
                // 1s 的编辑间隔在长任务里会撞 Telegram 洪水控制：板上实测连续约 3 分钟
                // 每秒一次 editMessageText，就被罚 retry_after 169s / 181s。
                // 2.5s 把上限压到 24 次/分钟，加上"内容未变不发 edit"，实际远低于此。
                min_edit_interval_ms: 2500,
            },
        }
    }

    fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

#[async_trait]
impl<A: Api + 'static> Channel for TelegramChannel<A> {
    fn id(&self) -> ChannelId {
        ChannelId("telegram".into())
    }

    fn caps(&self) -> Caps {
        self.caps.clone()
    }

    async fn run(
        &self,
        tx: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> Result<(), ChannelError> {
        let mut offset = self.offsets.get("tg.offset").await.unwrap_or(0);

        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            match self.api.get_updates(offset, 25).await {
                Ok(updates) => {
                    for u in updates {
                        if u.update_id < offset {
                            continue;
                        }
                        offset = u.update_id + 1;
                        self.offsets.set("tg.offset", offset).await;

                        if let Some(cb) = u.callback_query {
                            let actor = cb.from.as_ref().map(|f| f.id).unwrap_or_default();
                            if !self.allowed.is_empty() && !self.allowed.contains(&actor) {
                                continue;
                            }
                            let conv = cb
                                .message
                                .as_ref()
                                .map(|m| m.chat.id.to_string())
                                .unwrap_or_else(|| actor.to_string());
                            let inbound = Inbound {
                                channel: self.id(),
                                conversation: conv,
                                actor: actor.to_string(),
                                kind: InboundKind::Callback,
                                text: cb.data,
                                dedupe_key: u.update_id.to_string(),
                                callback_id: Some(cb.id),
                                received_at: Self::now_secs() * 1000,
                                attachments: Vec::new(),
                            };
                            println!("inbound callback actor={actor} data={:?}", inbound.text);
                            if tx.send(inbound).await.is_err() {
                                return Err(ChannelError::Closed);
                            }
                            continue;
                        }

                        let Some(msg) = u.message else { continue };
                        // 新鲜度：断网恢复后不补发旧消息
                        if Self::now_secs() - msg.date > self.stale_after_s {
                            continue;
                        }
                        // 白名单：非白名单静默丢弃
                        if !self.allowed.is_empty()
                            && !msg.from.as_ref().map(|f| self.allowed.contains(&f.id)).unwrap_or(false)
                        {
                            continue;
                        }
                        let text = msg
                            .text
                            .clone()
                            .or_else(|| msg.caption.clone())
                            .unwrap_or_default();
                        // 通道只负责**把文件存下来**；要不要识别、怎么处理，由模型决定。
                        let mut attachments = Vec::new();
                        for pick in msg.media_picks() {
                            match self.api.download_file(&pick.file_id).await {
                                Ok(bytes) => match save_media(
                                    &self.media_dir,
                                    &bytes,
                                    &pick.name,
                                    &pick.mime,
                                ) {
                                    Ok(a) => attachments.push(a),
                                    Err(e) => tracing::warn!(error = %e, "保存附件失败"),
                                },
                                Err(e) => tracing::warn!(error = %e, "下载附件失败"),
                            }
                        }
                        let kind = if text.starts_with('/') {
                            InboundKind::Command
                        } else if !attachments.is_empty() {
                            InboundKind::Media
                        } else {
                            InboundKind::Text
                        };
                        let inbound = Inbound {
                            channel: self.id(),
                            conversation: msg.chat.id.to_string(),
                            actor: msg
                                .from
                                .as_ref()
                                .map(|f| f.id.to_string())
                                .unwrap_or_default(),
                            kind: kind.clone(),
                            text: Some(text.clone()),
                            dedupe_key: u.update_id.to_string(),
                            callback_id: None,
                            received_at: msg.date * 1000,
                            attachments,
                        };
                        println!(
                            "inbound chat={} actor={} kind={:?} text={:?}",
                            msg.chat.id,
                            msg.from.as_ref().map(|f| f.id).unwrap_or_default(),
                            kind,
                            text.chars().take(40).collect::<String>()
                        );
                        if tx.send(inbound).await.is_err() {
                            return Err(ChannelError::Closed);
                        }
                    }
                }
                Err(ApiError::RetryAfter(ms)) => {
                    tokio::time::sleep(Duration::from_millis(ms as u64)).await;
                }
                Err(ApiError::Transport(e)) => {
                    tracing::warn!(error = %e, "telegram poll failed, backing off");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "telegram poll error");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn deliver(&self, conversation: &str, view: View) -> Result<MsgRef, ChannelError> {
        let chat: i64 = conversation.parse().map_err(|_| ChannelError::BadRequest("bad chat".into()))?;
        let body = if self.caps.markdown { to_html(&render_plain(&view)) } else { render_plain(&view) };
        if view.buttons.is_empty() {
            self.api.send_message(chat, &body, self.caps.markdown).await.map_err(map_err)
        } else {
            let rows: Vec<Vec<(String, String)>> = view
                .buttons
                .iter()
                .map(|row| row.iter().map(|b| (b.label.clone(), b.data.clone())).collect())
                .collect();
            self.api.send_card(chat, &body, &rows).await.map_err(map_err)
        }
    }

    async fn update(
        &self,
        conversation: &str,
        msg: &MsgRef,
        view: View,
    ) -> Result<EditOutcome, ChannelError> {
        let chat: i64 = conversation.parse().map_err(|_| ChannelError::BadRequest("bad chat".into()))?;
        let body = if self.caps.markdown { to_html(&render_plain(&view)) } else { render_plain(&view) };
        let res = if !view.buttons.is_empty() {
            let rows: Vec<Vec<(String, String)>> = view
                .buttons
                .iter()
                .map(|row| row.iter().map(|b| (b.label.clone(), b.data.clone())).collect())
                .collect();
            self.api
                .edit_message_with_buttons(chat, msg, &body, &rows)
                .await
                .map(|_| EditOutcome::Edited)
        } else {
            self.api.edit_message(chat, msg, &body, self.caps.markdown).await
        };
        match res {
            Ok(o) => Ok(o),
            Err(ApiError::NotModified) => Ok(EditOutcome::NotModified),
            // 429 不是错误：交给渲染器按 retry_after 推迟下一拍，别把整轮 run 打死
            Err(ApiError::RetryAfter(ms)) => Ok(EditOutcome::Rejected { retry_after_ms: ms }),
            Err(e) => Err(map_err(e)),
        }
    }

    async fn ack(&self, callback: &str, text: &str) -> Result<(), ChannelError> {
        self.api.answer_callback(callback, text).await.map_err(map_err)
    }

    async fn deliver_prompt(
        &self,
        conversation: &str,
        prompt: harness_kernel::Prompt,
    ) -> Result<MsgRef, ChannelError> {
        let chat: i64 = conversation
            .parse()
            .map_err(|_| ChannelError::BadRequest("bad chat".into()))?;
        let body = to_html(&prompt.text);
        // 两个一行，避免键盘过宽
        let rows: Vec<Vec<(String, String)>> = prompt
            .choices
            .chunks(2)
            .map(|pair| {
                pair.iter()
                    .map(|c| (c.label.clone(), format!("appr:{}:{}", c.id, c.value)))
                    .collect()
            })
            .collect();
        self.api.send_card(chat, &body, &rows).await.map_err(map_err)
    }

    async fn send_file(
        &self,
        conversation: &str,
        path: &str,
        name: &str,
        mime: &str,
        caption: &str,
    ) -> Result<MsgRef, ChannelError> {
        let chat: i64 = conversation
            .parse()
            .map_err(|_| ChannelError::BadRequest("bad chat".into()))?;
        let file = std::path::Path::new(path);
        let is_photo =
            mime.starts_with("image/") && name.rsplit_once('.').map(|(_, e)| !e.eq_ignore_ascii_case("gif")).unwrap_or(true);
        let out = if is_photo {
            self.api.send_photo(chat, file, caption).await
        } else {
            self.api.send_document(chat, file, name, mime, caption).await
        };
        out.map_err(map_err)
    }

    async fn clear_buttons(
        &self,
        conversation: &str,
        msg: &MsgRef,
    ) -> Result<(), ChannelError> {
        let chat: i64 = conversation
            .parse()
            .map_err(|_| ChannelError::BadRequest("bad chat".into()))?;
        self.api.remove_buttons(chat, msg).await.map_err(map_err)
    }

    async fn clear_prompt(
        &self,
        conversation: &str,
        msg: &MsgRef,
        note: &str,
    ) -> Result<(), ChannelError> {
        let chat: i64 = conversation
            .parse()
            .map_err(|_| ChannelError::BadRequest("bad chat".into()))?;
        match self.api.delete_message(chat, msg).await {
            Ok(()) => Ok(()),
            Err(_) => self
                .api
                .edit_card(chat, msg, &to_html(note))
                .await
                .map_err(map_err),
        }
    }

    async fn typing(&self, conversation: &str) -> Result<(), ChannelError> {
        let chat: i64 = conversation
            .parse()
            .map_err(|_| ChannelError::BadRequest("bad chat".into()))?;
        self.api.send_chat_action(chat, "typing").await.map_err(map_err)
    }

    fn command_specs(&self) -> Vec<(String, String)> {
        vec![
            ("new".into(), "开启新会话（保留长期记忆）".into()),
            ("cancel".into(), "取消当前正在运行的任务".into()),
            ("status".into(), "查看运行状态".into()),
            ("facts".into(), "查看长期记忆".into()),
            ("reminders".into(), "查看/取消定时任务".into()),
            ("forget".into(), "删除一条记忆：/forget <id>".into()),
            ("approve".into(), "批准一次审批：/approve <id>".into()),
            ("deny".into(), "拒绝一次审批：/deny <id>".into()),
            ("help".into(), "查看可用指令".into()),
        ]
    }

    async fn register_commands(&self) -> Result<(), ChannelError> {
        let specs = self.command_specs();
        self.api.set_my_commands(&specs).await.map_err(map_err)
    }
}

/// 按内容摘要落盘：同一张图发两次只存一份。通道不理解内容，只保证文件在。
fn save_media(
    dir: &std::path::Path,
    bytes: &[u8],
    name: &str,
    mime: &str,
) -> std::io::Result<harness_kernel::Attachment> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let digest = format!("{h:016x}");
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| e.len() <= 5 && !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let path = dir.join(format!("{digest}{ext}"));
    if !path.exists() {
        let mut f = std::fs::File::create(&path)?;
        f.write_all(bytes)?;
    }
    Ok(harness_kernel::Attachment {
        path: path.to_string_lossy().into_owned(),
        name: name.to_owned(),
        mime: mime.to_owned(),
        digest,
        bytes: bytes.len() as u64,
    })
}

fn map_err(e: ApiError) -> ChannelError {
    match e {
        ApiError::RetryAfter(ms) => ChannelError::RateLimited { retry_after_ms: ms },
        ApiError::Transport(t) => ChannelError::Transport(t),
        ApiError::BadRequest(b) => ChannelError::BadRequest(b),
        ApiError::NotModified => ChannelError::BadRequest("not modified".into()),
    }
}

/// 时间戳辅助：Telegram 用秒，内核用毫秒。
pub fn secs_to_ms(s: i64) -> Timestamp {
    s * 1000
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use crate::telegram::fake::FakeApi;

    /// 防止"实现了指令但忘了注册"。
    #[test]
    fn all_user_facing_commands_are_registered() {
        let api = std::sync::Arc::new(FakeApi::new());
        let ch = TelegramChannel::new(
            api,
            std::sync::Arc::new(MemoryOffsets::default()),
            TelegramConfig::default(),
        );
        let names: Vec<String> = ch.command_specs().into_iter().map(|(c, _)| c).collect();
        for expected in
            [
                "new", "cancel", "status", "facts", "reminders", "forget", "approve", "deny",
                "help",
            ]
        {
            assert!(names.contains(&expected.to_string()), "缺少 /{expected}: {names:?}");
        }
    }
}
