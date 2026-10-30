//! Telegram Bot API 的最小抽象。真实实现与测试替身都走这里。

use async_trait::async_trait;
use harness_kernel::{EditOutcome, MsgRef};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub date: i64,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub photo: Option<serde_json::Value>,
    #[serde(default)]
    pub document: Option<serde_json::Value>,
    #[serde(default)]
    pub caption: Option<String>,
}

/// 一条消息里值得下载的媒体（图片取最大尺寸，文档取原文件）。
pub struct MediaPick {
    pub file_id: String,
    pub name: String,
    pub mime: String,
}

impl Message {
    pub fn media_picks(&self) -> Vec<MediaPick> {
        let mut out = Vec::new();
        if let Some(doc) = &self.document {
            if let Some(id) = doc["file_id"].as_str() {
                out.push(MediaPick {
                    file_id: id.to_owned(),
                    name: doc["file_name"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("document_{}", self.message_id)),
                    mime: doc["mime_type"]
                        .as_str()
                        .unwrap_or("application/octet-stream")
                        .to_owned(),
                });
            }
        }
        if let Some(photo) = self.photo.as_ref().and_then(|p| p.as_array()) {
            if let Some(biggest) = photo.last() {
                if let Some(id) = biggest["file_id"].as_str() {
                    out.push(MediaPick {
                        file_id: id.to_owned(),
                        name: format!("photo_{}.jpg", self.message_id),
                        mime: "image/jpeg".into(),
                    });
                }
            }
        }
        out
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("retry after {0}ms")]
    RetryAfter(u32),
    #[error("not modified")]
    NotModified,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("transport: {0}")]
    Transport(String),
}

#[async_trait]
pub trait Api: Send + Sync {
    async fn get_me(&self) -> Result<String, ApiError>;
    async fn get_updates(&self, offset: i64, timeout_s: u32) -> Result<Vec<Update>, ApiError>;
    async fn send_message(&self, chat: i64, text: &str, html: bool) -> Result<MsgRef, ApiError>;
    async fn edit_message(
        &self,
        chat: i64,
        msg: &MsgRef,
        text: &str,
        html: bool,
    ) -> Result<EditOutcome, ApiError>;
    async fn answer_callback(&self, id: &str, text: &str) -> Result<(), ApiError>;
    /// 给某条消息挂状态反应；`emoji` 为空表示清空（D24）
    async fn set_reaction(&self, chat: i64, msg: &MsgRef, emoji: &str) -> Result<(), ApiError>;
    async fn set_my_commands(&self, commands: &[(String, String)]) -> Result<(), ApiError>;
    async fn send_chat_action(&self, chat: i64, action: &str) -> Result<(), ApiError>;
    async fn delete_message(&self, chat: i64, msg: &MsgRef) -> Result<(), ApiError>;
    async fn edit_message_with_buttons(
        &self,
        chat: i64,
        msg: &MsgRef,
        text: &str,
        buttons: &[Vec<(String, String)>],
    ) -> Result<(), ApiError>;
    /// 覆盖文本并清空按钮（删除失败时兜底）
    async fn edit_card(&self, chat: i64, msg: &MsgRef, text: &str) -> Result<(), ApiError>;
    /// 撤掉消息上的 inline keyboard（消息正文不变）
    async fn remove_buttons(&self, chat: i64, msg: &MsgRef) -> Result<(), ApiError>;
    /// 下载用户发来的文件（getFile → 文件流）
    async fn download_file(&self, file_id: &str) -> Result<Vec<u8>, ApiError>;
    /// 上传文档（sendDocument）
    async fn send_document(
        &self,
        chat: i64,
        path: &std::path::Path,
        name: &str,
        mime: &str,
        caption: &str,
    ) -> Result<MsgRef, ApiError>;
    /// 上传图片（sendPhoto）
    async fn send_photo(
        &self,
        chat: i64,
        path: &std::path::Path,
        caption: &str,
    ) -> Result<MsgRef, ApiError>;
    async fn send_card(
        &self,
        chat: i64,
        text: &str,
        buttons: &[Vec<(String, String)>],
    ) -> Result<MsgRef, ApiError>;
}
