//! 真实 HTTP 传输：POST 请求体，按字节流返回 SSE。
//!
//! `base_url` 接受两种写法：
//! - `https://host/v1`            → 自动补 `/chat/completions`
//! - `https://host/v1/chat/completions` → 原样使用

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use harness_kernel::EngineError;
use tokio_util::sync::CancellationToken;

use crate::transport::{HttpRequest, SseBytes, SseTransport};

pub struct ReqwestTransport {
    client: reqwest::Client,
    url: String,
    api_key: String,
}

impl ReqwestTransport {
    pub fn new(base_url: &str, api_key: impl Into<String>) -> Result<Self, EngineError> {
        Self::new_with_proxy(base_url, api_key, std::env::var("CHUAN_LLM_PROXY").ok().as_deref())
    }

    pub fn new_with_proxy(
        base_url: &str,
        api_key: impl Into<String>,
        proxy: Option<&str>,
    ) -> Result<Self, EngineError> {
        let url = normalize_url(base_url);
        // 模型调用默认**直连**，不继承 https_proxy（TG 需要代理，模型通常不需要）。
        let mut b = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(180))
            .connect_timeout(Duration::from_secs(15));
        if let Some(proxy) = proxy {
            if !proxy.trim().is_empty() {
                b = b.proxy(
                    reqwest::Proxy::all(proxy)
                        .map_err(|e| EngineError::Provider(format!("bad proxy: {e}")))?,
                );
            }
        }
        let client = b.build().map_err(|e| EngineError::Provider(e.to_string()))?;
        Ok(Self { client, url, api_key: api_key.into() })
    }
}

pub fn normalize_url(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_owned()
    } else {
        format!("{base}/chat/completions")
    }
}

struct ReqwestBytes {
    stream: reqwest::Response,
    cancel: CancellationToken,
}

#[async_trait]
impl SseBytes for ReqwestBytes {
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, EngineError>> {
        if self.cancel.is_cancelled() {
            return None;
        }
        // 空闲超时：provider 有时会把连接吊住（socket 还在、一个字节都不来），
        // 这种情况下总超时可能不触发，表现为"永远正在输入"。90s 没数据就断。
        match tokio::time::timeout(Duration::from_secs(90), self.stream.chunk()).await {
            Ok(Ok(Some(bytes))) => Some(Ok(bytes.to_vec())),
            Ok(Ok(None)) => None,
            Ok(Err(e)) => Some(Err(EngineError::Provider(e.to_string()))),
            Err(_) => Some(Err(EngineError::Provider(
                "SSE 空闲超过 90 秒，连接疑似被挂起".into(),
            ))),
        }
    }
}

#[async_trait]
impl SseTransport for ReqwestTransport {
    async fn open(&self, req: HttpRequest) -> Result<Box<dyn SseBytes>, EngineError> {
        let mut rb = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.api_key))
            .body(req.body);

        for (k, v) in &req.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }

        let resp = rb.send().await.map_err(|e| EngineError::Provider(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let body = resp.text().await.unwrap_or_default();
            return Err(classify(status.as_u16(), &body, retry_after));
        }

        Ok(Box::new(ReqwestBytes { stream: resp, cancel: CancellationToken::new() }))
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn appends_path_only_when_needed() {
        assert_eq!(normalize_url("https://a/v1"), "https://a/v1/chat/completions");
        assert_eq!(normalize_url("https://a/v1/"), "https://a/v1/chat/completions");
        assert_eq!(
            normalize_url("https://a/v1/chat/completions"),
            "https://a/v1/chat/completions"
        );
    }
}

/// 把 HTTP 错误归类：鉴权 / 额度 / 其他。
pub fn classify(status: u16, body: &str, retry_after_s: Option<u64>) -> EngineError {
    let lower = body.to_lowercase();
    let quota_words = [
        "insufficient_quota",
        "quota",
        "exceeded",
        "rate limit",
        "too many requests",
        "balance",
        "arrears",
        "insufficient balance",
    ];
    let auth_words = ["invalid_api_key", "api_key_expired", "unauthorized", "authentication", "invalid api key"];

    if status == 401 || auth_words.iter().any(|w| lower.contains(w)) {
        return EngineError::Auth(short(body));
    }
    if status == 402 || status == 429 || quota_words.iter().any(|w| lower.contains(w)) {
        let retry = retry_after_s.or_else(|| {
            // 尝试从 body 里抓 retry_after / reset 时间
            for key in ["retry_after", "retryafter", "reset_after"] {
                if let Some(i) = lower.find(key) {
                    let tail: String = body[i..].chars().take(40).collect();
                    if let Some(n) = tail
                        .split(|c: char| !c.is_ascii_digit())
                        .filter(|s| !s.is_empty())
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        return Some(n);
                    }
                }
            }
            None
        });
        return EngineError::Quota { message: short(body), retry_after_s: retry };
    }
    EngineError::Provider(format!("http {status}: {}", short(body)))
}

fn short(body: &str) -> String {
    let cleaned: String = body.chars().filter(|c| !c.is_control()).collect();
    cleaned.chars().take(280).collect()
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    #[test]
    fn maps_auth() {
        assert!(matches!(classify(401, "{\"error\":{\"code\":\"api_key_expired\"}}", None), EngineError::Auth(_)));
    }

    #[test]
    fn maps_quota_from_status_and_body() {
        assert!(matches!(classify(429, "too many requests", None), EngineError::Quota { .. }));
        assert!(matches!(
            classify(400, "{\"error\":{\"code\":\"insufficient_quota\"}}", None),
            EngineError::Quota { .. }
        ));
    }

    #[test]
    fn picks_retry_after() {
        match classify(429, "limited", Some(30)) {
            EngineError::Quota { retry_after_s, .. } => assert_eq!(retry_after_s, Some(30)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn falls_back_to_provider() {
        assert!(matches!(classify(500, "boom", None), EngineError::Provider(_)));
    }
}
