//! `chuan oauth-mcp`：OAuth 授权码 + PKCE（Rust 版，单二进制内置）。
//!
//! 第一阶段只做手动粘贴回调：`begin` 打印授权 URL；浏览器跳到 127.0.0.1 失败后，
//! 把地址栏 URL 粘给 `finish` 的 stdin。provider 由 manifest 描述，不写死具体厂商。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const DEFAULT_PROVIDER: &str = "cloudflare-mcp";
const DEFAULT_MANIFEST: &str =
    include_str!("../../../adapters/oauth-mcp/providers.d/cloudflare-mcp.toml");
const UA: &str = "chuan-oauth/0.1";
const PENDING_TTL: i64 = 600;

#[derive(Clone, Debug, Deserialize)]
struct Manifest {
    schema: u32,
    id: String,
    kind: String,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    discovery: Option<String>,
    #[serde(default)]
    allow_private: bool,
    #[serde(default)]
    client: ClientCfg,
    #[serde(default)]
    authorize: AuthorizeCfg,
    #[serde(default)]
    token: TokenCfg,
    #[serde(default)]
    callback: CallbackCfg,
    #[serde(default)]
    output: OutputCfg,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ClientCfg {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret_file: Option<String>,
    #[serde(default)]
    token_endpoint_auth: Option<String>,
    #[serde(default)]
    pkce: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AuthorizeCfg {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    extra: std::collections::BTreeMap<String, toml::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TokenCfg {
    #[serde(default)]
    url: Option<String>,
    #[serde(default = "default_true")]
    refresh: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
struct CallbackCfg {
    #[serde(default = "default_cb_mode")]
    mode: String,
    #[serde(default = "default_cb_path")]
    path: String,
    #[serde(default = "default_cb_port")]
    port: u16,
    #[serde(default)]
    opentunnel_bin: Option<String>,
    #[serde(default)]
    opentunnel_profile: Option<String>,
}

impl Default for CallbackCfg {
    fn default() -> Self {
        Self {
            mode: default_cb_mode(),
            path: default_cb_path(),
            port: default_cb_port(),
            opentunnel_bin: None,
            opentunnel_profile: None,
        }
    }
}

fn default_cb_mode() -> String {
    "manual".into()
}
fn default_cb_path() -> String {
    "/cb".into()
}
fn default_cb_port() -> u16 {
    8765
}

#[derive(Clone, Debug, Default, Deserialize)]
struct OutputCfg {
    #[serde(default)]
    token_file: Option<String>,
    #[serde(default)]
    header: Option<String>,
}

#[derive(Debug)]
struct Discovered {
    authorize: String,
    token: String,
    register: Option<String>,
    resource: Option<String>,
}

#[derive(Clone)]
struct Ctx {
    _base: PathBuf,
    provider: String,
    manifest: Manifest,
    dir: PathBuf,
    token_file: Option<PathBuf>,
    client: reqwest::Client,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    b64url(&b)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/var/lib/chuan".into()))
}

fn default_base() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("chuan-oauth")
}

fn read_json(path: &Path) -> Option<Value> {
    let s = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

fn write_json(path: &Path, v: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 {} 失败：{e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let text = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("替换 {} 失败：{e}", path.display()))?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<Manifest, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读 {} 失败：{e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("解析 {} 失败：{e}", path.display()))
}

fn manifest_errors(m: &Manifest) -> Vec<String> {
    let mut e = Vec::new();
    if m.schema != 1 {
        e.push("schema 必须是 1".into());
    }
    if m.id.trim().is_empty() {
        e.push("缺少 id".into());
    }
    if m.kind != "oauth2" && m.kind != "mcp" {
        e.push("kind 必须是 oauth2 或 mcp".into());
    }
    let mode = m.client.mode.as_deref().unwrap_or("");
    if !matches!(mode, "static" | "dcr" | "public") {
        e.push("client.mode 必须是 static / dcr / public".into());
    }
    if mode == "static" && m.client.client_id.is_none() {
        e.push("static client 需要 client_id".into());
    }
    if matches!(m.client.pkce.as_deref(), Some(x) if x != "S256" && x != "none") {
        e.push("pkce 只支持 S256 或 none".into());
    }
    if m.kind == "oauth2" {
        if !m
            .authorize
            .url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
        {
            e.push("authorize.url 必须是 https URL".into());
        }
        if !m
            .token
            .url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
        {
            e.push("token.url 必须是 https URL".into());
        }
    } else {
        if !m
            .resource
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
        {
            e.push("mcp 需要 https resource".into());
        }
        if !m
            .discovery
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
        {
            e.push("mcp 需要 https discovery".into());
        }
    }
    if !matches!(
        m.callback.mode.as_str(),
        "manual" | "opentunnel" | "relay" | "loopback"
    ) {
        e.push("callback.mode 必须是 manual / opentunnel / relay / loopback".into());
    }
    if !m.callback.path.starts_with('/') {
        e.push("callback.path 必须以 / 开头".into());
    }
    if m.authorize.scopes.is_empty() {
        e.push("authorize.scopes 不能为空".into());
    }
    if m.authorize
        .scopes
        .iter()
        .any(|s| s.contains('*') || s.trim().is_empty())
    {
        e.push("scope 必须是非空字符串且不能含 *".into());
    }
    if let Some(tf) = &m.output.token_file {
        if !tf.starts_with('/') {
            e.push("output.token_file 必须是绝对路径".into());
        }
        if tf.split('/').any(|p| p == "..") {
            e.push("output.token_file 不允许 ..".into());
        }
    }
    e
}

async fn http_json(
    client: &reqwest::Client,
    url: &str,
    form: Option<&[(&str, String)]>,
) -> Result<Value, String> {
    let mut req = client
        .post(url)
        .header("user-agent", UA)
        .header("accept", "application/json");
    if let Some(form) = form {
        let mut enc = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in form {
            enc.append_pair(k, v);
        }
        req = req
            .header("content-type", "application/x-www-form-urlencoded")
            .body(enc.finish());
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("请求 {url} 失败：{e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取 {url} 响应失败：{e}"))?;
    if !status.is_success() {
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error_description")
                    .or_else(|| v.get("error"))
                    .cloned()
            })
            .map(|v| v.to_string())
            .unwrap_or_else(|| text.chars().take(300).collect());
        return Err(format!("{url} HTTP {}：{msg}", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|e| format!("{url} 响应不是 JSON：{e}"))
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<Value, String> {
    let resp = client
        .get(url)
        .header("user-agent", UA)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("请求 {url} 失败：{e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取 {url} 失败：{e}"))?;
    if !status.is_success() {
        return Err(format!("{url} HTTP {}", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|e| format!("{url} 响应不是 JSON：{e}"))
}

async fn discover(client: &reqwest::Client, m: &Manifest) -> Result<Discovered, String> {
    let resource = m.resource.clone().unwrap_or_default();
    let discovery = m.discovery.clone().unwrap_or_default();
    let mut meta = get_json(client, &discovery).await?;
    let resource = meta
        .get("resource")
        .and_then(Value::as_str)
        .unwrap_or(&resource)
        .to_owned();
    let servers = meta
        .get("authorization_servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let as_url = servers
        .first()
        .and_then(Value::as_str)
        .ok_or("discovery 里没有 authorization_servers")?;
    let as_meta = get_json(
        client,
        &format!(
            "{}/.well-known/oauth-authorization-server",
            as_url.trim_end_matches('/')
        ),
    )
    .await?;
    let authorize = as_meta
        .get("authorization_endpoint")
        .and_then(Value::as_str)
        .ok_or("授权服务器缺少 authorization_endpoint")?
        .to_owned();
    let token = as_meta
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or("授权服务器缺少 token_endpoint")?
        .to_owned();
    let register = as_meta
        .get("registration_endpoint")
        .and_then(Value::as_str)
        .map(str::to_owned);
    meta["resource"] = json!(resource);
    Ok(Discovered {
        authorize,
        token,
        register,
        resource: Some(resource),
    })
}

fn spawn_opentunnel_callback(ctx: Ctx) -> Result<(), String> {
    let bin = ctx
        .manifest
        .callback
        .opentunnel_bin
        .clone()
        .unwrap_or_else(|| "/usr/local/lib/chuan/opentunnel/opentunnel".into());
    let profile = ctx
        .manifest
        .callback
        .opentunnel_profile
        .clone()
        .unwrap_or_else(|| "chuan".into());
    let port = ctx.manifest.callback.port;
    let path = ctx.manifest.callback.path.clone();
    let mut child = Command::new(&bin)
        .args(["--profile", &profile, "serve"])
        .env("HOME", home())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动 opentunnel 失败（{bin}）：{e}"))?;
    let dir = ctx.dir.clone();
    let down_bin = bin.clone();
    let down_profile = profile.clone();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                let _ = write_json(
                    &dir.join("callback-result.json"),
                    &json!({"ok":false,"error":e.to_string()}),
                );
                return;
            }
        };
        let server = match tiny_http::Server::http(("127.0.0.1", port)) {
            Ok(s) => s,
            Err(e) => {
                let _ = write_json(
                    &dir.join("callback-result.json"),
                    &json!({"ok":false,"error":format!("监听回调端口失败：{e}")}),
                );
                return;
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                let _ = write_json(
                    &dir.join("callback-result.json"),
                    &json!({"ok":false,"error":"授权回调超时"}),
                );
                break;
            }
            match server.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(Some(req)) => {
                    let url = req.url().to_owned();
                    let path_ok = url.split('?').next() == Some(path.as_str());
                    if req.method() != &tiny_http::Method::Get || !path_ok {
                        let _ = req.respond(tiny_http::Response::empty(404));
                        continue;
                    }
                    let full = format!("http://127.0.0.1:{port}{url}");
                    let result = rt.block_on(ctx.finish(&full));
                    let body = match &result {
                        Ok(_) => "授权完成，可以回到聊天了。",
                        Err(_) => "授权失败，请回到聊天查看状态。",
                    };
                    let _ = req.respond(tiny_http::Response::from_string(body));
                    let out = match result {
                        Ok(tok) => json!({"ok":true,"expires_in":tok.get("expires_in")}),
                        Err(e) => json!({"ok":false,"error":e}),
                    };
                    let _ = write_json(&dir.join("callback-result.json"), &out);
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = write_json(
                        &dir.join("callback-result.json"),
                        &json!({"ok":false,"error":e.to_string()}),
                    );
                    break;
                }
            }
        }
        // 回调结束就撤掉公网入口；先停子进程，再 down 清理 control socket。
        let _ = child.kill();
        let _ = child.wait();
        if let Ok(mut c) = Command::new(&down_bin)
            .args(["--profile", &down_profile, "down"])
            .env("HOME", home())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let _ = c.wait();
        }
    });
    Ok(())
}

impl Ctx {
    fn new(
        base: PathBuf,
        provider: String,
        manifest: Manifest,
        token_file: Option<PathBuf>,
    ) -> Result<Self, String> {
        let dir = base.join(&provider);
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP 客户端初始化失败：{e}"))?;
        Ok(Self {
            _base: base,
            provider,
            manifest,
            dir,
            token_file,
            client,
        })
    }

    fn redirect(&self) -> Result<String, String> {
        match self.manifest.callback.mode.as_str() {
            "manual" | "loopback" => Ok(format!(
                "http://127.0.0.1:{}{}",
                self.manifest.callback.port, self.manifest.callback.path
            )),
            "opentunnel" => tunnel_info()
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "还没有 OpenTunnel 回调；先运行 bootstrap".to_owned()),
            other => Err(format!("callback.mode={other} 还没实现")),
        }
    }

    fn client_json(&self) -> Option<Value> {
        read_json(&self.dir.join("client.json"))
    }

    fn client_secret(&self) -> Option<String> {
        let path = self.manifest.client.client_secret_file.as_deref()?;
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    fn add_client_auth(&self, form: &mut Vec<(&str, String)>) {
        if let Some(secret) = self.client_secret() {
            let mode = self
                .manifest
                .client
                .token_endpoint_auth
                .as_deref()
                .unwrap_or("client_secret_post");
            if mode != "none" {
                form.push(("client_secret", secret));
            }
        }
    }

    async fn ensure_client(&self, d: &Discovered) -> Result<Value, String> {
        if let Some(c) = self.client_json() {
            if c.get("client_id").and_then(Value::as_str).is_some() {
                return Ok(c);
            }
        }
        let mode = self.manifest.client.mode.as_deref().unwrap_or("");
        if mode == "static" || mode == "public" {
            let client_id = self
                .manifest
                .client
                .client_id
                .clone()
                .ok_or("static client 需要 client_id")?;
            let c = json!({"client_id": client_id});
            write_json(&self.dir.join("client.json"), &c)?;
            return Ok(c);
        }
        let register = d
            .register
            .as_deref()
            .ok_or("provider 没有 registration_endpoint，不能 DCR")?;
        let body = json!({
            "client_name": "chuan oauth-mcp",
            "redirect_uris": [self.redirect()?],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": self.manifest.client.token_endpoint_auth.clone().unwrap_or_else(|| "none".into()),
            "scope": self.manifest.authorize.scopes.join(" "),
        });
        let resp = self
            .client
            .post(register)
            .header("user-agent", UA)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("DCR 请求失败：{e}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "DCR HTTP {}：{}",
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            ));
        }
        let c: Value =
            serde_json::from_str(&text).map_err(|e| format!("DCR 响应不是 JSON：{e}"))?;
        if c.get("client_id").and_then(Value::as_str).is_none() {
            return Err("DCR 没有返回 client_id".into());
        }
        write_json(&self.dir.join("client.json"), &c)?;
        Ok(c)
    }

    async fn endpoints(&self) -> Result<Discovered, String> {
        if self.manifest.kind == "oauth2" {
            return Ok(Discovered {
                authorize: self
                    .manifest
                    .authorize
                    .url
                    .clone()
                    .ok_or("manifest 缺少 authorize.url")?,
                token: self
                    .manifest
                    .token
                    .url
                    .clone()
                    .ok_or("manifest 缺少 token.url")?,
                register: None,
                resource: self.manifest.resource.clone(),
            });
        }
        discover(&self.client, &self.manifest).await
    }

    async fn begin(&self) -> Result<String, String> {
        let d = self.endpoints().await?;
        let client = self.ensure_client(&d).await?;
        let client_id = client
            .get("client_id")
            .and_then(Value::as_str)
            .ok_or("client 缺少 client_id")?
            .to_owned();
        let state = random_b64(32);
        let verifier = random_b64(64);
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        let pending = json!({
            "state": state,
            "verifier": verifier,
            "redirect_uri": self.redirect()?,
            "client_id": client_id,
            "token_endpoint": d.token,
            "resource": d.resource,
            "scope": self.manifest.authorize.scopes.join(" "),
            "created_at": now(),
        });
        write_json(&self.dir.join("pending.json"), &pending)?;
        if self.manifest.callback.mode == "opentunnel" {
            spawn_opentunnel_callback(self.clone())?;
        }
        let mut qs = url::form_urlencoded::Serializer::new(String::new());
        qs.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &self.redirect()?)
            .append_pair("scope", &self.manifest.authorize.scopes.join(" "))
            .append_pair("state", &pending["state"].as_str().unwrap_or(""))
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(r) = &d.resource {
            qs.append_pair("resource", r);
        }
        for (k, v) in &self.manifest.authorize.extra {
            if let Some(s) = v.as_str() {
                qs.append_pair(k, s);
            }
        }
        Ok(format!("{}?{}", d.authorize, qs.finish()))
    }

    async fn finish(&self, pasted: &str) -> Result<Value, String> {
        let pending =
            read_json(&self.dir.join("pending.json")).ok_or("没有待完成的授权；先 begin")?;
        let created = pending
            .get("created_at")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if now() - created > PENDING_TTL {
            let _ = std::fs::remove_file(self.dir.join("pending.json"));
            return Err("授权已超过 10 分钟，重新 begin".into());
        }
        let (code, state) = if pasted.contains("code=") {
            let q = url::Url::parse(pasted).map_err(|e| format!("URL 不合法：{e}"))?;
            let mut code = String::new();
            let mut state = String::new();
            for (k, v) in q.query_pairs() {
                match k.as_ref() {
                    "code" => code = v.into_owned(),
                    "state" => state = v.into_owned(),
                    _ => {}
                }
            }
            (code, state)
        } else {
            (pasted.trim().to_owned(), String::new())
        };
        if code.is_empty() {
            return Err("URL 里没有 code".into());
        }
        if !state.is_empty() && pending.get("state").and_then(Value::as_str) != Some(state.as_str())
        {
            return Err("state 不匹配".into());
        }
        let token_endpoint = pending
            .get("token_endpoint")
            .and_then(Value::as_str)
            .ok_or("pending 缺少 token_endpoint")?
            .to_owned();
        let client_id = pending
            .get("client_id")
            .and_then(Value::as_str)
            .ok_or("pending 缺少 client_id")?
            .to_owned();
        let verifier = pending
            .get("verifier")
            .and_then(Value::as_str)
            .ok_or("pending 缺少 verifier")?
            .to_owned();
        let redirect = pending
            .get("redirect_uri")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or(self.redirect()?)
            .to_owned();
        let mut form: Vec<(&str, String)> = vec![
            ("grant_type", "authorization_code".into()),
            ("code", code),
            ("redirect_uri", redirect),
            ("client_id", client_id.clone()),
            ("code_verifier", verifier),
        ];
        if let Some(r) = pending.get("resource").and_then(Value::as_str) {
            form.push(("resource", r.to_owned()));
        }
        self.add_client_auth(&mut form);
        let tok = http_json(&self.client, &token_endpoint, Some(&form)).await?;
        if tok.get("access_token").and_then(Value::as_str).is_none() {
            return Err("token 响应没有 access_token".into());
        }
        self.save_tokens(
            &tok,
            pending.get("scope").and_then(Value::as_str).unwrap_or(""),
            &token_endpoint,
            &client_id,
        )?;
        let _ = std::fs::remove_file(self.dir.join("pending.json"));
        Ok(tok)
    }

    fn save_tokens(
        &self,
        tok: &Value,
        fallback_scope: &str,
        token_endpoint: &str,
        client_id: &str,
    ) -> Result<(), String> {
        let expires_at = now()
            + tok
                .get("expires_in")
                .and_then(Value::as_i64)
                .unwrap_or(3600);
        let stored = json!({
            "schema": 1,
            "provider": self.provider,
            "access_token": tok["access_token"],
            "refresh_token": tok.get("refresh_token").cloned().unwrap_or(Value::Null),
            "token_type": tok.get("token_type").cloned().unwrap_or_else(|| json!("Bearer")),
            "scope": tok.get("scope").cloned().unwrap_or_else(|| json!(fallback_scope)),
            "expires_at": expires_at,
            "token_endpoint": token_endpoint,
            "client_id": client_id,
            "resource": self.manifest.resource,
        });
        write_json(&self.dir.join("tokens.json"), &stored)?;
        let access = self
            .token_file
            .clone()
            .or_else(|| {
                self.manifest
                    .output
                    .token_file
                    .as_deref()
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| self.dir.join("access_token"));
        if let Some(parent) = access.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&access, tok["access_token"].as_str().unwrap_or(""))
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&access, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    async fn refresh(&self) -> Result<Value, String> {
        let tokens = read_json(&self.dir.join("tokens.json")).ok_or("没有 token")?;
        let rt = tokens
            .get("refresh_token")
            .and_then(Value::as_str)
            .ok_or("没有 refresh_token")?;
        let client_id = tokens
            .get("client_id")
            .and_then(Value::as_str)
            .ok_or("tokens 缺少 client_id（重新 begin/finish）")?;
        let token_endpoint = tokens
            .get("token_endpoint")
            .and_then(Value::as_str)
            .ok_or("tokens 缺少 token_endpoint（重新 begin/finish）")?;
        let mut form = vec![
            ("grant_type", "refresh_token".into()),
            ("refresh_token", rt.to_owned()),
            ("client_id", client_id.to_owned()),
        ];
        if let Some(r) = tokens.get("resource").and_then(Value::as_str) {
            form.push(("resource", r.to_owned()));
        }
        self.add_client_auth(&mut form);
        let tok = http_json(&self.client, &token_endpoint, Some(&form)).await?;
        if tok.get("access_token").and_then(Value::as_str).is_none() {
            return Err("刷新响应没有 access_token".into());
        }
        let mut merged = tok;
        if merged.get("refresh_token").is_none() {
            merged["refresh_token"] = json!(rt);
        }
        self.save_tokens(
            &merged,
            tokens.get("scope").and_then(Value::as_str).unwrap_or(""),
            token_endpoint,
            client_id,
        )?;
        Ok(merged)
    }
}

fn status(ctx: &Ctx) -> Value {
    let Some(t) = read_json(&ctx.dir.join("tokens.json")) else {
        return json!({"state":"disconnected","provider":ctx.provider});
    };
    let left = t.get("expires_at").and_then(Value::as_i64).unwrap_or(0) - now();
    json!({
        "state": if left > 0 {"connected"} else {"expired"},
        "provider": ctx.provider,
        "scope": t.get("scope"),
        "expires_in": left.max(0),
        "has_refresh": t.get("refresh_token").and_then(Value::as_str).is_some(),
        "access_token_file": ctx.token_file.clone().or_else(|| ctx.manifest.output.token_file.clone().map(PathBuf::from)),
    })
}

fn tunnel_info() -> Value {
    let path = std::env::var("CHUAN_OAUTH_TUNNEL_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/state/opentunnel.json"));
    match read_json(&path) {
        Some(v) if v.get("url").and_then(Value::as_str).is_some() => json!({
            "state": "ready",
            "url": v.get("url"),
            "hostname": v.get("hostname"),
        }),
        _ => json!({
            "state": "missing",
            "message": "还没有创建 OpenTunnel 回调；安装时用 --bootstrap-oauth-tunnel，或运行 deploy/bootstrap-oauth-tunnel.sh。当前仍可用手动粘贴 finish，但没有自动公网回调。"
        }),
    }
}

fn manifest_for(base: &Path, provider: &str, explicit: Option<&Path>) -> Result<Manifest, String> {
    if let Some(p) = explicit {
        return read_manifest(p);
    }
    let p = base.join("providers.d").join(format!("{provider}.toml"));
    if p.exists() {
        return read_manifest(&p);
    }
    if provider == DEFAULT_PROVIDER {
        return toml::from_str(DEFAULT_MANIFEST).map_err(|e| e.to_string());
    }
    Err(format!("找不到 manifest：{}", p.display()))
}

fn token_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut x = 0u8;
    for (x1, x2) in a.as_bytes().iter().zip(b.as_bytes()) {
        x |= x1 ^ x2;
    }
    x == 0
}

fn ensure_token_file(path: &Path) -> Result<String, String> {
    if let Ok(s) = std::fs::read_to_string(path) {
        let s = s.trim().to_owned();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let s = random_b64(32);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, &s).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(s)
}

fn mcp_tools(allow_revoke: bool) -> Value {
    let mut tools = vec![
        json!({
            "name": "auth_begin",
            "description": "开始一次 OAuth 授权。返回公开链接；把链接发给用户。needs_bootstrap=true 表示还没有 OpenTunnel 回调，应通过当前通道告诉用户。",
            "inputSchema": {"type":"object","properties":{
                "provider":{"type":"string","default":DEFAULT_PROVIDER}
            },"additionalProperties":false}
        }),
        json!({
            "name": "auth_status",
            "description": "查看 OAuth 状态（不返回 token）。",
            "inputSchema": {"type":"object","properties":{
                "provider":{"type":"string","default":DEFAULT_PROVIDER}
            },"additionalProperties":false}
        }),
        json!({
            "name": "auth_refresh",
            "description": "用 refresh token 换新的 access token，并更新 token_file。",
            "inputSchema": {"type":"object","properties":{
                "provider":{"type":"string","default":DEFAULT_PROVIDER}
            },"additionalProperties":false}
        }),
    ];
    if allow_revoke {
        tools.push(json!({
            "name": "auth_revoke",
            "description": "撤销本地 OAuth 授权并删除 token（破坏性）。",
            "inputSchema": {"type":"object","properties":{
                "provider":{"type":"string","default":DEFAULT_PROVIDER}
            },"additionalProperties":false}
        }));
    }
    json!({"tools": tools})
}

fn mcp_dispatch(
    rt: &tokio::runtime::Runtime,
    base: &Path,
    provider: &str,
    method: &str,
    params: &Value,
    allow_revoke: bool,
) -> Result<Value, String> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "chuan-oauth-mcp", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "OAuth helper. Call auth_begin and give the URL to the user; never ask for tokens or authorization codes in chat."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(mcp_tools(allow_revoke)),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or("tools/call 缺 name")?;
            if name == "auth_revoke" && !allow_revoke {
                return Err("auth_revoke 未启用".into());
            }
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let p = args
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or(provider);
            let manifest = match manifest_for(base, p, None) {
                Ok(m) => m,
                Err(e) => return Err(e),
            };
            let ctx = Ctx::new(base.to_path_buf(), p.to_owned(), manifest, None)?;
            let data = match name {
                "auth_begin" => {
                    if ctx.manifest.callback.mode == "opentunnel"
                        && tunnel_info().get("state").and_then(Value::as_str) == Some("missing")
                    {
                        let t = tunnel_info();
                        return Ok(json!({
                            "content": [{"type":"text","text": t.get("message").cloned().unwrap_or(json!("needs_bootstrap")).to_string()}],
                            "structuredContent": {
                                "state": "needs_bootstrap",
                                "provider": p,
                                "tunnel": t,
                                "needs_bootstrap": true,
                            },
                            "isError": false,
                        }));
                    }
                    let url = rt.block_on(ctx.begin())?;
                    let t = tunnel_info();
                    let mut v = json!({
                        "state": "pending",
                        "provider": p,
                        "url": url,
                        "finish": format!("chuan oauth-mcp finish --provider {p}"),
                        "tunnel": t,
                        "needs_bootstrap": t.get("state").and_then(Value::as_str) == Some("missing"),
                    });
                    if v["needs_bootstrap"] == json!(true) {
                        v["message"] = t.get("message").cloned().unwrap_or(Value::Null);
                    }
                    v
                }
                "auth_status" => {
                    let mut v = status(&ctx);
                    v["tunnel"] = tunnel_info();
                    v
                }
                "auth_refresh" => rt.block_on(ctx.refresh())?,
                _ => return Err(format!("未知工具：{name}")),
            };
            Ok(json!({
                "content": [{"type":"text","text": serde_json::to_string_pretty(&data).unwrap_or_default()}],
                "structuredContent": data,
                "isError": false,
            }))
        }
        _ => Err(format!("未知方法：{method}")),
    }
}

fn http_response(code: u16, body: Value) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    tiny_http::Response::from_data(bytes)
        .with_status_code(code)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        )
}

fn serve(
    base: PathBuf,
    host: &str,
    port: u16,
    token_path: &Path,
    allow_revoke: bool,
) -> Result<(), String> {
    let token = ensure_token_file(token_path)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime 初始化失败：{e}"))?;
    let server = tiny_http::Server::http(format!("{host}:{port}"))
        .map_err(|e| format!("监听 {host}:{port} 失败：{e}"))?;
    eprintln!("oauth-mcp listening on http://{host}:{port}/mcp");
    eprintln!("local token file: {}", token_path.display());
    for mut req in server.incoming_requests() {
        let authorized = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .map(|h| h.value.as_str() == format!("Bearer {token}"))
            .unwrap_or(false);
        if !authorized {
            let _ = req.respond(tiny_http::Response::empty(401));
            continue;
        }
        if req.method() != &tiny_http::Method::Post || req.url().split('?').next() != Some("/mcp") {
            let _ = req.respond(tiny_http::Response::empty(404));
            continue;
        }
        let mut body = String::new();
        if req.as_reader().read_to_string(&mut body).is_err() {
            let _ = req.respond(http_response(400, json!({"error":"read body"})));
            continue;
        }
        let msg: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                let _ = req.respond(http_response(400, json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}})));
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        if method.starts_with("notifications/") || id.is_none() {
            let _ = req.respond(tiny_http::Response::empty(202));
            continue;
        }
        match mcp_dispatch(
            &rt,
            &base,
            DEFAULT_PROVIDER,
            method,
            msg.get("params").unwrap_or(&Value::Null),
            allow_revoke,
        ) {
            Ok(result) => {
                let _ = req.respond(http_response(
                    200,
                    json!({"jsonrpc":"2.0","id":id,"result":result}),
                ));
            }
            Err(e) => {
                let _ = req.respond(http_response(
                    200,
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e}}),
                ));
            }
        }
    }
    Ok(())
}

fn usage() -> String {
    "用法：chuan oauth-mcp <manifest validate|plan <file> | begin|finish|status|refresh | serve> \
     [--provider ID] [--dir DIR] [--manifest FILE] [--token-file FILE] \
     [--host 127.0.0.1] [--port 8790] [--allow-revoke]"
        .into()
}

pub async fn run(args: &[String]) -> Result<(), String> {
    let mut provider = DEFAULT_PROVIDER.to_owned();
    let mut base = default_base();
    let mut manifest_path: Option<PathBuf> = None;
    let mut token_file: Option<PathBuf> = None;
    let mut host = "127.0.0.1".to_owned();
    let mut port: u16 = 8790;
    let mut allow_revoke = false;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--provider" => {
                i += 1;
                provider = args.get(i).cloned().ok_or("--provider 需要参数")?;
            }
            "--dir" => {
                i += 1;
                base = PathBuf::from(args.get(i).ok_or("--dir 需要参数")?);
            }
            "--manifest" => {
                i += 1;
                manifest_path = Some(PathBuf::from(args.get(i).ok_or("--manifest 需要参数")?));
            }
            "--token-file" => {
                i += 1;
                token_file = Some(PathBuf::from(args.get(i).ok_or("--token-file 需要参数")?));
            }
            "--host" => {
                i += 1;
                host = args.get(i).cloned().ok_or("--host 需要参数")?;
            }
            "--port" => {
                i += 1;
                port = args
                    .get(i)
                    .ok_or("--port 需要参数")?
                    .parse()
                    .map_err(|_| "--port 不合法")?;
            }
            "--allow-revoke" => allow_revoke = true,
            a => positional.push(a.to_owned()),
        }
        i += 1;
    }
    if positional.is_empty() {
        return Err(usage());
    }
    if positional[0] == "manifest" {
        let cmd = positional.get(1).ok_or("manifest 需要 validate 或 plan")?;
        let file = positional.get(2).ok_or("manifest 需要文件路径")?;
        let m = read_manifest(Path::new(file))?;
        let errs = manifest_errors(&m);
        if !errs.is_empty() {
            for e in errs {
                eprintln!("- {e}");
            }
            return Err("manifest 校验失败".into());
        }
        if cmd == "plan" {
            println!("{}", serde_json::to_string_pretty(&json!({
                "id": m.id, "kind": m.kind,
                "client_mode": m.client.mode,
                "authorize_url": m.authorize.url, "discovery": m.discovery,
                "scopes": m.authorize.scopes,
                "callback": {"mode": m.callback.mode, "path": m.callback.path, "port": m.callback.port},
                "token_file": m.output.token_file,
            })).unwrap());
        } else {
            println!("manifest OK: {}（{}）", m.id, m.kind);
        }
        return Ok(());
    }

    if positional[0] == "serve" {
        let token_path = token_file.unwrap_or_else(|| base.join("mcp-token"));
        let base_for_serve = base.clone();
        let host_for_serve = host.clone();
        tokio::task::spawn_blocking(move || {
            serve(
                base_for_serve,
                &host_for_serve,
                port,
                &token_path,
                allow_revoke,
            )
        })
        .await
        .map_err(|e| format!("oauth-mcp serve 线程失败：{e}"))??;
        return Ok(());
    }

    let manifest = if let Some(p) = manifest_path {
        read_manifest(&p)?
    } else {
        let p = base
            .join("providers.d")
            .join(format!("{consumer}.toml", consumer = provider));
        if p.exists() {
            read_manifest(&p)?
        } else if provider == DEFAULT_PROVIDER {
            toml::from_str(DEFAULT_MANIFEST).map_err(|e| e.to_string())?
        } else {
            return Err(format!("找不到 manifest：{}", p.display()));
        }
    };
    if provider != manifest.id {
        return Err(format!(
            "provider {provider} 与 manifest id {} 不一致",
            manifest.id
        ));
    }
    let ctx = Ctx::new(base, provider, manifest, token_file)?;
    match positional[0].as_str() {
        "begin" => println!("{}", ctx.begin().await?),
        "finish" => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| e.to_string())?;
            let tok = ctx.finish(s.trim()).await?;
            println!(
                "connected: scope={} expires_in={}",
                tok.get("scope").cloned().unwrap_or(Value::Null),
                tok.get("expires_in").cloned().unwrap_or(Value::Null)
            );
        }
        "status" => println!("{}", serde_json::to_string_pretty(&status(&ctx)).unwrap()),
        "refresh" => {
            let tok = ctx.refresh().await?;
            println!(
                "refreshed: expires_in={}",
                tok.get("expires_in").cloned().unwrap_or(Value::Null)
            );
        }
        other => return Err(format!("未知命令：{other}\n{}", usage())),
    }
    Ok(())
}
