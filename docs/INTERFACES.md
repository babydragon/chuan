# 契约

> 替换型：**Channel / Store / Retriever / TurnDriver**；组合型：**Tool / ExecutionEnv**；内核内策略：**Renderer**。
> 数据模型 `DATA-MODEL.md`，运维 `OPS.md`，术语 `GLOSSARY.md`。

---

## 0. 总则

- 四个替换型各配一致性测试；Tool / ExecutionEnv 靠 schema 与单测。
- 必须有 `EphemeralStore` 与 `EchoChannel` 两个桩，逼接口中立。
- 依赖守卫：`kernel` 依赖树中不得出现 `reqwest`/`teloxide-core`/`rusqlite`/`rmcp`/`image`。

---

## 1. Channel

```rust
pub struct Caps {
    pub edit: bool, pub buttons: bool, pub markdown: bool,
    pub max_text: usize, pub media: MediaCaps,
    pub min_edit_interval_ms: u32,     // TG 暂定 1000，ROADMAP V3 实测后调整
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
}
```

1. 通道只收 `View`，不拼字符串；排版/分片/转义在 Renderer。
2. `ActorRef ≠ 人`；跨通道合并靠应用层。
3. `NotModified` 当成功、`TooOld` 让 Renderer 换新消息、`Rejected` 全局退避。

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

### 6.1 出站文件

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

## 6. Tool / ExecutionEnv

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn effect(&self) -> EffectClass;
    async fn execute(&self, args: Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, ToolError>;
}

pub struct ToolCtx<'a> {
    pub conv: ConversationId, pub call: CallId, pub run: RunId,
    pub workspace: &'a Workspace,
    pub env: &'a dyn ExecutionEnv,
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
| `HostExec` | 宿主/提权执行 | `deny` |

`[policy] require_approval` 只能把 `allow` 提成 `ask`，不能放开 `deny`。

内置工具的副作用等级（P0 验收要用）：

| 工具 | effect | 说明 |
|---|---|---|
| `read` / `list` | `Pure` | 只读 |
| `write` | `Idempotent { key: hash(path, content) }` | 同内容重写无害 |
| `exec` | `External` | 任意命令，**默认不重跑**；技能可在 manifest 声明更弱的等级 |
| `remind` | `Idempotent { key: hash(chat, due_at, text) }` | 重复入队被去重 |
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
| `playbook` | `Pure` | 只读文本 |

域声明：`publish_skill` / `rollback_skill` = `SelfModify`，其余内置工具 = `Sandbox`
（`web_fetch` = `Outbound`；未来的 `send` = `SelfModify`、`host_exec` = `HostExec`；打印等由技能实现，不占域）。

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

pub struct MessagePage { pub msgs: Vec<Message>, pub next: Option<Seq>, pub truncated: bool }

pub struct ExecOpts { pub timeout_ms: u64, pub cwd: Option<String>, pub env: Vec<(String, String)> }
pub struct ExecOutput { pub code: Option<i32>, pub stdout: String, pub stderr: String, pub truncated: bool }
```

---

## 9. 一致性测试

- **Store**：重复 `dedupe_key` 只一条；`append_many` 原子；`lease_task` 并发仅一成功；`resolve_approval` 第二次 `AlreadyResolved`；`seq` 会话内单调唯一；`messages` 分页游标不切半轮；**删投影后 `rebuild` 完全一致**；写事务内无工具执行（日志断言）。
- **Channel**：caps 兑现；重复 `dedupe_key` 不重投；`NotModified`/`TooOld` 正确；取消能终止 `run()`。
- **Retriever**：无越权；域内不丢。
- **TurnDriver**：delta 顺序；`ToolStarted` 参数完整（无分片）；usage 提取；cancel 后 `step()` 返回 `Cancelled` 且无 pending。
