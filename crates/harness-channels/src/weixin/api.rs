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
    #[serde(default)]
    pub image_item: Option<ImageItem>,
    #[serde(default)]
    pub file_item: Option<FileItem>,
}

/// CDN 引用（`WEIXIN.md` §4）：优先 `full_url`，否则用 `encrypt_query_param` 拼。
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MediaRef {
    #[serde(default)]
    pub encrypt_query_param: String,
    #[serde(default)]
    pub aes_key: String,
    #[serde(default)]
    pub encrypt_type: i64,
    #[serde(default)]
    pub full_url: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ImageItem {
    #[serde(default)]
    pub media: Option<MediaRef>,
    /// 图片专用密钥：32 位 hex，优先于 `media.aes_key`
    #[serde(default)]
    pub aeskey: String,
    /// 密文字节数（仅展示）
    #[serde(default)]
    pub mid_size: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FileItem {
    #[serde(default)]
    pub media: Option<MediaRef>,
    #[serde(default)]
    pub file_name: String,
    /// 明文字节数（十进制字符串）
    #[serde(default)]
    pub len: String,
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
    ///
    /// 语音走一条捷径：微信可能自带转写（`voice_item.text`，见 `docs/VOICE.md` U1）。
    /// 有转写就直接当正文，加个「（语音）」前缀让模型知道这是说出来的话。
    pub fn text(&self) -> Option<String> {
        let mut out = String::new();
        for it in &self.item_list {
            if let Some(t) = it.text() {
                out.push_str(t);
            }
        }
        if out.is_empty() {
            for it in &self.item_list {
                if it.kind == 3 {
                    if let Some(v) = &it.voice_item {
                        let t = v.text.trim();
                        if !t.is_empty() {
                            return Some(format!("（语音）{t}"));
                        }
                    }
                }
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

/// 上传成功后回填到 `media` 里需要的东西（`WEIXIN.md` §3.3）。
#[derive(Clone, Debug, Default)]
pub struct Uploaded {
    /// CDN 返回的 `x-encrypted-param`
    pub download_param: String,
    /// 16 字节密钥的 **hex 字符串**（发消息时要再 base64 一次）
    pub aeskey_hex: String,
    /// 密文长度（`mid_size` 用）
    pub cipher_size: usize,
    /// 明文长度（`file_item.len` 用）
    pub plain_size: usize,
}

#[async_trait]
pub trait Api: Send + Sync {
    /// 长轮询；服务端超时（无消息）应返回空 `Updates` 而不是错误。
    async fn get_updates(&self, cursor: &str, timeout_ms: i64) -> Result<Updates, ApiError>;

    /// 下载并解密一条媒体（图片/文件）。`key_hint` 是图片专用的 `aeskey`，
    /// 为空时回落到 `media.aes_key`（`WEIXIN.md` §4）。
    async fn download_media(
        &self,
        media: &MediaRef,
        key_hint: &str,
    ) -> Result<Vec<u8>, ApiError>;

    /// 上传一份媒体到 CDN（AES-128-ECB 加密）。`media_type`：1 图 / 2 视频 / 3 文件。
    async fn upload_media(
        &self,
        to: &str,
        bytes: &[u8],
        media_type: i64,
    ) -> Result<Uploaded, ApiError>;

    /// 发一条图片消息（`image_item`）。
    async fn send_image(
        &self,
        to: &str,
        ctx: &str,
        up: &Uploaded,
        client_id: &str,
    ) -> Result<String, ApiError>;

    /// 发一条文件消息（`file_item`）。
    async fn send_file_item(
        &self,
        to: &str,
        ctx: &str,
        up: &Uploaded,
        name: &str,
        client_id: &str,
    ) -> Result<String, ApiError>;

    /// 设置/取消"正在输入"。失败可以忽略（best-effort）。
    async fn typing(&self, user: &str, ctx: &str, on: bool) -> Result<(), ApiError>;

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
