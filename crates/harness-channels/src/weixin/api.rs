//! 微信 ClawBot（iLink）的最小抽象：长轮询、发文本、输入状态。
//!
//! 只覆盖文本：媒体（图/文件/语音）留到后续，见 `docs/VOICE.md` 与 `docs/WEIXIN.md`。
//! 协议事实（端点、头、返回码）一律以 `docs/WEIXIN.md` 为准。

use async_trait::async_trait;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    /// `ret`/`errcode` = -14：会话失效，只能重新扫码
    #[error("session expired")]
    SessionExpired,
}

/// `getupdates` 的响应。成功响应**没有 `ret` 字段**（`WEIXIN.md` F3）。
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Updates {
    #[serde(default)]
    pub msgs: Vec<WeixinMessage>,
    #[serde(default)]
    pub get_updates_buf: String,
    #[serde(default)]
    pub longpolling_timeout_ms: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct WeixinMessage {
    #[serde(default)]
    pub seq: i64,
    /// 消息 id：既当去重键，也当状态反应的句柄
    #[serde(default)]
    pub message_id: i64,
    #[serde(default)]
    pub from_user_id: String,
    #[serde(default)]
    pub create_time_ms: i64,
    /// 回复时必须原样带回；企业微信的叫法不同，这里统一叫 context_token
    #[serde(default)]
    pub context_token: String,
    #[serde(default)]
    pub item_list: Vec<Item>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Item {
    #[serde(default, rename = "type")]
    pub kind: i64,
    #[serde(default)]
    pub text_item: Option<TextItem>,
    #[serde(default)]
    pub voice_item: Option<VoiceItem>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TextItem {
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct VoiceItem {
    /// 微信自带的语音转写（`VOICE.md` U1：是否每条都有未验证）
    #[serde(default)]
    pub text: String,
}

impl Item {
    /// 1 = 文本；其余（2 图 / 3 语音 / 4 文件 / 5 视频）本步一律跳过。
    pub fn text(&self) -> Option<&str> {
        if self.kind == 1 {
            self.text_item.as_ref().map(|t| t.text.as_str())
        } else {
            None
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            1 => "text",
            2 => "image",
            3 => "voice",
            4 => "file",
            5 => "video",
            11 => "tool_call_start",
            12 => "tool_call_result",
            _ => "unknown",
        }
    }
}

impl WeixinMessage {
    /// 取这条消息的文本内容（多条文本 item 时按顺序拼接）。
    pub fn text(&self) -> Option<String> {
        let mut out = String::new();
        for it in &self.item_list {
            if let Some(t) = it.text() {
                out.push_str(t);
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

#[async_trait]
pub trait Api: Send + Sync {
    /// 长轮询；服务端超时（无消息）应返回空 `Updates` 而不是错误。
    async fn get_updates(&self, cursor: &str, timeout_ms: i64) -> Result<Updates, ApiError>;

    /// 发一条文本，返回平台 `message_id`。
    /// `ctx` 为入站消息里的 `context_token`（`WEIXIN.md` §3.2）。
    async fn send_text(
        &self,
        to: &str,
        ctx: &str,
        text: &str,
        client_id: &str,
    ) -> Result<String, ApiError>;
}
