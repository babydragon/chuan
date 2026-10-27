# 选型

> 判定方法：每层只问三个问题——**① 正确形态是否已收敛？② 有没有库覆盖？③ 覆盖它的代价是否小于自写？**
> 决策理由见 `DECISIONS.md`，实测数据见 `PLATFORM.md`。

---

## 1. 结论表

| 层 | 结论 | 选择 | 一句话理由 |
|---|---|---|---|
| L1 通道 | 买协议，建调度 | `teloxide-core`（不用 dispatcher） | 协议类型量大易错；dispatcher 的并发语义与"单消息持续编辑 + 会话串行"冲突 |
| L2 状态 | 建 schema，买引擎 | 自建 journal + `rusqlite(bundled)` | 单文件零运维；没有库提供"按崩溃点分级恢复" |
| L4 上下文 | 建 | 自建 | 压缩与注入策略和产品强绑定 |
| L5 工具面 | 建注册表，买 schema | 自建 registry；`schemars`；MCP 预留 | 工具契约简单；MCP 需要时再加 |
| L6 执行 | 买二进制，建编排 | `bwrap` + `libc` + cgroup v2 | 命名空间只能靠内核工具 |
| L7 治理 | 建 | 自建 | 策略/审批/幂等键都是产品语义 |
| L10 可观测 | 买采集，建语义 | `tracing` + `tracing-subscriber` | 结构化日志有标准件；按轮记账是业务语义 |
| L3/L9 循环与编排 | 建 | 自建 `TurnDriver` | loop 是差异化所在（`DECISIONS.md` D2） |
| L4 检索 | 建抽象，实现从简 | 自建 `Retriever`；先只做 FTS5 | 检索与存储是两个扩展点；向量等知识库需求 |

**一句话：买协议适配器与存储引擎，建编排与治理。**

---

## 2. LLM 客户端

| 方案 | crates | 实测 | 结论 |
|---|---|---|---|
| 自写（`reqwest 0.12 + ring`） | 151 | ✅ 14.5s / 4.0MB | **采用** |
| `rig-core 0.45` | 232 | ✅ 编译链接通过（含 aws-lc） | 备选（仅 wire 层） |
| `async-openai` | — | 未测 | 排除：静默丢 `reasoning_content` |
| `openai-api-rs` | — | 未测 | 排除：流式 tool-call 丢参数分片 |

理由与失败条件见 `DECISIONS.md` D2。

---

## 3. 依赖清单

| 用途 | crate | 特性 / 说明 |
|---|---|---|
| Telegram 协议 | `teloxide-core` | 只用 API 与类型，不用 dispatcher |
| HTTP / TLS | `reqwest` 0.12 | `rustls-tls`（ring），已实测交叉编译 |
| 序列化 | `serde` / `serde_json` | — |
| 异步 | `tokio` | `worker_threads=4`，`max_blocking_threads=8` |
| SQLite | `rusqlite` | `bundled`，避免依赖系统 libsqlite3 |
| JSON Schema | `schemars` | 工具参数与结构化输出 |
| MCP | 自写（~300 行） | 只做 Streamable HTTP + token；不引 `rmcp`，不做 stdio |
| 日志 | `tracing` / `tracing-subscriber` | 输出到 journald |
| 进程控制 | `libc` | `setsid` / `killpg` |
| 时间 | `chrono` 或 `time` | 存储一律 UTC epoch |

**不引入**：ORM、Web 框架、消息队列、向量库（当前）、WASM 运行时、动态插件系统。

---

## 4. 排除项

| 候选 | 原因 |
|---|---|
| `teloxide`（含 dispatcher） | 每 update 起 task，与会话串行冲突；用它等于再实现一层队列 |
| `rig` 门面 + `agent` feature | 实测 655 crates（lancedb/fastembed/datafusion/rav1e） |
| `rig-harness` | Bevy ECS 编程模型、0.46-rc、122 次下载、jsonl 存储 |
| `goose` | tree-sitter×10 + aws-lc + 可选 llama-cpp；是 app 不是库 |
| `zeroclaw` | 形态最像但 v0.8.6、crates 未发布、正在微内核重构 |
| `libsql` | 等于换数据库引擎；仅百万级 ANN 时才有价值 |
| `LanceDB` | 依赖很重（arrow/datafusion/object_store） |
| Bun / Deno / WASM 运行时 | 与"Rust 单二进制"冲突 |

---

## 5. 依赖守卫

CI 用 `cargo metadata` 断言：`kernel` 的依赖树中不得出现 `reqwest`、`teloxide-core`、`rusqlite`、`rmcp`、`image`。

**第一周就要有**，否则半年后没人拆得开。

---

## 6. 升级策略

| 依赖 | 策略 |
|---|---|
| `teloxide-core` | 锁 minor；升级只影响 `channels/telegram` |
| `reqwest` | 锁 minor；大版本升级需重跑交叉编译与板上 smoke |
| `rusqlite` | 锁 minor；升级需跑 `StoreConformance` |
| `rmcp` | **本轮不引入**；需要时再锁 minor |
| 自写 engine | 无升级税；用录制 fixture 做回归 |
