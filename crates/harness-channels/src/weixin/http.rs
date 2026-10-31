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

use super::api::{Api, ApiError, MediaRef, Updates, Uploaded};

/// 媒体 CDN 基址（`WEIXIN.md` §4）
const CDN_BASE: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

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
    /// 发一条消息（单个 item）。文本/图片/文件都走这里。
    async fn send_item(
        &self,
        to: &str,
        ctx: &str,
        item: Value,
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
                "item_list": [item],
            },
            "base_info": Self::base_info(),
        });
        let v = self.post("/ilink/bot/sendmessage", body).await?;
        Ok(v.get("message_id")
            .and_then(Value::as_i64)
            .map(|id| id.to_string())
            .unwrap_or_default())
    }

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

/// `media` 里三个字段的组装（`WEIXIN.md` §3.3）：`aes_key` 是 **hex 字符串的 base64**。
fn media_ref(up: &Uploaded) -> Value {
    json!({
        "encrypt_query_param": up.download_param,
        "aes_key": base64::engine::general_purpose::STANDARD.encode(up.aeskey_hex.as_bytes()),
        "encrypt_type": 1,
    })
}

fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(bytes);
    to_hex(&h.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// AES-128-ECB + PKCS#7 后的密文长度。
fn aes_ecb_padded_size(n: usize) -> usize {
    (n / 16 + 1) * 16
}

/// 随机字节：不引 rand，用纳秒 + 线性同余足够（只要求唯一/不可预测性一般）。
fn random_bytes(n: usize) -> Vec<u8> {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9)
        | 1;
    (0..n)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        })
        .collect()
}

fn random_hex(n: usize) -> String {
    to_hex(&random_bytes(n))
}

fn aes128_ecb_encrypt(bytes: &[u8], key: &[u8; 16]) -> Vec<u8> {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let cipher = aes::Aes128::new(key.into());
    // PKCS#7 填充
    let pad = 16 - (bytes.len() % 16);
    let mut buf = bytes.to_vec();
    buf.extend(std::iter::repeat(pad as u8).take(pad));
    let mut out = Vec::with_capacity(buf.len());
    for chunk in buf.chunks(16) {
        let mut block = aes::Block::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}

/// 只转义会破坏 query 的字符；`encrypt_query_param` 本身是 base64 串。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 32 位 hex → 16 字节。
fn hex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut k = [0u8; 16];
    for i in 0..16 {
        k[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(k)
}

/// 密钥有两种编码（`WEIXIN.md` §4）：32 位 hex 原文，或它的 base64（也可能是 16 字节原始密钥的 base64）。
fn parse_key(s: &str) -> Option<[u8; 16]> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(k) = hex16(s) {
        return Some(k);
    }
    let raw = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    if raw.len() == 16 {
        let mut k = [0u8; 16];
        k.copy_from_slice(&raw);
        return Some(k);
    }
    hex16(std::str::from_utf8(&raw).ok()?)
}

/// AES-128-ECB + PKCS#7 去填充。
fn aes128_ecb_decrypt(bytes: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, ApiError> {
    use aes::cipher::{BlockDecrypt, KeyInit};
    if bytes.is_empty() || bytes.len() % 16 != 0 {
        return Err(ApiError::BadRequest(format!("密文长度不对：{}", bytes.len())));
    }
    let cipher = aes::Aes128::new(key.into());
    let mut out = Vec::with_capacity(bytes.len());
    for chunk in bytes.chunks(16) {
        let mut block = aes::Block::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    // PKCS#7：末字节 n，且 1<=n<=16，则去掉 n 个
    let n = *out.last().unwrap_or(&0) as usize;
    if (1..=16).contains(&n) && out.len() >= n && out[out.len() - n..].iter().all(|b| *b as usize == n) {
        out.truncate(out.len() - n);
    }
    Ok(out)
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

    async fn upload_media(&self, to: &str, bytes: &[u8], media_type: i64) -> Result<Uploaded, ApiError> {
        let rawsize = bytes.len();
        let rawfilemd5 = md5_hex(bytes);
        let filesize = aes_ecb_padded_size(rawsize);
        let filekey = random_hex(16);
        let aeskey = random_bytes(16);
        let aeskey_hex = to_hex(&aeskey);

        let v = self
            .post(
                "/ilink/bot/getuploadurl",
                json!({
                    "filekey": filekey,
                    "media_type": media_type,
                    "to_user_id": to,
                    "rawsize": rawsize,
                    "rawfilemd5": rawfilemd5,
                    "filesize": filesize,
                    "no_need_thumb": true,
                    "aeskey": aeskey_hex,
                    "base_info": Self::base_info(),
                }),
            )
            .await?;

        let full = v.get("upload_full_url").and_then(Value::as_str).unwrap_or("").trim().to_owned();
        if full.is_empty() {
            // 协议允许用 upload_param + filekey 拼地址，但拼接规则文档没写全；
            // 实测若服务端不给 full_url，再补这一段（WEIXIN M2）。
            return Err(ApiError::BadRequest(
                "getuploadurl 没给 upload_full_url（upload_param 拼接未实现）".into(),
            ));
        }

        let mut key16 = [0u8; 16];
        key16.copy_from_slice(&aeskey);
        let cipher = aes128_ecb_encrypt(bytes, &key16);
        let mut last: Option<ApiError> = None;
        for attempt in 1..=3 {
            let resp = self
                .client
                .post(&full)
                .timeout(Duration::from_secs(60))
                .header("Content-Type", "application/octet-stream")
                .body(cipher.clone())
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let status = r.status();
                    if status.is_success() {
                        let param = r
                            .headers()
                            .get("x-encrypted-param")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        if !param.is_empty() {
                            return Ok(Uploaded {
                                download_param: param,
                                aeskey_hex,
                                cipher_size: cipher.len(),
                                plain_size: rawsize,
                            });
                        }
                        last = Some(ApiError::BadRequest("上传响应缺少 x-encrypted-param".into()));
                    } else if status.as_u16() < 500 {
                        // 4xx 立即放弃（照官方：客户端错误重试无意义）
                        let msg = r
                            .headers()
                            .get("x-error-message")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        return Err(ApiError::BadRequest(format!("CDN 上传 HTTP {status} {msg}")));
                    } else {
                        last = Some(ApiError::Transport(format!("CDN 上传 HTTP {status}")));
                    }
                }
                Err(e) => last = Some(ApiError::Transport(format!("CDN 上传失败: {e}"))),
            }
            tracing::warn!(attempt, "CDN 上传失败，重试");
        }
        Err(last.unwrap_or_else(|| ApiError::Transport("CDN 上传失败".into())))
    }

    async fn send_image(
        &self,
        to: &str,
        ctx: &str,
        up: &Uploaded,
        client_id: &str,
    ) -> Result<String, ApiError> {
        let item = json!({
            "type": 2,
            "image_item": {
                "media": media_ref(up),
                "mid_size": up.cipher_size,
            },
        });
        self.send_item(to, ctx, item, client_id).await
    }

    async fn send_file_item(
        &self,
        to: &str,
        ctx: &str,
        up: &Uploaded,
        name: &str,
        client_id: &str,
    ) -> Result<String, ApiError> {
        let item = json!({
            "type": 4,
            "file_item": {
                "media": media_ref(up),
                "file_name": name,
                "len": up.plain_size.to_string(),
            },
        });
        self.send_item(to, ctx, item, client_id).await
    }

    async fn download_media(&self, media: &MediaRef, key_hint: &str) -> Result<Vec<u8>, ApiError> {
        let url = if !media.full_url.is_empty() {
            media.full_url.clone()
        } else {
            format!(
                "{CDN_BASE}/download?encrypted_query_param={}",
                percent_encode(&media.encrypt_query_param)
            )
        };
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| ApiError::Transport(format!("下载媒体失败: {e}")))?;
        if !resp.status().is_success() {
            return Err(ApiError::BadRequest(format!("下载媒体 HTTP {}", resp.status())));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Transport(format!("读媒体失败: {e}")))?;
        // 密钥：图片专用 aeskey 优先，其次 media.aes_key；都没有就按明文处理
        let key = parse_key(key_hint).or_else(|| parse_key(&media.aes_key));
        match key {
            Some(k) => aes128_ecb_decrypt(&bytes, &k),
            None => Ok(bytes.to_vec()),
        }
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
        let item = json!({ "type": 1, "text_item": { "text": text } });
        self.send_item(to, ctx, item, client_id).await
    }
}
