//! 真实实现：reqwest + iLink HTTP/JSON。
//!
//! 请求头与 `base_info` 照 `docs/WEIXIN.md` §2.2 抄；`channel_version` 固定上报官方插件版本，
//! 因为服务端可能据此判断能力（探针就是用 2.4.8 跑通的）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};

use super::api::{Api, ApiError, Updates};

/// 官方插件版本；`iLink-App-ClientVersion` = 0x00MMNNPP = 0x00020408 = 132104
const CHANNEL_VERSION: &str = "2.4.8";
const APP_CLIENT_VERSION: &str = "132104";

pub struct HttpApi {
    client: reqwest::Client,
    token: String,
    base: String,
    /// `ilink_user_id` → `typing_ticket`（`getconfig` 拿一次，之后复用）
    tickets: Mutex<HashMap<String, String>>,
}

impl HttpApi {
    pub fn new(token: impl Into<String>, base: impl Into<String>, proxy: Option<&str>) -> Result<Self, ApiError> {
        let mut b = reqwest::Client::builder();
        if let Some(url) = proxy.filter(|u| !u.trim().is_empty()) {
            b = b.proxy(
                reqwest::Proxy::all(url).map_err(|e| ApiError::Transport(format!("bad proxy: {e}")))?,
            );
        }
        let client = b.build().map_err(|e| ApiError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            token: token.into(),
            base: base.into(),
            tickets: Mutex::new(HashMap::new()),
        })
    }

    fn base_info() -> Value {
        json!({ "channel_version": CHANNEL_VERSION, "bot_agent": format!("chuan/{}", env!("CARGO_PKG_VERSION")) })
    }

    /// 每次请求随机：uint32 的十进制字符串再 base64（`WEIXIN.md` §2.2）。
    fn wechat_uin() -> String {
        // 不引 rand：用纳秒做种子足够（这个头只要求"每次不同"）
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(1);
        base64::engine::general_purpose::STANDARD.encode(nanos.to_string())
    }

    /// `typing_ticket`：`getconfig` 拿一次就够，缓存在内存里；失败/过期由调用方清缓存。
    async fn ticket(&self, user: &str) -> Result<String, ApiError> {
        if let Some(t) = self.tickets.lock().unwrap().get(user) {
            return Ok(t.clone());
        }
        let v = self
            .post(
                "/ilink/bot/getconfig",
                json!({ "ilink_user_id": user, "base_info": Self::base_info() }),
            )
            .await?;
        let t = v
            .get("typing_ticket")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !t.is_empty() {
            self.tickets
                .lock()
                .unwrap()
                .insert(user.to_owned(), t.clone());
        }
        Ok(t)
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, ApiError> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(20))
            .header("Content-Type", "application/json")
            .header("AuthorizationType", "ilink_bot_token")
            .header("Authorization", format!("Bearer {}", self.token))
            .header("X-WECHAT-UIN", Self::wechat_uin())
            .header("iLink-App-Id", "bot")
            .header("iLink-App-ClientVersion", APP_CLIENT_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Transport(format!("{path}: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ApiError::Transport(format!("{path}: 读响应失败 {e}")))?;
        if !status.is_success() {
            return Err(ApiError::BadRequest(format!("{path} HTTP {status}: {text}")));
        }
        // 实测存在 HTTP 200 + 空 body（`WEIXIN.md` F5），按失败处理但保留原文
        if text.trim().is_empty() {
            return Err(ApiError::BadRequest(format!("{path} 返回空 body")));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| ApiError::Transport(format!("{path} 非 JSON: {e}: {text}")))?;

        // 业务码：成功响应没有 ret；失败是 -1/-2，-14 表示会话失效（F3/F4/F12/F17）
        let code = v
            .get("ret")
            .or_else(|| v.get("errcode"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if code == -14 {
            return Err(ApiError::SessionExpired);
        }
        if code != 0 {
            let msg = v.get("errmsg").and_then(Value::as_str).unwrap_or("");
            // -2 是通用拒绝码：额度耗尽 / 文本过长都会落到这里（WEIXIN F12）
            let hint = if code == -2 {
                "（可能额度耗尽：每条入站只给 10 条出站；也可能是单条文本过长）"
            } else {
                ""
            };
            return Err(ApiError::BadRequest(format!("{path} ret={code} {msg}{hint}")));
        }
        Ok(v)
    }
}

#[async_trait]
impl Api for HttpApi {
    async fn get_updates(&self, cursor: &str, timeout_ms: i64) -> Result<Updates, ApiError> {
        let url = format!("{}/ilink/bot/getupdates", self.base);
        let body = json!({ "get_updates_buf": cursor, "base_info": Self::base_info() });
        let resp = self
            .client
            .post(&url)
            .timeout(Duration::from_millis((timeout_ms.max(1_000) + 15_000) as u64))
            .header("Content-Type", "application/json")
            .header("AuthorizationType", "ilink_bot_token")
            .header("Authorization", format!("Bearer {}", self.token))
            .header("X-WECHAT-UIN", Self::wechat_uin())
            .header("iLink-App-Id", "bot")
            .header("iLink-App-ClientVersion", APP_CLIENT_VERSION)
            .json(&body)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            // 长轮询超时是正常的空结果，不是错误
            Err(e) if e.is_timeout() => return Ok(Updates::default()),
            Err(e) => return Err(ApiError::Transport(format!("getupdates: {e}"))),
        };
        let status = resp.status();
        let text = resp.text().await.map_err(|e| ApiError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ApiError::BadRequest(format!("getupdates HTTP {status}: {text}")));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| ApiError::Transport(format!("getupdates 非 JSON: {e}")))?;
        let code = v
            .get("ret")
            .or_else(|| v.get("errcode"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if code == -14 {
            return Err(ApiError::SessionExpired);
        }
        if code != 0 {
            let msg = v.get("errmsg").and_then(Value::as_str).unwrap_or("");
            return Err(ApiError::BadRequest(format!("getupdates ret={code} {msg}")));
        }
        serde_json::from_value(v).map_err(|e| ApiError::Transport(format!("getupdates 解析失败: {e}")))
    }

    async fn typing(&self, user: &str, ctx: &str, on: bool) -> Result<(), ApiError> {
        let ticket = match self.ticket(user).await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = %e, "getconfig 失败，跳过 typing");
                return Ok(());
            }
        };
        if ticket.is_empty() {
            return Ok(()); // 没有 ticket 就是没有这个能力，不报错
        }
        let body = json!({
            "ilink_user_id": user,
            "typing_ticket": ticket,
            "status": if on { 1 } else { 2 },
            "base_info": Self::base_info(),
        });
        if let Err(e) = self.post("/ilink/bot/sendtyping", body).await {
            // ticket 可能过期：清掉缓存，下次重新取
            self.tickets.lock().unwrap().remove(user);
            tracing::debug!(error = %e, "sendtyping 失败");
        }
        let _ = ctx;
        Ok(())
    }

    async fn send_text(
        &self,
        to: &str,
        ctx: &str,
        text: &str,
        client_id: &str,
    ) -> Result<String, ApiError> {
        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "context_token": ctx,
                "item_list": [{ "type": 1, "text_item": { "text": text } }],
            },
            "base_info": Self::base_info(),
        });
        let v = self.post("/ilink/bot/sendmessage", body).await?;
        Ok(v.get("message_id")
            .and_then(Value::as_i64)
            .map(|id| id.to_string())
            .unwrap_or_default())
    }
}
