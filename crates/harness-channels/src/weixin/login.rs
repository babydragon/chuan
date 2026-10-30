//! 扫码登录：把 bot 绑到操作者自己的微信，返回凭据。
//!
//! 协议见 `docs/WEIXIN.md` §2.3。**不做渲染、不读 stdin**：二维码内容交给调用方展示，
//! 验证码由调用方通过 hook 提供——通道层不该知道终端长什么样。

use std::time::{Duration, Instant};

use super::Credential;
use serde_json::{json, Value};

const APP_CLIENT_VERSION: &str = "132104";

pub struct Hooks<'a> {
    /// 展示二维码内容（调用方自己决定画到终端还是出图片）
    pub on_qr: Box<dyn Fn(&str) + 'a>,
    /// 服务端要求验证码时调用；返回 None 表示放弃
    pub ask_code: Box<dyn Fn() -> Option<String> + 'a>,
}

pub async fn qr_login(
    base_url: &str,
    proxy: Option<&str>,
    hooks: Hooks<'_>,
) -> Result<Credential, String> {
    let mut base = base_url.to_string();
    let client = build_client(proxy)?;

    println!("== 微信登录：取二维码 ==");
    let resp = post_login(&client, &base, "/ilink/bot/get_bot_qrcode?bot_type=3", json!({ "local_token_list": [] }))
        .await?;
    let qrcode = resp
        .get("qrcode")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("响应里没有 qrcode：{}", shorten(&resp.to_string())))?;
    let img = resp
        .get("qrcode_img_content")
        .and_then(Value::as_str)
        .unwrap_or("");

(hooks.on_qr)(img);

    let started = Instant::now();
    let mut verify: Option<String> = None;
    loop {
        if started.elapsed() > Duration::from_secs(480) {
            return Err("二维码 8 分钟未确认，重跑 weixin-login".into());
        }
        let st = get_status(&client, &base, qrcode, verify.as_deref()).await;
        let status = st.get("status").and_then(Value::as_str).unwrap_or("wait");
        match status {
            "wait" => {}
            "scaned" => println!("已扫码，请在手机上确认…"),
            "need_verifycode" => match (hooks.ask_code)() {
                Some(code) => verify = Some(code),
                None => return Err("需要验证码但未提供，重跑登录".into()),
            },
            "verify_code_blocked" => return Err("验证码错误次数过多，重跑 weixin-login".into()),
            "expired" => return Err("二维码过期，重跑 weixin-login".into()),
            "scaned_but_redirect" => {
                if let Some(host) = st.get("redirect_host").and_then(Value::as_str) {
                    println!("服务端要求换地址：{host}");
                    base = format!("https://{host}");
                }
            }
            "binded_redirect" => println!("该 bot 已绑定过实例，继续尝试取凭据…"),
            "confirmed" => {
                let bot_token = st
                    .get("bot_token")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if bot_token.is_empty() {
                    return Err(format!("confirmed 但没有 bot_token：{}", shorten(&st.to_string())));
                }
                let cred = Credential {
                    bot_token,
                    bot_id: st.get("ilink_bot_id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    user_id: st.get("ilink_user_id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    base_url: st
                        .get("baseurl")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| base.clone()),
                };
                return Ok(cred);
            }
            other => println!("未知状态 {other}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn build_client(proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder();
    if let Some(url) = proxy.filter(|u| !u.trim().is_empty()) {
        b = b.proxy(reqwest::Proxy::all(url).map_err(|e| format!("代理地址无效：{e}"))?);
    }
    b.build().map_err(|e| format!("构建 HTTP 客户端失败：{e}"))
}

/// 登录期请求头：带 AuthorizationType 与随机 UIN，**不带** Bearer（`WEIXIN.md` §2.2）
fn headers(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let uin = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(1);
        {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(nanos.to_string())
        }
    };
    rb.header("Content-Type", "application/json")
        .header("AuthorizationType", "ilink_bot_token")
        .header("X-WECHAT-UIN", uin)
        .header("iLink-App-Id", "bot")
        .header("iLink-App-ClientVersion", APP_CLIENT_VERSION)
}

async fn post_login(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    body: Value,
) -> Result<Value, String> {
    let url = format!("{base}{path}");
    let resp = headers(client.post(&url).timeout(Duration::from_secs(20)).json(&body))
        .send()
        .await
        .map_err(|e| format!("POST {path} 失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("POST {path} HTTP {status}: {}", shorten(&text)));
    }
    serde_json::from_str(&text).map_err(|e| format!("POST {path} 返回非 JSON：{e}"))
}

async fn get_status(
    client: &reqwest::Client,
    base: &str,
    qrcode: &str,
    verify: Option<&str>,
) -> Value {
    let url = format!("{base}/ilink/bot/get_qrcode_status");
    let mut q: Vec<(&str, &str)> = vec![("qrcode", qrcode)];
    if let Some(v) = verify {
        q.push(("verify_code", v));
    }
    // 文档口径：轮询失败一律当「等待」，不能中断登录
    let resp = headers(client.get(&url).timeout(Duration::from_secs(60)).query(&q))
        .send()
        .await;
    let Ok(resp) = resp else {
        return json!({ "status": "wait" });
    };
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_else(|_| json!({ "status": "wait" }))
}

fn shorten(s: &str) -> String {
    s.chars().take(200).collect()
}
