//! 统一配置：**CLI > 环境变量 > TOML > 内置默认**。
//!
//! 密钥只允许通过 `*_file` 引用，不写进 TOML；prompt 同理只给路径。

use std::path::{Path, PathBuf};

use harness_kernel::{ApprovalPolicy, Verdict};
use serde::Deserialize;

#[derive(Debug, Default)]
pub struct Cli {
    pub config: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub db: Option<String>,
    pub backup: bool,
}

impl Cli {
    pub fn parse<I: Iterator<Item = String>>(mut args: I) -> Result<Self, String> {
        // 跳过程序名
        let _ = args.next();
        let mut cli = Cli::default();
        while let Some(a) = args.next() {
            match a.as_str() {
                "backup" => cli.backup = true,
                "--config" => cli.config = args.next().map(PathBuf::from),
                "--provider" => cli.provider = args.next(),
                "--model" => cli.model = args.next(),
                "--db" => cli.db = args.next(),
                "-h" | "--help" => return Err(usage()),
                other => {
                    if let Some(v) = other.strip_prefix("--config=") {
                        cli.config = Some(v.into());
                    } else if let Some(v) = other.strip_prefix("--provider=") {
                        cli.provider = Some(v.into());
                    } else if let Some(v) = other.strip_prefix("--model=") {
                        cli.model = Some(v.into());
                    } else if let Some(v) = other.strip_prefix("--db=") {
                        cli.db = Some(v.into());
                    } else {
                        return Err(format!("未知参数 {other}\n\n{}", usage()));
                    }
                }
            }
        }
        Ok(cli)
    }
}

fn usage() -> String {
    "用法: chuan [backup] [--config <toml>] [--provider <p>] [--model <m>] [--db <path>]".into()
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub app: AppFile,
    #[serde(default)]
    pub telegram: TelegramFile,
    #[serde(default)]
    pub engine: EngineFile,
    #[serde(default)]
    pub actor: ActorFile,
    #[serde(default)]
    pub storage: StorageFile,
    #[serde(default)]
    pub backup: BackupFile,
    #[serde(default)]
    pub policy: PolicyFile,
    #[serde(default)]
    pub skills: SkillsFile,
    #[serde(default)]
    pub runtime: RuntimeFile,
    #[serde(default)]
    pub net: NetFile,
    #[serde(default)]
    pub identity: IdentityFile,
    #[serde(default)]
    pub media: MediaFile,
    #[serde(default)]
    pub proxy: ProxyFile,
    #[serde(default)]
    pub vision: VisionFile,
    #[serde(default)]
    pub sandbox: SandboxFile,
    #[serde(default)]
    pub library: LibraryFile,
    /// 命名 HTTP 服务：`http(service, …)` 用
    #[serde(default)]
    pub services: std::collections::HashMap<String, ServiceFile>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct ServiceFile {
    /// "http"（默认）或 "mcp"
    #[serde(default = "default_service_kind")]
    pub kind: String,
    pub base: String,
    pub token_file: Option<String>,
    /// 默认 `Authorization: Bearer {token}`
    #[serde(default = "default_auth_header")]
    pub header: String,
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    /// MCP：只用这些工具（空 = 全部）
    #[serde(default)]
    pub tools: Vec<String>,
    /// MCP：目录形态 names（默认）/ full / summary
    pub catalog: Option<String>,
}

fn default_service_kind() -> String {
    "http".into()
}

fn default_auth_header() -> String {
    "Authorization: Bearer {token}".into()
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct LibraryFile {
    /// 文件库（永久保存的产物）
    pub dir: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ProxyFile {
    /// 宿主上的 HTTP 代理，如 http://127.0.0.1:7890
    pub url: Option<String>,
    /// 沙盒脚本默认是否带代理：auto（按需）/ always / never
    pub sandbox: Option<String>,
    /// web_fetch 默认：auto（直连失败再试代理）/ always / never
    pub web_fetch: Option<String>,
    /// http/mcp 服务默认是否走代理：never / always
    pub services: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct VisionFile {
    /// 开启后，聊天里的图片会以多模态内容发给模型厂商
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct SandboxFile {
    /// 主人显式授权的额外挂载（把宿主服务暴露进沙盒）
    #[serde(default)]
    pub mounts: Vec<MountFile>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MountFile {
    pub src: String,
    pub dst: String,
    #[serde(default = "default_true")]
    pub ro: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct MediaFile {
    /// 通道收到的附件落盘目录（沙盒里以 /media 只读可见）
    pub dir: Option<String>,
    /// 保留天数（0 = 不按时间删）
    pub keep_days: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct IdentityFile {
    /// 找不到绑定时的兜底用户
    pub default_user: Option<String>,
    #[serde(default)]
    pub user: Vec<UserFile>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UserFile {
    pub id: String,
    pub name: Option<String>,
    /// "channel:addr"，例如 "telegram:81865154"
    #[serde(default)]
    pub channels: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct NetFile {
    /// 出网访问记录（JSONL，供 net_log 审计）
    pub log: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct RuntimeFile {
    /// 依赖装在哪（venv / node_modules）
    pub dir: Option<String>,
    /// pip 源（国内更快；不给用系统配置）
    pub pip_index: Option<String>,
    /// npm registry
    pub npm_registry: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct SkillsFile {
    pub dir: Option<String>,
    /// 沉淀流程说明（不给则用内置 DISTILL）
    pub distill_prompt_file: Option<String>,
    /// 操作规范目录（playbook）：`<name>.md` 可覆盖内置文本
    pub playbooks_dir: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PolicyFile {
    /// 额外要求逐条确认的工具名（只能加严，不能放松域级 Deny）
    #[serde(default)]
    pub require_approval: Vec<String>,
    /// 沙盒内：/ws 可写、宿主不可见、无网络
    #[serde(default = "verdict_allow")]
    pub sandbox: Verdict,
    /// 改自身：装/回滚技能、改配置
    #[serde(default = "verdict_ask")]
    pub self_modify: Verdict,
    /// 出网：真正的闸是域名白名单
    #[serde(default = "verdict_allow")]
    pub outbound: Verdict,
    /// 内网调试（有网卡）
    #[serde(default = "verdict_ask")]
    pub lan: Verdict,
    /// 装依赖（供应链）
    #[serde(default = "verdict_ask")]
    pub install: Verdict,
    /// 宿主执行 / 提权：默认关闭
    #[serde(default = "verdict_deny")]
    pub host_exec: Verdict,
}

impl Default for PolicyFile {
    fn default() -> Self {
        Self {
            require_approval: Vec::new(),
            sandbox: Verdict::Allow,
            self_modify: Verdict::Ask,
            outbound: Verdict::Allow,
            lan: Verdict::Ask,
            install: Verdict::Ask,
            host_exec: Verdict::Deny,
        }
    }
}

fn verdict_allow() -> Verdict {
    Verdict::Allow
}
fn verdict_ask() -> Verdict {
    Verdict::Ask
}
fn verdict_deny() -> Verdict {
    Verdict::Deny
}

#[derive(Clone, Debug, Deserialize)]
pub struct AppFile {
    #[serde(default = "default_name")]
    pub name: String,
    /// 定时任务用的本地时区偏移，如 "+08:00"（不做 DST）
    #[serde(default = "default_utc_offset")]
    pub utc_offset: String,
    #[serde(default = "default_tz")]
    pub timezone: String,
}
impl Default for AppFile {
    fn default() -> Self {
        Self { name: default_name(), utc_offset: default_utc_offset(), timezone: default_tz() }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TelegramFile {
    #[serde(default)]
    pub allowed_users: Vec<i64>,
    #[serde(default = "default_stale")]
    pub stale_after_s: i64,
    pub token_file: Option<String>,
    pub token: Option<String>,
}
impl Default for TelegramFile {
    fn default() -> Self {
        Self { allowed_users: vec![], stale_after_s: default_stale(), token_file: None, token: None }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct EngineFile {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_file: Option<String>,
    pub api_key: Option<String>,
    /// 模型调用走代理；不设则直连
    pub proxy: Option<String>,
    pub system_prompt_file: Option<String>,
    /// 追加到 system prompt 末尾（不替换内置规则）；调教用，改完重启即生效
    pub system_prompt_extra_file: Option<String>,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    #[serde(default = "default_backoff")]
    pub retry_backoff_ms: u64,
}
impl Default for EngineFile {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            base_url: None,
            model: None,
            api_key_file: None,
            api_key: None,
            proxy: None,
            system_prompt_file: None,
            system_prompt_extra_file: None,
            max_retries: default_retries(),
            retry_backoff_ms: default_backoff(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ActorFile {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_wall")]
    pub wall_clock_ms: u64,
    pub workspace: Option<String>,
    /// 用户投递目录（只读挂到沙盒 /inbox）
    pub inbox: Option<String>,
    #[serde(default = "default_ctx_tokens")]
    pub context_budget_tokens: usize,
    /// workspace 清理：超过这么多天的文件删掉（0 = 不按时间删）
    #[serde(default = "default_ws_keep_days")]
    pub workspace_keep_days: u64,
    /// workspace 总量上限（MB，0 = 不限）
    #[serde(default = "default_ws_max_mb")]
    pub workspace_max_mb: u64,
}
impl Default for ActorFile {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            wall_clock_ms: default_wall(),
            workspace: None,
            inbox: None,
            context_budget_tokens: default_ctx_tokens(),
            workspace_keep_days: default_ws_keep_days(),
            workspace_max_mb: default_ws_max_mb(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct StorageFile {
    pub db: Option<String>,
    pub lock: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BackupFile {
    #[serde(default = "default_backup_dir")]
    pub dir: String,
    #[serde(default = "default_keep_days")]
    pub keep_days: u64,
    #[serde(default = "default_keep_min")]
    pub keep_min: usize,
    #[serde(default = "default_max_mb")]
    pub max_total_mb: u64,
}
impl Default for BackupFile {
    fn default() -> Self {
        Self {
            dir: default_backup_dir(),
            keep_days: default_keep_days(),
            keep_min: default_keep_min(),
            max_total_mb: default_max_mb(),
        }
    }
}

fn default_name() -> String { "home".into() }
fn default_utc_offset() -> String {
    "+08:00".into()
}
fn default_tz() -> String { "Asia/Shanghai".into() }
fn default_stale() -> i64 { 300 }
fn default_provider() -> String { "echo".into() }
fn default_max_turns() -> u32 { 8 }
fn default_retries() -> u32 { 3 }
fn default_ctx_tokens() -> usize { 24_000 }
fn default_ws_keep_days() -> u64 { 7 }
fn default_ws_max_mb() -> u64 { 256 }
fn default_backoff() -> u64 { 1000 }
// 0 = 不限：长任务优先，靠 max_turns 与手工 /cancel 收口
fn default_wall() -> u64 { 0 }
fn default_backup_dir() -> String {
    format!("{}/.local/share/chuan/backups", std::env::var("HOME").unwrap_or_default())
}
fn default_keep_days() -> u64 { 7 }
fn default_keep_min() -> usize { 2 }
fn default_max_mb() -> u64 { 256 }

pub struct Config {
    pub app_name: String,
    pub timezone: String,
    pub allowed_users: Vec<i64>,
    pub stale_after_s: i64,
    pub token: String,
    pub provider: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: String,
    pub llm_proxy: Option<String>,
    pub system_prompt: String,
    pub max_turns: u32,
    pub wall_clock_ms: u64,
    pub workspace: String,
    pub inbox: String,
    pub db: String,
    pub lock: String,
    pub backup_dir: String,
    pub backup_keep_days: u64,
    pub backup_keep_min: usize,
    pub backup_max_mb: u64,
    pub policy: ApprovalPolicy,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
    pub context_budget_chars: usize,
    pub skills_dir: String,
    pub distill_prompt: String,
    pub playbooks_dir: String,
    pub tz_offset_min: i32,
    pub identity: std::sync::Arc<harness_kernel::Directory>,
    pub net_log: String,
    pub media_dir: String,
    pub media_keep_days: u64,
    pub sandbox_mounts: Vec<harness_capabilities::Mount>,
    pub vision: bool,
    pub proxy: harness_capabilities::ProxyConfig,
    pub library_dir: String,
    pub services: Vec<harness_capabilities::Service>,
    pub mcp_servers: Vec<std::sync::Arc<harness_capabilities::McpServer>>,
    pub config_dir: std::path::PathBuf,
    pub runtime_dir: String,
    pub pip_index: Option<String>,
    pub npm_registry: Option<String>,
    pub workspace_keep_days: u64,
    pub workspace_max_mb: u64,
}

/// "+08:00" / "-05:30" / "8" → 分钟
pub fn parse_offset(s: &str) -> Result<i32, String> {
    let s = s.trim();
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None => (rest, "0"),
    };
    let hh: i32 = h.trim().parse().map_err(|_| format!("时区偏移不合法：{s}"))?;
    let mm: i32 = m.trim().parse().map_err(|_| format!("时区偏移不合法：{s}"))?;
    if !(0..=14).contains(&hh) || !(0..60).contains(&mm) {
        return Err(format!("时区偏移超出范围：{s}"));
    }
    Ok(sign * (hh * 60 + mm))
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()))
    } else {
        PathBuf::from(p)
    }
}

fn read_secret(inline: &Option<String>, file: &Option<String>, label: &str) -> Result<String, String> {
    if let Some(v) = inline {
        if !v.trim().is_empty() {
            return Ok(v.trim().to_owned());
        }
    }
    if let Some(f) = file {
        let p = expand_tilde(f);
        return std::fs::read_to_string(&p)
            .map(|s| s.trim().to_owned())
            .map_err(|e| format!("读取 {label} 失败 {}: {e}", p.display()));
    }
    Err(format!("缺少 {label}（用 *_file 引用）"))
}

/// 查找配置文件：CLI > CHUAN_CONFIG > ~/.config/chuan/chuan.toml > /etc/chuan/chuan.toml
fn find_config(cli: &Cli) -> Option<PathBuf> {
    if let Some(p) = &cli.config {
        return Some(p.clone());
    }
    if let Ok(p) = std::env::var("CHUAN_CONFIG") {
        return Some(expand_tilde(&p));
    }
    let user = expand_tilde("~/.config/chuan/chuan.toml");
    if user.exists() {
        return Some(user);
    }
    let etc = PathBuf::from("/etc/chuan/chuan.toml");
    if etc.exists() {
        return Some(etc);
    }
    None
}

pub fn load(cli: &Cli) -> Result<Config, String> {
    let path = find_config(cli);
    let mut file = FileConfig::default();
    if let Some(p) = &path {
        let text = std::fs::read_to_string(p)
            .map_err(|e| format!("读取配置 {}: {e}", p.display()))?;
        file = toml::from_str(&text).map_err(|e| format!("解析 {}: {e}", p.display()))?;
    }

    // ---- 运行期新增的服务（services.d/*.toml，agent 用 service 工具写的）----
    let config_dir = path
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| {
            std::path::PathBuf::from(format!(
                "{}/.config/chuan",
                std::env::var("HOME").unwrap_or_default()
            ))
        });
    let svc_dir = config_dir.join("services.d");
    if let Ok(rd) = std::fs::read_dir(&svc_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("toml") {
                continue;
            }
            let Some(name) = p.file_stem().and_then(|s| s.to_str()) else { continue };
            match std::fs::read_to_string(&p)
                .map_err(|e| e.to_string())
                .and_then(|t| toml::from_str::<ServiceFile>(&t).map_err(|e| e.to_string()))
            {
                Ok(svc) => {
                    file.services.insert(name.to_owned(), svc);
                }
                Err(e) => eprintln!("跳过服务配置 {}：{e}", p.display()),
            }
        }
    }

    // ---- 环境变量覆盖 ----
    let mut provider = file.engine.provider.clone();
    let mut base_url = file.engine.base_url.clone();
    let mut model = file.engine.model.clone();
    let mut api_key_file = file.engine.api_key_file.clone();
    let mut api_key_inline = file.engine.api_key.clone();
    let mut llm_proxy = file.engine.proxy.clone();
    let mut prompt_file = file.engine.system_prompt_file.clone();
    let mut prompt_extra_file = file.engine.system_prompt_extra_file.clone();
    let mut token = file.telegram.token.clone();
    let mut token_file = file.telegram.token_file.clone();
    let mut allowed = file.telegram.allowed_users.clone();
    let mut db = file.storage.db.clone();
    let mut lock = file.storage.lock.clone();
    let mut workspace = file.actor.workspace.clone();
    let mut require_approval = file.policy.require_approval.clone();

    if let Ok(v) = std::env::var("CHUAN_PROVIDER") { provider = v; }
    if let Ok(v) = std::env::var("CHUAN_BASE_URL") { base_url = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_MODEL") { model = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_KEY_FILE") { api_key_file = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_API_KEY") { api_key_inline = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_LLM_PROXY") { llm_proxy = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_SYSTEM_PROMPT_FILE") { prompt_file = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_SYSTEM_PROMPT_EXTRA_FILE") {
        prompt_extra_file = Some(v);
    }
    if let Ok(v) = std::env::var("CHUAN_TELEGRAM_TOKEN") { token = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_TELEGRAM_TOKEN_FILE") { token_file = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_ALLOWED_USERS") {
        allowed = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
    }
    if let Ok(v) = std::env::var("CHUAN_DB") { db = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_LOCK") { lock = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_WORKSPACE") { workspace = Some(v); }
    if let Ok(v) = std::env::var("CHUAN_REQUIRE_APPROVAL") {
        require_approval = v.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
    }

    // ---- CLI 覆盖 ----
    if let Some(v) = &cli.provider { provider = v.clone(); }
    if let Some(v) = &cli.model { model = Some(v.clone()); }
    if let Some(v) = &cli.db { db = Some(v.clone()); }

    // 未显式配置时用约定路径
    let token_file = token_file.or_else(|| {
        Some(format!("{}/.config/chuan/secrets/telegram_token", std::env::var("HOME").unwrap_or_default()))
    });
    let token = read_secret(&token, &token_file, "telegram token")?;
    let api_key = if provider == "echo" {
        String::new()
    } else {
        read_secret(&api_key_inline, &api_key_file, "模型 api key")?
    };

    let system_prompt = match prompt_file {
        Some(f) => std::fs::read_to_string(expand_tilde(&f))
            .map_err(|e| format!("读取 prompt {f}: {e}"))?,
        None => crate::system_prompt::DEFAULT.to_owned(),
    };
    // 追加段：内置规则不动，只往后接。调教时改这个文件，不必重新编译。
    // 显式配置但文件不存在 → 报错，不静默跳过（否则你以为改生效了，其实没有）。
    let extra = match prompt_extra_file {
        Some(f) => Some(
            std::fs::read_to_string(expand_tilde(&f))
                .map_err(|e| format!("读取追加 prompt {f}: {e}"))?,
        ),
        None => None,
    };
    let system_prompt = append_prompt(system_prompt, extra);

    let home = std::env::var("HOME").unwrap_or_default();
    Ok(Config {
        app_name: file.app.name,
        timezone: file.app.timezone,
        allowed_users: allowed,
        stale_after_s: file.telegram.stale_after_s,
        token,
        provider,
        base_url,
        model,
        api_key,
        llm_proxy,
        system_prompt,
        max_turns: file.actor.max_turns,
        wall_clock_ms: file.actor.wall_clock_ms,
        workspace: workspace.unwrap_or_else(|| format!("{home}/.local/share/chuan/workspaces")),
        inbox: file
            .actor
            .inbox
            .clone()
            .unwrap_or_else(|| format!("{home}/.local/share/chuan/inbox")),
        db: db.unwrap_or_else(|| format!("{home}/.local/share/chuan/chuan.db")),
        lock: lock.unwrap_or_else(|| format!("{home}/.local/share/chuan/chuan.lock")),
        backup_dir: file.backup.dir,
        backup_keep_days: file.backup.keep_days,
        backup_keep_min: file.backup.keep_min,
        backup_max_mb: file.backup.max_total_mb,
        policy: ApprovalPolicy {
            sandbox: file.policy.sandbox,
            self_modify: file.policy.self_modify,
            outbound: file.policy.outbound,
            lan: file.policy.lan,
            install: file.policy.install,
            host_exec: file.policy.host_exec,
            extra_tools: require_approval,
        },
        max_retries: file.engine.max_retries,
        retry_backoff_ms: file.engine.retry_backoff_ms,
        // 粗估：中英混排约 3 字符/token
        context_budget_chars: file.actor.context_budget_tokens.saturating_mul(3),
        skills_dir: file
            .skills
            .dir
            .unwrap_or_else(|| format!("{home}/.local/share/chuan/skills")),
        workspace_keep_days: file.actor.workspace_keep_days,
        workspace_max_mb: file.actor.workspace_max_mb,
        distill_prompt: match file.skills.distill_prompt_file.as_deref() {
            Some(f) => std::fs::read_to_string(f)
                .map_err(|e| format!("读取沉淀 prompt {f}: {e}"))?,
            None => {
                let f = format!("{home}/.config/chuan/playbooks/distill.md");
                match std::fs::read_to_string(&f) {
                    Ok(s) if !s.trim().is_empty() => s,
                    _ => crate::system_prompt::DISTILL.to_owned(),
                }
            }
        },
        playbooks_dir: file
            .skills
            .playbooks_dir
            .unwrap_or_else(|| format!("{home}/.config/chuan/playbooks")),
        tz_offset_min: parse_offset(&file.app.utc_offset)?,
        identity: std::sync::Arc::new(harness_kernel::Directory {
            users: file
                .identity
                .user
                .iter()
                .map(|u| harness_kernel::User {
                    id: u.id.clone(),
                    name: u.name.clone(),
                    channels: u
                        .channels
                        .iter()
                        .filter_map(|c| harness_kernel::Binding::parse(c))
                        .collect(),
                })
                .collect(),
            default_user: file.identity.default_user.clone(),
        }),
        net_log: file
            .net
            .log
            .unwrap_or_else(|| format!("{home}/.local/share/chuan/net-access.jsonl")),
        media_dir: file
            .media
            .dir
            .unwrap_or_else(|| format!("{home}/.local/share/chuan/media")),
        media_keep_days: file.media.keep_days.unwrap_or(30),
        config_dir,
        mcp_servers: file
            .services
            .iter()
            .filter(|(_, s)| s.kind.eq_ignore_ascii_case("mcp"))
            .filter_map(|(name, s)| {
                harness_capabilities::McpServer::new(
                    name.clone(),
                    s.base.clone(),
                    s.token_file.as_ref().map(|p| expand_tilde(p)),
                    s.header.clone(),
                    s.extra_headers.clone(),
                    s.tools.clone(),
                    s.catalog.clone().unwrap_or_else(|| "names".into()),
                    match (
                        harness_capabilities::ProxyMode::parse(
                            file.proxy.services.as_deref().unwrap_or("never"),
                        ),
                        file.proxy.url.as_ref(),
                    ) {
                        (harness_capabilities::ProxyMode::Always, Some(url)) => {
                            harness_capabilities::proxy_for(url)
                        }
                        _ => None,
                    },
                )
                .map_err(|e| tracing::warn!(service = %name, error = %e, "mcp 初始化失败"))
                .ok()
            })
            .collect(),
        services: file
            .services
            .iter()
            .filter(|(_, s)| !s.kind.eq_ignore_ascii_case("mcp"))
            .map(|(name, s)| harness_capabilities::Service {
                name: name.clone(),
                base: s.base.clone(),
                token_file: s.token_file.as_ref().map(|p| expand_tilde(p)),
                header: s.header.clone(),
                extra_headers: s.extra_headers.clone(),
            })
            .collect(),
        library_dir: file
            .library
            .dir
            .unwrap_or_else(|| format!("{home}/.local/share/chuan/files")),
        vision: file.vision.enabled,
        proxy: harness_capabilities::ProxyConfig {
            // 没写配置就沿用标准环境变量（守护进程通常从 systemd 环境继承）
            url: file
                .proxy
                .url
                .clone()
                .or_else(|| std::env::var("https_proxy").ok())
                .or_else(|| std::env::var("http_proxy").ok())
                .filter(|u| !u.trim().is_empty()),
            sandbox: harness_capabilities::ProxyMode::parse(
                file.proxy.sandbox.as_deref().unwrap_or("auto"),
            ),
            web_fetch: harness_capabilities::ProxyMode::parse(
                file.proxy.web_fetch.as_deref().unwrap_or("auto"),
            ),
            services: harness_capabilities::ProxyMode::parse(
                file.proxy.services.as_deref().unwrap_or("never"),
            ),
        },
        sandbox_mounts: file
            .sandbox
            .mounts
            .iter()
            .map(|m| harness_capabilities::Mount {
                src: expand_tilde(&m.src),
                dst: std::path::PathBuf::from(&m.dst),
                ro: m.ro,
            })
            .collect(),
        runtime_dir: file
            .runtime
            .dir
            .unwrap_or_else(|| format!("{home}/.local/share/chuan/rt")),
        pip_index: file.runtime.pip_index,
        npm_registry: file.runtime.npm_registry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde() {
        let p = expand_tilde("~/x/y");
        assert!(p.to_string_lossy().ends_with("/x/y"));
        assert!(!p.to_string_lossy().starts_with('~'));
    }

    #[test]
    fn parses_cli() {
        let args = vec![
            "chuan".to_string(),
            "--provider=zai".into(),
            "--model".into(),
            "glm-5.3-flash".into(),
            "backup".into(),
        ];
        let cli = Cli::parse(args.into_iter()).unwrap();
        assert_eq!(cli.provider.as_deref(), Some("zai"));
        assert_eq!(cli.model.as_deref(), Some("glm-5.3-flash"));
        assert!(cli.backup);
    }

    #[test]
    fn parses_toml() {
        let text = r#"
[telegram]
allowed_users = [1, 2]
[engine]
provider = "zai"
model = "glm-5.3-flash"
api_key_file = "~/k"
[backup]
keep_days = 3
"#;
        let f: FileConfig = toml::from_str(text).unwrap();
        assert_eq!(f.telegram.allowed_users, vec![1, 2]);
        assert_eq!(f.engine.provider, "zai");
        assert_eq!(f.backup.keep_days, 3);
        assert_eq!(f.backup.keep_min, 2);
    }

    #[test]
    fn rejects_unknown_arg() {
        let args = vec!["chuan".to_string(), "--nope".to_string()];
        assert!(Cli::parse(args.into_iter()).is_err());
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("app_name", &self.app_name)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("db", &self.db)
            .field("mcp_servers", &self.mcp_servers.len())
            .finish_non_exhaustive()
    }
}

/// 把追加 prompt 接到基础 prompt 末尾（不替换内置规则）。空白内容视为没有。
fn append_prompt(base: String, extra: Option<String>) -> String {
    match extra {
        Some(e) if !e.trim().is_empty() => {
            format!("{}\n\n{}", base.trim_end(), e.trim())
        }
        _ => base,
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::append_prompt;

    #[test]
    fn extra_is_appended_not_replacing() {
        let base = "内置规则".to_owned();
        assert_eq!(append_prompt(base.clone(), None), "内置规则");
        assert_eq!(append_prompt(base.clone(), Some("  \n ".into())), "内置规则");
        assert_eq!(
            append_prompt(base, Some("追加一句".into())),
            "内置规则\n\n追加一句"
        );
    }
}
