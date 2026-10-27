//! 命名 HTTP 服务：`http(service, method, path, …)`。
//!
//! 解决的是"带鉴权的第三方 API"：滴答、微信读书这类。
//! 三条硬规则：
//! 1. **token 只在宿主注入**，模型既看不到也写不进 URL / 日志 / 技能里；
//! 2. 只能访问该服务配置的 base host（模型给的是 path，不是完整 URL）；
//! 3. 每次调用写审计日志（服务名、方法、路径、状态、耗时），**不记 header、不记 body**。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};


const MAX_RESP_BYTES: usize = 2 * 1024 * 1024;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    pub base: String,
    pub token_file: Option<PathBuf>,
    /// 模板：`Authorization: Bearer {token}`
    pub header: String,
    pub extra_headers: Vec<(String, String)>,
}

impl Service {
    fn header_value(&self) -> Result<Option<(String, String)>, String> {
        if self.header.trim().is_empty() {
            return Ok(None);
        }
        let Some((name, value)) = self.header.split_once(':') else {
            return Err(format!("服务 {} 的 header 配置不合法（应为 `Name: 值`）", self.name));
        };
        let mut value = value.trim().to_owned();
        if value.contains("{token}") {
            let file = self
                .token_file
                .as_ref()
                .ok_or_else(|| format!("服务 {} 需要 token 但没配 token_file", self.name))?;
            let token = std::fs::read_to_string(file)
                .map_err(|e| format!("读取 {} 的 token 失败：{e}", self.name))?
                .trim()
                .to_owned();
            if token.is_empty() {
                return Err(format!("服务 {} 的 token 是空文件", self.name));
            }
            value = value.replace("{token}", &token);
        }
        Ok(Some((name.trim().to_owned(), value)))
    }

    /// 把错误信息里可能出现的 token 抹掉（防手滑打日志）。
    fn redact(&self, text: &str) -> String {
        let Ok(Some((_, value))) = self.header_value() else { return text.to_owned() };
        let secret = value.rsplit(' ').next().unwrap_or(&value);
        if secret.len() < 6 {
            return text.to_owned();
        }
        text.replace(secret, "***")
    }
}

pub struct HttpServiceTool {
    services: std::sync::RwLock<BTreeMap<String, Service>>,
    client: reqwest::Client,
    log: PathBuf,
}

impl HttpServiceTool {
    pub fn new(
        services: Vec<Service>,
        log: impl Into<PathBuf>,
    ) -> Result<Arc<Self>, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            // 不跟重定向：避免被跳到别的域名（token 只该发给配置的那个 host）
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("chuan/0.1")
            .build()
            .map_err(|e| format!("HTTP 客户端初始化失败：{e}"))?;
        Ok(Arc::new(Self {
            services: std::sync::RwLock::new(
                services.into_iter().map(|s| (s.name.clone(), s)).collect(),
            ),
            client,
            log: log.into(),
        }))
    }

    pub fn names(&self) -> Vec<String> {
        self.services.read().unwrap().keys().cloned().collect()
    }

    /// 探活：证明 base 能连上（不看状态码——很多 API 对根路径/HEAD 回 404）。
    pub async fn probe(&self, svc: &Service) -> String {
        let mut req = self.client.head(&svc.base);
        if let Ok(Some((k, v))) = svc.header_value() {
            req = req.header(&k, &v);
        }
        match req.send().await {
            Ok(r) => format!("连接正常（HTTP {}）", r.status().as_u16()),
            Err(e) => format!("⚠️ 连接失败：{}", svc.redact(&e.to_string())),
        }
    }

    /// 运行时注册（`service` 工具用）。同名覆盖。
    pub fn upsert(&self, svc: Service) {
        self.services.write().unwrap().insert(svc.name.clone(), svc);
    }

    pub fn remove(&self, name: &str) -> bool {
        self.services.write().unwrap().remove(name).is_some()
    }

    fn log_line(&self, svc: &str, method: &str, path: &str, status: Option<u16>, bytes: usize, ms: i64, note: &str) {
        let rec = serde_json::json!({
            "at": now_ms(),
            "method": method,
            "url": format!("{svc}:{path}"),
            "host": svc,
            "query_len": path.split_once('?').map(|(_, q)| q.chars().count()).unwrap_or(0),
            "status": status,
            "bytes": bytes,
            "ms": ms,
            "note": note,
        });
        crate::net::append_log(&self.log, &rec.to_string());
    }
}

#[async_trait]
impl Tool for HttpServiceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "http".into(),
            description: format!(
                "调用配置好的第三方 HTTP 服务（鉴权由宿主注入，你不用也不能碰 token）。\
可用服务：{}。\n**先 `playbook(\"<服务名>-api\")` 取接口规范，再调**：\
这类服务通常只有一个入口路径（例如 `/api/agent/gateway`）+ 请求体内指定 `api_name`，\
不要凭直觉拼 REST 路径（会 404）。只给 path，不要自己拼域名。",
                if self.names().is_empty() { "（未配置）".to_owned() } else { self.names().join("、") }
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "description": "服务名，见工具说明" },
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                    "path": { "type": "string", "description": "以 / 开头的路径，可带 query" },
                    "body": { "type": "object", "description": "JSON 请求体（可选）" }
                },
                "required": ["service", "method", "path"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 可能有副作用（创建/删除任务），失败不自动重发
        EffectClass::External
    }

    fn domain(&self) -> harness_kernel::Domain {
        harness_kernel::Domain::Outbound
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let name = args
            .get("service")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 service", false))?;
        let svc = {
            let map = self.services.read().unwrap();
            map.get(name).cloned().ok_or_else(|| {
                ToolError::new(
                    ErrorCode::NotFound,
                    format!("没有这个服务：{name}（可用：{}）", self.names().join("、")),
                    false,
                )
            })?
        };
        let svc = &svc;
        let method = args
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 path", false))?;
        if path.contains("://") {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "path 里不允许出现协议或域名：只给 /open/v1/… 这样的路径",
                false,
            ));
        }
        if path.contains("..") {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "path 里不允许出现 ..", false));
        }
        if !path.starts_with('/') {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "path 必须以 / 开头（只给路径，域名由配置决定）",
                false,
            ));
        }

        let url = format!("{}{}", svc.base.trim_end_matches('/'), path);
        let (hname, hvalue) = match svc.header_value() {
            Ok(Some(h)) => h,
            Ok(None) => (String::new(), String::new()),
            Err(e) => {
                return Err(ToolError::new(ErrorCode::Unavailable, svc.redact(&e), false));
            }
        };

        let started = Instant::now();
        let mut req = match method.as_str() {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "PATCH" => self.client.patch(&url),
            "DELETE" => self.client.delete(&url),
            other => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("不支持的 method：{other}"),
                    false,
                ));
            }
        };
        if !hname.is_empty() {
            req = req.header(&hname, &hvalue);
        }
        for (k, v) in &svc.extra_headers {
            req = req.header(k, v);
        }
        if let Some(body) = args.get("body") {
            req = req.json(body);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let note = format!("请求失败：{e}");
                self.log_line(&svc.name, &method, path, None, 0, started.elapsed().as_millis() as i64, &note);
                return Err(ToolError::new(ErrorCode::Unavailable, svc.redact(&note), false));
            }
        };
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if status.is_redirection() {
            let note = format!("服务返回 {} 重定向（不跟随）", status.as_u16());
            self.log_line(&svc.name, &method, path, Some(status.as_u16()), 0, started.elapsed().as_millis() as i64, &note);
            return Err(ToolError::new(ErrorCode::Unavailable, note, false));
        }

        let mut body = Vec::new();
        let mut resp = resp;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    body.extend_from_slice(&chunk);
                    if body.len() >= MAX_RESP_BYTES {
                        body.truncate(MAX_RESP_BYTES);
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let note = format!("读取响应中断：{e}");
                    self.log_line(&svc.name, &method, path, Some(status.as_u16()), body.len(), started.elapsed().as_millis() as i64, &note);
                    return Err(ToolError::new(ErrorCode::Unavailable, svc.redact(&note), false));
                }
            }
        }
        let ms = started.elapsed().as_millis() as i64;
        self.log_line(
            &svc.name,
            &method,
            path,
            Some(status.as_u16()),
            body.len(),
            ms,
            if status.is_success() { "ok" } else { "http-error" },
        );

        let text = String::from_utf8_lossy(&body).to_string();
        if !status.is_success() {
            let hint = if status.as_u16() == 404 {
                "\n提示：路径可能不对。先取 `playbook(\"<服务名>-api\")` 看规范；\
不少服务只有单一入口（如 /api/agent/gateway）+ 请求体里的 api_name。"
            } else {
                ""
            };
            return Err(ToolError::new(
                ErrorCode::Unavailable,
                svc.redact(&format!(
                    "HTTP {} · {method} {url}\n{}{hint}",
                    status.as_u16(),
                    text.chars().take(800).collect::<String>()
                )),
                false,
            ));
        }

        // 统一策略：文本有预算，JSON 给结构，二进制只给元信息 + 路径
        let rendered = crate::payload::render(
            &body,
            ctype.as_deref(),
            &_ctx.workspace.root,
            &_ctx.call.0,
            None,
        );
        let mut out = format!(
            "{method} {} · HTTP {} · {} 字节 · {ms}ms\n\n",
            svc.name,
            status.as_u16(),
            body.len()
        );
        out.push_str(&rendered.text);
        Ok(ToolOutput {
            content: vec![Block::Paragraph(out)],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}
