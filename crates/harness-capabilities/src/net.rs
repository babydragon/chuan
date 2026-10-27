//! 出网取数：`web_fetch`（取一页）与 `net_log`（审计访问记录）。
//!
//! 三条硬规则：
//! 1. **只走 http/https**，且拒绝回环/内网/链路本地地址——内网调试有单独的车道（`lan_exec`），
//!    不在这里开 SSRF 口子；
//! 2. 单次响应有上限，超了截断（不把几百 MB 塞进上下文）；
//! 3. **每次访问都留痕**（JSONL），`net_log` 能按时间/域名/参数聚合，供事后审计。
//!
//! 访问记录是**运维日志（Log）**，不是状态（Journal）：写文件、可轮转，不进账本。

use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

const MAX_REDIRECTS: usize = 5;
const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;
/// 回给模型的正文上限（字符），避免一页 HTML 吃光上下文
const MAX_BODY_CHARS: usize = 20_000;
const LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
struct Parts {
    scheme: String,
    host: String,
    port: u16,
    /// 仅用于调试/展示；请求直接复用原 URL
    #[allow(dead_code)]
    path: String,
}

/// 手写 URL 拆分（不引 url crate）：够用即可，不认用户信息段。
fn split_url(raw: &str) -> Result<Parts, String> {
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| "URL 需要 http:// 或 https://".to_string())?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!("只支持 http/https，收到 {scheme}"));
    }
    if rest.contains('@') {
        return Err("URL 里不允许带用户信息（user@host）".into());
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err("URL 缺少主机".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p.parse().map_err(|_| "端口不合法".to_string())?;
            (h.to_owned(), port)
        }
        None => (
            authority.to_owned(),
            if scheme == "https" { 443 } else { 80 },
        ),
    };
    if host.is_empty() {
        return Err("URL 缺少主机".into());
    }
    Ok(Parts { scheme, host, port, path: path.to_owned() })
}

/// 只允许公网地址：私网/回环/链路本地/组播/未指定一律拒绝。
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.is_multicast())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80) // fe80::/10 link local
        }
    }
}

async fn guard_host(host: &str, port: u16) -> Result<(), String> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("解析 {host} 失败：{e}"))?;
    let mut any = false;
    for a in addrs {
        any = true;
        if !is_public_ip(a.ip()) {
            return Err(format!("拒绝访问内网/回环地址：{}", a.ip()));
        }
    }
    if !any {
        return Err(format!("{host} 没有解析到地址"));
    }
    Ok(())
}

pub(crate) fn append_log(path: &Path, line: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 简单轮转：超过上限就滚一份 .1，避免无限增长
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > LOG_MAX_BYTES {
            let _ = std::fs::rename(path, path.with_extension("jsonl.1"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        // 记录里有完整 URL（可能带 token），只给自己读
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        let _ = writeln!(f, "{line}");
    }
}

pub struct WebFetchTool {
    /// 直连客户端
    client: reqwest::Client,
    /// 经代理的客户端（没配代理时为 None）
    proxied: Option<reqwest::Client>,
    /// 默认策略：Auto = 直连失败再试代理
    mode: crate::proxy::Mode,
    log: PathBuf,
    max_bytes: usize,
}

impl WebFetchTool {
    pub fn new(log: impl Into<PathBuf>) -> Result<Arc<Self>, String> {
        Self::with_proxy(log, &crate::proxy::ProxyConfig::default())
    }

    pub fn with_proxy(
        log: impl Into<PathBuf>,
        proxy: &crate::proxy::ProxyConfig,
    ) -> Result<Arc<Self>, String> {
        let build = |p: Option<reqwest::Proxy>| {
            let mut b = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("chuan/0.1 (+home assistant)");
            b = match p {
                Some(p) => b.proxy(p),
                None => b.no_proxy(),
            };
            b.build().map_err(|e| format!("HTTP 客户端初始化失败：{e}"))
        };
        let client = build(None)?;
        let proxied = match proxy.reqwest() {
            Some(p) => Some(build(Some(p))?),
            None => None,
        };
        Ok(Arc::new(Self {
            client,
            proxied,
            mode: proxy.web_fetch,
            log: log.into(),
            max_bytes: DEFAULT_MAX_BYTES,
        }))
    }

    fn log_line(&self, url: &str, host: &str, status: Option<u16>, bytes: usize, ms: i64, note: &str) {
        let rec = serde_json::json!({
            "at": now_ms(),
            "url": url,
            "host": host,
            "query_len": url.split_once('?').map(|(_, q)| q.chars().count()).unwrap_or(0),
            "status": status,
            "bytes": bytes,
            "ms": ms,
            "note": note,
        });
        append_log(&self.log, &rec.to_string());
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: "取一个 http/https 页面的正文（纯文本，超长会截断）。\
沙盒里没有网络，所以外部资料一律用这个工具取；不要用 exec 里的 curl/wget。\
拒绝访问内网/回环地址。每次访问都会留痕，可被审计。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "max_chars": { "type": "integer", "description": "回给模型的正文上限，默认 20000" },
                    "proxy": {
                        "type": "boolean",
                        "description": "true = 走宿主代理；false = 直连；不传用默认策略（直连失败再试代理）"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    fn domain(&self) -> harness_kernel::Domain {
        harness_kernel::Domain::Outbound
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let mut url = args
            .get("url")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 url", false))?
            .to_owned();
        let cap = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(MAX_BODY_CHARS)
            .clamp(500, MAX_BODY_CHARS);

        let started = Instant::now();
        let mut hops = 0;
        loop {
            let parts = split_url(&url)
                .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
            if let Err(e) = guard_host(&parts.host, parts.port).await {
                self.log_line(&url, &parts.host, None, 0, started.elapsed().as_millis() as i64, &e);
                return Err(ToolError::new(ErrorCode::NotPermitted, e, false));
            }

            // 代理策略：显式 true/false 覆盖；Auto 时先直连，失败再走代理
            let want = match self.mode {
                crate::proxy::Mode::Always => Some(true),
                crate::proxy::Mode::Never => Some(false),
                crate::proxy::Mode::Auto => args.get("proxy").and_then(|v| v.as_bool()),
            };
            let mut attempts: Vec<Option<reqwest::Client>> = match (want, &self.proxied) {
                (Some(true), Some(p)) => vec![Some(p.clone())],
                (Some(false), _) => vec![None],
                (None, Some(p)) if self.mode == crate::proxy::Mode::Auto => {
                    vec![None, Some(p.clone())]
                }
                _ => vec![None],
            };
            let mut resp = None;
            let mut last_err = String::new();
            while let Some(client) = attempts.pop() {
                let c = client.unwrap_or_else(|| self.client.clone());
                match c.get(&url).send().await {
                    Ok(r) => {
                        resp = Some(r);
                        break;
                    }
                    Err(e) => {
                        last_err = e.to_string();
                    }
                }
            }
            let Some(resp) = resp else {
                let note = format!("请求失败：{last_err}");
                self.log_line(&url, &parts.host, None, 0, started.elapsed().as_millis() as i64, &note);
                return Err(ToolError::new(ErrorCode::Unavailable, note, false));
            };
            let status = resp.status();
            let ctype = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);

            // 手动跟重定向：每一跳都重新做地址检查，并记录最终 URL
            if status.is_redirection() {
                hops += 1;
                if hops > MAX_REDIRECTS {
                    let note = "重定向次数过多";
                    self.log_line(&url, &parts.host, Some(status.as_u16()), 0, started.elapsed().as_millis() as i64, note);
                    return Err(ToolError::new(ErrorCode::Unavailable, note, false));
                }
                let Some(loc) = resp.headers().get(reqwest::header::LOCATION) else {
                    let note = "重定向缺少 Location";
                    self.log_line(&url, &parts.host, Some(status.as_u16()), 0, started.elapsed().as_millis() as i64, note);
                    return Err(ToolError::new(ErrorCode::Unavailable, note, false));
                };
                let loc = String::from_utf8_lossy(loc.as_bytes()).to_string();
                url = if loc.starts_with("http://") || loc.starts_with("https://") {
                    loc
                } else if let Some(stripped) = loc.strip_prefix('/') {
                    format!("{}://{}:{}{}", parts.scheme, parts.host, parts.port, format!("/{stripped}"))
                } else {
                    format!("{}://{}:{}/{}", parts.scheme, parts.host, parts.port, loc)
                };
                continue;
            }

            if !status.is_success() {
                let note = format!("HTTP {}", status.as_u16());
                self.log_line(&url, &parts.host, Some(status.as_u16()), 0, started.elapsed().as_millis() as i64, &note);
                return Err(ToolError::new(
                    ErrorCode::Unavailable,
                    format!("{note}（{url}）"),
                    false,
                ));
            }

            let mut body = Vec::new();
            let mut truncated = false;
            let mut resp = resp;
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        body.extend_from_slice(&chunk);
                        if body.len() >= self.max_bytes {
                            body.truncate(self.max_bytes);
                            truncated = true;
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let note = format!("读取中断：{e}");
                        self.log_line(&url, &parts.host, Some(status.as_u16()), body.len(), started.elapsed().as_millis() as i64, &note);
                        return Err(ToolError::new(ErrorCode::Unavailable, note, false));
                    }
                }
            }

            let ms = started.elapsed().as_millis() as i64;
            self.log_line(&url, &parts.host, Some(status.as_u16()), body.len(), ms, "ok");

            // 二进制被 2MB 上限截断 = 文件残缺，落盘也是废的：直接报错，别假装成功
            if truncated {
                let kind = crate::payload::classify(ctype.as_deref(), &body);
                if matches!(
                    kind,
                    crate::payload::Kind::Image
                        | crate::payload::Kind::Audio
                        | crate::payload::Kind::Video
                        | crate::payload::Kind::Binary
                ) {
                    return Err(ToolError::new(
                        ErrorCode::Unavailable,
                        format!(
                            "文件超过 {}MB 上限（已截断，不能当完整文件用）：{url}\n\
                             需要下大文件时说明用途，改用专门的下载方式，不要用 web_fetch。",
                            DEFAULT_MAX_BYTES / 1024 / 1024
                        ),
                        false,
                    ));
                }
            }
            // 统一策略：HTML 去标签、文本有预算、二进制只给元信息 + 路径
            let rendered = crate::payload::render(
                &body,
                ctype.as_deref(),
                &_ctx.workspace.root,
                &_ctx.call.0,
                Some(cap),
            );
            let mut out = format!(
                "GET {url}\nHTTP {} · {} 字节 · {ms}ms{}\n\n",
                status.as_u16(),
                body.len(),
                if truncated { " · 已按上限截断" } else { "" }
            );
            out.push_str(&rendered.text);
            return Ok(ToolOutput {
                content: vec![Block::Paragraph(out)],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            });
        }
    }
}

/// `net_log`：把访问记录按域名聚合成一张小表，供模型做审计分析。
pub struct NetLogTool {
    log: PathBuf,
}

impl NetLogTool {
    pub fn new(log: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self { log: log.into() })
    }
}

#[async_trait]
impl Tool for NetLogTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "net_log".into(),
            description: "读取出网访问记录（按域名汇总 + 最近明细），用于回答“这周访问过哪些域名、\
有没有可疑请求/参数泄漏”这类审计问题。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "days": { "type": "integer", "description": "回看天数，默认 7" },
                    "limit": { "type": "integer", "description": "明细行数上限，默认 60" }
                }
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    fn domain(&self) -> harness_kernel::Domain {
        // 读的是宿主上的日志文件，不外联
        harness_kernel::Domain::Sandbox
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let days = args
            .get("days")
            .and_then(|v| v.as_u64())
            .unwrap_or(7)
            .clamp(1, 90) as i64;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(60)
            .clamp(1, 500) as usize;
        let since = now_ms() - days * 86_400_000;

        let text = match std::fs::read_to_string(&self.log) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput {
                    content: vec![Block::Paragraph(format!("最近 {days} 天没有出网记录。"))],
                    doc_writes: Vec::new(),
                    artifacts: Vec::new(),
                });
            }
            Err(e) => {
                return Err(ToolError::new(
                    ErrorCode::Unavailable,
                    format!("读取访问日志失败：{e}"),
                    false,
                ));
            }
        };

        #[derive(Default, Clone)]
        struct Row {
            at: i64,
            url: String,
            status: Option<u16>,
            bytes: usize,
            note: String,
        }
        let mut rows: Vec<Row> = Vec::new();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let at = v["at"].as_i64().unwrap_or(0);
            if at < since {
                continue;
            }
            rows.push(Row {
                at,
                url: v["url"].as_str().unwrap_or("?").to_owned(),
                status: v["status"].as_u64().map(|s| s as u16),
                bytes: v["bytes"].as_u64().unwrap_or(0) as usize,
                note: v["note"].as_str().unwrap_or("").to_owned(),
            });
        }
        rows.sort_by_key(|r| r.at);

        // 按域名聚合
        use std::collections::BTreeMap;
        let mut agg: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new(); // 次数, 字节, 失败
        for r in &rows {
            let host = r
                .url
                .split_once("://")
                .and_then(|(_, rest)| rest.split('/').next())
                .unwrap_or("?")
                .split(':')
                .next()
                .unwrap_or("?")
                .to_owned();
            let e = agg.entry(host).or_insert((0, 0, 0));
            e.0 += 1;
            e.1 += r.bytes;
            if r.status.map(|s| s >= 400).unwrap_or(true) {
                e.2 += 1;
            }
        }

        let mut out = format!(
            "最近 {days} 天：{} 次访问，{} 个域名\n\n域名\t次数\t字节\t失败\n",
            rows.len(),
            agg.len()
        );
        for (host, (n, bytes, fail)) in &agg {
            out.push_str(&format!("{host}\t{n}\t{bytes}\t{fail}\n"));
        }

        if !rows.is_empty() {
            out.push_str("\n最近明细（时间 · 状态 · 字节 · URL）：\n");
            for r in rows.iter().rev().take(limit) {
                out.push_str(&format!(
                    "- {} · {} · {}B · {}\n",
                    harness_kernel::schedule::format_local(r.at, 480),
                    r.status.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
                    r.bytes,
                    if r.note == "ok" { r.url.clone() } else { format!("{}（{}）", r.url, r.note) }
                ));
            }
        }
        out.push_str(
            "\n提示：URL 里带 query 的要看参数是否含密钥/token；失败多、域名陌生、\
端口非 80/443 的都值得单独说明。",
        );
        Ok(ToolOutput {
            content: vec![Block::Paragraph(out)],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_urls() {
        let p = split_url("https://example.com/a?b=1").unwrap();
        assert_eq!((p.scheme.as_str(), p.host.as_str(), p.port), ("https", "example.com", 443));
        assert_eq!(p.path, "/a?b=1");
        let p = split_url("http://example.com:8080").unwrap();
        assert_eq!((p.port, p.path.as_str()), (8080, "/"));
        assert!(split_url("ftp://example.com").is_err());
        assert!(split_url("https://u:p@example.com").is_err());
        assert!(split_url("https://example.com:notaport").is_err());
    }

    #[test]
    fn refuses_private_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "169.254.1.1", "::1", "fd00::1"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} 应被拒绝");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} 应放行");
        }
    }
}
