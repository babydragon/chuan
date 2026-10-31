# 契约

> 替换型：**Channel / Store / Retriever / TurnDriver**；组合型：**Tool / ExecutionEnv / AgentSpawner**；内核内策略：**Renderer**。
> 数据模型 `DATA-MODEL.md`，运维 `OPS.md`，术语 `GLOSSARY.md`。

---

## 0. 总则

- 四个替换型各配一致性测试；Tool / ExecutionEnv 靠 schema 与单测；AgentSpawner 是组合型
  （内核自己实现，工具只依赖 trait），靠"父上下文只增结论 / 禁递归"的集成测试。
- 必须有 `EphemeralStore` 与 `EchoChannel` 两个桩，逼接口中立。
- 依赖守卫：`kernel` 依赖树中不得出现 `reqwest`/`teloxide-core`/`rusqlite`/`rmcp`/`image`。

---

## 1. Channel

```rust
pub struct Caps {
    pub edit: bool, pub buttons: bool, pub markdown: bool,
    pub max_text: usize, pub media: MediaCaps,
    pub min_edit_interval_ms: u32,     // TG 2500：1s 持续约 3 分钟会被洪水控制（板上实测 retry_after 169–181s）
}

pub enum EditOutcome { Edited, NotModified, TooOld, Rejected { retry_after_ms: u32 } }

#[async_trait]
pub trait Channel: Send + Sync + 'static {
    fn id(&self) -> ChannelId;
    fn caps(&self) -> Caps;
    async fn run(&self, tx: mpsc::Sender<Inbound>, cancel: CancellationToken) -> Result<()>;
    async fn deliver(&self, conversation: &str, view: View) -> Result<MsgRef>;
    async fn update(&self, conversation: &str, msg: &MsgRef, view: View) -> Result<EditOutcome>;
    async fn ack(&self, callback: &str, text: &str) -> Result<()>;
    async fn download(&self, f: &FileRef) -> Result<Media>;

    /// 给**入站消息**挂一个状态标记（表情反应）。
    /// 通道没有该能力时什么都不做——默认实现即"无能力"。
    /// 语义是 best-effort：失败只记 trace，不影响 run 成败，也不得阻塞。
    async fn react(&self, _conversation: &str, _msg: &MsgRef, _emoji: &str) -> Result<()> {
        Ok(())
    }
}
```

0. **一个通道实现 = 一条通道**（`id()` 唯一）。多通道并存时，actor 持一张
   `ChannelId → Arc<dyn Channel>` 表，按 `Inbound.channel` 路由；**会话键是 `{channel}:{addr}`**，
   所以"同一个人在 TG 和微信"是两个互不相干的会话（锁、`/cancel`、`/status` 都按这个键）。
1. 通道只收 `View`，不拼字符串；排版/分片/转义在 Renderer。
2. `ActorRef ≠ 人`；跨通道合并靠应用层。
3. `NotModified` 当成功、`TooOld` 让 Renderer 换新消息、`Rejected` 全局退避。
   **`update` 的 429 必须返回 `Rejected`，不得上抛 `Err`**：上抛会终止整轮 run（板上事故：43s 的洪水控制把 run 打死，界面停在"正在输入"）。
   `typing` 收到 429 同样按 `retry_after` 退避，否则打字提示每 4s 一次会把洪水控制一直续上。

### 1.1 `react` 与 `update` 的分工

| | `update` | `react` |
|---|---|---|
| 改的是 | **自己发的**消息 | **对方发的**消息 |
| 受 `Caps.edit` 约束 | 是 | 否（与 `Caps` 无关） |
| 失败语义 | `Rejected` → 渲染器退避 / `TooOld` → 换新消息 | 只记 trace，**不重试、不上抛** |
| 用途 | 流式呈现同一轮回复 | 标记"这条入站消息处理到哪一步" |

`Inbound.msg` 是平台侧消息句柄；`None` 表示这条入站不是"平台上真实存在的一条"
（定时任务合成、内部派发、polish），此时 `react` 必然 no-op。
状态映射（未开始/处理中/成功/失败）属于应用策略，见 `DECISIONS.md` D24 与 `ROADMAP.md §5.11`。

### 1.2 接入新通道：状态反应怎么做

以后接别的通道（飞书、Slack、Discord…）时，照这一节判断与落地，不要重新设计。

**判据**：平台有没有"给**已经发出去的那条**消息挂一个标记"的接口？

| 情况 | 做法 |
|---|---|
| 有（Telegram `setMessageReaction`、Slack `reactions.add`…） | 实现 `react()` |
| 没有（微信 iLink） | **什么都不写**。默认实现就是正确行为，不要退化成"再发一条消息" |

**两个前提**（缺一个就别做）：

1. 入站消息带平台 id —— 在 `run()` 里把它填进 `Inbound.msg`（`None` 就别实现 `react`）
2. 平台能按消息 id **修改**，而不是只能新增

**实现契约**（四条，从 Telegram 实现里抽出来的）：

| 要求 | 为什么 |
|---|---|
| 平台侧必须是**替换**语义，不是叠加 | 状态机会反复打状态；叠加会变成一串表情 |
| 空 `emoji` 表示**清空** | 取消既不是成功也不是失败（D24） |
| 失败一律吞掉（记 trace 即止） | best-effort：状态是附加信息，不能反过来决定 run 成败 |
| 不得阻塞 turn | 一次 reaction = 一次额外网络往返 |

**已知平台差异**：

| 平台 | 能力 | 约束 |
|---|---|---|
| Telegram | ✅ `setMessageReaction` | 每 bot 每条消息**只能一个**；emoji 只有**73 个白名单**，`⏳/✅/❌` 会被 `400 REACTION_INVALID` 拒；服务类消息不能加；实测返回值是 `true` 不是消息对象 |
| 微信 iLink | ❌ 无接口 | 只能发消息，而每条出站占 10 条额度中的 1 条 → 明确不做（`WEIXIN.md` F14） |
| 飞书 / 钉钉 / Slack | ⚠️ 未核 | 各自有 reaction API，但要先确认"bot 能否给**用户发的**消息加"，以及是否有 emoji 白名单 |

**实现索引**（照抄即可）：

| 位置 | 职责 |
|---|---|
| `harness-kernel/src/channel.rs` | `Channel::react`（默认空实现）、`Inbound.msg` |
| `harness-kernel/src/actor.rs` | `RunEnd` / `mark()` / `handle_run` 包装 / `run()` 里的在途计数器 |
| `telegram/api.rs::set_reaction` → `http.rs` → `mod.rs::react` | 三层：协议方法 → HTTP（空串转 `reaction: []`）→ 通道 |
| `tests/runner_flow.rs::FakeChannel.reactions` | 测试替身记录 `(msg_id, emoji)` |

**新增通道的验收**（5 条，缺一不算接完；括号里是 Telegram 侧对应的用例，可照着改）：

| # | 验收 | 参考用例 |
|---|---|---|
| 1 | 连发 3 条 → 第 2、3 条先 `😐`，每条都经过 `👀` | `reactions_track_queue_then_success` |
| 2 | 每条最后落到 `👍` | 同上 |
| 3 | 跑失败 → `👎` | `failed_run_marks_fail` |
| 4 | 取消 → **清空**（不是 `👎`） | `cancel_clears_reaction` |
| 5 | 合成入站（`msg=None`）零反应 | `synthesized_inbound_has_no_reaction` |
| 6 | reaction 接口报错时，正常回复照常送达 | `reaction_failure_does_not_break_run` |

---

### 1.3 审批卡（面向非程序员）

审批卡是**给不看代码的人**判断用的，所以必须由模型自己解释，不能只贴命令行。

模型在需要审批的工具调用里带一个保留字段（**执行前由内核剥掉，不传给工具**）：

```json
"_approval": {
  "why":    "用 lpstat 查一下打印机在不在线（要走内网）",
  "impact": "只读：只查询状态，不改任何配置，也不会打印"
}
```

卡片渲染（三段，顺序固定）：

```
🔐 需要你确认

{why}                     ← 模型的一句话；缺失时显示「（模型未说明）」
影响：{impact}             ← 只读 / 写文件 / 装软件 / 向设备发指令 / 改自身
工具：{tool}
命令：{brief}              ← 参数摘要，≤ 200 字，超出截断

回复 1 = 允许    回复 2 = 拒绝
（30 分钟内有效）
```

**短码审批**（无按钮通道，如微信）：

- 同一会话同一时刻只有一条待审批（run 持会话锁，审批门串行）；
- 收到**完全等于** `1` / `2` 的文本 → 直接作为该条的允许/拒绝，**不跑模型**；
- 没有待审批时，`1` / `2` 是普通消息；审批超时后短码自动失效；
- `/approve <id>` 保留为精确路径（Telegram 按钮、日志里复制用）。

理由见 `DECISIONS.md` D28。

---

## 2. Store

```rust
#[async_trait]
pub trait Store: Send + Sync + 'static {
    async fn begin_write(&self) -> Result<Box<dyn Tx>, StoreError>;
    async fn begin_read(&self)  -> Result<Box<dyn ReadTx>, StoreError>;
    async fn migrate(&self) -> Result<MigrationReport, StoreError>;
}

#[async_trait]
pub trait ReadTx: Send {
    async fn view(&mut self, conv: ConversationId) -> Result<ConversationView, StoreError>;
    async fn messages(&mut self, conv: ConversationId, from: Seq, max_turns: u16)
        -> Result<MessagePage, StoreError>;          // 语义见 DATA-MODEL.md §5
    async fn get_doc(&mut self, id: DocId) -> Result<Option<Doc>, StoreError>;
    async fn list_docs(&mut self, q: DocQuery) -> Result<Vec<Doc>, StoreError>;
    async fn usage_total(&mut self, since: Timestamp) -> Result<UsageTotal, StoreError>;
    async fn task(&mut self, id: TaskId) -> Result<Option<TaskRecord>, StoreError>;
    /// 恢复用：该会话全部工具调用及其状态（intent/dispatched/finished/abandoned）。
    async fn tool_calls(&mut self, conv: ConversationId) -> Result<Vec<ToolCallRow>, StoreError>;
    async fn rollback(self: Box<Self>);
}

#[async_trait]
pub trait Tx: Send {
    async fn append(&mut self, ev: NewEvent) -> Result<Seq, StoreError>;      // seq 由本事务分配
    async fn append_many(&mut self, evs: Vec<NewEvent>) -> Result<Vec<Seq>, StoreError>;

    async fn open_tool_calls(
        &mut self, conv: ConversationId, run: RunId, turn: Option<TurnId>,
        calls: Vec<NewToolCall>,
    ) -> Result<(), StoreError>;
    async fn mark_dispatched(&mut self, id: CallId) -> Result<(), StoreError>;
    async fn close_tool_call(&mut self, id: CallId, outcome: ToolOutcome) -> Result<(), StoreError>;
    async fn abandon_tool_call(&mut self, id: CallId, reason: AbandonReason) -> Result<(), StoreError>;

    async fn create_approval(&mut self, a: NewApproval) -> Result<ApprovalId, StoreError>;
    async fn attach_approval_card(&mut self, id: ApprovalId, card: MsgRef) -> Result<(), StoreError>;
    async fn resolve_approval(&mut self, id: ApprovalId, d: Decision) -> Result<Resolve, StoreError>;
    async fn expire_approvals(&mut self, now: Timestamp) -> Result<u32, StoreError>;   // 同事务追加事件

    async fn enqueue_task(&mut self, t: NewTask) -> Result<TaskId, StoreError>;
    async fn lease_task(&mut self, now: Timestamp, ttl_ms: u64) -> Result<Option<Leased>, StoreError>;
    async fn finish_task(&mut self, id: TaskId, r: TaskResult) -> Result<(), StoreError>;
    async fn cancel_task(&mut self, id: TaskId, reason: CancelReason) -> Result<(), StoreError>;

    // 归属必须随写操作传入：doc/usage 事件要落在正确的会话与 run 上，否则 rebuild 无法还原。
    async fn upsert_doc(&mut self, conv: ConversationId, run: RunId, doc: Doc) -> Result<(), StoreError>;
    async fn record_usage(&mut self, conv: ConversationId, run: RunId, u: UsageRecord) -> Result<(), StoreError>;

    async fn commit(self: Box<Self>) -> Result<(), StoreError>;
}
```

### 2.1 三段式执行（无长事务）

| 阶段 | 事务 | 内容 |
|---|---|---|
| ① | 短写 | `turn.started` + `msg.assistant` + `tool.call_started`（意图） |
| ② | 无事务 | 审批（卡片另开短事务 `attach_approval_card`）→ `mark_dispatched`（短事务）→ 执行工具（≤60s） |
| ③ | 短写 | `tool.call_finished` + `doc.upserted` + `artifact.created` + `msg.tool_result` |

- 工具不持事务；副作用通过 `ToolOutput.doc_writes` 声明。
- **tx3 可按 `call_id` 幂等重试**：`doc_writes` 以 `(doc_id, version)` 去重，`artifact.created` 以 sha256 去重。
- tx3 永久失败 → 进降级，账本保持 open call，重启按 `call_dispatched` 规则处理。
- 取消/超时用一个事务收尾：`approval.resolved(Deny|Expire)` + `tool.call_abandoned(Cancelled|ApprovalExpired)` + `turn.finished` + `run.finished(cancelled)`。

### 2.2 硬规则

1. 写事务内禁止：执行工具、调 LLM、发消息、等审批。
2. 写队列有界；满时 `StoreError::Busy{retry_after_ms}`，内核退避，不丢事件。
3. CAS 在 trait 内：`lease_task` / `resolve_approval`。
4. `seq` 会话内单调唯一（分配规则见 `DATA-MODEL.md §2.1`）。
5. 组装一轮上下文用**同一个 ReadTx**，保证快照一致。
6. SQLite 后端在同一写事务内维护 `docs_fts`。
7. `messages(conv, from, max_turns)`：边界是 `run.started` 或 `turn.started`，`from` 对齐到首个边界（落在半轮则回退到该轮开头）；`run.started` 映射为 `Role::User`；`next` 指向下一轮的 `turn.started.seq`。

---

## 3. Retriever

```rust
#[async_trait]
pub trait Retriever: Send + Sync + 'static {
    fn id(&self) -> RetrieverId;
    async fn search(&self, q: Query) -> Result<Vec<Hit>, RetrieverError>;
    async fn health(&self) -> Health;
    /// 可选：仅外部索引器（向量）实现；FTS5 由 Store 事务内维护
    async fn index(&self, _refs: &[DocRef]) -> Result<(), RetrieverError> { Ok(()) }
}

pub struct Query {
    pub text: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub scopes: Vec<Scope>,
    pub kinds: Vec<DocKind>,
    pub k: usize,
    pub overfetch: usize,        // 建议 4*k
}
```

1. FTS5 = Store 的投影（写事务内维护），`search` 只读。
2. 向量 = 后台任务异步维护，走 `index()`，复用 Store 写线程句柄，**不开第二个连接**。
3. ACL 两条断言：**无越权**、**域内不丢**；实现取 `LIMIT (k+overfetch)` 后在 Rust 过滤截断。
4. 混合检索 RRF 融合在 Rust 里做；向量触发条件与选型事实见 §3.4。

### 3.4 要不要向量

现在不做。触发条件：语料数千条以上 / 查语义改写或跨语言 / 需要模糊相似。

| 方案 | 形态 | 事实 | 定位 |
|---|---|---|---|
| `sqlite-vec` 0.1.9 | SQLite 扩展 | `cc` 静态编译 + `auto_extension`，与 `rusqlite(bundled)` 共存；**暴力扫描**；~10 万级；纯 C，SIMD 可选 | 家庭/中小 KB |
| `libsql` 0.10 | SQLite fork | 原生 DiskANN；捆绑 C 引擎 | 百万级且愿换引擎 |
| `pgvector` 0.8.7 | PG 扩展 | HNSW/IVFFlat；Debian trixie 有 riscv64 包 | 高 QPS |

SBC 注意：暴力扫描成本随维度线性增长（3072d 比 128d 慢两个数量级）；真要上优先 256–512 维或 int8 量化。

---

## 4. TurnDriver / Turn 与事件流

```rust
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted { call_id: CallId, name: String },   // 参数由 engine 聚合完整后交付
    ToolFinished { call_id: CallId, ok: bool },
}

pub struct EventSink { tx: mpsc::Sender<StreamEvent>, dropped: Arc<AtomicU64> }
impl EventSink {
    /// 非阻塞；通道满时丢弃并计数。View 是全量快照，丢事件只影响一帧新鲜度。
    pub fn emit(&self, ev: StreamEvent) { if self.tx.try_send(ev).is_err() { self.dropped.fetch_add(1, Relaxed); } }
    pub fn dropped(&self) -> u64;
}

#[async_trait]
pub trait TurnDriver: Send + Sync + 'static {
    async fn begin(&self, spec: TurnSpec, history: &[Event], tools: &[ToolSpec], sink: EventSink)
        -> Result<Box<dyn Turn>, EngineError>;
}

#[async_trait]
pub trait Turn: Send {
    async fn step(&mut self) -> Result<Step, EngineError>;    // 发起一次模型请求并等待完成
    /// 只记录工具结果；推进由下一次 `step()` 负责。
    async fn on_tool_result(&mut self, call: CallId, ok: bool, preview: String)
        -> Result<(), EngineError>;
    fn canceller(&self) -> CancellationToken;
    fn usage(&self) -> UsageSnapshot;
}

pub enum Step {
    Model { turn_no: u32 },
    Tools { calls: Vec<ToolCallRequest> },
    Done  { text: String, finish: FinishReason },
}
```

- **不通过 sink 传 tool 参数分片**：engine 内部聚合完整参数后才 `ToolStarted`，避免"丢一片参数就坏"。
**engine 内部分四层，只有 `wire` 与协议相关**：

```
transport  字节搬运（SseTransport / SseBytes），不认识协议
   ↓
wire       协议编解码（Wire / WireConversation / WireDelta）
   ↓
stream     中立聚合（文本、推理、工具参数拼接、usage）
   ↓
driver     Turn 驱动（调 wire、消费 stream、产出 Step）
```

- `WireDelta`：`Text | Reasoning | ToolCall{index,id,name,args} | Finish | Usage`。`args` 既可是分片（OpenAI/Anthropic），也可是完整 JSON（Gemini）。
- `Wire::decode(payload) -> Vec<WireDelta>`；`Wire::conversation(history, tools) -> Box<dyn WireConversation>`。
- `WireConversation`：`encode(model, tools) -> Vec<u8>` / `on_assistant` / `on_tool_result`。消息模型、system 位置、tool_result 形状都归它管。
- **加一个新协议 = 新写一个 `Wire` 实现**；聚合器、传输、driver、内核一行不动。
- P0 只实现 `OpenAiChat`（覆盖 DeepSeek/OpenRouter/Ollama/Kimi/智谱 等兼容面）。Gemini / Anthropic 原生协议、OpenAI Responses 都留待需要时再加。
- **工具调用标识**：`CallId` 直接采用 provider 的 `tool_calls[].id`（缺失才合成 `call_<index>`），内核不再维护第二套 id 映射。
- `emit` 用 `try_send`，不会背压阻塞 loop。
- 取消：`canceller()` 克隆后 `cancel()`；之后 `step()` 返回 `Cancelled`，半截文本丢弃。

---

## 5. Renderer

```rust
pub enum RenderEvent { Stream(StreamEvent), Status(String), ToolRetry { call_id: CallId } }

pub enum RenderAction { None, Edit(View), Send(View) }

pub trait Renderer: Send {
    fn push(&mut self, ev: RenderEvent);
    fn flush(&mut self, now: Timestamp) -> RenderAction;
    fn on_edit_result(&mut self, r: EditOutcome);
    /// 发送失败（如 429）：重新置起待发送，并在此窗口内不再碰通道。
    fn on_send_failed(&mut self, retry_after_ms: u32);
    /// 通道成功发出新消息后回填引用，后续才能原地编辑。
    fn on_sent(&mut self, msg: MsgRef);
    /// 用权威终稿替换累积文本（流式 delta 与最终文本可能不一致）。
    fn replace_text(&mut self, text: String);
    /// 定稿可能分多条，因此返回动作列表。
    fn finish(&mut self, status: RunStatus) -> Vec<RenderAction>;
}
```

- **View 是全量快照**（不是增量），所以丢事件只让某一帧稍旧，不会渲染错乱。
- **驱动方**：`ConversationActor` 持 `tokio::time::interval(caps.min_edit_interval_ms)`，每 tick 调 `flush(now)`；工具静默期也能刷新"调用中"。
- `TooOld` → `on_edit_result` 让 Renderer 下次返回 `Send`。
- **内容未变不发 edit**：快照与上次成功发送的一致时 `flush` 返回 `None`。长任务里模型思考/等工具时快照是静止的，每秒一次编辑会点着 Telegram 的洪水控制（429 + 数十秒退避）。
- 429 不是失败：`Rejected` 只推迟下一拍（`last_flush += retry_after_ms`）并让同一份快照稍后重发；进度刷新被限流时 run 继续跑。
- 发送（`deliver`）被限流：`on_send_failed` 记下 `send_after`，窗口内一次都不发，恢复后自动补发。
- 定稿（`finish`）被限流：run 已结束，按 `retry_after` 等窗口后重发，最多 3 次、单次上限 60s；仍失败只记日志，不改变 run 结论。
- 分片：`View.shard` / `View.is_last`；**只有最后一片参与后续 edit**，按钮只挂最后一片。
- 降级：`caps.markdown == false` → 纯文本；`caps.edit == false` → 只在 `finish` 时 `Send`。
- **格式化归通道**：Renderer 只产出 `Block`，Telegram 通道负责 `markdown-lite → HTML`（`DECISIONS.md` D13）；长文本先按原始文本分片再逐片格式化，保证每片标签天然闭合。

```rust
pub struct View { pub shard: u16, pub is_last: bool,
                  pub blocks: Vec<Block>, pub status: Option<String>,
                  pub buttons: Vec<Vec<Button>> }

pub enum Block {
    Paragraph(String),
    Code { lang: Option<String>, text: String },
    Image { uri: String, alt: String },
    File { path: String, name: String, mime: String },
    Divider,
    Table { head: Vec<String>, rows: Vec<Vec<String>> },
    ToolCall { call_id: CallId, name: String, state: ToolState, preview: String },
}
```

---

### 5.1 出站文件

工具不能直接拿整个 Channel，只能通过 `ToolCtx.outbox` 往**当前会话**发一个文件：

```rust
#[async_trait]
pub trait Outbox: Send + Sync {
    async fn send_file(&self, path: &str, name: &str, mime: &str, caption: &str) -> Result<(), String>;
}
```

- 端到端实现：`send_file` 工具 → `Outbox` → `Channel::send_file`（默认降级成文本，Telegram 用 `sendDocument`/`sendPhoto`）；
- 只允许发 `/ws` 内的文件（`/media` 里的先 `cp` 到 `/ws`），单文件 50MB；
- 定时任务里的工具调用拿 `NullOutbox`，调用会明确报错而不是静默丢弃。

---

## 6. Tool / ExecutionEnv / AgentSpawner

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn effect(&self) -> EffectClass;
    /// 工具档位：0 通用（子 agent 可用）/ 1 仅主会话
    fn tier(&self) -> u8 { 0 }
    /// 并发车道：默认 io；技能/装包类声明 compile
    fn lane(&self) -> Lane { Lane::Io }
    async fn execute(&self, args: Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, ToolError>;
}

pub struct ToolCtx<'a> {
    pub conv: ConversationId,
    pub actor: &'a str,
    pub run: RunId, pub call: CallId,
    pub workspace: &'a Workspace,
    pub env: &'a dyn ExecutionEnv,
    pub outbox: &'a dyn Outbox,
    pub docs: &'a dyn DocReader,
    /// 本 run 的 system 指令（子 agent 复用以命中前缀缓存）
    pub system_prompt: &'a str,
    /// 只有主会话 run 注入；`None` = 不能派生（子 agent / 定时任务）
    pub spawner: Option<&'a dyn AgentSpawner>,
    pub cancel: CancellationToken,
}

pub struct ToolOutput {
    pub content: Vec<Block>,
    pub doc_writes: Vec<DocWrite>,        // 内核在 tx3 统一提交
    pub artifacts: Vec<ArtifactWrite>,
}
```

审批轴是**执行域**（`Tool::domain()`），不是命令黑名单——模型会写脚本走 `exec`，
黑名单追不上；而 `exec` 在沙盒里本来就出不去：

| 域 | 判定依据 | 默认 |
|---|---|---|
| `Sandbox` | /ws 可写、宿主不可见、无网络 | `allow`（问也没用） |
| `SelfModify` | 装/回滚技能、改配置、加服务、排定时任务 | `ask` |
| `Outbound` | 出网：`web_fetch` / `http` / `mcp` | `allow`（白名单未做，靠审计） |
| `Lan` | 内网调试 lane（有网卡） | `ask` |
| `Install` | 装依赖（pip/npm，供应链） | `ask` |
| `Host` | 宿主执行（L1，服务账号身份，路径白名单） | `ask` |
| `HostElevated` | 宿主提权（L2，经 `chuan hostd`，argv 授权表） | `deny` |
| `HostObserve` | 宿主只读观测（L3：`/proc`、`/sys`、`systemctl status`） | `allow` |

`[policy] require_approval` 只能把 `allow` 提成 `ask`，不能放开 `deny`。
`host` / `host_elevated` 是 L1 / L2 的**天花板**：置 `deny` 时授权表里所有条目一并失效。
旧键 `host_exec` 从未实现过（只有枚举与配置位），直接改名为上面两个；读到旧键打 warning 并忽略。

内置工具的副作用等级（P0 验收要用）：

| 工具 | effect | 说明 |
|---|---|---|
| `read` / `list` | `Pure` | 只读 |
| `write` | `Idempotent { key: hash(path, content) }` | 同内容重写无害 |
| `exec` | `External` | 任意命令，**默认不重跑**；技能可在 manifest 声明更弱的等级 |
| `remind` | `External` | 建/改/取消定时任务；`update`/`del` 不在幂等集合里，崩溃后不重放 |
| `reminders` | `Pure` | 只读列出/查看定时任务（含技能参数） |
| `remember` | `Idempotent { key: hash(subject, predicate) }` | upsert 语义 |
| `run_skill` | `External` | 脚本可能有副作用，失败不自动重发 |
| `publish_skill` / `rollback_skill` | `External` | 改技能目录属于"改自身"，重放无意义 |
| ~~`print`~~ | — | **不做工具**：打印/查询由技能实现，内核只提供 `[sandbox] mounts` |
| `web_fetch` | `Pure` | 只读外部数据 |
| `http` / `mcp` / `service` | `External` | 可能改远端状态；`service` 还改本地配置 |
| `send_file` | `External` | 发消息，失败不重发 |
| `keep` | `Idempotent { key: "keep" }` | 内容寻址，重复保存等价 |
| `files` | `External` | 导出/删除有副作用 |
| `install_deps` | `External` | 拉外部代码、改运行时 |
| `host` | `External` | L1 宿主执行；命令任意，**默认不重跑** |
| `sys` | `Pure` | L3 只读 `/proc` / `/sys` |
| `capability` | `Idempotent { key: hash(request) }` | 只写草稿文件并通知；不改任何权限 |
| `playbook` | `Pure` | 只读文本 |

域声明：`publish_skill` / `rollback_skill` = `SelfModify`，其余内置工具 = `Sandbox`
（`web_fetch` = `Outbound`；`host` = `Host`、`hostd` 通道 = `HostElevated`、`sys` = `HostObserve`；
`capability` 只写草稿、不改变权限，按 `Sandbox` 域免问；未来的 `send` = `SelfModify`；
打印等由技能实现，不占域）。

```rust
#[async_trait]
pub trait ExecutionEnv: Send + Sync {
    async fn exec(&self, argv: &[String], opts: ExecOpts, cancel: CancellationToken) -> Result<ExecOutput, EnvError>;
    async fn read(&self, path: &str) -> Result<Vec<u8>, EnvError>;
    async fn write(&self, path: &str, data: &[u8]) -> Result<(), EnvError>;
    async fn list(&self, path: &str) -> Result<Vec<Entry>, EnvError>;
}
```

### 6.1 Workspace

- **P0**：进程内**共享一个 workspace**，单可写根 `/ws`（`--bind <workspace> /ws`，已实测）；按 run 隔离留到 P4（`ExecutionEnv` 的挂载根需与 `Workspace.root` 一致）。
- `in/out/scratch` 三段式列 P2，需重测挂载集。
- 路径 `/var/lib/chuan/workspaces/<run_id>/`；恢复复用同一目录，新 run 用新目录。

### 6.2 出网

沙盒的默认 lane 永不联网；外部数据有两条路：宿主 `web_fetch` 代取（拒绝内网/回环、逐次审计），
或技能声明 `network: true` 走独立联网 lane（`lan_exec` 同款）。凭据由宿主注入，不进上下文。

### 6.3 子 agent（委托）

契约见 `DECISIONS.md` D19（形态与约束）、D20（记账与闸门）。内核只提供**派生与回传**，不做调度策略。

```rust
pub struct AgentSpec {
    pub task: String,
    pub profile: AgentProfile,        // 角色：决定提示词、模型与工具子集
    pub inputs: Vec<String>,          // /ws 下的文件路径，只传引用不传内容
    pub max_turns: u32, pub max_tokens: u64, pub wall_ms: u64,   // 0 = 用 [agents] 默认值
    pub parent_run: RunId,
    pub depth: u8,                    // 恒为 1；>1 直接拒绝
    /// 接手上一次：任务号 `task-…` 或子会话 `sub:…#n`。内核解析成一段"上次进展"简报
    /// （结论 + 产物路径，≤1600 字）注进子 agent，不重放历史。
    pub continue_from: Option<String>,
    /// 预分配的子会话 id（后台任务入队时定），只能在本父会话名下使用。
    pub child_conv: Option<String>,
}

pub struct AgentProfile {
    pub name: String,                 // "inherit" | [agents.roles] 里的角色名
    pub model: Option<String>,        // None = 角色或 [engine]
    pub tools: Option<Vec<String>>,   // 只能更窄（白名单 ∩ 角色 ∩ 这里）
}

pub struct RoleSpec {                 // [agents.roles.<name>] / roles_dir/<name>.toml
    pub description: Option<String>,  // 注入父 system 的角色清单
    pub system_prompt: Option<String>,// 追加在父前缀之后
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,   // 只能收窄
    pub max_turns: u32, pub max_tokens: u64, pub wall_ms: u64,  // 0 = 跟随 [agents]
}

pub enum AgentStatus { Done, Budget, Timeout, Failed, Cancelled }

pub struct Conclusion {
    pub status: AgentStatus,
    pub summary: String,              // ≤2000 字符，按字符截断；全文在 child_conv 的账本里
    pub child_conv: String,           // sub:<parent_conv>#<n>
    pub child_run: String,
    pub artifacts: Vec<String>,       // /ws 下的产物路径（P4.1 暂为空，路径写在 summary 里）
    pub pending_actions: Vec<String>, // 需要父会话完成的动作（子 agent 无权做）
    pub usage: UsageSnapshot,         // P4.1 为空，归属统计随 P4.2 落地
}

/// 后台任务的状态快照（`task` 工具与 `/tasks` 共用）
pub struct TaskInfo {
    pub id: String, pub status: String, pub text: String,
    pub created_ms: i64, pub started_ms: Option<i64>,
    pub child_conv: Option<String>, pub result: Option<String>,
}

/// 角色来源：每次调用重新读盘，所以 `role` 工具写完即刻生效。
pub trait RoleSource: Send + Sync {
    fn all(&self) -> Vec<(String, RoleSpec)>;
}

#[async_trait]
pub trait AgentSpawner: Send + Sync {
    async fn spawn(&self, spec: AgentSpec, cancel: CancellationToken)
        -> Result<Conclusion, AgentError>;
    /// 后台任务（P4.3）：立刻返回任务号；结果之后推回原会话。
    async fn enqueue(&self, spec: AgentSpec) -> Result<String, AgentError> {
        Err(AgentError::Other("当前实现不支持后台任务".into()))
    }
    /// 父 agent 管理任务用（`task` 工具）；不要拿来做进度轮询。
    async fn tasks(&self) -> Result<Vec<TaskInfo>, AgentError>;
    async fn cancel_task(&self, id: &str) -> Result<String, AgentError>;
}
```

- `ToolCtx.spawner: Option<&'a dyn AgentSpawner>`：**只有主会话 run 注入**。
  `None` 表示该 run 不可派生——子 agent 自身与定时任务即此形态（结构性禁递归）。
- `Tool::tier() -> u8`（`0` 通用 / `1` 父级专用；见 `tool.rs`）。**不是 `ToolSpec` 字段**——
  `ToolSpec` 会原样进 wire，档位不能漏给模型。`ToolRegistry::specs_for(tier)` 按 `(tier, name)`
  排序后取**前缀**；只削尾部才不破坏 prompt cache。
- `Tool::lane() -> Lane`（`io` / `compile` / `external` / `agent`）。默认 `io`；
  `run_skill` / `publish_skill` / `install_deps` 声明 `compile`（单槽，防编译饿死 TG）。
  模型请求恒走 `external`（单槽），由 Runner 统一加锁。
- `delegate` 工具恒为 `tier = 1`、effect `External`（子执行可能有副作用，崩溃不重跑）；
  参数 `role` 只能点名配置里或 `roles_dir` 里的角色，未知角色 → `AgentError::Other("未知角色…")`。
- `role` 工具（`tier=1`、`SelfModify`）：`list` / `add` / `remove`，写 `roles_dir/<name>.{toml,md}`。
  提示词存独立 `.md`（不做 TOML 转义），注册后由 `RoleSource` 热加载。
- 结论按三节约定：`结论` / `产物` / `[待父执行]`。内核用 `split_pending()` 把最后一节
  切成 `pending_actions`（每行 `- ` 开头，最多 8 条，`- 无` 视为空），正文里不再重复；
  `render()` 会单列一节交给父模型。**这是"子 agent 没有审批权"的补偿路径**：
  需要人确认的动作（发布技能、装依赖、发消息、打印）由父会话在本轮完成。
- 子 run 写自己的 conversation（`sub:<parent_conv>#<n>`）与 journal；父只写
  `agent.spawned` / `agent.concluded` + 一条 `msg.tool_result`。
- 子 run 不装配任何 `ask` 域工具：命中即 `ToolError`，**不进入审批流程**（子 agent 没有通道）。
- 取消沿 `CancellationToken` 向下传；父崩溃 → 子 `run.finished(interrupted)`，重启不重放。
- 后台任务由 `task` 工具（`tier=1`）暴露给父 agent：`list` / `status` / `cancel`。
  前台委托是阻塞的，模型在等结果时无法调用工具，所以"管理前台"只能靠 `/cancel` 或 `wall_ms`。
- 后台任务落 doc `kind="agent_task"` / `scope="task"`：payload 带 `text/role/inputs/status/conv/chat/attempts`，
  由 actor 的 5s tick 领取（提醒仍 30s）；`running` 在重启后标 `interrupted` 并通知，**不自动重跑**（D4）。

---

### 6.4 宿主执行（L1 / L2 / L3）

设计理由见 `DECISIONS.md` D22。这里只写契约。

| 边界 | 实现 | 关键参数 |
|---|---|---|
| L1 `Host` | `BwrapEnv` 的宿主变体 | 去掉 `--unshare-pid` 与 `--unshare-net`；**`--ro-bind /proc /proc`**（不能沿用 `--proc /proc`，那是新 procfs，看不到宿主进程）；路径按 `[host.path]` 逐条挂；`env -i` |
| L2 `HostElevated` | 独立单元 `chuan hostd`（`User=root`） | unix socket `/run/chuan/host.sock`（`0660 root:chuan`）；`SO_PEERCRED` 校验对端 uid；只接受 JSON 请求；`env -i` |
| L3 `HostObserve` | 进程内读文件 | `/proc`、`/sys`；零子进程，不占 lane |

**授权表**（两份，均为数据；按 mtime 热加载，解析失败 fail-closed）：

```toml
[host]
net = "host"                     # 共享宿主 netns
[host.path]
"/usr"  = { mode = "ro" }                        # 免问
"/home" = { mode = "ro", approval = "once" }     # 默认不挂；每次在 grant[] 里点名（只读）
"/var/log/journal" = { mode = "ro" }
"/run/systemd/system" = { mode = "ro" }   # sd_booted()
"/run/dbus" = { mode = "ro" }
"/var/lib/chuan/timers" = { mode = "rw", approval = "always" }

[[host_elevated.grant]]
argv = ["/usr/bin/apt-get", "install"]   # 前缀匹配，无 shell
approval = "once"
snapshot = "dpkg"
```

**三档 scope**（实现上只有两个来源：调用参数，或授权表）：

| scope | 来源 | 权限 | 生命周期 |
|---|---|---|---|
| 本次调用（临时挂载） | `host(grant=[…])` | **只 ro**，≤ 8 条 | 一次 exec，不落盘 |
| 本对话 | 审批卡勾选「本对话有效」 | ro | 1800s，重启失效 |
| 持久 | `[host.path]`，缺省 `always` | ro / rw | `always` 默认挂载且免问；`once` 默认不挂、点名才给（只读） |

**结构化拒绝**（模型据此写申请单；申请单第 2 节直接抄这里的字段）：

```rust
EnvError::NotGranted { kind: Path | Argv | Root, item: String, hint: String }
```

```
permission denied: /opt/foo 不在 [host.path] 里
已授权（ro）：/usr /lib /etc /proc /sys
需要：path = "/opt/foo"  mode = "ro"
下一步：capability(kind="host_path", item="/opt/foo", mode="ro", why="…")
```

**工具面**：

- `host`（`tier=1`，`Host` 域）：`argv | script`、`timeout_ms`、`grant[]`。
  命中 `approval="once"` 的条目或带 `grant[]` 时，审批卡必须列出：规范化路径 / argv 前缀、
  第几条授权、模型给的理由；目录类授权额外显示**文件数与总大小**（人靠这个分辨"一个文件"和"整个家目录"）。
- `sys`（`tier=1`，`HostObserve`）：`metric`（`load` / `mem` / `stat` / `psi` / `cgroup` / `units` / `journal`）+ 可选 `pid`。
  `journal` 走只读宿主 exec（`journalctl`），需要 `systemd-journal` 组。
- `capability`（`tier=1`）：`list` / `check` / `request`。**只有 `request` 落盘，且只写草稿**：
  `capabilities/<ts>-<slug>.md`，同时回一条消息。**不修改任何授权表**（D22 已定：暂时不自动生效）。

**申请单 8 节**（缺节拒绝写入，防止模型糊一段话）：现场 / 最小请求 / 必要性 / 可见范围 /
更小选项 / 副作用 / 回滚 / 自验。SOP 五条硬规则（先 `list`、一次只申请一类、同一失败只申请一次、
申请后停下、改完自验）随二进制发布为 `playbook("capability")`，可用
`~/.config/chuan/playbooks/capability.md` 覆盖。

### 6.5 `publish_playbook`（未实现，D23）

```rust
// topic: [a-z0-9-]{1,32}；正文 ≤20_000 字符（与 Playbook 读取上限一致）
publish_playbook(topic: String, content: String, based_on: Option<String>, override: bool)
```

- 域 `SelfModify`（默认 ask）、`tier=1`、effect `External`；
- **只在审批通过后落盘**（工具的执行本身就在批准之后），审批卡必须给出变更行；
- `builtin` 主题（`capability` / `timer` / `distill` / `import-skill` / `schedule-job` / `webhook`）
  默认拒绝覆盖，`override=true` 才允许；
- 落盘前把旧版复制成 `<topic>.md.bak`；文件头强制三行溯源（时间 / 基于 / 上一版哈希）；
- `playbook` 工具在 `list` 时返回每个 topic 的 mtime 与来源（builtin / 主人 / agent）。

**硬边界**：playbook 只影响模型"怎么想"，**不参与任何授权判定**——代码不读它（D23）。

---

## 7. 组合约束

| Store | Retriever | 备注 |
|---|---|---|
| `SqliteStore` | `Fts5Retriever`（当前）/ `SqliteVecRetriever`（未来） | 共享写线程 |
| `PgStore`（未来） | `PgvectorRetriever`（未来） | 高 QPS |
| `EphemeralStore`（桩） | `EphemeralRetriever`（桩） | 逼接口中立 |

非法组合在配置加载期报错。

---

## 8. 核心类型

```rust
pub type Timestamp = i64;
pub type Seq = i64;
pub struct ConversationId(pub String);
pub struct RunId(pub String);
pub struct TurnId(pub String);
pub struct CallId(pub String);
pub struct TaskId(pub String);
pub struct ApprovalId(pub String);
pub struct DocId(pub String);
pub struct ChannelId(pub String);
pub struct MsgRef(pub String);

pub struct ActorRef { pub channel: ChannelId, pub external_id: String }
pub struct Scope(pub String);
pub struct DocKind(pub String);

pub struct NewEvent { pub conv: ConversationId, pub run: RunId, pub turn: Option<TurnId>,
                      pub kind: EventKind, pub payload: serde_json::Value,
                      pub dedupe_key: Option<String> }

pub enum Decision { Approve, ApproveAlways, Deny, Expire }
pub enum Resolve { Applied, AlreadyResolved, NotFound }
pub enum AbandonReason { Crash, ApprovalExpired, Cancelled }
pub enum EffectClass { Pure, Idempotent { key: String }, External }
pub enum FinishReason { Stop, Length, ToolCalls, Error, Cancelled }
pub enum RunStatus { Done, Failed, Cancelled }
pub enum ToolState { Running, Ok, Failed }

pub struct DocWrite { pub id: DocId, pub kind: DocKind, pub scope: Scope,
                      pub payload: serde_json::Value, pub version: u64,
                      pub target: Option<DocId>, pub op: DocOp }
pub enum DocOp { Upsert, Supersede, Expire }

pub struct ArtifactRef { pub path: String, pub bytes: u64, pub sha256: String }

pub struct MessagePage { pub msgs: Vec<Message>, pub next: Option<Seq>, pub truncated: bool }

pub struct ExecOpts { pub timeout_ms: u64, pub cwd: Option<String>, pub env: Vec<(String, String)> }
pub struct ExecOutput { pub code: Option<i32>, pub stdout: String, pub stderr: String, pub truncated: bool }
```

---

## 9. 一致性测试

- **Store**：重复 `dedupe_key` 只一条；`append_many` 原子；`lease_task` 并发仅一成功；`resolve_approval` 第二次 `AlreadyResolved`；`seq` 会话内单调唯一；`messages` 分页游标不切半轮；**删投影后 `rebuild` 完全一致**；写事务内无工具执行（日志断言）。
- **Channel**：caps 兑现；重复 `dedupe_key` 不重投；`NotModified`/`TooOld` 正确；取消能终止 `run()`；
  有 `react` 的通道另测 5 条（见 §1.2）：排队态、终态、取消清空、`msg=None` 零反应、反应失败不影响送达。
- **Retriever**：无越权；域内不丢。
- **TurnDriver**：delta 顺序；`ToolStarted` 参数完整（无分片）；usage 提取；cancel 后 `step()` 返回 `Cancelled` 且无 pending。
- **子 agent**：父上下文只增一条结论；`depth > 1` 拒绝；子工具集无 `delegate` 与 `tier=1`；
  预算耗尽返回 `Conclusion{status=budget}` 而非 error/panic；父取消能终止子 run。
