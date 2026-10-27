//! MCP over HTTP（Streamable HTTP）客户端。
//!
//! 只做三件事：`initialize` → `tools/list` → `tools/call`。
//! 和 `http` 服务同一条底线：**token 只在宿主注入**，模型只给 `server` / `tool` / `args`。
//!
//! 不做 stdio 形态；不引 `rmcp`（协议子集很小，自己写反而可控）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, SkillSource, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

/// 声明的协议版本；服务端不认时会在 initialize 回包里给它的版本，我们照单接受。
const PROTOCOL: &str = "2025-06-18";
const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_TOOLS_IN_CATALOG: usize = 40;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

pub struct McpServer {
    pub name: String,
    /// 只用这些工具（空 = 全用）。53 个工具全进 prompt 太贵，允许按需裁剪。
    allow: Vec<String>,
    /// 目录形态：`names`（默认，只列名字）/ `full`（名字+描述）/ `summary`（只报数量）
    catalog_mode: String,
    base: String,
    auth: Option<(String, String)>,
    extra_headers: Vec<(String, String)>,
    client: reqwest::Client,
    session: Mutex<Option<String>>,
    id: AtomicU64,
    tools: Mutex<Vec<McpToolInfo>>,
}

impl McpServer {
    pub fn new(
        name: impl Into<String>,
        base: impl Into<String>,
        token_file: Option<PathBuf>,
        header: String,
        extra_headers: Vec<(String, String)>,
        allow: Vec<String>,
        catalog_mode: String,
        proxy: Option<reqwest::Proxy>,
    ) -> Result<Arc<Self>, String> {
        // 默认不继承环境代理；显式配置 `[proxy] services = "always"` 时走代理。
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(30));
        b = match proxy {
            Some(p) => b.proxy(p),
            None => b.no_proxy(),
        };
        let client = b
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("chuan/0.1")
            .build()
            .map_err(|e| format!("HTTP 客户端初始化失败：{e}"))?;
        let auth = if header.trim().is_empty() {
            None
        } else {
            let (k, v) = header
                .split_once(':')
                .ok_or_else(|| format!("header 配置不合法：{header}"))?;
            let mut v = v.trim().to_owned();
            if v.contains("{token}") {
                let file = token_file
                    .as_ref()
                    .ok_or("需要 token 但没配 token_file")?;
                let token = std::fs::read_to_string(file)
                    .map_err(|e| format!("读取 token 失败：{e}"))?
                    .trim()
                    .to_owned();
                if token.is_empty() {
                    return Err("token 文件为空".into());
                }
                v = v.replace("{token}", &token);
            }
            Some((k.trim().to_owned(), v))
        };
        Ok(Arc::new(Self {
            name: name.into(),
            allow,
            catalog_mode,
            base: base.into(),
            auth,
            extra_headers,
            client,
            session: Mutex::new(None),
            id: AtomicU64::new(1),
            tools: Mutex::new(Vec::new()),
        }))
    }

    fn redact(&self, text: &str) -> String {
        match &self.auth {
            Some((_, v)) => {
                let secret = v.rsplit(' ').next().unwrap_or(v);
                if secret.len() < 6 {
                    text.to_owned()
                } else {
                    text.replace(secret, "***")
                }
            }
            None => text.to_owned(),
        }
    }

    /// 发一条 JSON-RPC。`id=None` 表示通知（不等结果）。
    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
        id: Option<u64>,
    ) -> Result<serde_json::Value, String> {
        let mut body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        if let Some(i) = id {
            body["id"] = serde_json::json!(i);
        }
        let mut req = self
            .client
            .post(&self.base)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL);
        if let Some((k, v)) = &self.auth {
            req = req.header(k, v);
        }
        for (k, v) in &self.extra_headers {
            req = req.header(k, v);
        }
        if let Some(s) = self.session.lock().unwrap().clone() {
            req = req.header("mcp-session-id", s);
        }

        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| self.redact(&format!("{method} 请求失败：{e}")))?;

        // 服务端在 initialize 时给 session id
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock().unwrap() = Some(sid.to_owned());
        }

        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if status.as_u16() == 202 && id.is_none() {
            return Ok(serde_json::Value::Null);
        }
        let text = resp
            .text()
            .await
            .map_err(|e| self.redact(&format!("{method} 读取响应失败：{e}")))?;
        if text.len() > MAX_BODY {
            return Err(format!("{method} 响应过大"));
        }
        if !status.is_success() {
            return Err(self.redact(&format!(
                "{method} HTTP {}：{}",
                status.as_u16(),
                text.chars().take(400).collect::<String>()
            )));
        }

        // 兼容两种回法：application/json 或 text/event-stream
        let value = if ctype.contains("event-stream") {
            let mut found = None;
            for line in text.lines() {
                let Some(data) = line.strip_prefix("data:") else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(data.trim()) else { continue };
                if id.is_none() || v.get("id").and_then(|x| x.as_u64()) == id {
                    found = Some(v);
                }
            }
            found.ok_or_else(|| format!("{method} 的 SSE 里没有对应的响应"))?
        } else {
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| format!("{method} 响应不是 JSON：{e}"))?
        };

        if let Some(err) = value.get("error") {
            return Err(self.redact(&format!(
                "{method} 返回错误：{}",
                err.to_string().chars().take(400).collect::<String>()
            )));
        }
        Ok(value.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// initialize + notifications/initialized + tools/list
    pub async fn connect(&self) -> Result<Vec<McpToolInfo>, String> {
        let res = self
            .rpc(
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL,
                    "capabilities": {},
                    "clientInfo": { "name": "chuan", "version": env!("CARGO_PKG_VERSION") }
                }),
                Some(self.id.fetch_add(1, Ordering::SeqCst)),
            )
            .await?;
        let server_name = res["serverInfo"]["name"].as_str().unwrap_or("?").to_owned();
        tracing::info!(server = %self.name, remote = %server_name, "mcp initialized");

        // 通知：不等响应
        self.rpc("notifications/initialized", serde_json::json!({}), None)
            .await?;

        let list = self
            .rpc(
                "tools/list",
                serde_json::json!({}),
                Some(self.id.fetch_add(1, Ordering::SeqCst)),
            )
            .await?;
        let mut tools = Vec::new();
        for t in list["tools"].as_array().cloned().unwrap_or_default() {
            let Some(name) = t["name"].as_str() else { continue };
            tools.push(McpToolInfo {
                name: name.to_owned(),
                description: t["description"].as_str().unwrap_or("").to_owned(),
                schema: t["inputSchema"].clone(),
            });
        }
        if !self.allow.is_empty() {
            let before = tools.len();
            tools.retain(|t| self.allow.iter().any(|a| a == &t.name));
            for a in &self.allow {
                if !tools.iter().any(|t| &t.name == a) {
                    tracing::warn!(server = %self.name, tool = %a, "配置的工具不存在");
                }
            }
            tracing::info!(server = %self.name, kept = tools.len(), before, "mcp tools filtered");
        }
        *self.tools.lock().unwrap() = tools.clone();
        Ok(tools)
    }

    pub fn tools(&self) -> Vec<McpToolInfo> {
        self.tools.lock().unwrap().clone()
    }

    pub async fn call(
        &self,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<String, String> {
        if !self.tools().iter().any(|t| t.name == tool) {
            return Err(format!("{} 没有工具 {tool}", self.name));
        }
        let res = self
            .rpc(
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": args }),
                Some(self.id.fetch_add(1, Ordering::SeqCst)),
            )
            .await?;
        let is_error = res["isError"].as_bool().unwrap_or(false);
        let mut out = String::new();
        for c in res["content"].as_array().cloned().unwrap_or_default() {
            match c["type"].as_str() {
                Some("text") => out.push_str(c["text"].as_str().unwrap_or("")),
                Some(other) => out.push_str(&format!("[{other} 内容]")),
                None => {}
            }
            out.push('\n');
        }
        if out.trim().is_empty() {
            // 有些服务把结果放 structuredContent；全都空就退回紧凑 JSON，
            // 不要连 MCP 信封（content/isError）一起塞给模型。
            out = match res.get("structuredContent") {
                Some(v) if !v.is_null() => v.to_string(),
                _ => serde_json::to_string(&res).unwrap_or_default(),
            };
        }
        if is_error {
            return Err(self.redact(out.trim()));
        }
        Ok(out.trim().to_owned())
    }
}

/// 多个 MCP server 的集合：既能当目录源，也能当调用工具。
pub struct McpHub {
    servers: std::sync::RwLock<BTreeMap<String, Arc<McpServer>>>,
}

impl McpHub {
    pub fn new(servers: Vec<Arc<McpServer>>) -> Arc<Self> {
        Arc::new(Self {
            servers: std::sync::RwLock::new(
                servers.into_iter().map(|s| (s.name.clone(), s)).collect(),
            ),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.read().unwrap().is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.servers.read().unwrap().keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<McpServer>> {
        self.servers.read().unwrap().get(name).cloned()
    }

    /// 运行时注册 + 立刻握手（`service` 工具用）。
    pub async fn upsert(&self, server: Arc<McpServer>) -> Result<usize, String> {
        let tools = server.connect().await?;
        self.servers
            .write()
            .unwrap()
            .insert(server.name.clone(), Arc::clone(&server));
        Ok(tools.len())
    }

    pub fn remove(&self, name: &str) -> bool {
        self.servers.write().unwrap().remove(name).is_some()
    }

    pub async fn connect_all(&self) {
        let list: Vec<(String, Arc<McpServer>)> = {
            let map = self.servers.read().unwrap();
            map.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect()
        };
        for (name, s) in list {
            match s.connect().await {
                Ok(tools) => tracing::info!(server = %name, tools = tools.len(), "mcp tools loaded"),
                Err(e) => tracing::warn!(server = %name, error = %e, "mcp connect failed"),
            }
        }
    }
}

impl SkillSource for McpHub {
    fn catalog(&self) -> String {
        let mut out = String::new();
        let servers: Vec<(String, Arc<McpServer>)> = {
            let map = self.servers.read().unwrap();
            map.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect()
        };
        for (name, s) in &servers {
            let tools = s.tools();
            if tools.is_empty() {
                continue;
            }
            let hint = format!(
                "MCP 服务 `{name}`（{} 个工具）；调用 `mcp(server=\"{name}\", tool=…, args=…)`；\
                 清单/参数 `mcp(server=\"{name}\", tool=\"__list__\")`",
                tools.len()
            );
            match s.catalog_mode.as_str() {
                "summary" => {
                    out.push_str(&format!("\n{hint}\n"));
                }
                "full" => {
                    out.push_str(&format!("\n{hint}\n"));
                    for (i, t) in tools.iter().enumerate() {
                        if i >= MAX_TOOLS_IN_CATALOG {
                            out.push_str(&format!(
                                "- …（还有 {} 个，用 __list__ 查看）\n",
                                tools.len() - MAX_TOOLS_IN_CATALOG
                            ));
                            break;
                        }
                        let desc = t.description.split_whitespace().collect::<Vec<_>>().join(" ");
                        let desc: String = desc.chars().take(60).collect();
                        out.push_str(&format!("- {}：{}\n", t.name, desc));
                    }
                }
                _ => {
                    // names：只列名字，一行装得下，模型自己决定要不要查细节
                    out.push_str(&format!("{hint}\n工具："));
                    for (i, t) in tools.iter().enumerate() {
                        if i >= 60 {
                            out.push_str(&format!(" …（+{}）", tools.len() - 60));
                            break;
                        }
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(&t.name);
                    }
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// `mcp(server, tool, args)`
pub struct McpCallTool {
    hub: Arc<McpHub>,
    log: PathBuf,
}

impl McpCallTool {
    pub fn new(hub: Arc<McpHub>, log: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self { hub, log: log.into() })
    }
}

#[async_trait]
impl Tool for McpCallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "mcp".into(),
            description: "调用 MCP 服务暴露的工具。可用服务与工具见系统提示里的目录；\
鉴权由宿主注入，你不需要也不应该接触 token。第一次用某个服务前，先看目录里的参数名。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "tool": { "type": "string" },
                    "args": { "type": "object", "description": "工具参数（先用 tool=\"__list__\" 查）" },
                    "query": {
                        "type": "string",
                        "description": "配合 tool=\"__list__\"：按关键词过滤工具（名字或描述）"
                    }
                },
                "required": ["server", "tool"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
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
        let server = args
            .get("server")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 server", false))?;
        let tool = args
            .get("tool")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 tool", false))?;

        // 目录里只放了名字，参数细节按需查；可按关键词过滤
        if tool == "__list__" {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            let Some(s) = self.hub.get(server) else {
                return Err(ToolError::new(
                    ErrorCode::NotFound,
                    format!("没有这个 MCP 服务：{server}"),
                    false,
                ));
            };
            let all = s.tools();
            let matched: Vec<_> = all
                .iter()
                .filter(|t| {
                    query.is_empty()
                        || t.name.to_lowercase().contains(&query)
                        || t.description.to_lowercase().contains(&query)
                })
                .cloned()
                .collect();
            let mut out = format!("{} 的工具（{} 个", server, matched.len());
            if !query.is_empty() {
                out.push_str(&format!("，筛选 `{query}`，共 {} 个", all.len()));
            }
            out.push_str("）：\n");
            for t in &matched {
                let params = t
                    .schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|o| {
                        let req: Vec<&str> = t
                            .schema
                            .get("required")
                            .and_then(|r| r.as_array())
                            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                            .unwrap_or_default();
                        o.keys()
                            .map(|k| {
                                if req.contains(&k.as_str()) {
                                    format!("{k}*")
                                } else {
                                    k.clone()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                out.push_str(&format!(
                    "- {}（{}）：{}\n",
                    t.name,
                    if params.is_empty() { "无参数" } else { &params },
                    t.description.split_whitespace().collect::<Vec<_>>().join(" ")
                ));
            }
            return Ok(ToolOutput {
                content: vec![Block::Paragraph(out)],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            });
        }
        let payload = args.get("args").cloned().unwrap_or_else(|| serde_json::json!({}));

        let Some(s) = self.hub.get(server) else {
            return Err(ToolError::new(
                ErrorCode::NotFound,
                format!(
                    "没有这个 MCP 服务：{server}（可用：{}）",
                    self.hub.names().join("、")
                ),
                false,
            ));
        };

        let started = Instant::now();
        let result = s.call(tool, payload).await;
        let rec = serde_json::json!({
            "at": now_ms(),
            "method": "tools/call",
            "url": format!("mcp:{server}:{tool}"),
            "host": server,
            "status": if result.is_ok() { 200 } else { 500 },
            "bytes": result.as_ref().map(|t| t.len()).unwrap_or(0),
            "ms": started.elapsed().as_millis() as i64,
            "note": if result.is_ok() { "ok" } else { "error" },
        });
        crate::net::append_log(&self.log, &rec.to_string());

        let text = result.map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
        Ok(ToolOutput {
            content: vec![Block::Paragraph(text)],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}
