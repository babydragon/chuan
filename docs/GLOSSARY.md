# 术语表（Glossary）

> **本表是中英命名与口径的唯一标准。** 写代码、写文档、起变量名都以这里为准；
> 同一概念不允许出现第二种叫法。新增术语必须先加进本表。

---

## A. 系统结构

| 术语 | 英文 / 代码名 | 定义 | 不要这样说 |
|---|---|---|---|
| **管家（本系统）** | Harness / `chuan` | 整个可运行实体：通道 + 内核 + 扩展 + 应用配置 | 机器人、框架、平台 |
| **内核** | Kernel / `harness-kernel` | 编排、状态机、治理接口；不碰 IO、不含业务 | 引擎、runtime |
| **引擎** | Engine / `harness-engine` | `TurnDriver` 的实现：请求构造、SSE 解析、usage 提取 | 内核、客户端库 |
| **通道** | Channel | 传输适配器：收消息、发/编辑消息、声明能力 | 网关、适配器、connector |
| **存储** | Store | 持久化后端：事务、seq、CAS、迁移 | 数据库层、DAO |
| **检索器** | Retriever | 索引与召回；返回 `doc_id` 与分数构成 | 搜索、向量库 |
| **执行环境** | ExecutionEnv | 工具执行的抽象（接口） | 沙盒（沙盒是它的实现） |
| **沙盒** | Sandbox | `ExecutionEnv` 的 bwrap 实现 | 容器、jail |
| **应用** | App / `apps/<name>` | 薄二进制 crate + 数据目录（config/prompts/skills/evals） | 实例、部署、租户 |
| **能力层** | Tool Surface | 工具注册表的整体 | 工具箱 |
| **回合驱动** | TurnDriver | 生成请求、解析流、产出 `Step` 的接口；engine 是它的实现 | loop 库、agent 框架 |
| **渲染器** | Renderer | 把 Event 编排成 View、做节流/分片/降级 | 格式化器 |
| **凭据** | Credential / Secret | 一份 token/密钥；**值只存在于宿主侧**，沙盒里看不到（D18） | key、密码、token 当同义词混用 |
| **服务** | Service | 配置好的命名外部端点（`kind = http` 或 `mcp`），模型只说服务名与路径 | 插件、连接器 |
| **规范** | Playbook | 给模型看的操作流程文本（`playbook(topic)` 按需取，永久不进 prompt） | 文档、说明书、SOP 混用 |
| **子 agent** | SubAgent | 由父会话派生、独立上下文、只回传结论的执行体 | 分身、worker |

## B. 会话与状态

| 术语 | 英文 / 代码名 | 定义 | 不要这样说 |
|---|---|---|---|
| **会话** | Conversation | 一条持续对话，隔离单位（按 chat） | session、对话线程 |
| **会话运行时** | ConversationActor | 进程内承载会话串行执行的 actor | session |
| **一次运行** | Run | 从一条用户输入到终态的完整过程 | 请求、任务 |
| **轮** | Turn | 一次模型请求 + 该轮的工具调用 | 回合、迭代 |
| **视图** | View | Renderer 产出的可渲染结果（blocks + status + buttons） | 消息体、HTML |
| **内容块** | Block | 视图的最小单元（段落/代码/图片/分隔线/表格） | 富文本片段 |
| **步** | Step | 状态机的一次迁移（`Model` / `Tools` / `Done`） | 阶段、step 之外的说法 |
| **账本** | Journal | append-only 状态日志，唯一真相 | 日志（日志指 tracing 输出） |
| **快照** | Snapshot | 某时刻会话的结构化视图，用于重连与渲染 | 缓存、state |
| **租约** | Lease | 任务执行权的时间窗，过期即可被抢占 | 锁 |
| **尝试** | Attempt | 一次执行尝试，计入预算与告警 | 重试次数 |
| **产物** | Artifact | 工具产出并落盘的文件，按 sha256 寻址 | 输出、附件 |
| **墓碑** | Tombstone | 媒体过期后的占位记录，保证回放不报错 | 已删除 |

## C. 工具与技能

| 术语 | 英文 / 代码名 | 定义 | 不要这样说 |
|---|---|---|---|
| **工具** | Tool | 模型可调用的动作，带 JSON Schema 与副作用等级 | function、action |
| **技能** | Skill | `manifest + scripts/`，**纯函数**（workspace in → workspace out） | 插件、扩展、plugin |
| **清单** | Manifest | 技能的参数 schema 与元数据（`SKILL.md` frontmatter） | 配置、descriptor |
| **技能说明** | `SKILL.md` | 人类可读说明；**正文永不进上下文** | prompt、文档 |
| **工具包** | Tool Pack | 一组工具的集合（内置 / 技能 / MCP） | 插件包 |
| **注册表** | Tool Registry | 运行时装配与角色裁剪的中枢 | 工厂、容器 |
| **MCP** | Model Context Protocol | 外部工具协议；工具名 `mcp__<server>__<tool>` | 插件协议 |

## D. 安全与治理

| 术语 | 英文 / 代码名 | 定义 | 不要这样说 |
|---|---|---|---|
| **策略** | Policy | 白名单、角色、路径护栏等静态规则 | 权限系统 |
| **能力边界** | Capability Boundary | 授权轴：能否出网/打印/发送/改自身 | 权限等级 |
| **审批** | Approval | 越界时向主人发起的确认，单次消费 | 确认框、授权 |
| **本次允许** | `allow` | 仅本次调用有效 | 临时授权 |
| **始终允许** | `always` | 绑定"动作类别 + 目标"，有 TTL 与频率熔断 | 白名单 |
| **副作用等级** | `EffectClass`：`Pure` / `Idempotent { key }` / `External` | 决定崩溃后能否重跑 | 幂等性（泛指） |
| **幂等键** | Idempotency Key | `hash(tool + 规范化 args)`，与 call id 解耦 | 去重 id |
| **挂载集** | Mount Set | 沙盒可见的文件系统白名单 | 挂载配置 |
| **工作区** | Workspace | 按 **run** 创建；P0 为单可写根 `/ws`，三段式列 P2 | 临时目录 |
| **孤儿回收** | Orphan Reap | 启动时清理残留的沙盒子进程 | 清理 |

## E. 上下文与记忆

| 术语 | 英文 / 代码名 | 定义 | 不要这样说 |
|---|---|---|---|
| **上下文组装** | Context Assembly | 每轮拼装模型输入 | 拼 prompt |
| **压缩** | Compaction | 超阈值时摘要旧内容，保留未决事项 | 截断 |
| **工具结果清理** | Tool-result Clearing | 用占位符替换旧工具输出，保留调用记录 | 删除 |
| **固定事实** | Pinned | 始终注入的高置信事实 | 核心记忆 |
| **文档** | Doc | 存储层通用容器 `(type, scope, payload)` | 记录、行 |
| **文档类型** | DocKind | `fact` / `profile` / `mastery`（家教）等 | 表 |
| **作用域** | Scope | `user:<id>` / `family`，决定可见范围 | 租户 |
| **知识库** | KB | 外部资料的语料库，与记忆不同平面 | 记忆、语料 |
| **分块** | Chunk | KB 的检索单元，保留父级标题 | 片段、段落 |
| **入库管道** | Ingest | 解析 → 分块 → 元数据 → 索引 → 增量更新 | ETL、导入 |
| **位置** | Locator | 引用定位（页码/章节），供答案标注来源 | 引用路径 |
| **混合检索** | Hybrid Retrieval | BM25 + 向量，用 RRF 融合 | 多路搜索 |
| **RRF** | Reciprocal Rank Fusion | 只用排名融合，避免量纲不可比 | 加权求和 |

## F. 运行与观测

| 术语 | 英文 / 代码名 | 定义 | 不要这样说 |
|---|---|---|---|
| **事件** | Event | 内核向通道/观测发出的结构化消息 | 消息（消息属于通道） |
| **增量** | Delta | 流式文本/推理/参数的分片 | 流片段 |
| **轨迹** | Trace | 一次运行的结构化执行记录 | 日志 |
| **回放** | Replay | 用录制数据重跑，做回归 | 重演 |
| **一致性测试** | Conformance | 替换型扩展点必须通过的测试套件 | 集成测试 |
| **通道车道** | Lane | 并发配额：`io` / `compile` / `external` | 线程池、队列 |
| **降级模式** | Degraded Mode | 沙盒或依赖不可用时的收紧运行态 | 兜底 |

---

## G. 命名禁令（最容易混的几组）

1. **不要用 `session`**：对话叫 **Conversation**，进程内承载者叫 **ConversationActor**。`session` 一词在本项目禁用。
2. **不要用 `job`**：一律 **Task**（调度单元）。`Job` 只出现在外部库 API 里。
3. **不要用 `plugin` 指技能**：技能是 **Skill**；`plugin` 只用于描述第三方框架。
4. **不要用 `sandbox` 指接口**：接口是 **ExecutionEnv**，沙盒是它的 bwrap 实现。
5. **不要用 `memory` 指知识库**：**Memory = 结论**，**KB = 资料**。二者的正确性来源不同（置信度 vs 引用）。
6. **不要用 `prompt` 指整个上下文**：整个输入叫 **Context**；`prompt` 只指 system 指令那一段。
7. **不要用"用户"泛指**：主体是 **Actor**（`{channel, external_id}`）；人叫 **主人（owner）** 或 **成员（member）**。
8. **不要用"日志"**：状态用 **Journal（账本）**，运维输出用 **Log**，执行记录用 **Trace**。

---

## H. 命名对照速查

| 中文 | 英文 | 代码前缀 |
|---|---|---|
| 会话 | Conversation | `conv_` |
| 一次运行 | Run | `run_` |
| 轮 | Turn | `turn_` |
| 工具调用 | ToolCall | `call_` |
| 任务 | Task | `task_` |
| 审批 | Approval | `appr_` |
| 文档 | Doc | `doc_` |
| 技能 | Skill | `skill_` |
| 通道 | Channel | `ch_` |
| 车道 | Lane | `lane_` |

---

## I. 事件种类（与 `DATA-MODEL.md §3` 一一对应）

| 类别 | 种类 |
|---|---|
| 对话 | `msg.user` · `msg.assistant` · `msg.tool_result` · `msg.synthetic` |
| 工具 | `tool.call_started` · `tool.call_finished` · `tool.call_abandoned` |
| 审批 | `approval.requested` · `approval.resolved` |
| 任务 | `task.enqueued` · `task.leased` · `task.finished` |
| 其他 | `usage.recorded` · `artifact.created` · `ctx.compacted` · `doc.upserted` |

## J. 命名豁免

- `plugin` **允许**用于指"原生动态库插件"（本项目的明确非目标），但**不得**用来指技能。
- `memory` **允许**作为 Rust 固有词汇（如 `MemoryError`），但本项目自己的概念一律用 **Memory（结论）** 或 **KB（资料）**。
- `session` 在第三方库 API 名称中出现时保留原样（如 `systemctl` 输出），自建类型不得使用。
