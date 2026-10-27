# 项目说明与作业规范：家庭智能管家 Harness（`chuan`）

> 本文是**项目宪法**：约束什么不能变、怎么干活。
> 架构细节不在这里展开——一切以 `docs/` 为准（索引见文末）。

---

## 1. 这是什么

跑在**一块 RISC-V SBC 单板机（riscv64 / 8GB）**上的常驻个人助理。Telegram 是主入口，CLI 并级。

- **主人（owner）** 可扩展；**成员（member）** 只读。一期 `allowed_users` 内同权、暂不做隔离（见 ROADMAP P4.5），但身份结构已按「一个 user 绑多个通道」预留。
- 核心差异化：**自我提升闭环**——在沙盒里调试脚本 → 主人确认 → 泛化为技能 → 热加载生效。
- 它不是聊天机器人，是**能安全执行动作**的 agent 运行时：跑命令、取网、定时提醒、沉淀技能，且每次动作可审计、可恢复、可回滚。
  **打印这类领域能力由技能实现，不进内核**（见不变量 15）。
- **第一要务是准确性**：宁可说「不知道」，也不给听起来合理但没有依据的解释；实测、推断、不知道要分清。

**当前状态**：P0–P3 主体已实现并在开发机验证（沙盒、记忆、技能闭环、定时任务、出网与审计、依赖安装、内网 lane、身份层）；
**riscv64 端到端尚未上板**，逐条清单见 `docs/SBC-VERIFY.md`。开发机（x86_64）只用于构建与联调，生产只在目标板上。

---

## 2. 硬约束（不可协商）

| # | 约束 | 含义 |
|---|---|---|
| C1 | **Rust 单二进制 + systemd** | 不引入 Node/Deno/Bun；不用容器编排；单机部署 |
| C2 | **目标机 riscv64** | 所有依赖必须能交叉编译；新增依赖先验证 `cargo zigbuild` |
| C3 | **无第二台运行机器** | 重活也在本机；LLM 走远端 API |
| C4 | **单实例** | `flock` 独占；单写者 SQLite 与调度器要求绝对单实例 |
| C5 | **面向 SBC 的轻量** | 为单板机而设计：常驻内存与依赖重量应**显著低于 TS 系 harness**（Pi / Hermes 这类）。目标机虽是 8GB，仍不设硬性数字门槛；常驻 >150MB 或新增重型依赖需说明理由，不为省内存牺牲正确性 |
| C6 | **无中间件** | 不引入 Redis/Postgres/消息队列（除非单机确实不够，见 D12） |

---

## 3. 架构不变量（改动前先读 `docs/DECISIONS.md`）

1. **Journal 是状态的唯一真相**；`docs` 是结论的唯一真相。模型看见过的内容必须能从账本重建。
2. **不持久化任何框架/模型客户端的内部状态**（序列化格式会随版本失效）。
3. **三个数据平面分开**：状态（journal）/ 结论（docs）/ 资料（KB：文件库 `keep`/`files`，已有最小实现）；生命周期与备份方式不同。
4. **恢复按崩溃点分级**，不重放整个 turn；副作用分 `Pure` / `Idempotent{key}` / `External`。
5. **`call_dispatched` 是"从未执行"与"执行后丢结果"的唯一分界**；`External` 永不自动重发。
6. **三段式执行**：短事务写意图 → 无事务执行（含审批）→ 短事务写结果。**事务内禁止**执行工具、调 LLM、发消息、等审批。
7. **授权轴是执行域，不是命令黑名单**：`sandbox`（沙盒内，免审批）/ `self_modify`（改自身）/ `outbound`（出网）/ `lan`（内网调试）/ `install`（装依赖）/ `host_exec`（宿主提权）；`[policy]` 按域配置，`require_approval` 只能加严、不能放松。
8. **沙盒是安全边界**：白名单挂载（**绝不 `ro-bind /`**）、`/home` 不可见、默认断网、cgroup 内存与 pids 硬限、超时 `killpg`。
   联网是**独立 lane**（`lan_exec`、声明 `network: true` 的技能），不是默认能力；额外宿主挂载只能由主人在 `[sandbox] mounts` 里显式授权。
9. **出网只有两条路，且都留痕**：宿主 `web_fetch` 代取（拒绝内网/回环地址、逐次写审计日志）；或技能声明 `network: true` 走联网 lane。
   默认 lane 永远没有网络；**凭据由宿主注入，永不进上下文**。
10. **技能是纯函数**：只能读写 workspace；`SKILL.md` 正文**永不进上下文**（只注入结构化 manifest）；权限不可自授。
11. **loop 是 dumb loop**：只负责终止、预算、取消、续跑；聪明逻辑属于上下文层或工具层。
12. **记忆是 doc 容器 + 应用语义**：内核只提供 `doc(type, scope, payload)`，`fact` 只是其中一种。
13. **宿主/提权执行必须逐条审批**：`ExecProfile::Host` 需 owner 显式指令（仅下一条生效），`HostElevated` 只能使用 sudoers 白名单内的命令（见 `DECISIONS.md` D15）。
14. **凭据不进入执行环境**：密钥只经 systemd 凭据（`LoadCredential[Encrypted]`）注入；宿主/提权子进程必须 `env -i` 启动且不挂载凭据目录；沙盒永久不挂载凭据。工具需要的凭据由内核显式传 fd/stdin，不得经环境变量。
15. **内核只给边界与通用传输，领域能力下沉为技能**：判断标准是"换一个环境还要改 Rust 吗"——要，就说明它属于技能。
    打印/发现/查墨量、某个服务的 API 语义，都是技能或 playbook；只有 `[sandbox] mounts`、`http`/`mcp` 这类**通用传输**才进内核（见 D17）。

---

## 4. 技术栈（已定，不要再建议替代）

| 用途 | 选择 | 备注 |
|---|---|---|
| 运行时 | `tokio`，`worker_threads=4` | 并发按 lane：`io=2 / compile=1 / external=1` |
| Telegram | **`teloxide-core`**（不用 `teloxide` 的 dispatcher） | dispatcher 的每 update 起 task 与会话串行冲突 |
| HTTP/TLS | `reqwest 0.12` + `rustls`（ring） | 已实测交叉编译 |
| LLM 客户端 | **自写**（≈400 行，OpenAI 兼容 SSE） | 接口冻结为 `TurnDriver`；rig 只作备份方案 |
| 存储 | `rusqlite`（`bundled`） | 单写者线程 + WAL |
| Schema | `schemars` | 工具参数与结构化输出 |
| MCP | 自写 MCP-over-HTTP 客户端（`initialize` / `tools/list` / `tools/call`） | 不引 `rmcp`；**stdio 形态不做**，只做 https + token；`tools/list` 的 schema 动态注入上下文 |
| 日志 | `tracing` + journald | 不自己写日志文件 |
| 沙盒 | `bwrap`（目标机 0.11）+ cgroup v2 | 参数以 0.11 为准 |
| 向量 | **不做** | 触发条件见 `docs/INTERFACES.md §3.4` |

### 已废弃，不要重新引入

- ❌ Node/TypeScript 内核（Pi 方案）——用户坚持 Rust。
- ❌ `rig` 的 `AgentRun` 作为 loop——journal-first 下用不上，且每两周破坏性变更。
- ❌ `teloxide` 全量（含 dispatcher）。
- ❌ 30MB 这类硬性数字门槛（改为"低占用取向"，见 C5）、`RLIMIT_AS` 限 Python、`ro-bind / /`、系统 cron、`/tmp/workspace/<session>` 形态。
- ❌ 以命令黑名单作为安全边界。

---

## 5. 分层与依赖纪律

```
crates/harness-{kernel,engine,store,channels,capabilities,observe}
apps/<name>/            # 薄二进制 crate + config/prompts/skills/evals
```

- **`kernel` 的依赖树里不得出现** `reqwest` / `teloxide-core` / `rusqlite` / `rmcp` / `image`。
  CI 用 `cargo metadata` 断言；**第一周就要有**，否则半年后没人拆得开。
- 四个替换型扩展点（Channel / Store / Retriever / TurnDriver）必须有跨实现一致性测试；
  Tool / ExecutionEnv 是组合型，靠 schema 与单测。
- 必须存在 `EphemeralStore` 与 `EchoChannel` 两个桩——**只有两个实现时接口才中立**。

---

## 6. 作业方式

1. **设计先行**：非平凡改动先给方案（改什么、为什么、影响哪些层），等确认再写代码。
2. **文档是交付物的一部分**：改契约就改 `INTERFACES.md`；改取舍就改 `DECISIONS.md`；不要只在代码里"顺手实现"。
3. **实测优先于推断**：平台相关结论必须来自目标板；未验证的标 ⚠️ 并进 `ROADMAP.md §0`。
4. **验证到哪一步说哪一步**：声称"完成"前必须构建通过、相关一致性测试通过、无新增 TODO 绕过。
5. **不要擅自执行** `git commit` / `git push` / `git reset`，也不要删除文件或改系统配置。给出建议由我决定。
6. **一次只做一件事**：大任务拆步，每步有验收判据；不要顺手重构无关模块。

---

## 7. 术语与命名

**以 `docs/GLOSSARY.md` 为唯一标准。** 最容易犯的几条：

- 对话叫 **Conversation**，禁用 `session`。
- 调度单元叫 **Task**，禁用 `job`。
- 技能叫 **Skill**，禁用 `plugin`（`plugin` 只指"原生动态库"，是本项目的非目标）。
- 接口叫 **ExecutionEnv**，**Sandbox** 是它的 bwrap 实现。
- **Memory = 结论**，**KB = 资料**，两者不能混用。
- 整个输入叫 **Context**，`prompt` 只指 system 指令那一段。
- 主体是 **Actor**；人叫**主人**或**成员**。
- 状态叫 **Journal（账本）**，运维输出叫 **Log**，执行记录叫 **Trace**。

新增术语必须先写进 `GLOSSARY.md` 再用。

---

## 8. 常见错误清单（前几轮设计评审的真实教训）

- 把"能跑通"当成"设计对"：`--unshare-all` 失败其实是测试脚本在只读根上建挂载点，不是平台不支持。
- 用文档推断代替实测：`aws-lc` 被断言"交叉编译有坑"，实测能编过；`seq` 全局主键与"按会话单调"写在一起自相矛盾。
- 契约写得上天但不可实现：`Tool` 持有 `&mut Tx` 跑 60s 工具、`EventSink` 用 `send().await` 却声称"满则丢"。
- 统计口径不一致：同一件事在 README/DESIGN/PLATFORM 三种说法（"已实测"vs"待重跑"）。
- 过早抽象：没有第二个应用就设计插件系统、多租户、workflow 引擎。
- 把审批做成"全都问"：结果训练出橡皮图章，真正危险的反而被放行。

---

## 9. 文档索引（按此顺序读）

| 顺序 | 文件 | 内容 |
|---|---|---|
| 1 | `docs/README.md` | 索引 + 十条结论 + 维护规则 |
| 2 | `docs/DESIGN.md` | 分层（L1–L10）、模块、功能点、关键流程 |
| 3 | `docs/DATA-MODEL.md` | 层→模块→crate 映射、事件分类、Journal、恢复判定 |
| 4 | `docs/INTERFACES.md` | 契约：Channel / Store / Retriever / TurnDriver / Tool / ExecutionEnv / Renderer |
| 5 | `docs/DECISIONS.md` | ADR（D1–D13、D15–D17）+ 已废弃清单 |
| 6 | `docs/SELECTION.md` | 按层选型、依赖清单、排除项 |
| 7 | `docs/OPS.md` | 配置、密钥、systemd、时间、重试、备份、降级 |
| 8 | `docs/PLATFORM.md` | 目标板实测基线（沙盒/cgroup/glibc/bwrap） |
| 9 | `docs/ROADMAP.md` | P0–P5、前置验证 V1–V5、DoD |
| 10 | `docs/GLOSSARY.md` | 术语与命名标准 |
| 11 | `docs/SBC-VERIFY.md` | 只能在目标板确认的项（上板逐条勾，含性能与部署） |

图：`docs/architecture.png`（源 `docs/architecture.dot`）。
沙盒验证脚本：`deploy/verify-sandbox.sh`。
