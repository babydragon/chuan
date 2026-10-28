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
    /// 凭据文件里用哪个键（`key = value` 形式）；不写则取第 1 行
    pub token_key: Option<String>,
    /// 模板：`Authorization: Bearer {token}`；留空表示不注入请求头
    pub header: String,
    pub extra_headers: Vec<(String, String)>,
    /// 查询参数注入（值支持 `{token}` `{sign}` `{ts_s}` `{ts_ms}`）
    pub query: BTreeMap<String, String>,
    /// 请求体字段注入（飞书加签就放这里）
    pub body: BTreeMap<String, String>,
    /// HMAC-SHA256 签名（钉钉/飞书等 webhook）
    pub sign: Option<crate::auth::SignConfig>,
}

impl Default for Service {
    fn default() -> Self {
        Self {
            name: String::new(),
            base: String::new(),
            token_file: None,
            token_key: None,
            header: String::new(),
            extra_headers: Vec::new(),
            query: BTreeMap::new(),
            body: BTreeMap::new(),
            sign: None,
        }
    }
}

impl Service {
    fn secret_file(&self) -> Result<Option<crate::auth::SecretFile>, String> {
        match &self.token_file {
            Some(f) => crate::auth::SecretFile::load(f).map(Some),
            None => Ok(None),
        }
    }

    /// 解析本次请求的模板变量；签名在这里算（需要 secret 时缺了会明确报错）。
    fn vars(&self, path: &str) -> Result<crate::auth::Vars, String> {
        use crate::auth::Vars;
        let mut v = Vars::new(now_ms(), path);
        if let Some(f) = self.secret_file()? {
            // 允许主人直接粘整条 Webhook URL：这里归一成 token 本身
            v.token = f
                .get(self.token_key.as_deref(), 0)
                .map(|t| crate::auth::normalize_token(&t));
            if let Some(cfg) = &self.sign {
                v.secret = f.get(cfg.secret_key.as_deref(), 1);
            }
        }
        if let Some(cfg) = &self.sign {
            v.sign = Some(crate::auth::sign(cfg, &v)?);
        }
        Ok(v)
    }

    /// 生成注入后的请求头（名字, 值）。留空则不注入。
    fn header_of(&self, vars: &crate::auth::Vars) -> Result<Option<(String, String)>, String> {
        if self.header.trim().is_empty() {
            return Ok(None);
        }
        let rendered = vars.render(&self.header);
        let Some((name, value)) = rendered.split_once(':') else {
            return Err(format!("服务 {} 的 header 配置不合法（应为 `Name: 值`）", self.name));
        };
        let value = value.trim().to_owned();
        if value.is_empty() {
            return Ok(None);
        }
        Ok(Some((name.trim().to_owned(), value)))
    }

    /// 把文本里所有已知凭据值抹掉（响应回显、错误信息、审计日志都要过一遍）。
    fn scrub(&self, text: &str, extra: &[String]) -> String {
        let mut values: Vec<String> = extra.to_vec();
        if let Ok(Some(f)) = self.secret_file() {
            for line in f.raw_lines() {
                values.push(line.clone());
            }
        }
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        harness_kernel::leak::scrub_values(text, &refs)
    }

    /// 兼容旧调用：读凭据后把值抹掉。
    fn redact(&self, text: &str) -> String {
        self.scrub(text, &[])
    }
}

pub struct HttpServiceTool {
    services: std::sync::RwLock<BTreeMap<String, Service>>,
    client: reqwest::Client,
    log: PathBuf,
}

/// base 里自带的那段路径（`/` 视为没有）。用于检测"base 与 path 重复"。
fn base_path_suffix(base: &str) -> Option<String> {
    let u = reqwest::Url::parse(base).ok()?;
    let p = u.path().trim_end_matches('/');
    if p.is_empty() {
        None
    } else {
        Some(p.to_owned())
    }
}

/// 读 `/ws` 下的文件作为请求体：绝对路径、`..`、越界都拒绝，大小限 1MB。
fn read_workspace_file(root: &str, rel: &str) -> Result<Vec<u8>, String> {
    const MAX: u64 = 1024 * 1024;
    if rel.trim().is_empty() {
        return Err("body_file 不能为空".into());
    }
    if rel.starts_with('/') || rel.contains("..") {
        return Err("body_file 必须是 /ws 下的相对路径（例如 msg.json）".into());
    }
    let root = std::path::Path::new(root)
        .canonicalize()
        .map_err(|e| format!("workspace 不可用：{e}"))?;
    let full = root.join(rel);
    let canon = full
        .canonicalize()
        .map_err(|e| format!("读不到 body_file {rel}：{e}"))?;
    if !canon.starts_with(&root) {
        return Err("body_file 越出 workspace".into());
    }
    let meta = std::fs::metadata(&canon).map_err(|e| format!("读不到 body_file {rel}：{e}"))?;
    if meta.len() > MAX {
        return Err(format!("body_file 太大（{} 字节，上限 {MAX}）", meta.len()));
    }
    std::fs::read(&canon).map_err(|e| format!("读不到 body_file {rel}：{e}"))
}

impl HttpServiceTool {
    pub fn new(
        services: Vec<Service>,
        log: impl Into<PathBuf>,
    ) -> Result<Arc<Self>, String> {
        Self::with_proxy(services, log, &crate::proxy::ProxyConfig::default())
    }

    /// `services = always` 时才走代理；默认直连（国内服务绕代理反而慢）。
    pub fn with_proxy(
        services: Vec<Service>,
        log: impl Into<PathBuf>,
        proxy: &crate::proxy::ProxyConfig,
    ) -> Result<Arc<Self>, String> {
        let mut b = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            // 不跟重定向：避免被跳到别的域名（token 只该发给配置的那个 host）
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("chuan/0.1");
        b = match (proxy.services, proxy.reqwest()) {
            (crate::proxy::Mode::Always, Some(p)) => b.proxy(p),
            _ => b.no_proxy(),
        };
        let client = b.build().map_err(|e| format!("HTTP 客户端初始化失败：{e}"))?;
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
        if let Ok(vars) = svc.vars("/") {
            if let Ok(Some((k, v))) = svc.header_of(&vars) {
                req = req.header(&k, &v);
            }
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
可用服务：{}。\n**如果存在 `<服务名>-api` 的 playbook，先取它**（取不到就跳过，别反复试）；\
这类服务通常只有一个入口路径（例如 `/api/agent/gateway`）+ 请求体内指定 `api_name`，\
不要凭直觉拼 REST 路径（会 404）。只给 path，不要自己拼域名。\
注意：`kind = \"mcp\"` 的服务要用 `mcp(server=…, tool=…)`，不是这个工具。",
                if self.names().is_empty() { "（未配置）".to_owned() } else { self.names().join("、") }
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "description": "服务名，见工具说明" },
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                    "path": { "type": "string", "description": "以 / 开头的路径，可带 query" },
                    "body": { "type": "object", "description": "JSON 请求体（可选）" },
                    "body_file": { "type": "string", "description": "从 /ws 下的文件读请求体（脚本生成内容、宿主负责发；仅 JSON）" }
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

        // 模板变量 + 签名：`{token}` 可以出现在 path（飞书 hook id 就在路径里）、
        // query、body、header 里；值只在宿主侧替换，模型给的是占位符。
        let vars = match svc.vars(path) {
            Ok(v) => v,
            Err(e) => {
                return Err(ToolError::new(ErrorCode::Unavailable, svc.redact(&e), false));
            }
        };
        let path_rendered = vars.render(path);
        if path_rendered.contains("://") || path_rendered.contains("..") {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "渲染后的 path 不合法", false));
        }
        // base 带了路径、path 又写一遍 → 拼出 /x/x，必然是 404。
        // 提前讲清楚，别让人对着 404 猜（板上真实踩过：/robot/send/robot/send）。
        if let Some(suffix) = base_path_suffix(&svc.base) {
            if path_rendered == suffix || path_rendered.starts_with(&format!("{suffix}/")) {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!(
                        "路径重复：base 已经以 {suffix} 结尾，path 又写了 {path_rendered}，\
                         会拼成 {suffix}{path_rendered}。\n\
                         约定：base 只到域名（scheme://host[:port]），入口路径全部放进 path。"
                    ),
                    false,
                ));
            }
        }
        let mut url = format!("{}{}", svc.base.trim_end_matches('/'), path_rendered);
        if !svc.query.is_empty() {
            let mut u = match reqwest::Url::parse(&url) {
                Ok(u) => u,
                Err(e) => {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("base 或 path 拼不出合法 URL：{e}"),
                        false,
                    ));
                }
            };
            {
                let mut qp = u.query_pairs_mut();
                for (k, tpl) in &svc.query {
                    qp.append_pair(k, &vars.render(tpl));
                }
            }
            url = u.to_string();
        }
        // 审计日志里的 path 也要抹掉值（Feishu 的 hook id 就在路径上）
        let log_path = svc.scrub(&path_rendered, &vars.secrets());

        let (hname, hvalue) = match svc.header_of(&vars) {
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
        // 请求体：调用方给的对象 + 服务配置里的注入字段（飞书加签）；
        // 也可以从 /ws 读文件（脚本生成内容、宿主负责发）。
        let mut body_json = args.get("body").cloned();
        if !svc.body.is_empty() {
            let mut obj = match body_json.take() {
                Some(serde_json::Value::Object(m)) => m,
                Some(_) => {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        "配置了 body 注入的服务只能接受 JSON 对象作为请求体",
                        false,
                    ));
                }
                None => serde_json::Map::new(),
            };
            for (k, tpl) in &svc.body {
                obj.insert(k.clone(), serde_json::Value::String(vars.render(tpl)));
            }
            body_json = Some(serde_json::Value::Object(obj));
        }
        let mut raw_body: Option<Vec<u8>> = None;
        if let Some(fp) = args.get("body_file").and_then(|v| v.as_str()) {
            match read_workspace_file(&_ctx.workspace.root, fp) {
                Ok(bytes) => raw_body = Some(bytes),
                Err(e) => {
                    return Err(ToolError::new(ErrorCode::InvalidArgs, e, false));
                }
            }
        }
        if let Some(body) = body_json {
            req = req.json(&body);
        } else if let Some(bytes) = raw_body {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let note = format!("请求失败：{e}");
                self.log_line(&svc.name, &method, &log_path, None, 0, started.elapsed().as_millis() as i64, &note);
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
            self.log_line(&svc.name, &method, &log_path, Some(status.as_u16()), 0, started.elapsed().as_millis() as i64, &note);
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
                    self.log_line(&svc.name, &method, &log_path, Some(status.as_u16()), body.len(), started.elapsed().as_millis() as i64, &note);
                    return Err(ToolError::new(ErrorCode::Unavailable, svc.redact(&note), false));
                }
            }
        }
        let ms = started.elapsed().as_millis() as i64;
        self.log_line(
            &svc.name,
            &method,
            &log_path,
            Some(status.as_u16()),
            body.len(),
            ms,
            if status.is_success() { "ok" } else { "http-error" },
        );

        let text = svc.scrub(&String::from_utf8_lossy(&body), &vars.secrets());
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
            text.as_bytes(),
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
