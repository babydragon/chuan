# 两个一等扩展点：Channel 与 Storage

> 目标：**内核可复用，通道与存储可替换。**
> 其余部件（工具、LLM、执行）也是 trait，但契约等级不同——见 §4。

---

## 1. Channel：传输可替换

### 1.1 职责边界

| 归 Channel | 归 Renderer（内核侧） |
|---|---|
| 收发原始消息、媒体下载 | 节流合并、去重渲染 |
| offset / 去重键 | 分片策略（依据 `caps.max_text`） |
| 429 / 重试 / 退避 | 纯文本 vs 富文本降级 |
| 编辑能力、按钮能力声明 | 把事件编排成 View |
| 编辑失败语义（`NotModified` / `TooOld`） | 决定"降级为新消息"还是"放弃" |

一句话：**Channel 只管"能不能送、送失败是什么意思"，不管"送什么、多久送一次"。**

### 1.2 契约

```rust
pub struct Caps {
    pub edit: bool,              // 能否原地编辑
    pub buttons: bool,           // 能否带交互按钮
    pub markdown: bool,          // 能否富文本（false → 纯文本流）
    pub max_text: usize,         // 单条上限（TG 4096）
    pub media: MediaCaps,        // 图片/文件/大小限制
    pub min_edit_interval_ms: u32, // 建议的最小编辑间隔（TG ~1000）
}

pub struct Inbound {
    pub channel: ChannelId,      // "telegram:bot123"
    pub conv: ConversationRef,   // 通道内的会话标识
    pub actor: ActorRef,         // { channel, external_id }
    pub kind: InboundKind,       // Text | Media | Callback | Command
    pub dedupe_key: String,      // update_id / event id
    pub received_at: Timestamp,
}

pub enum EditOutcome { Edited, NotModified, TooOld, Rejected(RetryAfter) }

#[async_trait]
pub trait Channel: Send + Sync + 'static {
    fn id(&self) -> ChannelId;
    fn caps(&self) -> Caps;

    /// 持续收消息；offset 与去重由实现负责
    async fn run(&self, tx: mpsc::Sender<Inbound>, cancel: CancellationToken) -> Result<()>;

    async fn send(&self, to: ConversationRef, out: Outbound) -> Result<MsgRef>;
    async fn edit(&self, msg: &MsgRef, out: Outbound) -> Result<EditOutcome>;
    async fn ack(&self, cb: &CallbackRef, text: &str) -> Result<()>;
    async fn download(&self, f: &FileRef) -> Result<Media>;   // 不支持则返回 NotSupported
}

/// 与通道无关的"渲染文档"，不是格式化好的字符串
pub enum Outbound {
    Blocks(Vec<Block>),          // Paragraph | Code | Image | Divider | Table
    WithButtons(Vec<Block>, Vec<Vec<Button>>),
}
```

### 1.3 三条硬规则

1. **`Outbound` 是文档不是字符串**。通道自己决定怎么排版；内核不拼 Markdown/HTML，否则每加一个通道就要改内核。
2. **`ActorRef` ≠ 人**。`{channel, external_id}` 只是"这个通道上的这个身份"；同一个人跨 TG/Web 的合并靠身份映射表（应用层），内核不知道人是谁。
3. **编辑失败要有语义**。`NotModified` 当成功、`TooOld` 让渲染层换新消息、`RetryAfter` 触发全局退避——这些如果不进 trait，每个通道实现都会各自发明一套错误处理。

### 1.4 一致性测试

`harness-channels` 提供 `ChannelConformance` 测试套件（内存通道 + 一组断言：caps 兑现、编辑语义、去重、顺序、取消）。**任何新通道必须先跑通这套测试**，否则"可插拔"只是嘴上说说。

---

## 2. Storage：持久化可替换

### 2.1 职责边界

| 归 Store | 归内核 |
|---|---|
| 事务、原子性、seq 分配 | 什么时候开事务 |
| 乐观并发（CAS）、租约 | 冲突后怎么重试/降级 |
| 迁移、索引、schema | 需要哪些字段 |
| FTS / 检索实现 | 检索策略与注入多少条 |

### 2.2 契约

```rust
#[async_trait]
pub trait Store: Send + Sync + 'static {
    async fn begin(&self) -> Result<Box<dyn Tx>>;
    async fn load(&self, conv: ConversationId) -> Result<ConversationView>;   // 读快照
    async fn search(&self, q: SearchQuery) -> Result<Vec<Doc>>;               // 可选能力
    async fn migrate(&self) -> Result<MigrationReport>;
}

#[async_trait]
pub trait Tx: Send {
    async fn append_message(&mut self, m: NewMessage) -> Result<Seq>;         // 幂等：dedupe_key 唯一
    async fn begin_tool_calls(&mut self, conv: ConversationId, calls: Vec<NewToolCall>) -> Result<()>;
    async fn finish_tool_call(&mut self, call: CallId, r: ToolOutcome) -> Result<()>;
    async fn upsert_doc(&mut self, doc: Doc) -> Result<()>;                   // 记忆/配置/状态都用它
    async fn enqueue_task(&mut self, t: NewTask) -> Result<TaskId>;
    async fn lease_task(&mut self, now: Timestamp, ttl: Duration) -> Result<Option<Leased>>; // CAS
    async fn resolve_approval(&mut self, id: ApprovalId, d: Decision) -> Result<Resolve>;     // CAS
    async fn record_usage(&mut self, u: UsageRecord) -> Result<()>;
    async fn commit(self: Box<Self>) -> Result<()>;
}

pub enum Resolve { Applied, AlreadyResolved, NotFound }
```

### 2.3 四条硬规则

1. **Unit-of-Work，不是 CRUD**。"assistant 消息 + 本轮 pending tool_calls"必须一次提交；拆成两个方法等于把原子性责任丢给调用方。
2. **async 从第一天就写**。SQLite 实现是"async 外壳 + 单写者阻塞线程"，成本很低；反过来把核心写成同步，将来换 Postgres 就是重写。
3. **CAS 进 trait**：`lease_task`、`resolve_approval` 必须能返回"没抢到 / 已被消费"。这是正确性核心（审批幂等、任务不重复执行），放在应用层就一定会有漏洞。
4. **不泄漏后端类型**。trait 里不出现 `Row`、`Connection`、SQL 片段、查询构造器；只出现记录类型与查询意图。

### 2.4 关于"以后接 Postgres"

现在只写 SQLite 一个实现，但按"已经有两个后端"的纪律来写：

- seq 由 store 分配，且**按会话单调**（不是全局单调）——否则 Postgres 实现会被迫全局串行化；
- 检索能力是**可选 trait**（`Searchable`），不是 `Store` 的必需方法——SQLite 用 FTS5，Postgres 用 tsvector/pgvector，不该强迫每个后端都实现；
- 迁移由后端自己管，内核只调 `migrate()`；
- **`StoreConformance` 测试套件住在 core crate**，任何新后端跑通它才算实现完成。

### 2.5 已知的取舍

| 取舍 | 选择 | 理由 |
|---|---|---|
| 事务里能不能调 LLM/工具 | **不能** | 事务只覆盖状态写入；外部调用在事务外，靠 CAS + 幂等键兜底 |
| 跨会话事务 | 不需要 | 每个 turn 只碰自己的会话 |
| 超大 payload（图片/长文本） | 走 artifacts 存文件，库里只存引用与摘要 | 保证 DB 大小可控、备份快 |

---

## 3. 两个扩展点如何保证"真的可替换"

光有 trait 不够，需要三样配套：

1. **一致性测试**（§1.4 / §2.4）：换实现先跑测试，这是唯一的客观标准。
2. **依赖守卫**：`kernel` 的依赖树里不得出现 `teloxide-core` / `rusqlite` / `reqwest`；CI 用 `cargo metadata` 断言。
3. **第二个实现的演练**：至少写一个 `MemoryStore`（测试用）和一个 `EchoChannel`（CLI 用）。**只有两个实现存在时，接口才真的中立**——这是判断抽象是否合格的实用标准。

---

## 4. 扩展点的层级（哪些是"替换"，哪些是"组合"）

| 扩展点 | 契约 | 变化频率 | 需要的配套 |
|---|---|---|---|
| **Channel** | 替换（一个进程可挂多个） | 中（加一个平台） | caps + 一致性测试 |
| **Store** | 替换（一进程一后端） | 低（换数据库） | Unit-of-Work + 一致性测试 |
| **Retriever** | 替换/组合（可选能力） | 中（加向量、换索引） | 一致性测试 + 索引滞后语义 |
| Tool | 组合（运行时注册多个） | 高（每次加技能） | schema 校验 + 副作用分级 |
| LLM（`TurnDriver`） | 替换（一进程一实现） | 低 | 录制 fixture 回归 |
| ExecutionEnv | 组合/替换 | 中 | 资源限制 + 沙盒自检 |

**Channel / Store / Retriever 三者需要"跨实现一致性测试"**，因为它们承载正确性或语义边界；Tool 是组合型，靠 schema 与单测覆盖即可。

---

## 5. 检索（Retriever）：第三类扩展点

### 5.1 为什么不放进 Store

| | Store | Retriever |
|---|---|---|
| 关心 | 事务、顺序、CAS、一致性 | 相关性排序 |
| 失败模式 | 提交失败、租约丢失 | 召回不准、索引滞后 |
| 扩展节奏 | 换数据库（低频） | 加向量/换模型/混合排序（中频） |
| 生态 | SQLite / Postgres | FTS / 向量 / 混合，各自演进 |

把 `search` 塞进 `Store` 的直接后果：**每个存储后端都要重新实现一遍检索**，而且"换检索方式"会变成"改存储接口"。

### 5.2 现在要不要向量：**不要**

触发条件（满足任意一条才值得引入）：

- 语料大到关键词召回明显不够（大致 > 数千条片段）；
- 查询是语义改写/跨语言（"上次说的那个报税的" → "个人所得税申报"）；
- 需要模糊相似（家教里"找和这道题同知识点的题"）。

家庭助理现状是**数百条 facts + 全量 pinned + 时间衰减**，FTS5 足够。现在加向量会引入三个新问题：embedding 调用延迟、模型漂移导致必须重建索引、多一条失败路径——**收益为负**。

反例要记住：型号、单号、人名这类**精确 token**，BM25/FTS 通常优于向量召回。所以真要做也是**混合**，不是纯向量。

**真正的触发条件是"知识库"**（见 §5.8）：当 agent 需要检索大量外部资料（说明书、教材、工单库、产品文档）时，语义召回才划算。工具/技能目录不需要向量——它们靠 `description` 关键词 + 延迟加载就够；对话记忆也不需要——它靠 pinned + 时间衰减。

### 5.3 契约

```rust
#[async_trait]
pub trait Retriever: Send + Sync + 'static {
    fn id(&self) -> RetrieverId;
    async fn index(&self, docs: &[Doc]) -> Result<()>;   // 幂等 upsert
    async fn search(&self, q: Query) -> Result<Vec<Hit>>; // doc_id + score + 来源 + indexed_at
    async fn health(&self) -> Health;                     // 索引滞后 / 模型不匹配
}

pub struct Hit {
    pub doc_id: DocId,
    pub score: f32,
    pub source: HitSource,   // Fts | Vector | Pinned | Recency
    pub indexed_at: Timestamp,
}
```

三个设计要点：

1. **返回 `doc_id` 与分数构成，不返回文本**。注入多少条、如何拼进 prompt 属于上下文层，检索不该越界。
2. **分数可解释**（`HitSource`）：混合检索出问题时能定位是哪一路在乱排。
3. **`health()` 暴露滞后**：检索允许滞后，但必须可见，否则"刚记的事查不到"会被误判成记忆坏了。

### 5.4 低 / 高 QPS 的选型矩阵

| 场景 | Store | Retriever | 说明 |
|---|---|---|---|
| 家用（当前） | SQLite（rusqlite） | FTS5 | 单文件、零运维、备份=拷文件 |
| 家教（中型） | SQLite 或 PG | FTS5 + 向量（尽量同库） | 语料变大再加向量，避免多引入一个服务 |
| 客服（高 QPS、多实例） | PG | pgvector（HNSW） | 需要并发写与水平扩展，这时才值得上服务 |

**决策原则：只有当"单机单文件"真的不够时才换，换的时候 Store 与 Retriever 可以独立迁移**（比如先只把 Retriever 换成 pgvector 而 Store 仍用 SQLite——虽然少见，但接口允许）。

### 5.5 libsql / sqlite-vec / pgvector 的定位

| 方案 | 形态 | 已核实的事实 | riscv64 | 定位 |
|---|---|---|---|---|
| **sqlite-vec 0.1.9** | SQLite 扩展 | 官方 Rust crate **用 `cc` 静态编译 C 源码 + `sqlite3_auto_extension` 注册**，不需要 `load_extension`，与 `rusqlite(bundled)` 直接共存；**目前只有暴力扫描**（ANN 在 v0.2 路线图，main 分支的 DiskANN 藏在编译开关后未发布）；官方给的实用上限约 **10 万级**向量；纯 C、SIMD 是可选项（检测不到就走 portable C） | 低（无 SIMD 硬依赖；无公开构建记录） | **家庭/中小知识库首选**，单文件、零服务 |
| libsql 0.10 | SQLite fork | 原生向量（`F32_BLOB` + `vector_distance_*` + **DiskANN**）；`Builder::new_local` 本地文件无需服务；但捆绑整个 C 引擎并默认带 replication/HTTP 依赖 | 中（可编，无官方产物） | 百万级且愿意换引擎 |
| pgvector 0.8.7 | Postgres 扩展 | HNSW / IVFFlat 双索引；生态成熟（`pgvector` crate 支持 postgres/sqlx/diesel）；**Debian trixie 已有 riscv64 包（0.8.0）** | 低 | 高 QPS / 多实例 |
| LanceDB 0.40 | 自有格式 | 纯 Rust，但底层 arrow/datafusion/object_store，依赖很重 | 中（理论可编） | 不选 |
| sqlite-vss | faiss 后端 | 作者已停止维护，官方指向 sqlite-vec | — | 排除 |

**对 SBC 特别重要的一条**：sqlite-vec 是暴力扫描，成本随向量维度线性增长。官方基准（桌面 x86）1M×128d ≈ 33ms，但 **3072d 就到 8.5s**；SBC 单核约为 x86 的 3–8%，所以真要上向量时，**优先 256–512 维的 embedding 或 int8 量化**，别直接上 1536/3072 维。十万级 × 768 维在 SBC 上大概率是几百毫秒量级——可接受，但要实测。

倾向顺序：**`sqlite-vec`（≤10 万片段）→ `pgvector`（上 PG 时）→ `libsql`（不推荐，等于换引擎）**。

### 5.6 两个最容易漏的工程问题

1. **向量绑定 embedding 模型**。`doc_vectors(doc_id, model_id, dim, vec, created_at)`；换模型必须**影子重建**（新索引建好后原子切换），不能原地覆盖——否则新旧向量混在一个索引里，召回质量会静默变差。
2. **索引是异步的**。写入只标 dirty，后台任务补索引；检索必须能容忍滞后并如实报告 `indexed_at`，绝不能假装实时。

### 5.7 现在的实现范围

只做 `Fts5Retriever` + 一个 `MemoryRetriever` 桩（测试用）。向量要等**第二个应用**或**语料真的变大**再加——这条判断写在 `DESIGN.md §10` 的"不要提前抽象"里。

### 5.8 知识库：第三类数据平面

向量真正的用武之地是**知识库**，不是记忆。因此系统里应当区分三个数据平面，它们的生命周期、保留策略、备份方式都不同：

| 数据平面 | 内容 | 生命周期 | 检索方式 | 备份 |
|---|---|---|---|---|
| **状态**（journal） | 消息、工具调用、任务、审批 | 长期、必须精确 | 按 seq / 会话 | 每日全量 |
| **结论**（memory docs） | facts、掌握度、画像 | 长期、可收敛可遗忘 | FTS + pinned + 时间衰减 | 随库 |
| **资料**（knowledge base） | 说明书、教材、工单、产品文档 | 大批量、可整体重建 | 混合（BM25 + 向量） | 可重建 → 只需备份原始源 |

把 KB 混进记忆是常见错误：记忆是"关于你的结论"，KB 是"与你无关的原文"，后者的正确性来自**引用**而不是置信度。

**KB 需要一条入库管道**（这是它区别于 Retriever 的地方）：

```
source（文件/URL/目录）
  → parse（PDF/HTML/Markdown/OCR）
  → normalize + 分块（按标题层级/段落，保留父链）
  → 元数据（来源、页码、版本、ACL）
  → 索引（BM25 / 向量 / 两者）
  → 增量更新（按内容 hash 跳过未变；删除要传播）
```

四个容易做错的地方：

1. **ACL 过滤必须发生在检索内部，不是检索之后**。`Query { acl: ActorScope, .. }`——先过滤再 top-k。先取 top-k 再过滤，等于把不该看的文档取出来又丢掉，既泄漏（出现在日志/分数里）又浪费上下文。家教场景尤其致命（家长资料 vs 孩子可见）。
2. **分块策略比 embedding 模型更影响效果**。按标题层级切、保留父级标题、控制每块长度；不要按固定字符数硬切。
3. **必须能引用**。`Hit` 要带 `doc_id + 页码/章节`，最终答案要能标注来源；否则 KB 就退化成"模型瞎编的又一个来源"。
4. **删除与重建是常态**。文档有版本，索引有模型版本；两者都要支持"整库重建"（原始文档在文件系统/对象存储，索引可丢弃重算）。

**现在要做的**：只留接口形状（`Doc` / `chunk` / `Hit.source`），不建管道、不接向量。等真的要做知识库时，新增的是 `ingest` 模块与一个 `VectorRetriever`，内核与 Channel 都不用动。

### 5.9 关于 libsql 与"混合查询"

**libsql 不是 SQLite 的 drop-in。** 三层的兼容度完全不同：

| 层次 | 情况 |
|---|---|
| SQL 方言 | 高（SQLite fork，官方称 backwards compatible） |
| 文件格式 | 基础相同；用了 `F32_BLOB` / `libsql_vector_idx` 后产生专属影子表，原生 SQLite 读不懂索引 |
| **Rust 库** | **完全不同**：`libsql` crate ≠ `rusqlite`，另一套 C 引擎与 API |

另外 `sqlite-vec` 这类扩展依赖 SQLite C API 与 `sqlite3_auto_extension`；libsql 自带引擎，**扩展能否挂入无保证**。

**不要"rusqlite 存状态 + libsql 单独存向量"**：事务无法跨库，journal 成功而索引失败即永久不一致；且向量是从文档派生的数据，拆库后不一致无从发现。

二选一：

- 保持单文件 → `sqlite-vec`（**同一文件**内加向量表，与 FTS5 共存）；
- 要 ANN 且百万级 → libsql **整体替换** Store（写 `LibsqlStore` + 跑一致性测试）。

**混合查询**：`sqlite-vec` 提供 metadata 列与 partition key，可在向量查询内部过滤（ACL 必须放这里）；但它**没有内置 BM25 + 向量融合算子**，需要自己构造——两路各取 top-k，再用 RRF（倒数排名融合）合并，因为两路分数量纲不可比，手调权重不稳。融合建议放在 Rust 里（可单测、可换算法），只在追求单次往返时才写进 SQL。

```sql
WITH v AS (
  SELECT doc_id, row_number() OVER (ORDER BY distance) AS rk
  FROM vec_chunks
  WHERE embedding MATCH :qvec AND k = 50 AND acl_scope IN (:scopes)
),
f AS (
  SELECT doc_id, row_number() OVER () AS rk FROM (
    SELECT doc_id FROM chunks_fts
    WHERE chunks_fts MATCH :qtext AND acl_scope IN (:scopes)
    ORDER BY rank LIMIT 50
  )
)
SELECT COALESCE(v.doc_id, f.doc_id) AS doc_id,
       1.0/(60 + COALESCE(v.rk, 999)) + 1.0/(60 + COALESCE(f.rk, 999)) AS score
FROM v FULL OUTER JOIN f ON v.doc_id = f.doc_id
ORDER BY score DESC LIMIT 10;
```

### 5.10 Store 与 Retriever 的组合约束

接口上两者正交，**实现上不自由**。配置声明的是"一对"，启动时校验组合合法：

| Store | 可搭配的 Retriever | 备注 |
|---|---|---|
| `SqliteStore`（rusqlite） | `Fts5` / `SqliteVec` | 当前唯一实现 |
| `LibsqlStore`（备选，非待办） | libsql 原生向量 / FTS5 | 选它即放弃 sqlite-vec 路线 |
| `PgStore` | tsvector / `PgVector` | 高 QPS / 多实例 |
| `MemoryStore`（测试桩） | `MemoryRetriever` | 逼接口中立 |

两条纪律：

1. **`LibsqlStore` 是备选而非计划**。必须有的"第二实现"是 `MemoryStore`——它的价值是暴露接口里泄漏的后端语义，而不是提供功能。
2. **启动时校验组合**，不要留到运行时。例如 `SqliteVec` 配 `PgStore` 应在配置加载期直接报错，并提示可用的组合。
