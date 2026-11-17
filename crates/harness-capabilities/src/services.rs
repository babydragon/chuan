//! 命名 HTTP 服务：`http(service, method, path, …)`。
//!
//! 解决的是"带鉴权的第三方 API"：滴答、微信读书这类。
//! 三条硬规则：
//! 1. **token 只在宿主注入**，模型既看不到也写不进 URL / 日志 / 技能里；
//! 2. 只能访问该服务配置的 base host（模型给的是 path，不是完整 URL）；
//! 3. 每次调用写审计日志（服务名、方法、路径、状态、耗时），**不记 header、不记 body**。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;


const MAX_RESP_BYTES: usize = 2 * 1024 * 1024;
/// `save_to` 落盘上限：够放一本教材，又不至于把 8GB 单板机的盘写满。
/// 时间维度不设总时限，靠空闲超时兜底（见 `Timeouts`）。
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

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
    /// `save_to` 允许跟随的重定向主机（`*.aliyundrive.com` 这类通配）。
    /// 空 = 不跟随任何重定向（默认最严）。
    pub redirect_hosts: Vec<String>,
    /// 跟随重定向时，**按目标主机**补的静态头。
    /// 不能挂在 service 上：同一个 alist 后面挂着阿里云和百度，
    /// 把百度的 UA 也发给阿里云就是错的（见 D32）。
    pub redirect_rules: Vec<RedirectRule>,
    /// 是否允许技能脚本经宿主网关调用（D41）。默认 false。
    /// 打开后技能仍看不到凭据：网关只在宿主侧注入，见 `skill_gateway`。
    pub expose_to_skills: bool,
    /// 技能可用的 HTTP 方法。空 = 不允许（fail closed）。
    pub allow_methods: Vec<String>,
    /// 技能可用的路径前缀（必须以 `/` 开头；空 = 不允许）。
    pub allow_paths: Vec<String>,
    /// 技能可设置的请求头及取值白名单（名字不区分大小写，取值精确匹配）。
    /// 鉴权头不在这里——它由 `header` 模板在宿主侧注入，调用方永远设不了。
    pub header_allow: BTreeMap<String, Vec<String>>,
}

/// 一条"跳到这个主机时补这些头"的规则。
#[derive(Clone, Debug, Default)]
pub struct RedirectRule {
    /// 目标主机通配（与 `redirect_hosts` 同一套匹配规则）；空 = 永不命中
    pub hosts: Vec<String>,
    /// **字面值**，不做 `{token}` 渲染——结构上就不可能把凭据带到第二跳
    pub headers: Vec<(String, String)>,
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
            redirect_hosts: Vec::new(),
            redirect_rules: Vec::new(),
            expose_to_skills: false,
            allow_methods: Vec::new(),
            allow_paths: Vec::new(),
            header_allow: BTreeMap::new(),
        }
    }
}

/// 技能网关的一次请求（UDS 上的一行 JSON）。
#[derive(Clone, Debug, serde::Deserialize)]
pub struct GatewayRequest {
    #[serde(default = "gateway_default_method")]
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
}

fn gateway_default_method() -> String {
    "POST".into()
}

/// 网关的一次应答。上游的 4xx/5xx 也走这里（脚本需要看到状态），
/// 只有网络/协议层失败才返回 `Err`。
#[derive(Clone, Debug, serde::Serialize)]
pub struct GatewayReply {
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub body: String,
}

impl Service {
    /// 审批卡与技能清单里用的授权摘要：**核心字段必须能被人看懂**（D41）。
    pub fn skill_grant(&self) -> String {
        let mut out = format!("服务 `{}` → {}", self.name, self.base);
        out.push_str(&format!(
            "\n允许：{} {}",
            if self.allow_methods.is_empty() {
                "（未开放方法）".to_owned()
            } else {
                self.allow_methods.join(" / ")
            },
            if self.allow_paths.is_empty() {
                "（未开放路径）".to_owned()
            } else {
                self.allow_paths.join("、")
            }
        ));
        if self.header_allow.is_empty() {
            out.push_str("\n可设置的头：无（脚本只能给路径与请求体）");
        } else {
            let actions: Vec<String> = self
                .header_allow
                .iter()
                .map(|(k, v)| format!("{k} = {}", v.join("、")))
                .collect();
            out.push_str(&format!("\n可设置的头：{}", actions.join("；")));
        }
        if self.header.trim().is_empty() {
            out.push_str("\n鉴权：由宿主注入，技能看不到凭据");
        } else {
            let name = self.header.split(':').next().unwrap_or("").trim();
            out.push_str(&format!(
                "\n鉴权：宿主向 `{name}` 注入凭据，技能看不到值"
            ));
        }
        out
    }

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
    /// 普通 API 调用：短超时，卡住就失败。
    client: reqwest::Client,
    /// 上传/下载（`upload_from` / `save_to`）：不设总超时，只按"有没有新数据"判死。
    /// 两者必须分开——`reqwest` 的 `.timeout()` 覆盖建连到读完响应体的全过程。
    transfer_client: reqwest::Client,
    log: PathBuf,
}

/// 两个 client 的超时策略。
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// 非下载请求的总超时（含读完响应体）。
    pub api_total: Duration,
    /// 下载请求允许的**空闲**时长：连续这么久收不到新字节才断。
    pub download_idle: Option<Duration>,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            api_total: Duration::from_secs(30),
            download_idle: Some(Duration::from_secs(60)),
        }
    }
}

/// 请求头统一走 `HeaderMap::insert`（同名覆盖）。
/// **不要**用 `RequestBuilder::header`：它是 append，Content-Type 会出现两个值。
fn insert_header(
    map: &mut reqwest::header::HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), String> {
    let name = name.trim();
    let n = reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| format!("请求头名 `{name}` 不合法：{e}"))?;
    let v = reqwest::header::HeaderValue::from_str(value)
        .map_err(|e| format!("请求头 `{name}` 的值不合法：{e}"))?;
    map.insert(n, v);
    Ok(())
}

/// 技能网关的路径白名单：以 `/` 结尾的条目按前缀匹配，其余按精确匹配。
pub(crate) fn gateway_path_allowed(path: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|p| {
        if p.ends_with('/') {
            path.starts_with(p.as_str())
        } else {
            path == p
        }
    })
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
    let canon = resolve_workspace_file(root, rel, "body_file")?;
    let meta = std::fs::metadata(&canon).map_err(|e| format!("读不到 body_file {rel}：{e}"))?;
    if meta.len() > MAX {
        return Err(format!("body_file 太大（{} 字节，上限 {MAX}）", meta.len()));
    }
    std::fs::read(&canon).map_err(|e| format!("读不到 body_file {rel}：{e}"))
}

/// `/ws` 下的文件解析成绝对路径：挡掉绝对路径、`..`、软链接越界。
fn resolve_workspace_file(root: &str, rel: &str, what: &str) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err(format!("{what} 不能为空"));
    }
    if rel.starts_with('/') || rel.contains("..") {
        return Err(format!("{what} 必须是 /ws 下的相对路径（例如 msg.json）"));
    }
    let root = std::path::Path::new(root)
        .canonicalize()
        .map_err(|e| format!("workspace 不可用：{e}"))?;
    let full = root.join(rel);
    let canon = full
        .canonicalize()
        .map_err(|e| format!("读不到 {what} {rel}：{e}"))?;
    if !canon.starts_with(&root) {
        return Err(format!("{what} 越出 workspace"));
    }
    Ok(canon)
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
        Self::with_limits(services, log, proxy, Timeouts::default())
    }

    /// 超时可注入：测试用它把"总超时"缩到毫秒级，生产用 `Timeouts::default()`。
    pub fn with_limits(
        services: Vec<Service>,
        log: impl Into<PathBuf>,
        proxy: &crate::proxy::ProxyConfig,
        limits: Timeouts,
    ) -> Result<Arc<Self>, String> {
        let build = |total: Option<Duration>, idle: Option<Duration>| -> Result<reqwest::Client, String> {
            let mut b = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                // 不跟重定向：避免被跳到别的域名（token 只该发给配置的那个 host）
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("chuan/0.1");
            if let Some(t) = total {
                b = b.timeout(t);
            }
            if let Some(i) = idle {
                b = b.read_timeout(i);
            }
            b = match (proxy.services, proxy.reqwest()) {
                (crate::proxy::Mode::Always, Some(p)) => b.proxy(p),
                _ => b.no_proxy(),
            };
            b.build().map_err(|e| format!("HTTP 客户端初始化失败：{e}"))
        };
        // 下载不设总超时：上限是 512MB，用 30 秒总时限去卡它就是自相矛盾
        // （22MB 在 20KB/s 上会"下到一半就断"，看起来像网络问题）。
        // 改成空闲超时后，只要还有新字节就一直走，真断了也只是这一次失败。
        let client = build(Some(limits.api_total), None)?;
        let transfer_client = build(None, limits.download_idle)?;
        Ok(Arc::new(Self {
            services: std::sync::RwLock::new(
                services.into_iter().map(|s| (s.name.clone(), s)).collect(),
            ),
            client,
            transfer_client,
            log: log.into(),
        }))
    }

    pub fn names(&self) -> Vec<String> {
        self.services.read().unwrap().keys().cloned().collect()
    }

    /// 取一个已注册的服务（`service` 工具用它检查"重加会不会清掉边界配置"）。
    pub fn get(&self, name: &str) -> Option<Service> {
        self.services.read().unwrap().get(name).cloned()
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

    /// 技能网关专用：一次 JSON 请求，**不**支持下载/上传/重定向。
    ///
    /// 调用方（沙盒里的脚本）只能给 method / path / 白名单内的头 / JSON 体；
    /// 目的主机、鉴权头、固定头都由配置决定，凭据值只在这一侧出现（D41）。
    pub(crate) async fn gateway_send(
        &self,
        svc: &Service,
        req: GatewayRequest,
    ) -> Result<GatewayReply, String> {
        const MAX_BODY: usize = 64 * 1024;
        const MAX_RESP: usize = 1024 * 1024;

        let method = req.method.trim().to_ascii_uppercase();
        if !svc.allow_methods.iter().any(|m| m.eq_ignore_ascii_case(&method)) {
            return Err(format!(
                "服务 `{}` 没有对技能开放方法 {method}（已开放：{}）",
                svc.name,
                if svc.allow_methods.is_empty() {
                    "无".to_owned()
                } else {
                    svc.allow_methods.join("、")
                }
            ));
        }

        let path = req.path.trim();
        if !path.starts_with('/') || path.contains("://") || path.contains("..") {
            return Err("path 必须以 / 开头，且不能带协议、域名或 ..".into());
        }
        if !gateway_path_allowed(path, &svc.allow_paths) {
            return Err(format!(
                "服务 `{}` 没有对技能开放路径 {path}（已开放：{}）",
                svc.name,
                if svc.allow_paths.is_empty() {
                    "无".to_owned()
                } else {
                    svc.allow_paths.join("、")
                }
            ));
        }

        let mut req_headers: Vec<(String, String)> = Vec::new();
        for (k, v) in &req.headers {
            let lk = k.trim().to_ascii_lowercase();
            if matches!(
                lk.as_str(),
                "authorization"
                    | "proxy-authorization"
                    | "cookie"
                    | "set-cookie"
                    | "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
            ) {
                return Err(format!("请求头 `{k}` 不能由技能设置：鉴权与长度由宿主决定"));
            }
            let Some(allowed) = svc
                .header_allow
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(&lk))
                .map(|(_, v)| v)
            else {
                return Err(format!(
                    "服务 `{}` 不允许技能设置请求头 `{k}`（可设置：{}）",
                    svc.name,
                    if svc.header_allow.is_empty() {
                        "无".to_owned()
                    } else {
                        svc.header_allow.keys().cloned().collect::<Vec<_>>().join("、")
                    }
                ));
            };
            if !allowed.iter().any(|x| x.trim() == v.trim()) {
                return Err(format!("请求头 `{k}` 的取值不在允许清单里"));
            }
            req_headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }

        // 模板变量：只在这个进程里解析，返回给脚本之前还要过 scrub
        let vars = svc.vars(path)?;
        let path_rendered = vars.render(path);
        if path_rendered.contains("://") || path_rendered.contains("..") {
            return Err("渲染后的 path 不合法".into());
        }
        let mut url = format!("{}{}", svc.base.trim_end_matches('/'), path_rendered);
        if !svc.query.is_empty() {
            let mut u = reqwest::Url::parse(&url).map_err(|e| format!("base 或 path 拼不出合法 URL：{e}"))?;
            {
                let mut qp = u.query_pairs_mut();
                for (k, tpl) in &svc.query {
                    qp.append_pair(k, &vars.render(tpl));
                }
            }
            url = u.to_string();
        }

        let (hname, hvalue) = match svc.header_of(&vars)? {
            Some((k, v)) => (k, v),
            None => (String::new(), String::new()),
        };

        let mut body_json = req.body.clone();
        if !svc.body.is_empty() {
            let mut obj = match body_json.take() {
                Some(serde_json::Value::Object(m)) => m,
                Some(_) => return Err("配置了请求体注入的服务只接受 JSON 对象".into()),
                None => serde_json::Map::new(),
            };
            for (k, tpl) in &svc.body {
                obj.insert(k.clone(), serde_json::Value::String(vars.render(tpl)));
            }
            body_json = Some(serde_json::Value::Object(obj));
        }
        if let Some(b) = &body_json {
            let n = serde_json::to_vec(b).map(|v| v.len()).unwrap_or(0);
            if n > MAX_BODY {
                return Err(format!("请求体太大（{n} 字节，上限 {MAX_BODY}）"));
            }
        }

        let started = Instant::now();
        let mut req = match method.as_str() {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "PATCH" => self.client.patch(&url),
            "DELETE" => self.client.delete(&url),
            other => return Err(format!("不支持的 method：{other}")),
        };
        if let Some(b) = &body_json {
            req = req.json(b);
        }
        // 请求头统一走 HeaderMap：reqwest 的 `.header()` 是 **append**，
        // 对 Content-Type 这类单值头会产生重复值（nginx 直接 400）。
        // `insert` = 同名覆盖，配置的 Content-Type 压过 `.json()` 的默认值。
        let mut hdrs = reqwest::header::HeaderMap::new();
        if !hname.is_empty() {
            insert_header(&mut hdrs, &hname, &hvalue)?;
        }
        for (k, v) in svc.extra_headers.iter().chain(req_headers.iter()) {
            insert_header(&mut hdrs, k, v)?;
        }
        req = req.headers(hdrs);

        let mut log_path = svc.scrub(&path_rendered, &vars.secrets());
        if let Some((p, _)) = log_path.split_once('?') {
            log_path = p.to_owned();
        }

        let mut resp = match tokio::time::timeout(Duration::from_secs(20), req.send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let note = format!("请求失败：{e}");
                self.log_line(
                    &svc.name,
                    &method,
                    &log_path,
                    None,
                    0,
                    started.elapsed().as_millis() as i64,
                    "gateway 请求失败",
                );
                return Err(svc.redact(&note));
            }
            Err(_) => {
                self.log_line(
                    &svc.name,
                    &method,
                    &log_path,
                    None,
                    0,
                    started.elapsed().as_millis() as i64,
                    "gateway 超时",
                );
                return Err("服务 20 秒内没有响应".into());
            }
        };
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut buf = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    buf.extend_from_slice(&chunk);
                    if buf.len() >= MAX_RESP {
                        buf.truncate(MAX_RESP);
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    self.log_line(
                        &svc.name,
                        &method,
                        &log_path,
                        Some(status),
                        buf.len(),
                        started.elapsed().as_millis() as i64,
                        "gateway 读取中断",
                    );
                    return Err(svc.redact(&format!("读取响应中断：{e}")));
                }
            }
        }
        self.log_line(
            &svc.name,
            &method,
            &log_path,
            Some(status),
            buf.len(),
            started.elapsed().as_millis() as i64,
            "gateway",
        );
        Ok(GatewayReply {
            status,
            content_type,
            body: svc.scrub(&String::from_utf8_lossy(&buf), &vars.secrets()),
        })
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

    /// 把响应体流式落到 `/ws`，返回路径与 sha256；签名 URL 与 body 都不进上下文。
    async fn save_body(
        &self,
        svc: &Service,
        log_path: &str,
        mut resp: reqwest::Response,
        rel: &str,
        workspace: &str,
        started: Instant,
    ) -> Result<ToolOutput, ToolError> {
        let status = resp.status();
        let final_path = Path::new(workspace).join(rel);
        if let Some(dir) = final_path.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(|e| {
                ToolError::new(ErrorCode::Unavailable, format!("建目录失败：{e}"), false)
            })?;
        }
        let tmp = final_path.with_file_name(format!(
            ".{}.part",
            final_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "download".into())
        ));
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| {
            ToolError::new(ErrorCode::Unavailable, format!("写文件失败：{e}"), false)
        })?;
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    total += chunk.len() as u64;
                    if total > MAX_DOWNLOAD_BYTES {
                        let _ = tokio::fs::remove_file(&tmp).await;
                        let note = format!("save_to 超过 {} MB 上限", MAX_DOWNLOAD_BYTES / 1024 / 1024);
                        self.log_line(&svc.name, "GET", log_path, Some(status.as_u16()), 0, started.elapsed().as_millis() as i64, &note);
                        return Err(ToolError::new(ErrorCode::InvalidArgs, note, false));
                    }
                    hasher.update(&chunk);
                    if let Err(e) = file.write_all(&chunk).await {
                        let _ = tokio::fs::remove_file(&tmp).await;
                        return Err(ToolError::new(
                            ErrorCode::Unavailable,
                            format!("写文件失败：{e}"),
                            false,
                        ));
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    let note = format!("下载中断：{e}");
                    self.log_line(&svc.name, "GET", log_path, Some(status.as_u16()), total as usize, started.elapsed().as_millis() as i64, &note);
                    return Err(ToolError::new(ErrorCode::Unavailable, svc.redact(&note), false));
                }
            }
        }
        if let Err(e) = file.flush().await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(ToolError::new(ErrorCode::Unavailable, format!("写文件失败：{e}"), false));
        }
        drop(file);
        if let Err(e) = tokio::fs::rename(&tmp, &final_path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(ToolError::new(ErrorCode::Unavailable, format!("落盘失败：{e}"), false));
        }
        let digest = hex(hasher.finalize());
        self.log_line(
            &svc.name,
            "GET",
            log_path,
            Some(status.as_u16()),
            total as usize,
            started.elapsed().as_millis() as i64,
            &format!("save_to={rel} sha256={digest}"),
        );
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已保存 /ws/{rel}（{} KB，sha256 {digest}）",
                total / 1024
            ))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

/// `save_to` 只允许 `/ws` 下的相对路径（与 `body_file` 同一条边界）。
fn check_save_path(rel: &str) -> Result<(), String> {
    if rel.starts_with('/') || rel.starts_with('\\') || rel.contains("..") {
        return Err("save_to 必须是 /ws 下的相对路径：不能以 / 开头，不能含 ..".into());
    }
    Ok(())
}

/// JSON/文本响应不该落盘：误加 `save_to` 会把接口结果悄悄写进文件。
fn looks_binary(ctype: Option<&str>) -> bool {
    let Some(ct) = ctype else { return true };
    let c = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if c.is_empty() {
        return true;
    }
    !(c.starts_with("text/")
        || c == "application/json"
        || c == "application/xml"
        || c.ends_with("+json")
        || c.ends_with("+xml"))
}

/// 错误响应体只取前 `limit` 字节：够看清原因，又不把上游的整页 HTML 塞进上下文。
/// 读不到就当没有——诊断信息不该盖住真正的错误。
async fn body_snippet(resp: &mut reqwest::Response, limit: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        let take = limit.saturating_sub(buf.len()).min(chunk.len());
        buf.extend_from_slice(&chunk[..take]);
        if buf.len() >= limit {
            break;
        }
    }
    String::from_utf8_lossy(&buf).trim().to_owned()
}

/// 相对 Location 按 base 解析；只接受 http(s)。
fn resolve_target(base: &str, location: &str) -> Option<String> {
    let base = reqwest::Url::parse(base).ok()?;
    let url = base.join(location).ok()?;
    match url.scheme() {
        "http" | "https" => Some(url.to_string()),
        _ => None,
    }
}

/// 主机白名单：`*.aliyundrive.com` 匹配子域与本身，其它按完整主机名精确匹配。
fn host_allowed(url: &str, allow: &[String]) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let Some(host) = u.host_str() else { return false };
    let host = host.to_ascii_lowercase();
    allow.iter().any(|p| {
        let p = p.trim().to_ascii_lowercase();
        match p.strip_prefix("*.") {
            Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
            None => host == p,
        }
    })
}

/// 第二跳要补的头：**只取目标主机命中的规则**。
///
/// 为什么按主机而不是按 service：一个 alist 后面可能同时挂着阿里云和百度，
/// 百度的 `User-Agent: pan.baidu.com` 发给阿里云就是错的；反过来漏带又是 403。
fn redirect_headers_for(target: &str, svc: &Service) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for rule in &svc.redirect_rules {
        if !host_allowed(target, &rule.hosts) {
            continue;
        }
        for (name, value) in &rule.headers {
            let lower = name.trim().to_ascii_lowercase();
            // 第二跳不带凭据是硬边界（D32）。值虽然是宿主写的字面量，
            // 但"写死一个 token"和"注入一个 token"对日志/审计是一回事，直接拒绝。
            if matches!(
                lower.as_str(),
                "authorization" | "cookie" | "proxy-authorization" | "set-cookie"
            ) {
                return Err(format!(
                    "服务 `{}` 的重定向规则里不允许 `{name}`：\
                     第二跳只补不需要凭据的头（如 User-Agent / Referer），凭据只经 token_file 注入",
                    svc.name
                ));
            }
            out.push((name.trim().to_owned(), value.clone()));
        }
    }
    Ok(out)
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.as_ref().len() * 2);
    for b in bytes.as_ref() {
        let _ = write!(s, "{b:02x}");
    }
    s
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
取**文件本体**（二进制）用 `save_to`：宿主负责跟随已授权的重定向并流式落盘，\
临时凭证不会进上下文；也不要用 `lan_exec` + curl 自己拼。\
发**文件本体**用 `upload_from`（PUT/POST）+ `headers`（如 alist 的 `File-Path`），\
同样是宿主流式发送、token 只由宿主注入；不要为上传去用 host/lan_exec + curl。\
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
                    "body_file": { "type": "string", "description": "从 /ws 下的文件读请求体（脚本生成内容、宿主负责发；仅 JSON，≤1MB）" },
                    "upload_from": {
                        "type": "string",
                        "description": "把 /ws 下的**文件本体**流式上传（PUT/POST）。大小以 Content-Length 显式给出，不走内存；上限 512MB。不要和 body / body_file / save_to 同时用"
                    },
                    "headers": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "额外的请求头（可选，原样发送，不做模板替换）。禁止 authorization / cookie / host / content-length / transfer-encoding"
                    },
                    "save_to": { "type": "string", "description": "把**文件本体**流式存到 /ws 下的相对路径。响应是 3xx 时由宿主跟随一次已授权的重定向，临时凭证不会交给你；上限 512MB。只用于二进制响应，JSON/文本接口不要加这个参数" }
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
        let save_to = args
            .get("save_to")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let upload_from = args
            .get("upload_from")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if save_to.is_some() && upload_from.is_some() {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "upload_from 与 save_to 不能同时用（一次请求只有一个方向）",
                false,
            ));
        }
        if let Some(rel) = &save_to {
            check_save_path(rel).map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
            // 取文件本体只有 GET 一种形态；POST 加 save_to 会把"接口调用"变成"写文件"，难排查。
            if method != "GET" {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    "save_to 只支持 GET（取文件本体；接口调用不要加这个参数）",
                    false,
                ));
            }
        }
        if let Some(rel) = &upload_from {
            check_save_path(rel).map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
            if method != "PUT" && method != "POST" {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    "upload_from 只支持 PUT / POST（上传的是文件本体，不是查询）",
                    false,
                ));
            }
            if args.get("body").is_some() || args.get("body_file").is_some() {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    "upload_from 与 body / body_file 不能同时用（请求体就是那个文件）",
                    false,
                ));
            }
        }
        // 模型给的请求头：原样发送、不做模板替换；凭据类的名字一律拒绝，
        // 因为"第二跳/请求头里带 token"是这套工具的硬边界（D9/D32）。
        let mut req_headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(m) = args.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in m {
                let Some(val) = v.as_str() else {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("headers.{k} 必须是字符串"),
                        false,
                    ));
                };
                let key = k.trim().to_ascii_lowercase();
                if matches!(
                    key.as_str(),
                    "authorization"
                        | "cookie"
                        | "proxy-authorization"
                        | "host"
                        | "content-length"
                        | "transfer-encoding"
                        | "connection"
                ) {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("headers 里不允许 `{k}`：鉴权与长度由宿主决定"),
                        false,
                    ));
                }
                req_headers.insert(key, val.to_owned());
            }
        }
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
        let mut log_path = svc.scrub(&path_rendered, &vars.secrets());
        if save_to.is_some() {
            // 下载的 query 往往是签名（`?sign=…`）：拿到它就能换到文件本体，
            // 属于"能取到这个文件"的凭证。日志只留键、不留值（D32）。
            match log_path.split_once('?') {
                Some((p, q)) if !q.is_empty() => log_path = format!("{p}?…"),
                _ => {}
            }
        }

        let (hname, hvalue) = match svc.header_of(&vars) {
            Ok(Some(h)) => h,
            Ok(None) => (String::new(), String::new()),
            Err(e) => {
                return Err(ToolError::new(ErrorCode::Unavailable, svc.redact(&e), false));
            }
        };

        let started = Instant::now();
        // `save_to` / `upload_from` 是一次大流量传输：从一开始就用不设总超时的 client。
        // 否则 alist 开 Web 代理时，第一跳本身就是整份文件。
        let bulk = save_to.is_some() || upload_from.is_some();
        let client = if bulk { &self.transfer_client } else { &self.client };
        let mut req = match method.as_str() {
            "GET" => client.get(&url),
            "POST" => client.post(&url),
            "PUT" => client.put(&url),
            "PATCH" => client.patch(&url),
            "DELETE" => client.delete(&url),
            other => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("不支持的 method：{other}"),
                    false,
                ));
            }
        };
        let mut uploaded: Option<(String, u64)> = None;
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
        } else if let Some(rel) = &upload_from {
            // 文件本体流式上传：Content-Length 由宿主按文件大小写死，
            // 不把整份文件读进内存（单板机 MemoryMax=1G，512MB 上限不能靠 buffer 兜）。
            let path_on_disk = match resolve_workspace_file(&_ctx.workspace.root, rel, "upload_from") {
                Ok(p) => p,
                Err(e) => return Err(ToolError::new(ErrorCode::InvalidArgs, e, false)),
            };
            let meta = match tokio::fs::metadata(&path_on_disk).await {
                Ok(m) if m.is_file() => m,
                Ok(_) => {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("upload_from 不是普通文件：{rel}"),
                        false,
                    ));
                }
                Err(e) => {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("读不到 upload_from {rel}：{e}"),
                        false,
                    ));
                }
            };
            if meta.len() > MAX_DOWNLOAD_BYTES {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!(
                        "upload_from 太大（{} 字节，上限 {} MB）",
                        meta.len(),
                        MAX_DOWNLOAD_BYTES / 1024 / 1024
                    ),
                    false,
                ));
            }
            let file = match tokio::fs::File::open(&path_on_disk).await {
                Ok(f) => f,
                Err(e) => {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("打不开 upload_from {rel}：{e}"),
                        false,
                    ));
                }
            };
            let ctype = if req_headers.contains_key("content-type") {
                None
            } else {
                Some("application/octet-stream")
            };
            if let Some(c) = ctype {
                req = req.header(reqwest::header::CONTENT_TYPE, c);
            }
            req = req
                .header(reqwest::header::CONTENT_LENGTH, meta.len())
                .body(reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(
                    file,
                )));
            uploaded = Some((rel.clone(), meta.len()));
        }

        // 请求头统一走 HeaderMap：reqwest 的 `.header()` 是 **append**，对 Content-Type
        // 这类单值头会产生重复值（JNAP 后面的 nginx 会直接回 400）。
        // `insert` = 同名覆盖：配置的 Content-Type 压过 `.json()` 的默认值，
        // 调用方显式给的头最后覆盖同名项（已过名字白名单检查，不做模板替换）。
        let mut hdrs = reqwest::header::HeaderMap::new();
        if !hname.is_empty() {
            insert_header(&mut hdrs, &hname, &hvalue).map_err(|e| {
                ToolError::new(ErrorCode::InvalidArgs, svc.scrub(&e, &vars.secrets()), false)
            })?;
        }
        for (k, v) in &svc.extra_headers {
            insert_header(&mut hdrs, k, v).map_err(|e| {
                ToolError::new(ErrorCode::InvalidArgs, svc.scrub(&e, &vars.secrets()), false)
            })?;
        }
        for (k, v) in &req_headers {
            insert_header(&mut hdrs, k, v).map_err(|e| {
                ToolError::new(ErrorCode::InvalidArgs, svc.scrub(&e, &vars.secrets()), false)
            })?;
        }
        req = req.headers(hdrs);

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
            let Some(rel) = save_to.clone() else {
                let note = format!("服务返回 {} 重定向（不跟随）", status.as_u16());
                self.log_line(&svc.name, &method, &log_path, Some(status.as_u16()), 0, started.elapsed().as_millis() as i64, &note);
                return Err(ToolError::new(ErrorCode::Unavailable, note, false));
            };
            // `save_to` 时在宿主侧跳一次：目标主机必须在服务白名单里，
            // 第二跳**不带服务鉴权头**（签名 URL 自带凭证），只补该主机明确声明的静态头
            // （百度 >20MB 必须 `User-Agent: pan.baidu.com`），也永不回显。
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let Some(location) = location else {
                return Err(ToolError::new(
                    ErrorCode::Unavailable,
                    format!("服务返回 {} 重定向，但没有 Location，取不到文件", status.as_u16()),
                    false,
                ));
            };
            let Some(target) = resolve_target(&svc.base, &location) else {
                return Err(ToolError::new(
                    ErrorCode::Unavailable,
                    "重定向目标不是合法的 http(s) 地址",
                    false,
                ));
            };
            if !host_allowed(&target, &svc.redirect_hosts) {
                let host = reqwest::Url::parse(&target)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default();
                return Err(ToolError::new(
                    ErrorCode::NotPermitted,
                    format!(
                        "重定向目标未授权（主机 {host}）。在服务 `{}` 的配置里加 \
                         redirect_hosts = [\"*.example.com\"] 才允许跟随。",
                        svc.name
                    ),
                    false,
                ));
            }
            let hop_headers = match redirect_headers_for(&target, svc) {
                Ok(v) => v,
                Err(e) => {
                    return Err(ToolError::new(ErrorCode::Unavailable, svc.redact(&e), false));
                }
            };
            let mut hop = self.transfer_client.get(&target);
            for (k, v) in &hop_headers {
                hop = hop.header(k, v);
            }
            let resp = match hop.send().await {
                Ok(r) => r,
                Err(e) => {
                    self.log_line(&svc.name, &method, &log_path, None, 0, started.elapsed().as_millis() as i64, "重定向后请求失败");
                    return Err(ToolError::new(
                        ErrorCode::Unavailable,
                        svc.redact(&format!("重定向后请求失败：{e}")),
                        false,
                    ));
                }
            };
            if resp.status().is_redirection() {
                return Err(ToolError::new(
                    ErrorCode::Unavailable,
                    "重定向链超过一跳，不跟随",
                    false,
                ));
            }
            if !resp.status().is_success() {
                return Err(ToolError::new(
                    ErrorCode::Unavailable,
                    format!("重定向目标返回 HTTP {}", resp.status().as_u16()),
                    false,
                ));
            }
            return self
                .save_body(svc, &log_path, resp, &rel, &_ctx.workspace.root, started)
                .await;
        }

        if let Some(rel) = save_to.clone() {
            let mut resp = resp;
            // 先分状态、再看 content-type：上游 4xx/5xx 也常常是 JSON，
            // 但那是"环境/上游的问题"（Unavailable），不是"参数用错"（InvalidArgs）。
            // 两种情况都要把响应体带出来——否则模型只能靠猜（alist 的 `sign mismatch`
            // 就是这样丢掉线索，转头去试 percent-encoding 和 lan_exec 的）。
            if !status.is_success() {
                let snippet = body_snippet(&mut resp, 300).await;
                let note = if snippet.is_empty() {
                    format!("下载失败：HTTP {}", status.as_u16())
                } else {
                    format!("下载失败：HTTP {} · {snippet}", status.as_u16())
                };
                self.log_line(
                    &svc.name,
                    &method,
                    &log_path,
                    Some(status.as_u16()),
                    0,
                    started.elapsed().as_millis() as i64,
                    "save_to 上游错误",
                );
                return Err(ToolError::new(
                    ErrorCode::Unavailable,
                    svc.redact(&note),
                    false,
                ));
            }
            if !looks_binary(ctype.as_deref()) {
                let snippet = body_snippet(&mut resp, 300).await;
                let note = if snippet.is_empty() {
                    String::new()
                } else {
                    format!("：{snippet}")
                };
                self.log_line(
                    &svc.name,
                    &method,
                    &log_path,
                    Some(status.as_u16()),
                    0,
                    started.elapsed().as_millis() as i64,
                    "save_to 不是文件",
                );
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    svc.redact(&format!(
                        "响应不是文件（content-type: {}）{note}；JSON/文本接口不要加 save_to",
                        ctype.as_deref().unwrap_or("(未知)")
                    )),
                    false,
                ));
            }
            return self
                .save_body(svc, &log_path, resp, &rel, &_ctx.workspace.root, started)
                .await;
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
        let note = match (&uploaded, status.is_success()) {
            (Some((rel, n)), true) => format!("upload={rel} bytes={n} ok"),
            (Some((rel, n)), false) => format!("upload={rel} bytes={n} http-error"),
            (None, true) => "ok".to_owned(),
            (None, false) => "http-error".to_owned(),
        };
        self.log_line(
            &svc.name,
            &method,
            &log_path,
            Some(status.as_u16()),
            uploaded.as_ref().map(|(_, n)| *n as usize).unwrap_or(body.len()),
            ms,
            &note,
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
        let mut out = String::new();
        if let Some((rel, n)) = &uploaded {
            out.push_str(&format!("已发出 /ws/{rel}（{} KB）\n", n / 1024));
        }
        out.push_str(&format!(
            "{method} {} · HTTP {} · {} 字节 · {ms}ms\n\n",
            svc.name,
            status.as_u16(),
            body.len()
        ));
        out.push_str(&rendered.text);
        Ok(ToolOutput {
            content: vec![Block::Paragraph(out)],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}
