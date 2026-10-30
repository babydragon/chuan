//! 真实实现：reqwest + Bot API。默认沿用系统/环境代理（`https_proxy`），
//! 也可用 `CHUAN_PROXY` 显式指定。

use std::time::Duration;

use async_trait::async_trait;
use harness_kernel::{EditOutcome, MsgRef};
use serde_json::json;

use super::api::{Api, ApiError, Update};

pub struct HttpApi {
    client: reqwest::Client,
    token: String,
    base: String,
}

impl HttpApi {
    pub fn new(token: impl Into<String>) -> Result<Self, ApiError> {
        let from_env = std::env::var("CHUAN_PROXY").ok();
        Self::with_proxy(token, from_env.as_deref())
    }

    /// `[proxy] url` 优先，其次 `CHUAN_PROXY`；都没有则按 reqwest 默认（读 http_proxy/https_proxy）。
    pub fn with_proxy(
        token: impl Into<String>,
        proxy: Option<&str>,
    ) -> Result<Self, ApiError> {
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(60));
        if let Some(url) = proxy.filter(|u| !u.trim().is_empty()) {
            b = b.proxy(
                reqwest::Proxy::all(url)
                    .map_err(|e| ApiError::Transport(format!("bad proxy: {e}")))?,
            );
        }
        let client = b.build().map_err(|e| ApiError::Transport(e.to_string()))?;
        Ok(Self { client, token: token.into(), base: "https://api.telegram.org".into() })
    }

    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base, self.token, method)
    }

    /// 调试用：返回 `result` 原文（含 entities），用于校验 Telegram 实际解析出的格式。
    pub async fn send_message_verbose(
        &self,
        chat: i64,
        html: &str,
    ) -> Result<serde_json::Value, ApiError> {
        let v = self
            .post(
                "sendMessage",
                json!({
                    "chat_id": chat,
                    "text": html,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                }),
            )
            .await?;
        Ok(v["result"].clone())
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload(
        &self,
        method: &str,
        field: &str,
        chat: i64,
        path: &std::path::Path,
        name: &str,
        mime: &str,
        caption: &str,
    ) -> Result<MsgRef, ApiError> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| ApiError::BadRequest(format!("读取文件失败：{e}")))?;
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.to_owned())
            .mime_str(mime)
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat.to_string())
            .part(field.to_owned(), part);
        if !caption.is_empty() {
            form = form.text("caption", caption.to_owned()).text("parse_mode", "HTML");
        }
        let resp = self
            .client
            .post(self.url(method))
            .multipart(form)
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::BadRequest(format!("{method}: {body}")));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        Ok(MsgRef(v["result"]["message_id"].to_string()))
    }

    async fn post(&self, method: &str, body: serde_json::Value) -> Result<serde_json::Value, ApiError> {
        let resp = self
            .client
            .post(self.url(method))
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let status = resp.status();
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Transport(format!("bad json: {e}")))?;
        if v["ok"].as_bool().unwrap_or(false) {
            return Ok(v);
        }
        let desc = v["description"].as_str().unwrap_or_default().to_owned();
        if status.as_u16() == 429 {
            let retry = v["parameters"]["retry_after"].as_u64().unwrap_or(1) as u32;
            return Err(ApiError::RetryAfter(retry * 1000));
        }
        if desc.contains("message is not modified") {
            return Err(ApiError::NotModified);
        }
        Err(ApiError::BadRequest(desc))
    }
}

#[async_trait]
impl Api for HttpApi {
    async fn get_me(&self) -> Result<String, ApiError> {
        let v = self.post("getMe", json!({})).await?;
        Ok(v["result"]["username"].as_str().unwrap_or_default().to_owned())
    }

    async fn get_updates(&self, offset: i64, timeout_s: u32) -> Result<Vec<Update>, ApiError> {
        let v = self
            .post("getUpdates", json!({ "offset": offset, "timeout": timeout_s }))
            .await?;
        serde_json::from_value(v["result"].clone()).map_err(|e| ApiError::Transport(e.to_string()))
    }

    async fn send_message(&self, chat: i64, text: &str, html: bool) -> Result<MsgRef, ApiError> {
        let mut body = json!({
            "chat_id": chat,
            "text": text,
            "disable_web_page_preview": true,
        });
        if html {
            body["parse_mode"] = json!("HTML");
        }
        let v = self.post("sendMessage", body).await?;
        Ok(MsgRef(v["result"]["message_id"].to_string()))
    }

    async fn edit_message(
        &self,
        chat: i64,
        msg: &MsgRef,
        text: &str,
        html: bool,
    ) -> Result<EditOutcome, ApiError> {
        let mut body = json!({
            "chat_id": chat,
            "message_id": msg.0.parse::<i64>().map_err(|_| ApiError::BadRequest("bad message id".into()))?,
            "text": text,
            "disable_web_page_preview": true,
        });
        if html {
            body["parse_mode"] = json!("HTML");
        }
        match self.post("editMessageText", body).await {
            Ok(_) => Ok(EditOutcome::Edited),
            Err(ApiError::NotModified) => Ok(EditOutcome::NotModified),
            Err(e) => Err(e),
        }
    }

    async fn set_my_commands(&self, commands: &[(String, String)]) -> Result<(), ApiError> {
        let list: Vec<serde_json::Value> = commands
            .iter()
            .map(|(c, d)| json!({ "command": c, "description": d }))
            .collect();
        self.post("setMyCommands", json!({ "commands": list })).await.map(|_| ())
    }

    async fn delete_message(&self, chat: i64, msg: &MsgRef) -> Result<(), ApiError> {
        let id: i64 = msg
            .0
            .parse()
            .map_err(|_| ApiError::BadRequest("bad message id".into()))?;
        self.post("deleteMessage", json!({ "chat_id": chat, "message_id": id }))
            .await
            .map(|_| ())
    }

    async fn edit_message_with_buttons(
        &self,
        chat: i64,
        msg: &MsgRef,
        text: &str,
        buttons: &[Vec<(String, String)>],
    ) -> Result<(), ApiError> {
        let id: i64 = msg
            .0
            .parse()
            .map_err(|_| ApiError::BadRequest("bad message id".into()))?;
        let keyboard: Vec<Vec<serde_json::Value>> = buttons
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(label, data)| json!({ "text": label, "callback_data": data }))
                    .collect()
            })
            .collect();
        self.post(
            "editMessageText",
            json!({
                "chat_id": chat,
                "message_id": id,
                "text": text,
                "parse_mode": "HTML",
                "reply_markup": { "inline_keyboard": keyboard }
            }),
        )
        .await
        .map(|_| ())
    }

    async fn send_document(
        &self,
        chat: i64,
        path: &std::path::Path,
        name: &str,
        mime: &str,
        caption: &str,
    ) -> Result<MsgRef, ApiError> {
        let field = match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
            Some("md") | Some("py") | Some("rs") | Some("sh") => "document",
            _ => "document",
        };
        self.upload("sendDocument", field, chat, path, name, mime, caption).await
    }

    async fn send_photo(
        &self,
        chat: i64,
        path: &std::path::Path,
        caption: &str,
    ) -> Result<MsgRef, ApiError> {
        self.upload("sendPhoto", "photo", chat, path, "photo.jpg", "image/jpeg", caption)
            .await
    }

    async fn download_file(&self, file_id: &str) -> Result<Vec<u8>, ApiError> {
        const MAX_DOWNLOAD: u64 = 25 * 1024 * 1024;
        let v = self.post("getFile", json!({ "file_id": file_id })).await?;
        let path = v["result"]["file_path"]
            .as_str()
            .ok_or_else(|| ApiError::BadRequest(format!("getFile 没有 file_path：{v}")))?;
        let url = format!("{}/file/bot{}/{}", self.base, self.token, path);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Transport(format!("下载失败：HTTP {}", resp.status())));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        if bytes.len() as u64 > MAX_DOWNLOAD {
            return Err(ApiError::BadRequest("文件超过 25MB".into()));
        }
        Ok(bytes.to_vec())
    }

    async fn remove_buttons(&self, chat: i64, msg: &MsgRef) -> Result<(), ApiError> {
        let id: i64 = msg
            .0
            .parse()
            .map_err(|_| ApiError::BadRequest("bad message id".into()))?;
        self.post(
            "editMessageReplyMarkup",
            json!({
                "chat_id": chat,
                "message_id": id,
                "reply_markup": { "inline_keyboard": [] }
            }),
        )
        .await
        .map(|_| ())
    }

    async fn edit_card(&self, chat: i64, msg: &MsgRef, text: &str) -> Result<(), ApiError> {
        let id: i64 = msg
            .0
            .parse()
            .map_err(|_| ApiError::BadRequest("bad message id".into()))?;
        self.post(
            "editMessageText",
            json!({
                "chat_id": chat,
                "message_id": id,
                "text": text,
                "reply_markup": { "inline_keyboard": [] }
            }),
        )
        .await
        .map(|_| ())
    }

    async fn send_card(
        &self,
        chat: i64,
        text: &str,
        buttons: &[Vec<(String, String)>],
    ) -> Result<MsgRef, ApiError> {
        let keyboard: Vec<Vec<serde_json::Value>> = buttons
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(label, data)| json!({ "text": label, "callback_data": data }))
                    .collect()
            })
            .collect();
        let v = self
            .post(
                "sendMessage",
                json!({
                    "chat_id": chat,
                    "text": text,
                    "parse_mode": "HTML",
                    "reply_markup": { "inline_keyboard": keyboard },
                }),
            )
            .await?;
        Ok(MsgRef(v["result"]["message_id"].to_string()))
    }

    async fn send_chat_action(&self, chat: i64, action: &str) -> Result<(), ApiError> {
        self.post("sendChatAction", json!({ "chat_id": chat, "action": action }))
            .await
            .map(|_| ())
    }

    async fn set_reaction(&self, chat: i64, msg: &MsgRef, emoji: &str) -> Result<(), ApiError> {
        // 空串 = 清空。Telegram 每个 bot 对每条消息只能有一个 reaction，
        // 所以"改状态"和"清空"是同一支接口（D24）。
        let reaction = if emoji.is_empty() {
            json!([])
        } else {
            json!([{ "type": "emoji", "emoji": emoji }])
        };
        let id: i64 = msg
            .0
            .parse()
            .map_err(|_| ApiError::BadRequest(format!("bad message id: {}", msg.0)))?;
        self.post(
            "setMessageReaction",
            json!({ "chat_id": chat, "message_id": id, "reaction": reaction }),
        )
        .await?;
        Ok(())
    }

    async fn answer_callback(&self, id: &str, text: &str) -> Result<(), ApiError> {
        self.post("answerCallbackQuery", json!({ "callback_query_id": id, "text": text }))
            .await
            .map(|_| ())
    }
}
