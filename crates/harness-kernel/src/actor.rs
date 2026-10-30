//! 会话运行时：按会话并发，支持指令与取消。
//!
//! - 同一会话内串行（一把异步锁），跨会话并发；
//! - 每次 run 注册一个取消令牌，`/cancel` 才能打到运行中的任务；
//! - `/new` 通过 epoch 轮换会话 id，旧账本保留。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::channel::{Channel, ChannelError, Inbound, InboundKind};
use crate::driver::{EventSink, TurnDriver};
use crate::env::ExecutionEnv;
use crate::ids::{ApprovalId, CallId, ChannelId, ConversationId, MsgRef, RunId, now_ms};
use crate::effect::EffectClass;
use crate::event::{EventKind, NewEvent};
use crate::record::{NewApproval, ToolCallRecord};
use crate::identity::Directory;
use crate::render::{ActionOutcome, TurnRenderer, apply_action};
use crate::runner::{ApprovalGate, RunRequest, Runner};
use crate::store::Store;
use crate::tool::Workspace;
use crate::tool_registry::ToolRegistry;
use crate::view::{Block, RenderAction, RenderEvent, Renderer, RunStatus, View};

/// 审批等待上限：到点按拒绝处理，避免 run 永远挂着。
const APPROVAL_TTL_S: u64 = 300;

/// 1000 → `1.0k`。用量只求量级，不追求精确到个位。
fn short_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// 已知指令（与 `telegram::command_specs` 保持一致；漏一个就只是"未知指令"提示，不会误执行）。
fn is_known_command(name: &str) -> bool {
    matches!(
        name,
        "start"
            | "help"
            | "new"
            | "cancel"
            | "status"
            | "usage"
            | "prompt"
            | "agents"
            | "tasks"
            | "facts"
            | "reminders"
            | "forget"
            | "approve"
            | "deny"
    )
}

/// `/ws/demo 里那个脚本…` 不是指令，是路径。
///
/// 只按"首字符是 /"判断会把所有以绝对路径开头的话都吞掉（真实事故：
/// `/ws/demo` 被当成未知指令，用户以为机器人坏了）。
fn looks_like_path(name: &str) -> bool {
    name.contains('/') || name.starts_with('~')
}

/// 单行截断：列表里只显示第一行，避免多行内容把 /reminders 刷屏。
fn truncate_line(s: &str, n: usize) -> String {
    let one: String = s.lines().next().unwrap_or("").chars().take(n).collect();
    one
}

/// 按字符切成不超过 `n` 的片段（Telegram 单条上限 4096，留出格式余量）。
fn shard_text(s: &str, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if cur.chars().count() >= n {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `/usage` 的一行。`cost_est` 为 0 时明说未配价目，别让人以为是免费；
/// `limit > 0` 时把额度一起写出来，省得再去翻配置。
fn usage_line(label: &str, u: &crate::record::UsageTotal, limit: u64) -> String {
    let money = if u.cost_est > 0.0 {
        format!("¥{:.4}", u.cost_est)
    } else {
        "金额未配 [pricing]".to_owned()
    };
    let quota = if limit > 0 {
        format!(" · 额度 {}/{}", short_tokens(u.tokens()), short_tokens(limit))
    } else {
        String::new()
    };
    format!(
        "{label}：{} in / {} out · {money}{quota}",
        short_tokens(u.prompt_tokens),
        short_tokens(u.completion_tokens)
    )
}

#[derive(Clone)]
pub struct ActorConfig {
    pub model: String,
    /// 每轮注入的 system 指令（身份/语言/工具规矩）。
    pub system_prompt: String,
    pub max_turns: u32,
    pub wall_clock_ms: u64,
    pub workspace_root: String,
    /// 审批策略：按执行域判定，`extra_tools` 可额外加严
    pub policy: crate::policy::ApprovalPolicy,
    /// 上下文预算（估算字符数）
    pub context_budget_chars: usize,
    /// 并发车道（D11）。所有 run 共享同一组信号量。
    pub lanes: crate::lanes::Lanes,
    /// 子 agent 闸门与默认预算（D19/D20）
    pub agents: crate::agent::AgentLimits,
    /// token 预算（0 = 不限）。超限在 run 入口与 spawn 前直接拒绝。
    pub budget: crate::budget::BudgetLimits,
    /// 可热加载的角色目录（`[agents] roles_dir`）。`None` = 只有配置文件里的角色。
    pub roles: Option<Arc<dyn crate::agent::RoleSource>>,
    /// 技能目录（内核不解析格式，只取一段文本）
    pub skill_source: Option<Arc<dyn crate::retriever::SkillSource>>,
    /// 工具目录（MCP 等外部工具清单），同样只取一段文本
    pub tool_source: Option<Arc<dyn crate::retriever::SkillSource>>,
    /// 沉淀技能时注入给模型的指令（SOP 交给应用层配置，内核只负责触发）
    pub distill_prompt: String,
    /// 本地时区偏移（分钟，东八区=480）。定时任务只支持固定偏移，不做 DST。
    pub tz_offset_min: i32,
    /// 宿主机路径布局（由应用层按实际配置生成）。模型看不到这些路径，
    /// 但需要能回答"key/配置/技能该放哪"。
    pub host_paths: Option<String>,
    /// 是否把图片作为多模态内容发给模型（会把图上传给模型厂商，默认关）
    pub vision: bool,
    /// 代理说明（有代理时注入；固定内容，避免模型把地址写死进技能）
    pub proxy_hint: Option<String>,
    /// 身份表：channel+addr → user。单用户时只有一条绑定。
    pub identity: Arc<Directory>,
}

pub struct ConversationActor<S: Store, D: TurnDriver> {
    pub store: Arc<S>,
    pub driver: Arc<D>,
    pub tools: Arc<ToolRegistry>,
    pub env: Arc<dyn ExecutionEnv>,
    /// 通道表：`ChannelId` → 实现。多通道并存时按 `Inbound.channel` 路由。
    pub channels: std::collections::HashMap<String, Arc<dyn Channel>>,
    /// 找不到通道时的兜底（老提醒里可能写着已下线的通道）
    pub default_channel: String,
    pub cfg: ActorConfig,
    retriever: Option<Arc<dyn crate::retriever::Retriever>>,
    runs: Arc<Mutex<HashMap<String, CancellationToken>>>,
    approvals: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// 子会话序号：`sub:<parent>#<n>`（按父会话单调递增）
    agent_seq: Arc<Mutex<HashMap<String, u32>>>,
    /// 每个 run 当前派出的子 agent 数（名额闸门）
    agent_slots: Arc<Mutex<HashMap<String, usize>>>,
    /// 运行中的子 agent：child_conv → 现场
    agents_running: Arc<Mutex<HashMap<String, RunningAgent>>>,
    /// 最近一次前台活动的时刻（后台任务据此让路）
    last_active: Arc<Mutex<i64>>,
    /// 当前的 typing 令牌：`/cancel` 要能立刻停掉"正在输入"
    typing: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// 子 agent 的实时进度：child_conv → "第 3 轮 · 执行 web_fetch"
    agent_progress: Arc<Mutex<HashMap<String, String>>>,
    /// 运行中的后台任务：task_id → 取消令牌
    bg_running: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// 后台任务编号
    bg_seq: Arc<Mutex<u64>>,
    /// 未授权提示的节流：`{channel}:{addr}` → 上次提示时刻（每小时最多一次）
    unauth_notice: Arc<Mutex<HashMap<String, i64>>>,
}

impl<S: Store, D: TurnDriver> Clone for ConversationActor<S, D> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            driver: Arc::clone(&self.driver),
            tools: Arc::clone(&self.tools),
            env: Arc::clone(&self.env),
            channels: self.channels.clone(),
            default_channel: self.default_channel.clone(),
            cfg: self.cfg.clone(),
            retriever: self.retriever.clone(),
            runs: Arc::clone(&self.runs),
            approvals: Arc::clone(&self.approvals),
            locks: Arc::clone(&self.locks),
            agent_seq: Arc::clone(&self.agent_seq),
            agent_slots: Arc::clone(&self.agent_slots),
            agents_running: Arc::clone(&self.agents_running),
            last_active: Arc::clone(&self.last_active),
            typing: Arc::clone(&self.typing),
            agent_progress: Arc::clone(&self.agent_progress),
            bg_running: Arc::clone(&self.bg_running),
            bg_seq: Arc::clone(&self.bg_seq),
            unauth_notice: Arc::clone(&self.unauth_notice),
        }
    }
}

/// 一次 run 的执行现场：**任何**退出路径（含 `?` 早退、panic）都必须清理。
///
/// 之前清理写在函数末尾，被 429 的 `?` 跳过，导致：typing 循环继续每 4s 发一次
/// `sendChatAction`（看起来永远"正在输入"），`runs` 里留着一个死 token（`/status`
/// 一直说"运行中"，`/cancel` 取消的是空气）。
struct RunScope {
    runs: Arc<Mutex<HashMap<String, CancellationToken>>>,
    typing_map: Arc<Mutex<HashMap<String, CancellationToken>>>,
    conversation: String,
    typing: CancellationToken,
    typing_task: Option<tokio::task::JoinHandle<()>>,
}

impl RunScope {
    /// 正常收尾：先停 typing 并等它退出，避免脱离任务多活一拍
    async fn shutdown(&mut self) {
        self.typing.cancel();
        if let Some(t) = self.typing_task.take() {
            let _ = t.await;
        }
    }
}

impl Drop for RunScope {
    fn drop(&mut self) {
        self.typing.cancel();
        self.runs.lock().unwrap().remove(&self.conversation);
        self.typing_map.lock().unwrap().remove(&self.conversation);
        // JoinHandle 直接丢弃 = detach；typing 任务会在 stop.cancelled() 处自行退出
    }
}

/// 状态反应的四个 emoji（D24）。都在 Telegram 的 73 个白名单内；
/// ⏳/✅/❌ 不在白名单，用了会被 400 REACTION_INVALID 拒掉。
const EMOJI_QUEUED: &str = "😐";
const EMOJI_RUNNING: &str = "👀";
const EMOJI_OK: &str = "👍";
const EMOJI_FAIL: &str = "👎";

/// 一轮 run 的结局，只用于决定状态反应。`Cancelled` 是清空而不是失败：
/// 用户主动取消既不是成功也不是失败。
enum RunEnd {
    Done,
    Failed,
    Cancelled,
}

/// 终稿重发预算：run 已经结束，等满 retry_after 不会拖累 run（与流式刷新不同）。
const FINAL_ATTEMPTS: usize = 3;
const FINAL_WAIT_CAP_MS: u64 = 60_000;

impl<S: Store, D: TurnDriver> ConversationActor<S, D> {
    /// 终稿必须尽量送达：被限流就按 retry_after 等窗口再发，而不是丢掉。
    async fn apply_final(
        &self,
        ch: &dyn Channel,
        conversation: &str,
        renderer: &mut TurnRenderer,
        action: RenderAction,
    ) -> bool {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match apply_action(ch, conversation, renderer, action.clone()).await {
                Ok(ActionOutcome::Done) => return true,
                Ok(ActionOutcome::Deferred { retry_after_ms }) => {
                    if attempt >= FINAL_ATTEMPTS {
                        tracing::error!(retry_after_ms, attempt, "终稿仍被限流，放弃发送");
                        return false;
                    }
                    let wait = (retry_after_ms as u64).min(FINAL_WAIT_CAP_MS);
                    tracing::warn!(retry_after_ms, wait, attempt, "终稿被限流，等窗口后重发");
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "终稿发送失败");
                    return false;
                }
            }
        }
    }
}

impl<S: Store, D: TurnDriver> ConversationActor<S, D> {
    /// 单通道便捷构造（测试与单通道部署用）。
    pub fn new(
        store: Arc<S>,
        driver: Arc<D>,
        tools: Arc<ToolRegistry>,
        env: Arc<dyn ExecutionEnv>,
        channel: Arc<dyn Channel>,
        cfg: ActorConfig,
        retriever: Option<Arc<dyn crate::retriever::Retriever>>,
    ) -> Self {
        Self::with_channels(store, driver, tools, env, vec![channel], cfg, retriever)
    }

    /// 多通道并存：第一个是默认通道。
    pub fn with_channels(
        store: Arc<S>,
        driver: Arc<D>,
        tools: Arc<ToolRegistry>,
        env: Arc<dyn ExecutionEnv>,
        channels: Vec<Arc<dyn Channel>>,
        cfg: ActorConfig,
        retriever: Option<Arc<dyn crate::retriever::Retriever>>,
    ) -> Self {
        let mut map: std::collections::HashMap<String, Arc<dyn Channel>> =
            std::collections::HashMap::new();
        let mut default_channel = String::new();
        for (i, c) in channels.into_iter().enumerate() {
            if i == 0 {
                default_channel = c.id().0.clone();
            }
            map.insert(c.id().0.clone(), c);
        }
        assert!(!default_channel.is_empty(), "至少需要一个通道");
        Self {
            store,
            driver,
            tools,
            env,
            channels: map,
            default_channel,
            cfg,
            retriever,
            runs: Arc::new(Mutex::new(HashMap::new())),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            locks: Arc::new(Mutex::new(HashMap::new())),
            agent_seq: Arc::new(Mutex::new(HashMap::new())),
            agent_slots: Arc::new(Mutex::new(HashMap::new())),
            agents_running: Arc::new(Mutex::new(HashMap::new())),
            last_active: Arc::new(Mutex::new(now_ms())),
            typing: Arc::new(Mutex::new(HashMap::new())),
            agent_progress: Arc::new(Mutex::new(HashMap::new())),
            bg_running: Arc::new(Mutex::new(HashMap::new())),
            bg_seq: Arc::new(Mutex::new(0)),
            unauth_notice: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn run(
        self: Arc<Self>,
        mut inbound: mpsc::Receiver<Inbound>,
        cancel: CancellationToken,
    ) -> Result<(), ChannelError> {
        // 重启收尾：上次 running 的后台任务只通知、不重跑（D4）。
        let resumed = self.resume_agent_tasks().await;
        if resumed > 0 {
            tracing::warn!(tasks = resumed, "后台任务被重启打断，已通知主人");
        }
        let mut tasks = tokio::task::JoinSet::new();
        // 每个会话的"在跑 + 排队"计数。只在 `run()` 里维护：入站全从这里进，
        // 拿它判"是不是排队"比看 `runs` 可靠（`runs` 要等拿到锁才写入，同批消息会漏判）。
        let mut inflight: std::collections::HashMap<String, Arc<std::sync::atomic::AtomicUsize>> =
            std::collections::HashMap::new();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = inbound.recv() => {
                    let Some(mut inb) = msg else { break };
                    // 只有"人在说话"才算前台活动。查状态、取消这类指令不该把后台任务饿死
                    // （真实事故：用户每分钟 /tasks 一次，任务一直 pending）。
                    // 授权只认 `[identity]` 绑定（D26）：一人多账号 / 多人都是多条绑定，
                    // 未绑定一律不处理，并回一句"未授权"（每小时最多一次，防刷屏）。
                    // 身份表为空 = 没配置身份（内核保持中立，空表不做拦截）；应用层在启动时拒绝空表。
                    if !self.cfg.identity.users.is_empty()
                        && self
                            .cfg
                            .identity
                            .lookup(&inb.channel.0, &inb.actor)
                            .is_none()
                    {
                        self.notice_unauthorized(&inb).await;
                        continue;
                    }
                    if inb.kind == InboundKind::Text {
                        *self.last_active.lock().unwrap() = crate::ids::now_ms();
                    }
                    if inb.kind == InboundKind::Command {
                        // 以 `/` 开头但其实是路径的消息，按普通文本处理。
                        if let Some(t) = inb.text.as_deref() {
                            let first = t.trim().trim_start_matches('/');
                            let name = first.split_whitespace().next().unwrap_or("");
                            if !is_known_command(name) && looks_like_path(name) {
                                inb.kind = InboundKind::Text;
                            }
                        }
                    }
                    if inb.kind == InboundKind::Command {
                        if let Err(e) = self.handle_command(inb).await {
                            tracing::warn!(error = %e, "command failed");
                        }
                    } else if inb.kind == InboundKind::Callback {
                        self.handle_callback(inb).await;
                    } else {
                        // 会话里已经有别的 run（在跑或排队）→ 这条要等锁。
                        // 先把"未开始"标出来，否则界面上只看到上一条在动（D24）。
                        let counter = Arc::clone(
                            inflight
                                .entry(inb.conversation.clone())
                                .or_insert_with(|| Arc::new(std::sync::atomic::AtomicUsize::new(0))),
                        );
                        let ahead = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if ahead > 0 {
                            let me = Arc::clone(&self);
                            let ch = self.channel_for(&inb.channel);
                            let conv = inb.conversation.clone();
                            let m = inb.msg.clone();
                            tokio::spawn(async move {
                                me.mark(&*ch, &conv, m.as_ref(), EMOJI_QUEUED).await;
                            });
                        }
                        let me = Arc::clone(&self);
                        let done = Arc::clone(&counter);
                        tasks.spawn(async move {
                            let r = me.handle_run(inb).await;
                            done.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                            if let Err(e) = r {
                                tracing::warn!(error = %e, "run failed");
                            }
                        });
                    }
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }

        // 关停：取消所有进行中的 run，并等任务收尾（避免半写状态）
        for (_, t) in self.runs.lock().unwrap().iter() {
            t.cancel();
        }
        while tasks.join_next().await.is_some() {}
        Ok(())
    }

    /// 按 `ChannelId` 取通道；找不到回落到默认通道（并留一条 warn）。
    pub fn channel_for(&self, id: &ChannelId) -> Arc<dyn Channel> {
        if let Some(c) = self.channels.get(&id.0) {
            return Arc::clone(c);
        }
        tracing::warn!(channel = %id.0, default = %self.default_channel, "未注册的通道，回落到默认");
        Arc::clone(
            self.channels
                .get(&self.default_channel)
                .expect("至少有一个通道"),
        )
    }

    /// 同一会话的键：**通道 + 地址**。两个人同名地址（如都是 "1000"）不会互相抢锁。
    fn conv_key(inb: &Inbound) -> String {
        format!("{}:{}", inb.channel.0, inb.conversation)
    }

    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .unwrap()
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// 生效的角色：配置文件里的优先，目录里的补充（同名不覆盖，避免"改配置不生效"）。
    fn effective_roles(&self) -> HashMap<String, crate::agent::RoleSpec> {
        let mut map = self.cfg.agents.roles.clone();
        if let Some(src) = &self.cfg.roles {
            for (name, spec) in src.all() {
                map.entry(name).or_insert(spec);
            }
        }
        map
    }

    /// 本会话有没有子 agent 在跑（给状态行用）。
    fn child_status(&self, parent_conv: &str) -> Option<String> {
        let now = now_ms();
        self.agents_running
            .lock()
            .unwrap()
            .values()
            .find(|a| a.parent_conv == parent_conv)
            .map(|a| {
                let secs = now.saturating_sub(a.started_ms) / 1000;
                let head = a.task.lines().next().unwrap_or("");
                match self.agent_progress.lock().unwrap().get(parent_conv).cloned() {
                    Some(p) => format!("🤖 子 agent 运行中 · {secs}s · {p} · {head}"),
                    None => format!("🤖 子 agent 运行中 · {secs}s · {head}"),
                }
            })
    }

    /// 角色目录：让父模型知道自己有几个"小弟"、各能做什么。
    fn roles_catalog(&self) -> String {
        let roles = self.effective_roles();
        if roles.is_empty() {
            return String::new();
        }
        let mut names: Vec<&String> = roles.keys().collect();
        names.sort();
        let mut out = String::from("\n\n可派子 agent 的角色（用 delegate(role=…) 指定）：\n");
        for name in names {
            let r = &roles[name];
            let desc = r.description.as_deref().unwrap_or("（无说明）");
            let tools = r
                .tools
                .as_ref()
                .map(|t| t.join("/"))
                .unwrap_or_else(|| "跟随默认白名单".into());
            out.push_str(&format!("- `{name}`：{desc}（工具：{tools}）\n"));
        }
        out.push_str("不确定用哪个就不填 role。");
        out
    }

    /// 预算闸门：返回 `Some(说明)` 表示已用完。
    ///
    /// 每次 run 入口查一次就够：预算的意义是"别让今天跑飞"，
    /// 不是精确到每一轮；真要精确，P4.3 的 attempts 会补上。
    async fn budget_exceeded(&self) -> Option<String> {
        if self.cfg.budget.unlimited() {
            return None;
        }
        let (day_start, month_start) = crate::budget::windows(crate::ids::now_ms(), self.cfg.tz_offset_min);
        let mut rd = self.store.begin_read().await.ok()?;
        let day = rd.usage_total(day_start).await.ok()?;
        let month = rd.usage_total(month_start).await.ok()?;
        let split = rd.usage_split(day_start).await.unwrap_or_default();
        crate::budget::exceeded(&self.cfg.budget, &day, &month, &split)
    }

    async fn epoch(&self, inb: &Inbound) -> u64 {
        let key = format!("conv.epoch:{}:{}", inb.channel.0, inb.conversation);
        self.store
            .kv_get(&key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    async fn bump_epoch(&self, inb: &Inbound) -> u64 {
        let next = self.epoch(inb).await + 1;
        let key = format!("conv.epoch:{}:{}", inb.channel.0, inb.conversation);
        if let Err(e) = self.store.kv_set(&key, &next.to_string()).await {
            tracing::warn!(error = %e, "persist epoch failed");
        }
        next
    }

    async fn handle_command(self: &Arc<Self>, inb: Inbound) -> Result<(), ChannelError> {
        let text = inb.text.clone().unwrap_or_default();
        let cmd = text.trim().trim_start_matches('/').split_whitespace().next().unwrap_or("");
        match cmd {
            "start" | "help" => {
                self.reply(
                    &inb.channel.0,
                    &inb.conversation,
                    "可用指令：\n\
                     /new — 开启新会话（保留长期记忆）\n\
                     /cancel — 停掉前台任务与本会话的后台任务\n\
                     /status — 查看状态\n\
                     /facts — 查看长期记忆\n\
                     /reminders — 查看/取消定时任务\n\
                     /usage — 今日与本月用量\n\
                     /prompt — 查看实际生效的 system 指令\n\
                     /agents — 查看/取消运行中的子 agent\n\
                     /tasks — 查看/取消后台任务\n\
                     /forget <id> — 删除一条记忆\n\
                     /approve <id> /deny <id> — 无按钮时处理审批\n\
                     /help — 本说明\n\n\
                     直接发消息即可对话；改动过文件的任务终稿会带「沉淀为 Skill」按钮，\
                     要导入外部技能直接说就行（模型会先取 playbook）。",
                )
                .await
            }
            "new" => {
                let e = self.bump_epoch(&inb).await;
                self.reply(&inb.channel.0, &inb.conversation, &format!("已开启新会话（epoch {e}）。之前的记录仍保留。"))
                    .await
            }
            "cancel" => {
                // 统一入口：前台 run 与这个 chat 的后台任务一起停。
                // 以前 /cancel 只认前台，用户对着 pending 的后台任务按 /cancel 会得到
                // "当前没有进行中的任务"，只能再记一遍 /tasks cancel <id>。
                let mut stopped_run = false;
                let key = Self::conv_key(&inb);
                if let Some(t) = self.runs.lock().unwrap().get(&key).cloned() {
                    t.cancel();
                    stopped_run = true;
                    if let Some(typing) =
                        self.typing.lock().unwrap().get(&key).cloned()
                    {
                        typing.cancel();
                    }
                }
                let mut stopped_tasks = 0usize;
                for doc in self.list_tasks().await {
                    let chat = doc.payload["chat"].as_str().unwrap_or_default();
                    if chat != inb.conversation {
                        continue;
                    }
                    match doc.payload["status"].as_str().unwrap_or("") {
                        "pending" => {
                            let mut p = doc.payload.clone();
                            p["status"] = serde_json::json!("cancelled");
                            p["finished_at"] = serde_json::json!(crate::ids::now_ms());
                            self.put_task_doc(
                                &doc.id.0,
                                doc.payload["conv"].as_str().unwrap_or_default(),
                                p,
                                doc.version + 1,
                            )
                            .await;
                            stopped_tasks += 1;
                        }
                        "running" => {
                            if let Some(t) =
                                self.bg_running.lock().unwrap().get(&doc.id.0).cloned()
                            {
                                t.cancel();
                                stopped_tasks += 1;
                            }
                        }
                        _ => {}
                    }
                }
                let msg = match (stopped_run, stopped_tasks) {
                    (false, 0) => "当前没有进行中的任务（前台与后台都空）。".to_owned(),
                    (true, 0) => "已请求取消前台任务。正在进行的模型请求会在这一步结束时停下（通常几秒）；\n\
                                   30 秒内还没有「⏹ 已取消」就再发一次 /cancel。"
                        .to_owned(),
                    (run, n) => {
                        let mut s = String::new();
                        if run {
                            s.push_str("已请求取消前台任务。");
                        }
                        if n > 0 {
                            s.push_str(&format!("已取消 {n} 个后台任务。"));
                        }
                        s
                    }
                };
                self.reply(&inb.channel.0, &inb.conversation, &msg).await
            }
            "approve" | "deny" => {
                let rest = text.trim().split_whitespace().nth(1).unwrap_or("");
                if rest.is_empty() {
                    return self.reply(&inb.channel.0, &inb.conversation, "用法：/approve <id> 或 /deny <id>").await;
                }
                let yes = cmd == "approve";
                let sender = self.approvals.lock().unwrap().remove(rest);
                match sender {
                    Some(tx) => {
                        let _ = self
                            .store
                            .resolve_approval(&ApprovalId(rest.to_owned()), yes, "owner")
                            .await;
                        let _ = tx.send(yes);
                        self.reply(
                            &inb.channel.0,
                            &inb.conversation,
                            if yes { "✅ 已允许（记录见账本）" } else { "❌ 已拒绝" },
                        )
                        .await
                    }
                    None => self.reply(&inb.channel.0, &inb.conversation, "审批不存在或已过期").await,
                }
            }
            "facts" => {
                let user_id = self.cfg.identity.resolve(&inb.channel.0, &inb.actor);
                let docs = self.list_facts(&user_id).await;
                if docs.is_empty() {
                    return self.reply(&inb.channel.0, &inb.conversation, "还没有记住任何事实。").await;
                }
                let mut out = String::from("长期记忆：\n");
                for d in &docs {
                    out.push_str(&format!(
                        "- [{}] {} · {} = {}\n",
                        d.id.0,
                        d.payload["subject"].as_str().unwrap_or("?"),
                        d.payload["predicate"].as_str().unwrap_or("?"),
                        d.payload["value"].as_str().unwrap_or("?"),
                    ));
                }
                out.push_str("\n用 `/forget <id>` 删除。");
                self.reply(&inb.channel.0, &inb.conversation, &out).await
            }
            "forget" => {
                let prefix = text.trim().split_whitespace().nth(1).unwrap_or("");
                if prefix.is_empty() {
                    return self.reply(&inb.channel.0, &inb.conversation, "用法：/forget <id 前缀>").await;
                }
                let hits: Vec<_> = self
                    .list_facts(&self.cfg.identity.resolve(&inb.channel.0, &inb.actor))
                    .await
                    .into_iter()
                    .filter(|d| d.id.0.starts_with(prefix))
                    .collect();
                if hits.is_empty() {
                    return self.reply(&inb.channel.0, &inb.conversation, "没有匹配的事实。用 /facts 看 id。").await;
                }
                for d in &hits {
                    let _ = self.store.delete_doc(&d.id).await;
                }
                self.reply(&inb.channel.0, &inb.conversation, &format!("已删除 {} 条事实。", hits.len())).await
            }
            "reminders" => {
                let rest = text.trim().trim_start_matches("/reminders").trim();
                if let Some(id) = rest.strip_prefix("del ") {
                    let id = id.trim();
                    let removed = self
                        .store
                        .delete_doc(&crate::ids::DocId(id.to_owned()))
                        .await
                        .unwrap_or(false);
                    let msg = if removed {
                        format!("已删除 {id}")
                    } else {
                        format!("没有找到 {id}")
                    };
                    return self.reply(&inb.channel.0, &inb.conversation, &msg).await;
                }
                let me = self.cfg.identity.resolve(&inb.channel.0, &inb.actor);
                let docs = {
                    match self.store.begin_read().await {
                        Ok(mut rd) => rd
                            .list_docs(crate::record::DocQuery {
                                scopes: Vec::new(),
                                kinds: vec!["reminder".into()],
                                limit: 100,
                            })
                            .await
                            .unwrap_or_default(),
                        Err(_) => Vec::new(),
                    }
                };
                // 只看自己的：提醒是"自己提醒自己"，别人的不该出现
                let docs: Vec<_> = docs
                    .into_iter()
                    .filter(|d| {
                        d.payload["user"]
                            .as_str()
                            .map(|u| u == me)
                            .unwrap_or_else(|| d.payload["actor"].as_str() == Some(&inb.actor))
                    })
                    .collect();
                if docs.is_empty() {
                    return self.reply(&inb.channel.0, &inb.conversation, "还没有定时任务。").await;
                }
                let mut out = String::from("定时任务：\n");
                for d in &docs {
                    let text = d.payload["text"].as_str().unwrap_or("?");
                    let next = d.payload["next_at"].as_i64().unwrap_or(0);
                    let desc = crate::schedule::parse(&d.payload["schedule"], self.cfg.tz_offset_min)
                        .map(|s| s.describe())
                        .unwrap_or_else(|_| "?".into());
                    out.push_str(&format!(
                        "- `{}` · {desc} · 下次 {}\n",
                        d.id.0,
                        crate::schedule::format_local(next, self.cfg.tz_offset_min)
                    ));
                    // 技能参数必须显示：否则"技能升级了、任务还用旧参数"根本看不出来
                    if let Some(name) = d.payload["skill"]["name"].as_str() {
                        let args = &d.payload["skill"]["args"];
                        let args = if args.is_null() { "{}".to_owned() } else { args.to_string() };
                        out.push_str(&format!("  技能 `{name}` · 参数 {args}\n"));
                    }
                    if let Some(p) = d.payload["polish"].as_str() {
                        out.push_str(&format!("  加工：{}\n", truncate_line(p, 120)));
                    }
                    out.push_str(&format!("  {text}\n"));
                }
                out.push_str("\n取消：/reminders del <id>");
                self.reply(&inb.channel.0, &inb.conversation, &out).await
            }
            "status" => {
                let running = self
                    .runs
                    .lock()
                    .unwrap()
                    .contains_key(&Self::conv_key(&inb));
                let epoch = self.epoch(&inb).await;
                let state = if running { "运行中" } else { "空闲" };
                let limits = self.cfg.lanes.limits();
                let busy = format!(
                    "io {}/{}, compile {}/{}, external {}/{}, agent {}/{}",
                    limits.io - self.cfg.lanes.available(crate::lanes::Lane::Io),
                    limits.io,
                    limits.compile - self.cfg.lanes.available(crate::lanes::Lane::Compile),
                    limits.compile,
                    limits.external - self.cfg.lanes.available(crate::lanes::Lane::External),
                    limits.external,
                    limits.agent - self.cfg.lanes.available(crate::lanes::Lane::Agent),
                    limits.agent,
                );
                self.reply(&inb.channel.0, &inb.conversation, &format!("{state} · 会话 epoch {epoch}\n车道 {busy}")).await
            }
            "agents" => {
                let rest = text.trim().trim_start_matches("/agents").trim().to_owned();
                if let Some(id) = rest.strip_prefix("retry ") {
                    let id = id.trim().to_owned();
                    let doc = self.list_tasks().await.into_iter().find(|d| d.id.0 == id);
                    let Some(doc) = doc else {
                        return self.reply(&inb.channel.0, &inb.conversation, &format!("没有这个任务：{id}")).await;
                    };
                    let p = &doc.payload;
                    let spec = crate::agent::AgentSpec {
                        parent_conv: p["conv"].as_str().unwrap_or_default().to_owned(),
                        parent_run: RunId(format!("retry-{id}")),
                        task: p["text"].as_str().unwrap_or_default().to_owned(),
                        inputs: p["inputs"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        profile: crate::agent::AgentProfile {
                            name: p["role"].as_str().unwrap_or("inherit").to_owned(),
                            ..Default::default()
                        },
                        system_prompt: Some(self.cfg.system_prompt.clone()),
                        max_turns: p["max_turns"].as_u64().unwrap_or(0) as u32,
                        max_tokens: p["max_tokens"].as_u64().unwrap_or(0),
                        wall_ms: p["wall_ms"].as_u64().unwrap_or(0),
                        depth: 1,
                        // 新任务 + 旧任务做来源：新子 agent 会拿到上次的结论摘要
                        continue_from: Some(id.clone()),
                        child_conv: None,
                    };
                    return match self.enqueue_agent_task(spec).await {
                        Ok(new_id) => {
                            self.reply(
                                &inb.channel.0,
                                &inb.conversation,
                                &format!("已重排为 `{new_id}`（接续 `{id}` 的进展，前台静默后开跑）。"),
                            )
                            .await
                        }
                        Err(e) => {
                            self.reply(&inb.channel.0, &inb.conversation, &format!("重排失败：{e}")).await
                        }
                    };
                }
                if let Some(id) = rest.strip_prefix("show ") {
                    let id = id.trim().to_owned();
                    if !id.starts_with("sub:") {
                        return self
                            .reply(&inb.channel.0, &inb.conversation, "用法：/agents show <子会话 id>（列表里带 `sub:` 的那串）")
                            .await;
                    }
                    let page = match self.store.begin_read().await {
                        Ok(mut rd) => rd.messages(ConversationId(id.clone()), 0, 60).await.ok(),
                        Err(_) => None,
                    };
                    let Some(page) = page.filter(|p| !p.msgs.is_empty()) else {
                        return self
                            .reply(&inb.channel.0, &inb.conversation, &format!("没有这个子会话：{id}（可能已被清理）"))
                            .await;
                    };
                    let mut out = format!("子会话 `{id}` 的最近记录：\n\n");
                    let mut said: Vec<&crate::record::Message> = page
                        .msgs
                        .iter()
                        .filter(|m| m.role == crate::record::Role::Assistant)
                        .filter(|m| {
                            m.payload
                                .get("text")
                                .and_then(|v| v.as_str())
                                .map(|t| !t.trim().is_empty())
                                .unwrap_or(false)
                        })
                        .collect();
                    if said.len() > 2 {
                        said.drain(..said.len() - 2);
                    }
                    for m in said {
                        let text = m.payload["text"].as_str().unwrap_or("");
                        out.push_str(&format!("🧠 {}\n\n", text.chars().take(800).collect::<String>()));
                    }
                    let mut tools: Vec<&crate::record::Message> = page
                        .msgs
                        .iter()
                        .filter(|m| m.role == crate::record::Role::ToolResult)
                        .collect();
                    if tools.len() > 6 {
                        tools.drain(..tools.len() - 6);
                    }
                    if !tools.is_empty() {
                        out.push_str("工具轨迹（最后几条）：\n");
                        for m in tools {
                            let ok = m.payload["ok"].as_bool().unwrap_or(false);
                            let preview = m.payload["preview"].as_str().unwrap_or("");
                            out.push_str(&format!(
                                "- {} {}\n",
                                if ok { "✅" } else { "❌" },
                                preview.chars().take(160).collect::<String>()
                            ));
                        }
                        out.push('\n');
                    }
                    out.push_str("（推理/thinking 不落账本，只有正文与工具轨迹）");
                    for part in shard_text(&out, 3_000) {
                        let _ = self.reply(&inb.channel.0, &inb.conversation, &part).await;
                    }
                    return Ok(());
                }
                if let Some(id) = rest.strip_prefix("cancel ") {
                    let id = id.trim().to_owned();
                    let hit = self
                        .agents_running
                        .lock()
                        .unwrap()
                        .get(&id)
                        .map(|a| a.cancel.clone());
                    let msg = match hit {
                        Some(t) => {
                            t.cancel();
                            format!("已请求取消 {id}")
                        }
                        None => format!("没有运行中的子 agent：{id}"),
                    };
                    return self.reply(&inb.channel.0, &inb.conversation, &msg).await;
                }
                let me = format!("{}:{}", inb.channel.0, inb.conversation);
                let now = crate::ids::now_ms();
                let lines: Vec<String> = self
                    .agents_running
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, a)| a.parent_conv.starts_with(&me))
                    .map(|(id, a)| {
                        let secs = (now.saturating_sub(a.started_ms)) / 1000;
                        format!("- `{id}` · {secs}s · {}", a.task.lines().next().unwrap_or(""))
                    })
                    .collect();
                let msg = if lines.is_empty() {
                    "没有运行中的子 agent。".to_owned()
                } else {
                    format!("运行中的子 agent：\n{}\n\n取消：/agents cancel <id>　看详情：/agents show <id>", lines.join("\n"))
                };
                self.reply(&inb.channel.0, &inb.conversation, &msg).await
            }
            "tasks" => {
                let rest = text.trim().trim_start_matches("/tasks").trim().to_owned();
                if let Some(id) = rest.strip_prefix("cancel ") {
                    let id = id.trim().to_owned();
                    let msg = self
                        .cancel_one_task(&id)
                        .await
                        .unwrap_or_else(|| format!("没有这个任务：{id}"));
                    return self.reply(&inb.channel.0, &inb.conversation, &msg).await;
                }

                let now = now_ms();
                let docs = self.list_tasks().await;
                let mut lines = Vec::new();
                for d in docs.iter().filter(|d| {
                    matches!(d.payload["status"].as_str().unwrap_or(""), "pending" | "running")
                }) {
                    let started = d.payload["started_at"].as_i64().unwrap_or(now);
                    let status = d.payload["status"].as_str().unwrap_or("?");
                    let when = if status == "running" {
                        format!("{}s", now.saturating_sub(started) / 1000)
                    } else {
                        "等待前台静默".to_owned()
                    };
                    let cont = d.payload["continue_from"]
                        .as_str()
                        .map(|s| format!("（续 {s}）"))
                        .unwrap_or_default();
                    lines.push(format!(
                        "- `{}` · {} · {} · {}{cont}",
                        d.id.0,
                        status,
                        when,
                        d.payload["text"].as_str().unwrap_or("").lines().next().unwrap_or("")
                    ));
                }
                let msg = if lines.is_empty() {
                    "没有进行中的后台任务。\n要跑几分钟的活，跟我说「慢慢做，我一会儿回来看」就行。".to_owned()
                } else {
                    format!("后台任务：\n{}\n\n取消：/tasks cancel <id>", lines.join("\n"))
                };
                self.reply(&inb.channel.0, &inb.conversation, &msg).await
            }
            "prompt" => {
                // 让主人能核对"你到底给模型喂了什么"。核心段写死，配置只能追加。
                let text = self.cfg.system_prompt.clone();
                let chunks = shard_text(&text, 3_000);
                let total = text.chars().count();
                for (i, part) in chunks.iter().enumerate() {
                    let head = if i == 0 {
                        format!("system 指令（{total} 字，分 {} 条）：\n\n", chunks.len())
                    } else {
                        String::new()
                    };
                    let _ = self.reply(&inb.channel.0, &inb.conversation, &format!("{head}{part}")).await;
                }
                self.reply(
                    &inb.channel.0,
                    &inb.conversation,
                    "以上是配置文件与内置核心段。\n\
                     另有每轮动态注入（不在这里）：技能目录、MCP 工具目录、角色清单、长期记忆、时间锚点。",
                )
                .await
            }
            "usage" => {
                let now = crate::ids::now_ms();
                let (day_start, month_start) =
                    crate::budget::windows(now, self.cfg.tz_offset_min);
                let zero = || crate::record::UsageTotal {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cost_est: 0.0,
                };
                let (day, month, split) = match self.store.begin_read().await {
                    Ok(mut rd) => (
                        rd.usage_total(day_start).await.unwrap_or_else(|_| zero()),
                        rd.usage_total(month_start).await.unwrap_or_else(|_| zero()),
                        rd.usage_split(day_start).await.unwrap_or_default(),
                    ),
                    Err(_) => (zero(), zero(), Default::default()),
                };
                let mut text = format!(
                    "{}\n{}",
                    usage_line("今日", &day, self.cfg.budget.daily_tokens),
                    usage_line("本月", &month, self.cfg.budget.monthly_tokens)
                );
                let agent_tokens = split.agent.tokens();
                if agent_tokens > 0 {
                    text.push_str(&format!(
                        "\n其中子 agent 今日 {}（{} in / {} out）",
                        short_tokens(agent_tokens),
                        short_tokens(split.agent.prompt_tokens),
                        short_tokens(split.agent.completion_tokens)
                    ));
                }
                self.reply(&inb.channel.0, &inb.conversation, &text).await
            }
            other => {
                self.reply(&inb.channel.0, &inb.conversation, &format!("未知指令 /{other}，试试 /help")).await
            }
        }
    }

    /// 未绑定地址的提示：每小时最多一次，避免被当成回音壁，也避免刷微信额度。
    async fn notice_unauthorized(&self, inb: &Inbound) {
        let now = now_ms();
        let key = format!("{}:{}", inb.channel.0, inb.actor);
        {
            let mut g = self.unauth_notice.lock().unwrap();
            if now - g.get(&key).copied().unwrap_or(0) < 3_600_000 {
                return;
            }
            g.insert(key, now);
        }
        tracing::warn!(channel = %inb.channel.0, actor = %inb.actor, "未绑定的地址");
        let ch = self.channel_for(&inb.channel);
        if let Some(cb) = &inb.callback_id {
            let _ = ch.ack(cb, "未授权").await;
            return;
        }
        let _ = self
            .reply(
                &inb.channel.0,
                &inb.conversation,
                "⛔ 未授权：这个账号还没有绑定。请主人在 [identity] 里加上这条通道地址。",
            )
            .await;
    }

    /// best-effort 打状态反应：失败只记 trace，绝不影响 run（D24）。
    /// `emoji` 为空表示清空（取消时用）。
    async fn mark(&self, ch: &dyn Channel, conversation: &str, msg: Option<&MsgRef>, emoji: &str) {
        let Some(m) = msg else { return };
        if let Err(e) = ch.react(conversation, m, emoji).await {
            tracing::debug!(error = %e, emoji, "状态反应失败");
        }
    }

    /// 收尾时统一打状态：所有 `?` 早退、panic 之外的分支都从这里出去（D24）。
    async fn handle_run(&self, inb: Inbound) -> Result<(), ChannelError> {
        let ch = self.channel_for(&inb.channel);
        let msg = inb.msg.clone();
        let conv = inb.conversation.clone();
        let res = self.handle_run_inner(inb).await;
        let emoji = match &res {
            Ok(RunEnd::Done) => EMOJI_OK,
            Ok(RunEnd::Failed) | Err(_) => EMOJI_FAIL,
            Ok(RunEnd::Cancelled) => "", // 取消 = 清空
        };
        self.mark(&*ch, &conv, msg.as_ref(), emoji).await;
        res.map(|_| ())
    }

    async fn handle_run_inner(&self, inb: Inbound) -> Result<RunEnd, ChannelError> {
        let ch = self.channel_for(&inb.channel);
        // 记忆归属用 **user id**，与通道无关（D26）：同一个人两个账号看到同一份
        let user_id = self.cfg.identity.resolve(&inb.channel.0, &inb.actor);
        if let Some(msg) = self.budget_exceeded().await {
            self.reply(&inb.channel.0, &inb.conversation, &format!("⛔ {msg}")).await?;
            return Ok(RunEnd::Failed);
        }
        let key = Self::conv_key(&inb);
        let lock = self.lock_for(&key);
        let _guard = lock.lock().await;
        // 拿到锁才开始真正处理；此前的状态是"未开始"（D24）
        self.mark(&*ch, &inb.conversation, inb.msg.as_ref(), EMOJI_RUNNING).await;

        // 收到即显示"输入中"，之后每 4s 续一次（Telegram 的 typing 约 5s 过期）
        let typing_cancel = CancellationToken::new();
        let typing_task = {
            let channel = Arc::clone(&ch);
            let conv_key = inb.conversation.clone();
            let stop = typing_cancel.clone();
            tokio::spawn(async move {
                // 兜底：最多转 20 分钟，避免异常情况下永远"正在输入"
                let deadline = tokio::time::Instant::now() + Duration::from_secs(1200);
                loop {
                    // 被限流时不要再每 4s 添一次乱：按 retry_after 等，否则洪水控制会一直被续上
                    let wait = match channel.typing(&conv_key).await {
                        Err(crate::channel::ChannelError::RateLimited { retry_after_ms }) => {
                            (retry_after_ms as u64).min(30_000)
                        }
                        _ => 4_000,
                    };
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
                        _ = stop.cancelled() => break,
                    }
                    if tokio::time::Instant::now() > deadline {
                        tracing::warn!(conv = %conv_key, "typing 超时，停止提示");
                        break;
                    }
                }
            })
        };
        // 清理交给 RAII：`?` 早退 / panic 也必须停 typing、摘掉 runs 条目
        let mut scope = RunScope {
            runs: Arc::clone(&self.runs),
            typing_map: Arc::clone(&self.typing),
            conversation: key.clone(),
            typing: typing_cancel,
            typing_task: Some(typing_task),
        };
        // 让 /cancel 能立刻停掉"正在输入"（与 run 令牌分开：取消后 run 还要收尾）
        self.typing
            .lock()
            .unwrap()
            .insert(key.clone(), scope.typing.clone());

        tracing::info!(conv = %inb.conversation, "run: 已拿到会话锁，开始准备");
        // 前置步骤都加超时：卡在存储上不该让"正在输入"一直转
        let epoch = match tokio::time::timeout(Duration::from_secs(5), self.epoch(&inb)).await {
            Ok(e) => e,
            Err(_) => {
                tracing::warn!(conv = %inb.conversation, "epoch 读取超时");
                let _ = self.reply(&inb.channel.0, &inb.conversation, "⏱ 读取会话状态超时，请稍后再试（已取消）。").await;
                return Ok(RunEnd::Cancelled);
            }
        };
        let conv = ConversationId(format!("{}:{}:{}", inb.channel.0, inb.conversation, epoch));
        let run = RunId(format!("{}-{}", conv.0, inb.dedupe_key));
        let caps = ch.caps();
        let mut renderer = TurnRenderer::new(caps.clone());
        let pair = EventSink::channel(256);
        let sink = pair.sink;
        let mut events = pair.rx;

        // 长期记忆注入：作为稳定前缀的一部分
        let mut system = self.cfg.system_prompt.clone();
        if let Some(src) = &self.cfg.skill_source {
            let cat = src.catalog();
            if !cat.trim().is_empty() {
                system.push_str("\n\n");
                system.push_str(&cat);
            }
        }
        system.push_str(if self.cfg.vision {
            "\n\n用户发来的图片会作为图像内容一并提供（可以直接看）；历史里只保留文件路径。"
        } else {
            "\n\n你看不到图片内容（只会收到 `/media` 里的文件路径），需要时用 exec 处理，处理不了就直说。"
        });
        if let Some(hint) = &self.cfg.proxy_hint {
            if !hint.trim().is_empty() {
                system.push_str("\n\n");
                system.push_str(hint);
            }
        }
        let role_catalog = self.roles_catalog();
        if !role_catalog.is_empty() {
            system.push_str(&role_catalog);
        }
        if let Some(block) = &self.cfg.host_paths {
            if !block.trim().is_empty() {
                system.push_str("\n\n");
                system.push_str(block);
            }
        }
        if let Some(src) = &self.cfg.tool_source {
            let cat = src.catalog();
            if !cat.trim().is_empty() {
                system.push_str("\n\n");
                system.push_str(&cat);
            }
        }
        if let Ok(mut rd) = self.store.begin_read().await {
            let q = crate::record::DocQuery {
                scopes: vec!["family".into(), format!("user:{user_id}")],
                kinds: vec!["fact".into()],
                limit: 50,
            };
            if let Ok(mut docs) = rd.list_docs(q).await {
                if docs.len() > 30 {
                    if let Some(r) = &self.retriever {
                        let query = crate::retriever::Query {
                            text: inb.text.clone().unwrap_or_default(),
                            scopes: vec!["family".into(), format!("user:{user_id}")],
                            kinds: vec!["fact".into()],
                            k: 10,
                        };
                        if let Ok(hits) = r.search(query).await {
                            let ids: std::collections::HashSet<String> =
                                hits.into_iter().map(|h| h.doc_id.0).collect();
                            docs.retain(|d| ids.contains(&d.id.0));
                        }
                    }
                }
                if !docs.is_empty() {
                    system.push_str("\n\n已知事实（长期记忆，可直接使用，不必再问用户）：\n");
                    for d in &docs {
                        let subj = d.payload["subject"].as_str().unwrap_or("?");
                        let pred = d.payload["predicate"].as_str().unwrap_or("?");
                        let val = d.payload["value"].as_str().unwrap_or("?");
                        system.push_str(&format!("- {subj} · {pred} = {val}\n"));
                    }
                }
            }
        }

        // 时间锚点放在**消息**里而不是 system：system 是稳定前缀，逐分钟变动会让缓存失效。
        // 没有它，模型算不了"本周五"是几号。
        // 图片只在当轮注入：这里收集，历史里只留路径文字
        let images: Vec<crate::driver::ImagePart> = if self.cfg.vision {
            inb.attachments
                .iter()
                // 只发模型普遍支持的格式；heic/bmp 之类传过去只会 400，不如让它走文件路径
                .filter(|a| {
                    matches!(
                        a.mime.as_str(),
                        "image/jpeg" | "image/png" | "image/webp" | "image/gif"
                    )
                })
                .take(4)
                .map(|a| crate::driver::ImagePart {
                    path: a.path.clone(),
                    mime: a.mime.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let stamped = format!(
            "（现在：{}）\n\n{}{}",
            crate::schedule::now_line(now_ms(), self.cfg.tz_offset_min),
            inb.text.clone().unwrap_or_default(),
            attachment_lines(&inb.attachments)
        );
        let conv_key = conv.0.clone();
        let req = RunRequest {
            conv,
            run: run.clone(),
            text: stamped,
            model: self.cfg.model.clone(),
            max_turns: self.cfg.max_turns,
            wall_clock_ms: self.cfg.wall_clock_ms,
            system_prompt: Some(system),
            actor: inb.actor.clone(),
            images,
            workspace: Workspace { run: run.clone(), root: self.cfg.workspace_root.clone() },
            context_budget_chars: self.cfg.context_budget_chars,
            token_budget: None,
        };

        let outbox = RunOutbox { channel: &*ch, conversation: inb.conversation.clone() };
        let docs = StoreDocs { store: &*self.store };
        let runner = Runner {
            store: &*self.store,
            driver: &*self.driver,
            tools: &self.tools,
            env: &*self.env,
            lanes: &self.cfg.lanes,
            spawner: Some(self as &dyn crate::agent::AgentSpawner),
            gate: Some(self),
            outbox: Some(&outbox),
            docs: Some(&docs),
        };

        let run_cancel = CancellationToken::new();
        self.runs.lock().unwrap().insert(key.clone(), run_cancel.clone());

        let fut = runner.run(req, sink, run_cancel.clone());
        tokio::pin!(fut);

        let mut tick = tokio::time::interval(Duration::from_millis(
            caps.min_edit_interval_ms.max(200) as u64,
        ));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // 卡住检测：180s 没有任何进展就取消（单次 LLM 50s、工具 15s，留足余量）
        let mut last_progress = tokio::time::Instant::now();
        let stall = Duration::from_secs(180);
        let mut stalled = false;
        let mut child_shown = false;

        let outcome = loop {
            tokio::select! {
                biased;
                res = &mut fut => break res,
                maybe = events.recv() => {
                    if let Some(ev) = maybe {
                        last_progress = tokio::time::Instant::now();
                        renderer.push(RenderEvent::Stream(ev));
                    }
                }
                _ = tick.tick() => {
                    if last_progress.elapsed() > stall {
                        stalled = true;
                        run_cancel.cancel();
                        break Err(crate::runner::RunnerError::Budget("stalled".into()));
                    }
                    // 前台委托期间用户只看到"执行 delegate"，不知道在跑什么——
                    // 把子 agent 的任务与耗时顶到状态行。
                    if let Some(s) = self.child_status(&conv_key) {
                        renderer.push(RenderEvent::Status(s));
                        child_shown = true;
                    } else if child_shown {
                        renderer.push(RenderEvent::Status(String::new()));
                        child_shown = false;
                    }
                    let action = renderer.flush(now_ms());
                    if let Err(e) = apply_action(&*ch, &inb.conversation, &mut renderer, action).await {
                        if matches!(e, ChannelError::RateLimited { .. }) {
                            // 进度刷新发不出去不是失败：跳过这一拍，run 继续跑
                            tracing::warn!(error = %e, "进度刷新被限流，跳过本轮");
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
        };
        let _ = stalled;

        scope.shutdown().await;

        while let Ok(ev) = events.try_recv() {
            renderer.push(RenderEvent::Stream(ev));
        }

        let status = match &outcome {
            Ok(o) => {
                renderer.replace_text(o.text.clone());
                o.status.clone()
            }
            Err(_) => RunStatus::Failed,
        };
        // 本轮动过文件 → 终稿给一个「沉淀为 Skill」入口（点击后另起一次提炼 run）。
        // 沉淀 run 自身不再带按钮，避免无限套娃。
        let from_skill_button = inb.dedupe_key.starts_with("skill-");
        let wrote_files = matches!(
            &outcome,
            Ok(o) if o.status == RunStatus::Done
                && o.tools.iter().any(|t| t == "write" || t == "exec")
        );
        if !from_skill_button && wrote_files {
            let data = format!("skill:{}", run.0);
            if data.len() <= 64 {
                renderer.set_trailing_buttons(vec![vec![crate::view::Button {
                    label: "🧪 沉淀为 Skill".into(),
                    data,
                }]]);
            }
        }
        for action in renderer.finish(status) {
            if !self.apply_final(&*ch, &inb.conversation, &mut renderer, action).await {
                break;
            }
        }
        // 记住按钮挂在哪条消息上：点击后要把它撤掉，别让过期按钮留在对话里
        if !from_skill_button && wrote_files {
            if let Some(m) = renderer.msg_ref() {
                let _ = self
                    .store
                    .kv_set(&format!("skill.btn:{}", run.0), &m.0)
                    .await;
            }
        }

        match outcome {
            Ok(o) if o.status == RunStatus::Cancelled => {
                self.reply(&inb.channel.0, &inb.conversation, "已取消。").await.map(|_| RunEnd::Cancelled)
            }
            Ok(o) if o.status == RunStatus::Failed => Ok(RunEnd::Failed),
            Ok(_) => Ok(RunEnd::Done),
            Err(e) => {
                tracing::error!(error = %e, "run failed");
                let msg = match &e {
                    crate::runner::RunnerError::Engine(crate::error::EngineError::Quota {
                        message,
                        retry_after_s,
                    }) => {
                        let wait = retry_after_s
                            .map(|s| format!("约 {s} 秒后可再试"))
                            .unwrap_or_else(|| "请查看配额周期（5 小时 / 每周 / 每月）".into());
                        format!(
                            "⛔ 模型额度已用尽，本次任务已停止（不会自动重试）。{wait}\n\n{message}"
                        )
                    }
                    crate::runner::RunnerError::Engine(crate::error::EngineError::Auth(m)) => {
                        format!("🔑 模型鉴权失败（key 无效或已过期），已停止。\n\n{m}")
                    }
                    crate::runner::RunnerError::Budget(what) if what == "stalled" => format!(
                        "⏱ 任务卡住了：3 分钟内没有任何进展，已取消。\n\n\
                         可以换个说法重试；如果是某个服务无响应，先用 /status 看一下。"
                    ),
                    crate::runner::RunnerError::Budget(what) => format!(
                        "⏱ 任务超出预算（{what}），已停止。\n\n\
                         等你确认的时间不计入预算；仍超时就把任务拆小，\
                         或调大 [actor] 的 `wall_clock_ms` / `max_turns`。"
                    ),
                    other => format!("出错了：{other}"),
                };
                self.reply(&inb.channel.0, &inb.conversation, &msg).await.map(|_| RunEnd::Failed)
            }
        }
    }

    async fn list_facts(&self, actor: &str) -> Vec<crate::record::Doc> {
        let mut out = Vec::new();
        if let Ok(mut rd) = self.store.begin_read().await {
            let q = crate::record::DocQuery {
                scopes: vec!["family".into(), format!("user:{actor}")],
                kinds: vec!["fact".into()],
                limit: 50,
            };
            if let Ok(docs) = rd.list_docs(q).await {
                out = docs;
            }
        }
        out
    }

    async fn handle_callback(self: &Arc<Self>, inb: Inbound) {
        let ch = self.channel_for(&inb.channel);
        let data = inb.text.clone().unwrap_or_default();
        if let Some(run_id) = data.strip_prefix("skill:") {
            self.start_distill(inb, run_id.to_owned());
            return;
        }
        let parts: Vec<&str> = data.split(':').collect();
        if parts.len() != 3 || parts[0] != "appr" {
            if let Some(cb) = &inb.callback_id {
                let _ = ch.ack(cb, "无效操作").await;
            }
            return;
        }
        let id = parts[1].to_owned();
        let yes = parts[2] == "y";

        let sender = self.approvals.lock().unwrap().remove(&id);
        let card = self.store.approval_card(&ApprovalId(id.clone())).await.ok().flatten();
        match sender {
            Some(tx) => {
                let _ = self
                    .store
                    .resolve_approval(&ApprovalId(id.clone()), yes, "owner")
                    .await;
                let _ = tx.send(yes);
                if let Some((chat, msg)) = &card {
                    let note = if yes { "✅ 已允许（记录见账本）" } else { "❌ 已拒绝" };
                    let _ = ch.clear_prompt(chat, msg, note).await;
                }
                if let Some(cb) = &inb.callback_id {
                    let _ = ch.ack(cb, if yes { "已允许" } else { "已拒绝" }).await;
                }
            }
            None => {
                if let Some(cb) = &inb.callback_id {
                    let _ = ch.ack(cb, "已过期").await;
                }
            }
        }
    }

    /// 点「沉淀为 Skill」→ 另起一次普通 run，让模型自己走提炼 SOP。
    ///
    /// 刻意不在内核里写提炼逻辑：SOP 是应用层的可编辑文本（`distill_prompt`），
    /// 改流程不用改 Rust。
    /// 定时任务最多补跑这么久；超过说明设备关机/睡过去了，跳过这次，只排下一次。
    const REMINDER_GRACE_MS: i64 = 2 * 60 * 60 * 1000;

    /// 每 30 秒看一次有没有到点的提醒。
    pub fn spawn_scheduler(self: Arc<Self>, cancel: CancellationToken) {
        tokio::spawn(async move {
            let mut reminders = tokio::time::interval(Duration::from_secs(30));
            reminders.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // 后台任务单独用更密的 tick：30s 一次的话，任务最坏要等 30s 才被看见。
            // 查询只扫 agent_task 那几条 doc，代价可以忽略。
            let mut tasks = tokio::time::interval(Duration::from_secs(5));
            tasks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = reminders.tick() => {
                        let n = self.tick_reminders().await;
                        if n > 0 {
                            tracing::info!(fired = n, "reminders fired");
                        }
                    }
                    _ = tasks.tick() => {
                        if self.tick_agent_tasks().await > 0 {
                            tracing::info!("background agent task started");
                        }
                    }
                }
            }
        });
    }

    /// 扫一遍到期提醒：先把下一次排好（失败也不丢调度），再决定要不要补跑。
    pub async fn tick_reminders(self: &Arc<Self>) -> usize {
        let docs = match self.store.begin_read().await {
            Ok(mut rd) => rd
                .list_docs(crate::record::DocQuery {
                    scopes: Vec::new(),
                    kinds: vec!["reminder".into()],
                    limit: 200,
                })
                .await
                .unwrap_or_default(),
            Err(_) => return 0,
        };

        let now = now_ms();
        let mut fired = 0;
        for d in docs {
            let next_at = d.payload["next_at"].as_i64().unwrap_or(0);
            if next_at > now {
                continue;
            }
            let sched = match crate::schedule::parse(&d.payload["schedule"], self.cfg.tz_offset_min)
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(id = %d.id.0, error = %e, "bad reminder schedule; dropped");
                    let _ = self.store.delete_doc(&d.id).await;
                    continue;
                }
            };
            match sched.next_after(now, self.cfg.tz_offset_min) {
                Some(ts) => {
                    let mut payload = d.payload.clone();
                    payload["next_at"] = serde_json::json!(ts);
                    if let Ok(mut tx) = self.store.begin_write().await {
                        let doc = crate::record::Doc {
                            id: d.id.clone(),
                            kind: d.kind.clone(),
                            scope: d.scope.clone(),
                            payload,
                            version: d.version + 1,
                            valid_from: now,
                            valid_to: None,
                        };
                        let _ = tx
                            .upsert_doc(
                                ConversationId("__reminders__".into()),
                                RunId("__reminders__".into()),
                                doc,
                            )
                            .await;
                        let _ = tx.commit().await;
                    }
                }
                None => {
                    // 一次性提醒已经用掉
                    let _ = self.store.delete_doc(&d.id).await;
                }
            }

            if now - next_at > Self::REMINDER_GRACE_MS {
                tracing::warn!(id = %d.id.0, late_ms = now - next_at, "reminder missed its window");
                continue;
            }
            let Some(text) = d.payload["text"].as_str() else { continue };
            let chat = d.payload["chat"].as_str().unwrap_or_default().to_owned();
            if chat.is_empty() {
                continue;
            }
            let channel = d.payload["channel"].as_str().unwrap_or("telegram").to_owned();
            // 一条提醒可以投到多个通道地址（现在通常只有一个）
            let targets: Vec<(String, String)> = d.payload["targets"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| {
                            Some((
                                t["channel"].as_str()?.to_owned(),
                                t["addr"].as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
                .filter(|v: &Vec<(String, String)>| !v.is_empty())
                .unwrap_or_else(|| vec![(channel.clone(), chat.clone())]);

            // 静态提醒：到点只发这句文本，不跑模型也不跑脚本
            if let Some(msg) = d.payload.get("message").and_then(|v| v.as_str()) {
                let msg = msg.trim();
                if !msg.is_empty() {
                    fired += 1;
                    for (ch, addr) in &targets {
                        let _ = self.notify(ch, addr, msg).await;
                    }
                    continue;
                }
            }

            // 钉住脚本的任务：到点直接跑技能，把 stdout 原样发回，不经过模型
            if let Some(skill) = d.payload.get("skill").filter(|v| !v.is_null()) {
                fired += 1;
                let me = Arc::clone(self);
                let skill = skill.clone();
                let title = text.to_owned();
                let polish = d.payload["polish"].as_str().map(str::to_owned);
                let chan = channel.clone();
                tokio::spawn(async move {
                    me.run_pinned(&chan, &chat, &title, skill, polish, now).await;
                });
                continue;
            }

            let actor = d.payload["actor"].as_str().unwrap_or(&chat).to_owned();
            let inb = Inbound {
                channel: crate::ids::ChannelId(channel),
                conversation: chat,
                actor,
                kind: InboundKind::Text,
                text: Some(format!("（定时任务）{text}")),
                dedupe_key: format!("task-{now}"),
                callback_id: None,
                // 定时任务合成：平台上没有对应的那条消息，不挂状态反应（D24）
                msg: None,
                received_at: now,
            attachments: Vec::new(),
            };
            fired += 1;
            let me = Arc::clone(self);
            tokio::spawn(async move {
                if let Err(e) = me.handle_run(inb).await {
                    tracing::warn!(error = %e, "reminder run failed");
                }
            });
        }
        fired
    }

    /// 跑钉住的脚本：不调模型，时延与格式都由脚本决定。
    /// 定时任务开跑前记账：`run.started` + 预登记工具调用。
    /// 写失败只告警，不阻断投递——记账不能比送达更脆。
    async fn record_job_start(
        self: &Arc<Self>,
        conv: &ConversationId,
        run: &RunId,
        call: &CallId,
        skill: &str,
        args: &serde_json::Value,
        title: &str,
        channel: &str,
    ) {
        let Ok(mut tx) = self.store.begin_write().await else {
            tracing::warn!("job ledger: 写事务打不开，跳过记账");
            return;
        };
        let _ = tx
            .append(NewEvent {
                conv: conv.clone(),
                run: run.clone(),
                turn: None,
                kind: EventKind::RunStarted,
                payload: serde_json::json!({
                    "kind": "scheduled",
                    "skill": skill,
                    "args": args,
                    "title": title,
                    "channel": channel,
                }),
                dedupe_key: Some(format!("job-started-{}", run.0)),
            })
            .await;
        let _ = tx
            .open_tool_calls(
                conv.clone(),
                run.clone(),
                None,
                vec![ToolCallRecord {
                    call: call.clone(),
                    name: "run_skill".into(),
                    args: serde_json::json!({ "name": skill, "args": args }),
                    effect: EffectClass::Pure,
                    idem_key: format!("job-{}", run.0),
                }],
            )
            .await;
        if let Err(e) = tx.commit().await {
            tracing::warn!(error = %e, "job ledger: run.started 提交失败");
        }
    }

    /// 定时任务收尾：关闭工具调用 + `run.finished`。
    async fn record_job_end(
        self: &Arc<Self>,
        conv: &ConversationId,
        run: &RunId,
        call: &CallId,
        status: &str,
        preview: String,
        started_ms: i64,
    ) {
        let Ok(mut tx) = self.store.begin_write().await else { return };
        let _ = tx.close_tool_call(call.clone(), status, preview).await;
        let _ = tx
            .append(NewEvent {
                conv: conv.clone(),
                run: run.clone(),
                turn: None,
                kind: EventKind::RunFinished,
                payload: serde_json::json!({
                    "status": status,
                    "ms": now_ms().saturating_sub(started_ms),
                }),
                dedupe_key: Some(format!("job-finished-{}", run.0)),
            })
            .await;
        if let Err(e) = tx.commit().await {
            tracing::warn!(error = %e, "job ledger: run.finished 提交失败");
        }
    }

    async fn run_pinned(
        self: &Arc<Self>,
        channel: &str,
        chat: &str,
        title: &str,
        skill: serde_json::Value,
        polish: Option<String>,
        now: i64,
    ) {
        let name = skill["name"].as_str().unwrap_or_default().to_owned();
        let args = skill.get("args").cloned().unwrap_or_else(|| serde_json::json!({}));
        let header = format!(
            "⏰ 定时任务 · {}\n{title}",
            crate::schedule::format_local(now, self.cfg.tz_offset_min)
        );
        let Some(tool) = self.tools.get("run_skill") else {
            let _ = self.reply(channel, chat, "❌ 定时任务失败：没有 run_skill 工具").await;
            return;
        };
        let run = RunId(format!("job-{now}"));
        let conv = ConversationId(format!("job:{chat}"));
        let call = CallId(format!("job-{now}"));
        let started_ms = now_ms();
        // 账本：pinned 任务没有模型参与，但**必须留痕**（不变量 1）。
        // 否则崩溃后无法判断"跑过没跑过"，只能按 next_at 重排，可能重复或漏投。
        self.record_job_start(&conv, &run, &call, &name, &args, title, channel)
            .await;
        let ws = Workspace { run: run.clone(), root: self.cfg.workspace_root.clone() };
        let ctx = crate::tool::ToolCtx {
            conv: conv.clone(),
            actor: "job",
            outbox: &crate::tool::NullOutbox,
            docs: &crate::tool::NullDocReader,
            run: run.clone(),
            call: call.clone(),
            workspace: &ws,
            env: &*self.env,
            system_prompt: "",
            spawner: None,
            cancel: CancellationToken::new(),
        };
        // 定时任务也是后台执行：同样过车道，否则两个 pinned 任务同时编译
        // 会和前台 run 抢同一块 CPU。许可只罩住这次执行，不含后面的 polish run
        // （否则 polish 里的 exec 会等一个永远不释放的 compile 名额）。
        let executed = {
            let never = CancellationToken::new();
            let _lane = self.cfg.lanes.acquire(tool.lane(), &never).await;
            tool.execute(serde_json::json!({ "name": name, "args": args }), &ctx)
                .await
        };
        let body = match executed {
            Ok(out) => blocks_text(&out.content),
            Err(e) => {
                let msg = e.message.clone();
                self.record_job_end(&conv, &run, &call, "error", msg.clone(), started_ms)
                    .await;
                let _ = self.reply(channel, chat, &format!("{header}\n\n❌ 技能执行失败：{msg}")).await;
                return;
            }
        };

        let preview: String = body.chars().take(200).collect();
        self.record_job_end(&conv, &run, &call, "ok", preview, started_ms)
            .await;

        // 脚本只负责取数；分类/翻译/排版交给一次普通 run
        if let Some(instruction) = polish {
            let data: String = body.chars().take(12_000).collect();
            let inb = Inbound {
                channel: crate::ids::ChannelId(channel.to_owned()),
                conversation: chat.to_owned(),
                actor: chat.to_owned(),
                kind: InboundKind::Text,
                text: Some(format!(
                    "{instruction}\n\n（数据来自技能 `{name}`，原文如下，不要编造条目）\n\n{data}"
                )),
                dedupe_key: format!("polish-{now}"),
                callback_id: None,
                msg: None,
                received_at: now,
                attachments: Vec::new(),
            };
            if let Err(e) = self.handle_run(inb).await {
                tracing::warn!(error = %e, "polish run failed");
            }
            return;
        }

        let _ = self.notify(channel, chat, &format!("{header}\n\n{body}")).await;
    }

    fn start_distill(self: &Arc<Self>, mut inb: Inbound, run_id: String) {
        let me = Arc::clone(self);
        let ch = self.channel_for(&inb.channel);
        if me.cfg.distill_prompt.trim().is_empty() {
            let ch = Arc::clone(&ch);
            tokio::spawn(async move {
                if let Some(cb) = &inb.callback_id {
                    let _ = ch.ack(cb, "未配置沉淀流程").await;
                }
            });
            return;
        }
        tokio::spawn(async move {
            if let Some(cb) = &inb.callback_id {
                let _ = ch.ack(cb, "开始沉淀…").await;
            }
            // 按钮已点过：就地撤掉 keyboard，正文保留
            let key = format!("skill.btn:{run_id}");
            if let Ok(Some(msg_id)) = me.store.kv_get(&key).await {
                if !msg_id.is_empty() {
                    let _ = ch.clear_buttons(&inb.conversation, &MsgRef(msg_id)).await;
                }
                let _ = me.store.kv_set(&key, "").await;
            }
            inb.kind = InboundKind::Text;
            inb.callback_id = None;
            inb.text = Some(format!(
                "{}\n\n（来源 run：{run_id}；上一轮的脚本在 /ws，安装用 publish_skill）",
                me.cfg.distill_prompt
            ));
            // 每次点击都是新 run：换 dedupe key，避免与旧 run 的事件主键冲突
            inb.dedupe_key = format!("skill-{}", now_ms());
            if let Err(e) = me.handle_run(inb).await {
                tracing::warn!(error = %e, "distill run failed");
            }
        });
    }

    /// 发一条"系统通知"，同时写进账本（否则模型下次看不到自己发过什么）。
    async fn notify(&self, channel: &str, conversation: &str, text: &str) -> Result<(), ChannelError> {
        let key = format!("conv.epoch:{channel}:{conversation}");
        let epoch = self
            .store
            .kv_get(&key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let conv = ConversationId(format!("{channel}:{conversation}:{epoch}"));
        let run = RunId(format!("notice-{}", now_ms()));
        if let Ok(mut tx) = self.store.begin_write().await {
            let _ = tx
                .append(crate::event::NewEvent {
                    conv,
                    run,
                    turn: None,
                    kind: crate::event::EventKind::MsgAssistant,
                    payload: serde_json::json!({ "text": text }),
                    dedupe_key: None,
                })
                .await;
            let _ = tx.commit().await;
        }
        self.reply(channel, conversation, text).await
    }

    async fn reply(&self, channel: &str, conversation: &str, text: &str) -> Result<(), ChannelError> {
        let view = View {
            shard: 0,
            is_last: true,
            blocks: vec![Block::Paragraph(text.to_owned())],
            status: None,
            buttons: Vec::new(),
        };
        self.channel_for(&ChannelId(channel.to_owned()))
            .deliver(conversation, view)
            .await
            .map(|_| ())
    }
}

/// 附件的呈现：只给**沙盒内路径**和元信息，怎么处理由模型决定。
fn attachment_lines(items: &[crate::Attachment]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n附件（沙盒内只读，路径可直接给 exec 用）：\n");
    for a in items {
        let base = std::path::Path::new(&a.path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| a.path.clone());
        out.push_str(&format!(
            "- /media/{base}（{}，{} KB，原名 {}）\n",
            a.mime,
            a.bytes / 1024,
            a.name
        ));
    }
    out
}

/// 把工具产物发到"发起这次 run 的会话"。
struct RunOutbox<'a> {
    channel: &'a dyn Channel,
    conversation: String,
}

#[async_trait::async_trait]
impl crate::tool::Outbox for RunOutbox<'_> {
    async fn send_file(
        &self,
        path: &str,
        name: &str,
        mime: &str,
        caption: &str,
    ) -> Result<(), String> {
        self.channel
            .send_file(&self.conversation, path, name, mime, caption)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// 让工具读文档（文件库索引等）。
struct StoreDocs<'a> {
    store: &'a dyn Store,
}

#[async_trait::async_trait]
impl crate::tool::DocReader for StoreDocs<'_> {
    async fn list(&self, kind: &str, limit: usize) -> Result<Vec<crate::record::Doc>, String> {
        let mut rd = self.store.begin_read().await.map_err(|e| e.to_string())?;
        rd.list_docs(crate::record::DocQuery {
            scopes: Vec::new(),
            kinds: vec![kind.to_owned()],
            limit,
        })
        .await
        .map_err(|e| e.to_string())
    }

    async fn get(&self, id: &str) -> Result<Option<crate::record::Doc>, String> {
        let mut rd = self.store.begin_read().await.map_err(|e| e.to_string())?;
        rd.get_doc(crate::ids::DocId(id.to_owned())).await.map_err(|e| e.to_string())
    }

    async fn delete(&self, id: &str) -> Result<bool, String> {
        self.store
            .delete_doc(&crate::ids::DocId(id.to_owned()))
            .await
            .map_err(|e| e.to_string())
    }
}

fn blocks_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            Block::Paragraph(t) => t.clone(),
            Block::Code { text, .. } => text.clone(),
            Block::Divider => "---".to_owned(),
            Block::Image { alt, .. } => format!("[图片] {alt}"),
            Block::ToolCall { name, .. } => format!("[{name}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn summarize_args(args: &serde_json::Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in ["argv", "script", "path", "name", "text"] {
        match args.get(key) {
            Some(serde_json::Value::Array(items)) => {
                let joined = items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !joined.is_empty() {
                    parts.push(joined);
                }
            }
            Some(serde_json::Value::String(s)) if !s.is_empty() => parts.push(s.clone()),
            _ => {}
        }
    }
    let joined = if parts.is_empty() { args.to_string() } else { parts.join(" ") };
    let one_line = joined.replace('\n', " ");
    let count = one_line.chars().count();
    if count > 200 {
        format!("{}…", one_line.chars().take(200).collect::<String>())
    } else {
        one_line
    }
}




impl<S: Store, D: TurnDriver> ConversationActor<S, D> {
    /// 把"上一次做到哪里"压成一段可注入的简报。
    ///
    /// 原则：**只给结论与产物，不给过程**。子 agent 需要的是"已经有什么"，
    /// 不是把上一个会话的历史重放一遍（那等于把 token 花在复述上）。
    ///
    /// 支持两种来源：
    /// - `task-…`：后台任务 doc（跑完有 result.summary；没跑完就退回它的 child_conv）
    /// - `sub:…#n`：子会话，取最后两段 assistant 正文
    async fn prior_brief(&self, source: &str) -> Option<(String, String)> {
        if source.starts_with("sub:") {
            return self.brief_from_conv(source).await.map(|b| (source.to_owned(), b));
        }
        if !source.starts_with("task-") {
            return None;
        }
        let doc = self.list_tasks().await.into_iter().find(|d| d.id.0 == source)?;
        if let Some(s) = doc.payload["result"]["summary"].as_str() {
            if !s.trim().is_empty() {
                return Some((source.to_owned(), s.chars().take(1_600).collect()));
            }
        }
        // 被打断/失败：退回它名下的子会话，看有没有中途产出的正文。
        let child = doc.payload["child_conv"].as_str()?;
        let brief = self.brief_from_conv(child).await?;
        Some((format!("{source} → {child}"), brief))
    }

    async fn brief_from_conv(&self, conv: &str) -> Option<String> {
        let mut rd = self.store.begin_read().await.ok()?;
        let page = rd
            .messages(ConversationId(conv.to_owned()), 0, 40)
            .await
            .ok()?;
        let mut parts: Vec<String> = page
            .msgs
            .iter()
            .filter(|m| m.role == crate::record::Role::Assistant)
            .filter_map(|m| m.payload.get("text").and_then(|v| v.as_str()))
            .filter(|t| !t.trim().is_empty())
            .map(|t| t.trim().chars().take(800).collect::<String>())
            .collect();
        if parts.is_empty() {
            return None;
        }
        // 只要最后两段：更早的多半已经被后续结论覆盖
        let tail = parts.split_off(parts.len().saturating_sub(2));
        Some(tail.join("\n---\n"))
    }
}

/// 后台任务在 doc 容器里的种类与作用域（P4.3）。
/// 与 `remind` 同一条路：版本号 CAS、`/forget` 类操作直接复用。
const TASK_KIND: &str = "agent_task";
const TASK_SCOPE: &str = "task";

impl<S: Store, D: TurnDriver> ConversationActor<S, D> {
    /// 写/更新一条后台任务。失败只记日志：任务本身不该因为写账失败而卡住。
    async fn put_task_doc(&self, doc_id: &str, conv: &str, payload: serde_json::Value, version: u64) {
        let doc = crate::record::Doc {
            id: crate::ids::DocId(doc_id.to_owned()),
            kind: TASK_KIND.into(),
            scope: TASK_SCOPE.into(),
            payload,
            version,
            valid_from: now_ms(),
            valid_to: None,
        };
        let run = RunId(format!("job-{doc_id}"));
        let mut tx = match self.store.begin_write().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, task = doc_id, "后台任务：写账失败");
                return;
            }
        };
        if let Err(e) = tx.upsert_doc(ConversationId(conv.to_owned()), run, doc).await {
            tracing::warn!(error = %e, task = doc_id, "后台任务：upsert 失败");
            return;
        }
        if let Err(e) = tx.commit().await {
            tracing::warn!(error = %e, task = doc_id, "后台任务：提交失败");
        }
    }

    async fn list_tasks(&self) -> Vec<crate::record::Doc> {
        match self.store.begin_read().await {
            Ok(mut rd) => rd
                .list_docs(crate::record::DocQuery {
                    scopes: vec![TASK_SCOPE.into()],
                    kinds: vec![TASK_KIND.into()],
                    limit: 100,
                })
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// 入队：立刻返回任务号。执行由 30s tick 在"前台静默"后领。
    pub async fn enqueue_agent_task(&self, spec: crate::agent::AgentSpec) -> Result<String, crate::agent::AgentError> {
        if !self.cfg.agents.enabled {
            return Err(crate::agent::AgentError::Disabled);
        }
        if let Some(msg) = self.budget_exceeded().await {
            return Err(crate::agent::AgentError::Other(msg));
        }
        let n = {
            let mut s = self.bg_seq.lock().unwrap();
            *s += 1;
            *s
        };
        let id = format!("task-{}-{n}", now_ms());
        let conv = spec.parent_conv.clone();
        // 预分配子会话：打断后任务表才知道"上次干到哪个会话"，重派时能带上进展。
        // 注意这只是**命名**，不是复跑原会话——重派仍是全新上下文 + 简报。
        let child_conv = match spec.child_conv.clone() {
            Some(c) => c,
            None => {
                let seq = {
                    let mut m = self.agent_seq.lock().unwrap();
                    let v = m.entry(conv.clone()).or_insert(0);
                    *v += 1;
                    *v
                };
                format!("sub:{conv}#{seq}")
            }
        };
        // 推结果要知道"发到哪个 chat"：conv 形如 `telegram:<chat>:<epoch>`，
        // 与审批卡取 chat 的规则一致。
        let chat = conv.split(':').nth(1).unwrap_or_default().to_owned();
        let payload = serde_json::json!({
            "text": spec.task,
            "role": if spec.profile.name.is_empty() { "inherit".to_string() } else { spec.profile.name },
            "inputs": spec.inputs,
            "status": "pending",
            "conv": conv,
            "chat": chat,
            "attempts": 0,
            "created_at": now_ms(),
            "child_conv": child_conv,
            "continue_from": spec.continue_from,
            "max_turns": spec.max_turns,
            "max_tokens": spec.max_tokens,
            "wall_ms": spec.wall_ms,
        });
        self.put_task_doc(&id, &spec.parent_conv, payload, 1).await;
        Ok(id)
    }

    /// 取消一条任务：pending 直接作废，running 打断（与 `/tasks cancel` 同一条路）。
    async fn cancel_one_task(&self, id: &str) -> Option<String> {
        if let Some(t) = self.bg_running.lock().unwrap().get(id).cloned() {
            t.cancel();
            return Some(format!("已请求取消 {id}"));
        }
        let doc = self.list_tasks().await.into_iter().find(|d| d.id.0 == id)?;
        if doc.payload["status"] == "pending" {
            let mut p = doc.payload.clone();
            p["status"] = serde_json::json!("cancelled");
            p["finished_at"] = serde_json::json!(crate::ids::now_ms());
            self.put_task_doc(
                id,
                doc.payload["conv"].as_str().unwrap_or_default(),
                p,
                doc.version + 1,
            )
            .await;
            return Some(format!("已取消 {id}"));
        }
        Some(format!(
            "{id} 已经结束（{}）",
            doc.payload["status"].as_str().unwrap_or("?")
        ))
    }

    /// 启动时收尾：上次 `running` 的任务只通知、不重跑（D4：External 永不自动重发）。
    pub async fn resume_agent_tasks(self: &Arc<Self>) -> usize {
        let mut n = 0;
        for doc in self.list_tasks().await {
            if doc.payload["status"] != "running" {
                continue;
            }
            let chat = doc.payload["chat"].as_str().unwrap_or_default().to_owned();
            let mut p = doc.payload.clone();
            p["status"] = serde_json::json!("interrupted");
            p["finished_at"] = serde_json::json!(now_ms());
            self.put_task_doc(&doc.id.0, &doc.payload["conv"].as_str().unwrap_or_default(), p, doc.version + 1)
                .await;
            // 人看一条消息；模型也要能看见——写成 msg.synthetic 进父会话账本，
            // 下一轮组装上下文时它就在历史里，不用等主人转述。
            let body = format!(
                "后台任务 `{}`（{}）被服务重启打断，结果未知。没有自动重跑（避免重复副作用）；                 需要继续时请重新派一次，或问主人要不要续。",
                doc.id.0,
                doc.payload["text"].as_str().unwrap_or("").lines().next().unwrap_or("")
            );
            let conv = doc.payload["conv"].as_str().unwrap_or_default().to_owned();
            let chan = conv.split(':').next().unwrap_or_default().to_owned();
            if !conv.is_empty() {
                if let Ok(mut tx) = self.store.begin_write().await {
                    let _ = tx
                        .append(NewEvent {
                            conv: ConversationId(conv),
                            run: RunId(format!("job-{}", doc.id.0)),
                            turn: None,
                            kind: EventKind::MsgSynthetic,
                            payload: serde_json::json!({
                                "reason": "agent_task_interrupted",
                                "content": body,
                            }),
                            dedupe_key: Some(format!("task-interrupted-{}", doc.id.0)),
                        })
                        .await;
                    let _ = tx.commit().await;
                }
            }
            if !chat.is_empty() {
                let _ = self
                    .reply(
                        &chan,
                        &chat,
                        &format!(
                            "⚠️ 后台任务 `{}` 被服务重启打断，结果未知（没有自动重跑，避免重复副作用）。\n需要的话说一声，我重新派一次。",
                            doc.id.0
                        ),
                    )
                    .await;
            }
            n += 1;
        }
        n
    }

    /// 30s tick：前台静默且没有 run 时，领一条 pending 开跑。
    pub async fn tick_agent_tasks(self: &Arc<Self>) -> usize {
        if !self.cfg.agents.enabled {
            return 0;
        }
        let now = now_ms();
        let idle_ms = (self.cfg.agents.background_idle_s as i64) * 1000;
        if idle_ms > 0 && now.saturating_sub(*self.last_active.lock().unwrap()) < idle_ms {
            return 0;
        }
        if !self.runs.lock().unwrap().is_empty() || !self.bg_running.lock().unwrap().is_empty() {
            return 0;
        }
        let docs = self.list_tasks().await;
        let Some(doc) = docs.iter().find(|d| d.payload["status"] == "pending").cloned() else {
            return 0;
        };

        let mut p = doc.payload.clone();
        p["status"] = serde_json::json!("running");
        p["started_at"] = serde_json::json!(now);
        p["attempts"] = serde_json::json!(doc.payload["attempts"].as_u64().unwrap_or(0) + 1);
        self.put_task_doc(&doc.id.0, &doc.payload["conv"].as_str().unwrap_or_default(), p.clone(), doc.version + 1)
            .await;

        let token = CancellationToken::new();
        self.bg_running.lock().unwrap().insert(doc.id.0.clone(), token.clone());
        let me = Arc::clone(self);
        let id = doc.id.0.clone();
        tokio::spawn(async move {
            me.run_agent_task(id, p, token).await;
        });
        1
    }

    async fn run_agent_task(self: Arc<Self>, id: String, payload: serde_json::Value, token: CancellationToken) {
        let conv = payload["conv"].as_str().unwrap_or_default().to_owned();
        let chat = payload["chat"].as_str().unwrap_or_default().to_owned();
        let chan = conv.split(':').next().unwrap_or_default().to_owned();
        let text = payload["text"].as_str().unwrap_or_default().to_owned();
        let role = payload["role"].as_str().unwrap_or("inherit").to_owned();
        let inputs: Vec<String> = payload["inputs"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_owned())).collect())
            .unwrap_or_default();

        let spec = crate::agent::AgentSpec {
            parent_conv: conv.clone(),
            parent_run: RunId(format!("job-{id}")),
            task: text.clone(),
            inputs,
            profile: crate::agent::AgentProfile { name: role, ..Default::default() },
            system_prompt: Some(self.cfg.system_prompt.clone()),
            max_turns: payload["max_turns"].as_u64().unwrap_or(0) as u32,
            max_tokens: payload["max_tokens"].as_u64().unwrap_or(0),
            wall_ms: payload["wall_ms"].as_u64().unwrap_or(0),
            depth: 1,
            continue_from: payload["continue_from"].as_str().map(|s| s.to_owned()),
            child_conv: payload["child_conv"].as_str().map(|s| s.to_owned()),
        };

        let res = <Self as crate::agent::AgentSpawner>::spawn(&*self, spec, token.clone()).await;
        let status = match &res {
            Ok(c) => c.status,
            Err(_) => crate::agent::AgentStatus::Failed,
        };
        let mut p = payload.clone();
        p["status"] = serde_json::json!(status.as_str());
        p["finished_at"] = serde_json::json!(now_ms());
        match &res {
            Ok(c) => {
                p["result"] = serde_json::json!({
                    "summary": c.summary,
                    "child_conv": c.child_conv,
                    "pending_actions": c.pending_actions,
                });
            }
            Err(e) => {
                p["result"] = serde_json::json!({ "error": e.to_string() });
            }
        }
        // 用当前版本 +1，别猜：并发 tick 或人工取消都会改版本号。
        let version = self
            .list_tasks()
            .await
            .into_iter()
            .find(|d| d.id.0 == id)
            .map(|d| d.version + 1)
            .unwrap_or(2);
        self.put_task_doc(&id, &conv, p, version).await;
        self.bg_running.lock().unwrap().remove(&id);

        // 结论不仅推给人，也写进父会话账本：否则主人问"刚才那个任务怎样了"，
        // 模型完全不知道（它只看到过"已排入后台任务"那条）。
        let notice = match &res {
            Ok(c) => format!(
                "后台任务 `{id}` 已结束（{}）：\n{}",
                c.status.as_str(),
                c.render()
            ),
            Err(e) => format!("后台任务 `{id}` 失败：{e}"),
        };
        if let Ok(mut tx) = self.store.begin_write().await {
            let _ = tx
                .append(NewEvent {
                    conv: ConversationId(conv.clone()),
                    run: RunId(format!("job-{id}")),
                    turn: None,
                    kind: EventKind::MsgSynthetic,
                    payload: serde_json::json!({
                        "reason": "agent_task_done",
                        "content": notice,
                    }),
                    dedupe_key: Some(format!("task-done-{id}")),
                })
                .await;
            let _ = tx.commit().await;
        }

        if chat.is_empty() {
            return;
        }
        let first = text.lines().next().unwrap_or("后台任务");
        let msg = match &res {
            Ok(c) if c.status == crate::agent::AgentStatus::Done => {
                format!("⏱ 后台任务完成 · {first}\n\n{}", c.render())
            }
            Ok(c) => format!("⚠️ 后台任务未完成（{}）· {first}\n\n{}", c.status.as_str(), c.render()),
            Err(e) => format!("⚠️ 后台任务失败 · {first}\n{e}"),
        };
        let _ = self.reply(&chan, &chat, &msg).await;
    }
}

/// 子 run 的审批门：**永不打扰人**。
///
/// 策略照旧生效，只是 `ask` 一律按拒绝处理——子 agent 没有通道，
/// 让它弹审批等于绕过治理（D19）。拒绝会走 Runner 的 denied 分支，
/// 子模型看到的是"已拒绝执行"，而不是一个挂住的 run。
struct ChildGate<'a> {
    tools: &'a ToolRegistry,
    policy: &'a crate::policy::ApprovalPolicy,
}

#[async_trait::async_trait]
impl crate::runner::ApprovalGate for ChildGate<'_> {
    async fn ask(
        &self,
        _conv: &ConversationId,
        _call: &CallId,
        tool: &str,
        _args: &serde_json::Value,
    ) -> Result<bool, String> {
        let domain = self
            .tools
            .get(tool)
            .map(|t| t.domain())
            .unwrap_or(crate::policy::Domain::Sandbox);
        match self.policy.verdict(tool, domain) {
            crate::policy::Verdict::Allow => Ok(true),
            crate::policy::Verdict::Deny => Ok(false),
            crate::policy::Verdict::Ask => {
                tracing::info!(tool, "child agent cannot request approval; denied");
                Ok(false)
            }
        }
    }
}

/// 运行中的子 agent（`/agents` 用）。只活在内存里：重启后子 run 已经由
/// `run.finished(interrupted)` 收尾，不需要跨进程恢复这张表。
struct RunningAgent {
    parent_conv: String,
    task: String,
    started_ms: i64,
    cancel: CancellationToken,
}

/// 派发名额的 RAII 计数：任何退出路径（含 `?`）都会减回去。
struct SlotGuard {
    map: Arc<Mutex<HashMap<String, usize>>>,
    key: String,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.map.lock() {
            if let Some(n) = m.get_mut(&self.key) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    m.remove(&self.key);
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl<S: Store, D: TurnDriver> crate::agent::AgentSpawner for ConversationActor<S, D> {
    async fn enqueue(
        &self,
        spec: crate::agent::AgentSpec,
    ) -> Result<String, crate::agent::AgentError> {
        // 之前只实现了 spawn，delegate(background=true) 走 trait 默认实现 →
        // 永远返回"当前实现不支持后台任务"（真实事故）。
        self.enqueue_agent_task(spec).await
    }

    async fn tasks(&self) -> Result<Vec<crate::agent::TaskInfo>, crate::agent::AgentError> {
        Ok(self
            .list_tasks()
            .await
            .into_iter()
            .map(|d| crate::agent::TaskInfo {
                id: d.id.0.clone(),
                status: d.payload["status"].as_str().unwrap_or("?").to_owned(),
                text: d.payload["text"].as_str().unwrap_or_default().to_owned(),
                created_ms: d.payload["created_at"].as_i64().unwrap_or(0),
                started_ms: d.payload["started_at"].as_i64(),
                child_conv: d.payload["child_conv"].as_str().map(|s| s.to_owned()),
                result: d.payload["result"]["summary"].as_str().map(|s| s.to_owned()),
            })
            .collect())
    }

    async fn cancel_task(&self, id: &str) -> Result<String, crate::agent::AgentError> {
        self.cancel_one_task(id)
            .await
            .ok_or_else(|| crate::agent::AgentError::Other(format!("没有这个任务：{id}")))
    }

    async fn spawn(
        &self,
        spec: crate::agent::AgentSpec,
        cancel: CancellationToken,
    ) -> Result<crate::agent::Conclusion, crate::agent::AgentError> {
        use crate::agent::{clip_summary, split_pending, AgentError, AgentStatus, Conclusion};

        let limits = &self.cfg.agents;
        if !limits.enabled {
            return Err(AgentError::Disabled);
        }
        if let Some(msg) = self.budget_exceeded().await {
            return Err(AgentError::Other(msg));
        }
        // 单层拓扑：只接受 1，其余一律拒绝（不在"再放松一点"上留口子）。
        if spec.depth != 1 {
            return Err(AgentError::Depth(spec.depth));
        }

        // 角色：只能从配置/角色目录取，模型只能点名。拼错就报错，不静默回退到 inherit。
        let effective = self.effective_roles();
        let role_name = spec.profile.name.trim();
        let role = if role_name.is_empty() || role_name == "inherit" {
            None
        } else {
            Some(effective.get(role_name).ok_or_else(|| {
                AgentError::Other(format!(
                    "未知角色：{role_name}（[agents.roles] 里没有配置）"
                ))
            })?)
        };

        // 名额：先占再减，任何早退都会经 SlotGuard 释放。
        let _slot = {
            let mut m = self.agent_slots.lock().unwrap();
            let n = m.entry(spec.parent_run.0.clone()).or_insert(0);
            if *n >= limits.max_per_run {
                return Err(AgentError::Quota(limits.max_per_run));
            }
            *n += 1;
            SlotGuard { map: Arc::clone(&self.agent_slots), key: spec.parent_run.0.clone() }
        };

        // agent 车道：同一时刻只跑一个子 agent（SBC 上 CPU 与模型连接都要让给前台）。
        let _agent_lane = self
            .cfg
            .lanes
            .acquire(crate::lanes::Lane::Agent, &cancel)
            .await
            .map_err(|_| AgentError::Cancelled)?;

        // 子会话 id：可以用入队时预分配的，但必须确实是本父会话名下的，
        // 否则等于让调用方指定任意会话（跨会话串数据）。
        let prefix = format!("sub:{}#", spec.parent_conv);
        let child_conv = match spec.child_conv.as_deref() {
            Some(c) if c.starts_with(&prefix) => c.to_owned(),
            _ => {
                let seq = {
                    let mut m = self.agent_seq.lock().unwrap();
                    let n = m.entry(spec.parent_conv.clone()).or_insert(0);
                    *n += 1;
                    *n
                };
                format!("{prefix}{seq}")
            }
        };
        let child_run = RunId(format!("{child_conv}-a{}", now_ms()));

        // 父账本：先把"派出去了"写下来，崩溃时才能对上号（D3）。
        {
            let mut tx = self.store.begin_write().await.map_err(|e| AgentError::Other(e.to_string()))?;
            tx.append(NewEvent {
                conv: ConversationId(spec.parent_conv.clone()),
                run: spec.parent_run.clone(),
                turn: None,
                kind: EventKind::AgentSpawned,
                payload: serde_json::json!({
                    "child_conv": child_conv,
                    "child_run": child_run.0,
                    "role": if role_name.is_empty() { "inherit" } else { role_name },
                    "task": clip_summary(&spec.task),
                    "depth": spec.depth,
                }),
                dedupe_key: Some(format!("agent-spawned-{}", child_run.0)),
            })
            .await
            .map_err(|e| AgentError::Other(e.to_string()))?;
            tx.commit().await.map_err(|e| AgentError::Other(e.to_string()))?;
        }

        // 工具集三层收窄：`[agents] tools` → 角色 → profile。只能更窄，不能更宽。
        // `only()` 内部还会剔掉 tier ≥ 1（含 delegate），递归在工具集这层也不可能。
        let mut allowed: Vec<String> = match role.and_then(|r| r.tools.as_ref()) {
            Some(role_tools) => limits
                .tools
                .iter()
                .filter(|n| role_tools.contains(n))
                .cloned()
                .collect(),
            None => limits.tools.clone(),
        };
        if let Some(names) = &spec.profile.tools {
            allowed.retain(|n| names.contains(n));
        }
        let child_tools = self.tools.only(&allowed);

        let mut system = spec
            .system_prompt
            .clone()
            .unwrap_or_else(|| self.cfg.system_prompt.clone());
        // 接手背景：只给上次的结论与产物，不给整段历史——省 token，也不污染角色提示。
        if let Some(src) = spec.continue_from.as_deref() {
            if let Some((label, brief)) = self.prior_brief(src).await {
                system.push_str(&format!(
                    "\n\n【接手说明】这是继续之前的工作（来源：{label}），不要从零开始。\n\
                     上次的进展（节选）：\n{brief}\n\
                     先确认这些成果还在（/ws 下的文件、已落盘的产物），能复用就直接复用；\
                     缺什么补什么，不要重复已经做完的步骤。"
                ));
            } else {
                tracing::info!(source = src, "continue_from 找不到可复用的进展，按新任务处理");
            }
        }

        // 角色段追加在父前缀之后：前缀字节不变，provider 的 prompt cache 还能命中。
        if let Some(prompt) = role.and_then(|r| r.system_prompt.as_ref()) {
            if !prompt.trim().is_empty() {
                system.push_str("\n\n");
                system.push_str(prompt.trim());
            }
        }
        let my_tools: Vec<String> =
            child_tools.specs().into_iter().map(|s| s.name).collect();
        system.push_str(&format!(
            "\n\n你这次能用的工具只有：{}。\n\
             不在这个列表里的能力（联网、发消息、装依赖、改配置…）就是没有——直接说明，\
             不要反复尝试，也不要为了绕开它去写别的代码。\n",
            my_tools.join(" / ")
        ));
        system.push_str(
            "\n\n你现在是子 agent（depth 1），只做下面这一件事。\n\
             - 你看不到父会话的历史：缺信息就写进结论，不要猜。\n\
             - 需要用户确认、发消息、打印、装依赖、改配置的动作你做不到，也不要去试。\n\
             - 最终回复只写结论，不要复述过程，固定这个形状：\n\
             结论：<做了什么、查到什么，用户能直接读>\n\
             产物：<路径，没有就写「无」>\n\
             [待父执行]\n\
             - <需要父会话替你完成的动作，如 publish_skill(name)；没有就写「- 无」>\n",
        );
        if !spec.inputs.is_empty() {
            system.push_str("\n可用输入（/ws 下的文件）：\n");
            for p in &spec.inputs {
                system.push_str("- ");
                system.push_str(p);
                system.push('\n');
            }
        }

        let pick = |explicit: u64, role: u64, default: u64| {
            if explicit > 0 {
                explicit
            } else if role > 0 {
                role
            } else {
                default
            }
        };
        let max_turns = pick(
            spec.max_turns as u64,
            role.map(|r| r.max_turns).unwrap_or(0) as u64,
            limits.max_turns as u64,
        ) as u32;
        let max_tokens = pick(
            spec.max_tokens,
            role.map(|r| r.max_tokens).unwrap_or(0),
            limits.max_tokens,
        );
        let wall_ms = pick(spec.wall_ms, role.map(|r| r.wall_ms).unwrap_or(0), limits.wall_ms);
        if wall_ms == 0 {
            // 0 = 不限（只有**全局** [agents] wall_ms 能表达这个意思；角色里写 0 是"继承"）。
            // 子 run 没有父那套卡死检测，所以这里明确提醒：卡住会一直占着 agent 车道。
            tracing::warn!(
                child = %child_conv,
                "子 agent 墙钟不限（[agents] wall_ms=0）：卡住时会一直占着 agent 车道"
            );
        }
        let model = spec
            .profile
            .model
            .clone()
            .or_else(|| role.and_then(|r| r.model.clone()))
            .unwrap_or_else(|| self.cfg.model.clone());

        let req = RunRequest {
            conv: ConversationId(child_conv.clone()),
            run: child_run.clone(),
            text: spec.task.clone(),
            model,
            max_turns,
            wall_clock_ms: wall_ms,
            system_prompt: Some(system),
            actor: "agent".into(),
            images: Vec::new(),
            workspace: Workspace { run: child_run.clone(), root: self.cfg.workspace_root.clone() },
            context_budget_chars: self.cfg.context_budget_chars,
            token_budget: Some(max_tokens),
        };

        let docs = StoreDocs { store: &*self.store };
        let gate = ChildGate { tools: &child_tools, policy: &self.cfg.policy };
        let runner = Runner {
            store: &*self.store,
            driver: &*self.driver,
            tools: &child_tools,
            env: &*self.env,
            lanes: &self.cfg.lanes,
            spawner: None, // 结构上禁止递归
            gate: Some(&gate),
            outbox: None, // 子 agent 不能直接发文件
            docs: Some(&docs),
        };

        // 子 run 的事件不渲染到父消息（会把界面和上下文搅乱），但也不能全丢：
        // 抽干它，把"第几轮 / 在跑什么工具"记进进度表，父状态行和 /agents 用。
        let pair = EventSink::channel(128);
        let progress = Arc::clone(&self.agent_progress);
        let pump_key = child_conv.clone();
        let pump = tokio::spawn(async move {
            let mut rx = pair.rx;
            let mut turn = 0u32;
            while let Some(ev) = rx.recv().await {
                let text = match ev {
                    crate::driver::StreamEvent::TurnStarted { turn_no } => {
                        turn = turn_no;
                        format!("第 {turn} 轮 · 思考中")
                    }
                    crate::driver::StreamEvent::ToolStarted { name, .. } => {
                        format!("第 {turn} 轮 · 执行 {name}")
                    }
                    crate::driver::StreamEvent::ToolFinished { ok, .. } => {
                        format!("第 {turn} 轮 · {} 完成", if ok { "工具" } else { "失败" })
                    }
                    crate::driver::StreamEvent::Restarted => {
                        format!("第 {turn} 轮 · 重试中")
                    }
                    _ => continue,
                };
                progress.lock().unwrap().insert(pump_key.clone(), text);
            }
        });

        // 独立取消令牌：父取消照样联动（linker），但 `/agents cancel` 也能只杀子任务。
        let child_cancel = CancellationToken::new();
        let link = {
            let parent = cancel.clone();
            let child = child_cancel.clone();
            tokio::spawn(async move {
                parent.cancelled().await;
                child.cancel();
            })
        };
        self.agents_running.lock().unwrap().insert(
            child_conv.clone(),
            RunningAgent {
                parent_conv: spec.parent_conv.clone(),
                task: clip_summary(&spec.task),
                started_ms: now_ms(),
                cancel: child_cancel.clone(),
            },
        );

        let res = runner.run(req, pair.sink, child_cancel).await;
        self.agents_running.lock().unwrap().remove(&child_conv);
        self.agent_progress.lock().unwrap().remove(&child_conv);
        pump.abort();
        link.abort();
        let (status, summary) = match res {
            Ok(out) => {
                let status = match out.status {
                    RunStatus::Done => AgentStatus::Done,
                    RunStatus::Cancelled => AgentStatus::Cancelled,
                    RunStatus::Failed => AgentStatus::Failed,
                };
                (status, out.text)
            }
            Err(crate::runner::RunnerError::Budget(m)) => {
                let status = if m.contains("wall clock") {
                    AgentStatus::Timeout
                } else {
                    AgentStatus::Budget
                };
                (status, format!("子任务中止：{m}"))
            }
            Err(crate::runner::RunnerError::Engine(crate::error::EngineError::Cancelled)) => {
                (AgentStatus::Cancelled, "子任务被取消".to_owned())
            }
            Err(e) => (AgentStatus::Failed, format!("子任务失败：{e}")),
        };

        let (body, pending) = split_pending(&summary);
        let conclusion = Conclusion {
            status,
            summary: clip_summary(if body.trim().is_empty() { &summary } else { &body }),
            child_conv: child_conv.clone(),
            child_run: child_run.0.clone(),
            artifacts: Vec::new(),
            pending_actions: pending,
            usage: crate::driver::UsageSnapshot::default(),
        };

        if let Ok(mut tx) = self.store.begin_write().await {
            let _ = tx
                .append(NewEvent {
                    conv: ConversationId(spec.parent_conv.clone()),
                    run: spec.parent_run.clone(),
                    turn: None,
                    kind: EventKind::AgentConcluded,
                    payload: serde_json::json!({
                        "child_conv": child_conv,
                        "child_run": conclusion.child_run,
                        "status": conclusion.status.as_str(),
                        "summary": conclusion.summary,
                        "pending_actions": conclusion.pending_actions,
                    }),
                    dedupe_key: Some(format!("agent-concluded-{}", conclusion.child_run)),
                })
                .await;
            let _ = tx.commit().await;
        }

        Ok(conclusion)
    }
}

#[async_trait::async_trait]
impl<S: Store, D: TurnDriver> ApprovalGate for ConversationActor<S, D> {
    async fn ask(
        &self,
        conv: &ConversationId,
        call: &CallId,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<bool, String> {
        let domain = self
            .tools
            .get(tool)
            .map(|t| t.domain())
            .unwrap_or(crate::policy::Domain::Sandbox);
        match self.cfg.policy.verdict(tool, domain) {
            crate::policy::Verdict::Allow => return Ok(true),
            crate::policy::Verdict::Deny => {
                tracing::info!(tool, "denied by policy");
                return Ok(false);
            }
            crate::policy::Verdict::Ask => {
                // 工具可以在自己核对过授权表后声明"这一条不需要打扰人"（D22：
                // `approval = "always"` 的路径）。只能把 Ask 放成放行，Deny 已在上面返回。
                if self.tools.get(tool).is_some_and(|t| t.skip_approval(args)) {
                    tracing::info!(tool, "preapproved by tool (grant=always)");
                    return Ok(true);
                }
            }
        }
        let hint = self.tools.get(tool).and_then(|t| t.approval_hint(args));
        let brief = summarize_args(args);
        // 账本里记"人话"，事后翻审批记录才看得懂
        let action = match &hint {
            Some(h) => format!("{tool} {h}"),
            None => format!("{tool} {brief}"),
        };
        let id = self
            .store
            .create_approval(NewApproval {
                conv: conv.clone(),
                call: call.clone(),
                action,
                scope: "workspace".into(),
                ttl_s: APPROVAL_TTL_S,
            })
            .await
            .map_err(|e| e.to_string())?;

        let chat = conv.0.split(':').nth(1).unwrap_or_default().to_owned();
        // 先给人能判断的信息（什么时候、做什么），参数放后面供核对
        let text = match &hint {
            Some(h) => format!("🔐 需要确认\n\n{h}\n\n参数：{brief}"),
            None => format!("🔐 需要确认\n\n工具：{tool}\n参数：{brief}"),
        };
        let ch = self.channel_for(&ChannelId(
            conv.0.split(':').next().unwrap_or_default().to_owned(),
        ));
        let card = ch
            .deliver_prompt(
                &chat,
                crate::view::Prompt {
                    text,
                    choices: vec![
                        crate::view::Choice {
                            id: id.0.clone(),
                            label: "✅ 允许".into(),
                            value: "y".into(),
                        },
                        crate::view::Choice {
                            id: id.0.clone(),
                            label: "❌ 拒绝".into(),
                            value: "n".into(),
                        },
                    ],
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        self.store
            .attach_approval_card(&id, &chat, &card)
            .await
            .map_err(|e| e.to_string())?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.approvals.lock().unwrap().insert(id.0.clone(), tx);
        match tokio::time::timeout(Duration::from_secs(APPROVAL_TTL_S), rx).await {
            Ok(Ok(yes)) => Ok(yes),
            _ => {
                self.approvals.lock().unwrap().remove(&id.0);
                let _ = self.store.resolve_approval(&id, false, "timeout").await;
                let _ = ch
                    .clear_prompt(&chat, &card, "⏱ 审批超时，已拒绝执行。")
                    .await;
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RunScope, attachment_lines, summarize_args};

    /// 早退（`?` / panic）时也必须停 typing、摘掉 runs 条目，
    /// 否则：界面一直"正在输入"、`/status` 永远"运行中"。
    #[test]
    fn run_scope_cleans_up_on_drop() {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use tokio_util::sync::CancellationToken;

        let runs = Arc::new(Mutex::new(HashMap::new()));
        let token = CancellationToken::new();
        runs.lock().unwrap().insert("1000".to_owned(), token.clone());
        let typing = CancellationToken::new();
        {
            let _scope = RunScope {
                runs: Arc::clone(&runs),
                typing_map: Arc::new(Mutex::new(HashMap::new())),
                conversation: "1000".to_owned(),
                typing: typing.clone(),
                typing_task: None,
            };
        }
        assert!(runs.lock().unwrap().is_empty(), "drop 后 runs 条目应被摘掉");
        assert!(typing.is_cancelled(), "drop 后 typing 应停止");
        assert!(!token.is_cancelled(), "不该动 run 自己的取消令牌");
    }

    #[test]
    fn attachments_are_listed_with_sandbox_paths() {
        let items = vec![crate::Attachment {
            path: "/home/u/media/abc.pdf".into(),
            name: "通知.pdf".into(),
            mime: "application/pdf".into(),
            digest: "abc".into(),
            bytes: 2048,
        }];
        let text = attachment_lines(&items);
        assert!(text.contains("/media/abc.pdf"), "{text}");
        assert!(text.contains("application/pdf"), "{text}");
        assert!(text.contains("原名 通知.pdf"), "{text}");
        assert!(attachment_lines(&[]).is_empty());
    }

    #[test]
    fn summarize_prefers_argv_and_flattens_newlines() {
        let args = serde_json::json!({ "argv": ["ls", "-la", "/inbox"] });
        assert_eq!(summarize_args(&args), "ls -la /inbox");

        let args = serde_json::json!({ "script": "echo a\necho b" });
        assert_eq!(summarize_args(&args), "echo a echo b");
    }

    #[test]
    fn summarize_falls_back_to_json_and_truncates() {
        let args = serde_json::json!({ "k": 1 });
        assert!(summarize_args(&args).contains("\"k\""));
        let long = serde_json::json!({ "text": "x".repeat(300) });
        assert!(summarize_args(&long).ends_with('…'));
    }
}
