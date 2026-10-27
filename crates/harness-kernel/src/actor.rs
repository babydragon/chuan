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
use crate::ids::{ApprovalId, CallId, ConversationId, MsgRef, RunId, now_ms};
use crate::record::NewApproval;
use crate::identity::Directory;
use crate::render::{TurnRenderer, apply_action};
use crate::runner::{ApprovalGate, RunRequest, Runner};
use crate::store::Store;
use crate::tool::Workspace;
use crate::tool_registry::ToolRegistry;
use crate::view::{Block, RenderEvent, Renderer, RunStatus, View};

/// 审批等待上限：到点按拒绝处理，避免 run 永远挂着。
const APPROVAL_TTL_S: u64 = 300;

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
    pub channel: Arc<dyn Channel>,
    pub cfg: ActorConfig,
    retriever: Option<Arc<dyn crate::retriever::Retriever>>,
    runs: Arc<Mutex<HashMap<String, CancellationToken>>>,
    approvals: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl<S: Store, D: TurnDriver> Clone for ConversationActor<S, D> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            driver: Arc::clone(&self.driver),
            tools: Arc::clone(&self.tools),
            env: Arc::clone(&self.env),
            channel: Arc::clone(&self.channel),
            cfg: self.cfg.clone(),
            retriever: self.retriever.clone(),
            runs: Arc::clone(&self.runs),
            approvals: Arc::clone(&self.approvals),
            locks: Arc::clone(&self.locks),
        }
    }
}

impl<S: Store, D: TurnDriver> ConversationActor<S, D> {
    pub fn new(
        store: Arc<S>,
        driver: Arc<D>,
        tools: Arc<ToolRegistry>,
        env: Arc<dyn ExecutionEnv>,
        channel: Arc<dyn Channel>,
        cfg: ActorConfig,
        retriever: Option<Arc<dyn crate::retriever::Retriever>>,
    ) -> Self {
        Self {
            store,
            driver,
            tools,
            env,
            channel,
            cfg,
            retriever,
            runs: Arc::new(Mutex::new(HashMap::new())),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn run(
        self: Arc<Self>,
        mut inbound: mpsc::Receiver<Inbound>,
        cancel: CancellationToken,
    ) -> Result<(), ChannelError> {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = inbound.recv() => {
                    let Some(inb) = msg else { break };
                    if inb.kind == InboundKind::Command {
                        if let Err(e) = self.handle_command(inb).await {
                            tracing::warn!(error = %e, "command failed");
                        }
                    } else if inb.kind == InboundKind::Callback {
                        self.handle_callback(inb).await;
                    } else {
                        let me = Arc::clone(&self);
                        tasks.spawn(async move {
                            if let Err(e) = me.handle_run(inb).await {
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

    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .unwrap()
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
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
                    &inb.conversation,
                    "可用指令：\n\
                     /new — 开启新会话（保留长期记忆）\n\
                     /cancel — 取消当前任务\n\
                     /status — 查看状态\n\
                     /facts — 查看长期记忆\n\
                     /reminders — 查看/取消定时任务
                     /forget <id> — 删除一条记忆\n\
                     /help — 本说明\n\n\
                     直接发消息即可对话；改动过文件的任务终稿会带「沉淀为 Skill」按钮，\
                     要导入外部技能直接说就行（模型会先取 playbook）。",
                )
                .await
            }
            "new" => {
                let e = self.bump_epoch(&inb).await;
                self.reply(&inb.conversation, &format!("已开启新会话（epoch {e}）。之前的记录仍保留。"))
                    .await
            }
            "cancel" => {
                let token = self.runs.lock().unwrap().get(&inb.conversation).cloned();
                match token {
                    Some(t) => {
                        t.cancel();
                        self.reply(&inb.conversation, "正在取消当前任务…").await
                    }
                    None => self.reply(&inb.conversation, "当前没有进行中的任务。").await,
                }
            }
            "approve" | "deny" => {
                let rest = text.trim().split_whitespace().nth(1).unwrap_or("");
                if rest.is_empty() {
                    return self.reply(&inb.conversation, "用法：/approve <id> 或 /deny <id>").await;
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
                            &inb.conversation,
                            if yes { "✅ 已允许（记录见账本）" } else { "❌ 已拒绝" },
                        )
                        .await
                    }
                    None => self.reply(&inb.conversation, "审批不存在或已过期").await,
                }
            }
            "facts" => {
                let docs = self.list_facts(&inb.actor).await;
                if docs.is_empty() {
                    return self.reply(&inb.conversation, "还没有记住任何事实。").await;
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
                self.reply(&inb.conversation, &out).await
            }
            "forget" => {
                let prefix = text.trim().split_whitespace().nth(1).unwrap_or("");
                if prefix.is_empty() {
                    return self.reply(&inb.conversation, "用法：/forget <id 前缀>").await;
                }
                let hits: Vec<_> = self
                    .list_facts(&inb.actor)
                    .await
                    .into_iter()
                    .filter(|d| d.id.0.starts_with(prefix))
                    .collect();
                if hits.is_empty() {
                    return self.reply(&inb.conversation, "没有匹配的事实。用 /facts 看 id。").await;
                }
                for d in &hits {
                    let _ = self.store.delete_doc(&d.id).await;
                }
                self.reply(&inb.conversation, &format!("已删除 {} 条事实。", hits.len())).await
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
                    return self.reply(&inb.conversation, &msg).await;
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
                    return self.reply(&inb.conversation, "还没有定时任务。").await;
                }
                let mut out = String::from("定时任务：\n");
                for d in &docs {
                    let text = d.payload["text"].as_str().unwrap_or("?");
                    let next = d.payload["next_at"].as_i64().unwrap_or(0);
                    let desc = crate::schedule::parse(&d.payload["schedule"], self.cfg.tz_offset_min)
                        .map(|s| s.describe())
                        .unwrap_or_else(|_| "?".into());
                    out.push_str(&format!(
                        "- `{}` · {desc} · 下次 {}\n  {text}\n",
                        d.id.0,
                        crate::schedule::format_local(next, self.cfg.tz_offset_min)
                    ));
                }
                out.push_str("\n取消：/reminders del <id>");
                self.reply(&inb.conversation, &out).await
            }
            "status" => {
                let running = self.runs.lock().unwrap().contains_key(&inb.conversation);
                let epoch = self.epoch(&inb).await;
                let state = if running { "运行中" } else { "空闲" };
                self.reply(&inb.conversation, &format!("{state} · 会话 epoch {epoch}")).await
            }
            other => {
                self.reply(&inb.conversation, &format!("未知指令 /{other}，试试 /help")).await
            }
        }
    }

    async fn handle_run(&self, inb: Inbound) -> Result<(), ChannelError> {
        let key = inb.conversation.clone();
        let lock = self.lock_for(&key);
        let _guard = lock.lock().await;

        // 收到即显示"输入中"，之后每 4s 续一次（Telegram 的 typing 约 5s 过期）
        let typing_cancel = CancellationToken::new();
        let typing_task = {
            let channel = Arc::clone(&self.channel);
            let conv_key = inb.conversation.clone();
            let stop = typing_cancel.clone();
            tokio::spawn(async move {
                // 兜底：最多转 20 分钟，避免异常情况下永远"正在输入"
                let deadline = tokio::time::Instant::now() + Duration::from_secs(1200);
                loop {
                    let _ = channel.typing(&conv_key).await;
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(4)) => {}
                        _ = stop.cancelled() => break,
                    }
                    if tokio::time::Instant::now() > deadline {
                        tracing::warn!(conv = %conv_key, "typing 超时，停止提示");
                        break;
                    }
                }
            })
        };

        tracing::info!(conv = %inb.conversation, "run: 已拿到会话锁，开始准备");
        // 前置步骤都加超时：卡在存储上不该让"正在输入"一直转
        let epoch = match tokio::time::timeout(Duration::from_secs(5), self.epoch(&inb)).await {
            Ok(e) => e,
            Err(_) => {
                tracing::warn!(conv = %inb.conversation, "epoch 读取超时");
                typing_cancel.cancel();
                let _ = self.reply(&inb.conversation, "⏱ 读取会话状态超时，请稍后再试（已取消）。").await;
                return Ok(());
            }
        };
        let conv = ConversationId(format!("{}:{}:{}", inb.channel.0, inb.conversation, epoch));
        let run = RunId(format!("{}-{}", conv.0, inb.dedupe_key));
        let caps = self.channel.caps();
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
                scopes: vec!["family".into(), format!("user:{}", inb.actor)],
                kinds: vec!["fact".into()],
                limit: 50,
            };
            if let Ok(mut docs) = rd.list_docs(q).await {
                if docs.len() > 30 {
                    if let Some(r) = &self.retriever {
                        let query = crate::retriever::Query {
                            text: inb.text.clone().unwrap_or_default(),
                            scopes: vec!["family".into(), format!("user:{}", inb.actor)],
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
        };

        let outbox = RunOutbox { channel: &*self.channel, conversation: inb.conversation.clone() };
        let docs = StoreDocs { store: &*self.store };
        let runner = Runner {
            store: &*self.store,
            driver: &*self.driver,
            tools: &self.tools,
            env: &*self.env,
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
                    let action = renderer.flush(now_ms());
                    apply_action(&*self.channel, &inb.conversation, &mut renderer, action).await?;
                }
            }
        };
        let _ = stalled;

        self.runs.lock().unwrap().remove(&key);
        typing_cancel.cancel();
        let _ = typing_task.await;

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
            apply_action(&*self.channel, &inb.conversation, &mut renderer, action).await?;
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
                self.reply(&inb.conversation, "已取消。").await
            }
            Ok(_) => Ok(()),
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
                self.reply(&inb.conversation, &msg).await
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
        let data = inb.text.clone().unwrap_or_default();
        if let Some(run_id) = data.strip_prefix("skill:") {
            self.start_distill(inb, run_id.to_owned());
            return;
        }
        let parts: Vec<&str> = data.split(':').collect();
        if parts.len() != 3 || parts[0] != "appr" {
            if let Some(cb) = &inb.callback_id {
                let _ = self.channel.ack(cb, "无效操作").await;
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
                    let _ = self.channel.clear_prompt(chat, msg, note).await;
                }
                if let Some(cb) = &inb.callback_id {
                    let _ = self.channel.ack(cb, if yes { "已允许" } else { "已拒绝" }).await;
                }
            }
            None => {
                if let Some(cb) = &inb.callback_id {
                    let _ = self.channel.ack(cb, "已过期").await;
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
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        let n = self.tick_reminders().await;
                        if n > 0 {
                            tracing::info!(fired = n, "reminders fired");
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
            let _ = self.reply(chat, "❌ 定时任务失败：没有 run_skill 工具").await;
            return;
        };
        let run = RunId(format!("job-{now}"));
        let ws = Workspace { run: run.clone(), root: self.cfg.workspace_root.clone() };
        let ctx = crate::tool::ToolCtx {
            conv: ConversationId(format!("job:{chat}")),
            actor: "job",
            outbox: &crate::tool::NullOutbox,
            docs: &crate::tool::NullDocReader,
            run,
            call: CallId(format!("job-{now}")),
            workspace: &ws,
            env: &*self.env,
            cancel: CancellationToken::new(),
        };
        let body = match tool
            .execute(serde_json::json!({ "name": name, "args": args }), &ctx)
            .await
        {
            Ok(out) => blocks_text(&out.content),
            Err(e) => {
                let _ = self
                    .reply(chat, &format!("{header}\n\n❌ 技能执行失败：{}", e.message))
                    .await;
                return;
            }
        };

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
        if me.cfg.distill_prompt.trim().is_empty() {
            tokio::spawn(async move {
                if let Some(cb) = &inb.callback_id {
                    let _ = me.channel.ack(cb, "未配置沉淀流程").await;
                }
            });
            return;
        }
        tokio::spawn(async move {
            if let Some(cb) = &inb.callback_id {
                let _ = me.channel.ack(cb, "开始沉淀…").await;
            }
            // 按钮已点过：就地撤掉 keyboard，正文保留
            let key = format!("skill.btn:{run_id}");
            if let Ok(Some(msg_id)) = me.store.kv_get(&key).await {
                if !msg_id.is_empty() {
                    let _ = me.channel.clear_buttons(&inb.conversation, &MsgRef(msg_id)).await;
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
        self.reply(conversation, text).await
    }

    async fn reply(&self, conversation: &str, text: &str) -> Result<(), ChannelError> {
        let view = View {
            shard: 0,
            is_last: true,
            blocks: vec![Block::Paragraph(text.to_owned())],
            status: None,
            buttons: Vec::new(),
        };
        self.channel.deliver(conversation, view).await.map(|_| ())
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
            crate::policy::Verdict::Ask => {}
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
        let card = self
            .channel
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
                let _ = self
                    .channel
                    .clear_prompt(&chat, &card, "⏱ 审批超时，已拒绝执行。")
                    .await;
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{attachment_lines, summarize_args};

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
