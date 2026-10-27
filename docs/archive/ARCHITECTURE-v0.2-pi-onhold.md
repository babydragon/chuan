# 家庭智能管家 Harness — 架构设计 v0.2

> ⚠️ **本版已暂停，不要据此实现。**
> 用户确认了两条硬约束：**坚持 Rust**（拒绝 Node/TypeScript 运行时）、**只有这一块板子**（无远程执行机）。
> 因此本文件的 Pi/Node 方案仅作**架构思想参考**（Pi 的分层、durable task、SKILL.md 渐进披露、ExecutionEnv 抽象值得照搬），
> 技术栈需替换为 Rust 内核 + 单机执行。定稿见 **v0.3（Rust 内核选型中）**。
> 仍然有效的判断：沙盒（bwrap + cgroup）、记忆双时态、技能沉淀 dry-run 闸门、通道抽象、事件分级。

---

> **v0.2 是相对 v0.1 的修订，记录在此仅供追溯。** 变更原因：确认目标是「个人助理 + 家人只读共用 + 自我进化」，且明确要求 **agent loop 不自维护**、要支持子 agent 与长任务、channel 可替换。v0.1 的「自写 Rust loop + teloxide + rusqlite」全部作废，改为 **以 Pi（`pi-durable`）为内核，我们只写宿主、通道与扩展**。
>
> v0.1 唯一保留的部分：沙盒执行、记忆双时态、技能沉淀闸门这三条设计判断。

---

## 0. 结论先行

| 决策 | 结论 |
|---|---|
| Agent 内核 | **`@earendil-works/pi-durable` 1.4.0**（durable harness：会话/任务/工具 loop/压缩/存储） |
| LLM 接入 | `@earendil-works/pi-ai`（15+ provider，已含 OpenAI/Anthropic/Google/OpenRouter/Ollama） |
| 我们的代码 | **Node/TypeScript 宿主**：TG 通道 + 扩展包 + 部署，约 3–4k 行 |
| 部署目标 | RISC-V SBC / riscv64 / 8GB，Node 24 LTS（unofficial riscv64 build）+ systemd |
| 沙盒 | `ExecutionEnv` 实现（bwrap）+ 独立系统用户 |
| 子 agent | durable 的 task-owned conversation + `pi-env` RemoteExecutionEnv（重活甩给 x86 开发机） |
| 存储 | `node:sqlite`（Node 内置，无原生模块） |
| 定时/提醒 | durable task 的持久化 sleep，不用 cron |

**一句话**：Pi 提供内核，我们提供「家」——Telegram 脸、沙盒手、记忆、打印、定时，以及"越用越强"的沉淀管道。

---

## 1. 为什么是 Pi（而不是自写 loop 或其它框架）

### 1.1 需求 → Pi 原语映射

| 你的要求 | Pi 里对应的东西 | 备注 |
|---|---|---|
| agent loop 不自维护 | `Harness` + 内置 `pi.generation` / `pi.tool` 任务 | 崩溃可续跑，checkpoint 在每一步 |
| 核心清晰、扩展清晰 | `defineExtension({tools, sections, hooks, wraps, tasks})` + `Registry` | 热替换：`registry.install()` 同名覆盖，进行中的调用保留旧代码 |
| 子 agent | **task-owned conversation**（`ownership:{kind:"task"}`），自带 `subagent` 参考实现 | 上下文隔离，只回传结论；父忙=子忙；可 abort 级联 |
| 长任务 | durable task + `runtime.sleepUntil` + `background:true` | 进程重启后从 checkpoint 继续；重启死循环有 `restartLimit` 熔断 |
| 非 TG 的入口 | durable 的 host 模型：任何 host 都能 `submit()` + `watch()` | TG / CLI / HTTP 只是不同 host |
| 技能/能力扩展 | `sections` + `deferred` 工具 + `tool_search` + **SKILL.md 渐进披露** | 超过 30 个技能时只暴露目录，模型按需加载 |
| 上下文管理 | 自动 compaction（`reserveTokens`/`keepRecentTokens`/`backgroundTokens`）+ token 预算 | 别人踩过的坑，直接用 |
| 供应商无关 | `pi-ai` 统一多 provider + `models.json` | 换模型不改代码 |
| MCP | `@earendil-works/pi-mcp`（官方 SDK 之外的独立客户端） | 需要时接，不是主路径 |

### 1.2 边界：我们**不**做什么

- 不写 loop、不写 SSE 解析、不写 tool_call 增量拼装、不写 compaction。
- 不写 provider 适配（`pi-ai` 覆盖）。
- 不写会话存储引擎（Chord + `node:sqlite`）。
- 不做 TUI（headless host）。
- **不 fork Pi**。需要的行为用 extension / hook 实现；改不动的才提 issue。

### 1.3 代价（诚实清单）

1. `pi-durable` 标注 **Experimental**，API 可能在小版本间破坏 —— 用固定版本 + `src/kernel/` 单点封装来隔离。
2. Node on riscv64 是 **unofficial build**（见 §6）。
3. RSS 约 80–150MB（8GB 机器完全够，但比 Rust 方案重一个数量级）。
4. TS 生态在 SD 卡上 `node_modules` 占空间（约 200–400MB）。

---

## 2. 分层模型

```
┌──────────────────────────────────────────────────────────────┐
│ L3 表面层（可选，互相独立）                                     │
│   pi CLI（调试/编码）  ·  RPC 客户端  ·  MCP server  ·  HTTP API │
└──────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────┐
│ L2 宿主层（我们的代码，每个 channel 一个 host）                  │
│   telegram/  cli/  （未来）http/                               │
│   —— 只做：收消息 → submit()；watch() → 渲染；角色校验            │
└──────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────┐
│ L1 扩展层（我们的代码，装进 Registry）                           │
│   tools: print / sandbox_exec / remember / skill_run / subagent│
│   sections: persona / memory / skill catalog / time            │
│   hooks: approval / role-guard / cost-guard / path-guard       │
│   tasks: reminder / idle-consolidate / skill-propose           │
│   env:   bwrap ExecutionEnv · remote ExecutionEnv(devbox)      │
└──────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────┐
│ L0 Pi 内核（上游，只读依赖）                                     │
│   pi-durable (Harness/Conversation/Task/Extension/Compaction)  │
│   pi-ai (providers) · chord (state/storage) · pi-codemode      │
└──────────────────────────────────────────────────────────────┘
```

**唯一的架构纪律**：`@earendil-works/*` 只允许在 `src/kernel/` 里 import。其余代码依赖我们自己的接口。这样内核可换（退回 `pi-coding-agent` SDK、或未来别的 harness）而不用重写通道与扩展。

## 3. 目录结构

```
chuan/
  package.json                 # npm workspace
  src/
    main.ts                    # bootstrap → config → models → registry → Harness.open → hosts
    config.ts                  # env + config.toml（白名单、provider、限额、角色）
    kernel/                    # 唯一 import @earendil-works/* 的地方
      harness.ts               # open/resume, settings, env selector, report 回调
      storage.ts               # node:sqlite 路径、WAL、备份
      registry.ts              # 安装扩展（含热重载）
      extensions/
        print.ts               # 工具: lp（宿主执行，不进沙盒）
        sandbox.ts             # ExecutionEnv: bwrap
        remote.ts              # ExecutionEnv: pi-env（SSH 到开发机）
        memory.ts              # remember 工具 + 记忆 section + 归纳任务
        reminder.ts            # defineTask + sleepUntil
        subagent.ts            # spawn/wait/close（基于 task-owned conversation）
        skills.ts              # SKILL.md 扫描 → section + 按需加载工具
        guard.ts               # approval gate / role guard / 成本闸
      media.ts                 # TG 图片/文档 → ImageContent
    channels/
      telegram/
        poll.ts                # getUpdates + offset 持久化
        render.ts              # 状态机 + 节流 + 4096 分片 + 转义
        callbacks.ts           # inline keyboard → 审批/技能确认
        index.ts               # host: update → submit, watch → render
      cli/index.ts             # 同一套 host 接口的终端实现
  skills/                      # 用户可编辑的技能包（SKILL.md + scripts/）
  docs/                        # 本文件、运维手册
  deploy/
    chuan.service              # systemd unit（MemoryMax, Delegate=yes, 沙盒 cgroup）
    install.sh                 # Node 24 riscv64 + 依赖 + 目录权限
```

## 4. 关键数据流

### 4.1 一条 TG 消息

```
getUpdates → 鉴权(白名单/角色) → 解析(text/photo/document/callback)
  → conversation.submit({type:"input", content, entry:{kind:"tg.message",...}})
  → 落库即返回（durable：先提交再执行）
  → watchEvents() 产出增量：
       message_update.text_delta → render 缓冲
       tool_execution_start/update/end → 轨迹行
       compaction_start/end → “整理记忆中…”
  → render 每 ~1s 合并一次 → editMessageText
  → agent_settled → 定稿 + 若本轮有产物则附「沉淀为 Skill」按钮
```

- **同 chat 串行**不需要我们实现：conversation 忙时 `submit` 自动进 inbox（`whenBusy: "steer" | "followup" | "reject"`）。这正是 v0.1 要自己写的 per-user actor。
- **崩溃恢复**：进程重启后 `harness.resume()`，未完成的 generation/tool 从 checkpoint 继续；render 用 `watch()` 的初始快照重建，不需要重放。

### 4.2 定时提醒

```ts
defineTask({
  name: "app.reminder", version: 1,
  initial: () => ({phase: "wait"}),
  phases: {
    wait: (t, rt, ctx) => rt.sleepUntil(t.input.at, {phase: "deliver"}, ctx),
    deliver: async (t, rt, ctx) => {
      await tg.send(t.input.chatId, t.input.text);
      await rt.commit(() => ({status: "terminal", outcome: {status: "completed", result: null}}), ctx);
    },
  },
})
```

不起进程、不用 cron、重启不丢、错过时间点自动补。**整个 scheduler 模块消失**。

### 4.3 子 agent（编码/长任务）

```
主对话 tool: subagent(task, profile) 
  → commit() 里创建 task-owned conversation（新 ctx、新 toolset、指定 model）
  → 若 profile.env = "devbox"：该会话的 ExecutionEnv 指向 pi-env 的 RemoteExecutionEnv
  → 父等子结束（durable wait）；子只回传最终结论，中间噪音留在子上下文
  → 失败/取消：父 abort 级联到子
```

- `background: true` 的任务是边界：它拥有的工作不被父 abort 影响，也不让父保持 busy —— 这就是「长任务」的正确语义。
- 子 agent 禁止再派子 agent（防递归）通过 `configure({extensions:{remove:[Subagent]}})` 实现。

### 4.4 技能沉淀（自我进化闭环）

```
用户点「沉淀为 Skill」
  → 读 workspace 里实际成功过的脚本 + 本轮对话
  → 一次 LLM 调用做泛化提炼：去硬编码、参数化、写 SKILL.md frontmatter
  → 在沙盒里跑 examples/ 的样例（dry-run）
      ├─ 通过 → 原子写入 skills/<name>/ → registry.install() → 下一轮即可见
      └─ 失败 → 写 drafts/，回执失败原因
```

Pi 的 `registry.install()` 同名覆盖 + 「进行中的调用保留旧代码」正好是热加载的语义，不需要自己实现扫描/重载。

## 5. 通道抽象

Pi 的 host 模型天然是 channel 无关的：**host = 提交输入 + 消费事件**。

```ts
interface Host {
  onSubmit(): SubmitTarget          // 收消息 → conversation
  render(events: SessionEvent[]): void
  capabilities: { edit: boolean; buttons: boolean; markdown: boolean }
}
```

- `telegram/`：`edit:true, buttons:true, markdown:false(流式)`。
- `cli/`：直接 stdout 打印 delta，无节流。
- 未来 `http/`：SSE 出口，供 Web 或脚本调用。

降级策略：无编辑能力的通道只发终稿；有编辑能力的按 `latest-wins` 合并。**事件分级**（delta 可丢 / 终稿必达）是 v0.1 没有、但现在必须明确的一条。

## 6. 目标平台：SBC / riscv64

### 6.1 事实基线（已核实）

| 项 | 事实 |
|---|---|
| CPU | RISC-V SBC，8×X60，RVA22 + RVV1.0；单核约为现代 x86 的 **3–8%** |
| 系统 | 目标板发行版 2.x ≈ Ubuntu 24.04 基因（glibc 2.39 / systemd 255 / 内核 6.6） |
| Node riscv64 | 官方**不支持**；`nodejs/unofficial-builds` 有 **v24.22.1**（2026-10-21，LTS）与 v22.23.4 |
| Pi 依赖 | 全部纯 JS/WASM（`photon`、`quickjs-wasi`），**无原生模块** → 无需在板上编译 |
| 存储 | `node:sqlite`（Node 内置），`pi-durable` 的 SQLite 后端直接可用 |
| 性能含义 | LLM 必须远程；本机只做编排/IO —— 这不是妥协，是唯一正确做法 |

### 6.2 部署形状

```
SBC (Node 24 LTS riscv64)
 ├─ chuan 宿主（Pi 内核 + 扩展 + TG host）
 ├─ SQLite（state/chuan.db）+ 技能目录 + workspace
 └─ 沙盒子进程（bwrap）
        │ SSH（pi-env daemon，官方支持 x86_64）
        ▼
开发机 x86_64：编码子 agent 的 ExecutionEnv / 编译 / 重任务
```

- **重活不上板**：SBC 只跑编排与本地 IO；编码子 agent 的 `cwd` 与 `env` 指向开发机。
- `pi-env` daemon 是纯 Rust（仅 `encoding_rs/ignore/notify/serde_json/libc`），在 x86_64 开发机上是官方支持路径，**不需要 riscv64 版本**。
- 若将来要在板上跑远程环境 webserver，daemon 交叉编译到 riscv64 的成本也很低（那是唯一需要 cargo-zigbuild 的地方）。

### 6.3 红色风险（上板前 30 分钟 POC）

| # | 项 | 判据 |
|---|---|---|
| P1 | **目标板发行版 的 bwrap**（Ubuntu 24.04 基因可能启用 `apparmor_restrict_unprivileged_userns`） | `bwrap --unshare-all --ro-bind / / true`；失败则改 systemd 单元 + `AppArmorProfile` 或退回 sandbox-runtime 的替代后端 |
| P2 | Node 24 riscv64 跑 Pi | `node -e "require('node:sqlite')"`；起一个最小 durable harness + 一次真实 generation |
| P3 | SQLite 写放大（SD/eMMC） | `partialIntervalMs` 调到 300–500ms；WAL 落 tmpfs 或 `wal_autocheckpoint` 调优 |
| P4 | TG 429 实测 | 30 次/30s 编辑同一条消息，记录 `retry_after` |
| P5 | 沙盒内 Python + 中文字体 | `python3 -c "import PIL"` + `fc-list :lang=zh`（用 apt 包，不用 pip） |
| P6 | `lp` 可达 | 宿主进程调 `lp` 成功、用户属 `lp` 组；**沙盒内不调** |

## 7. 沙盒与权限

- **实现形态**：`ExecutionEnv`（Pi 的抽象），把 `exec` 全部导到 bwrap；文件读写走宿主路径 jail（canonicalize + 前缀校验），`bash` 走沙盒。
- **能力留在宿主**：`lp`、TG、LLM、文件下载。沙盒只产出文件。
- 内存/进程限制：systemd unit `Delegate=yes` → 子 cgroup `memory.max=512M`、`pids.max=64`；rlimit 兜底 `NPROC/NOFILE/FSIZE`；超时 `killpg`。
- 网络：默认断；技能 `SKILL.md` 声明 `network: true` 才放行，沉淀时向用户明示。
- 角色：`owner` 全量工具；`member` 只给只读工具（`modelTools` 裁剪 + `beforeTool` 兜底），这是两行配置级别的隔离，不是权限子系统。

## 8. 记忆

- **存储**：durable 的 document（`tx.doc`/`tx.entry`）而不是另起一张表；与 transcript 同事务提交。
- **写**：turn 内只在两种情况下写 —— 用户显式「记住」，或模型调 `remember` 工具。零额外 LLM 调用。
- **整理**：`idle-consolidate` task，条件 = 无运行中 run ≥10min 且 未归纳消息 ≥6 条 且 距上次 ≥30min；输入是水位线后的原始 entry，输出严格 JSON 操作集（`upsert/supersede/expire/noop`）。
- **双时态**：`(scope, subject, predicate)` 为键，新值 supersede 旧值（写 `valid_to`），不覆盖；与 pinned 冲突进 `conflicts`，下一轮问用户。
- **注入**：pinned 全量 + recency/FTS top-15，带 `[fact:ID]`；推测类前缀「（推测）」。
- **纠错**：`/memory`、`/forget`。

## 9. 技能体系（SKILL.md）

沿用 Agent Skills 的三级披露，这与 Pi 的做法一致：

| 级别 | 内容 | 注入时机 | token 成本 |
|---|---|---|---|
| L1 | `name` + `description`（≤1024 字符，必须写清"做什么 + 何时用"） | system prompt 的 skill catalog section | ~100/技能 |
| L2 | `SKILL.md` 全文（建议 <5000 token） | 模型判断命中后用 `read_skill` 工具读取 | 按需 |
| L3 | `scripts/`、`references/`、`assets/` | 只在执行/引用时读，**代码不进上下文** | 0 |

- 技能 ≤30 个全量暴露 L1；>30 走 Pi 的 `deferred` + `tool_search`。
- 沉淀闸门（dry-run）是**唯一**防止技能腐烂的手段，不可省。

## 10. 风险登记

| # | 风险 | 等级 | 处置 |
|---|---|---|---|
| R1 | `pi-durable` 破坏性变更 | 高 | 固定 1.4.x；`src/kernel/` 单点封装；升级前跑契约测试 |
| R2 | Node riscv64 非官方构建、安全更新滞后 | 高 | 固定版本 + 校验和；跟踪官方 riscv64 支持进展；保留「内核外置到 x86」的退路 |
| R3 | 目标板发行版 限制 userns 导致 bwrap 不可用 | 高 | P1 先验；备选 sandbox-runtime / systemd 沙箱 / 独立低权用户 + Landlock |
| R4 | SD 卡写放大（100ms 一次 partial commit） | 中 | 调 `progress` 间隔；日志与 WAL 分离；每日 `VACUUM INTO` 备份 |
| R5 | TG 限流 / 半截 Markdown | 中 | 单 chat 单 editor、纯文本流、`retry_after` 全局退避 |
| R6 | 技能污染与工具膨胀 | 中 | dry-run 闸门、drafts 隔离、失败 3 次降级、deferred 工具 |

## 11. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| M0 | 板卡基线：Node 24 riscv64 + bwrap + cgroup + Node SQLite | P1/P2 全绿，`/status` 能报版本 |
| M1 | 最小宿主：TG 长轮询 + 一个 conversation + 流式编辑 | 一条消息能实时编辑到最终态，重启不重复消费 |
| M2 | 扩展骨架：sandbox env + print + reminder + 角色门 | 恶意脚本只伤自己；提醒重启后照常触发 |
| M3 | 记忆 + 技能目录 + 子 agent（本机） | 子 agent 能跑完并只回传结论 |
| M4 | 远程子 agent（pi-env → 开发机）+ 技能沉淀闸门 | 沉淀一个技能并在下一轮直接命中 |
| M5 | 加固：崩溃恢复演练、成本闸、`/memory /forget /status /cancel`、备份 | kill -9 后自动续跑 |

## 12. 待定

1. `pi-durable`（强）vs `pi-coding-agent` SDK（稳）最终二选一 —— 建议先用 durable 做 M1 验证，踩到硬限制再退。
2. 是否在板上保留 `pi` CLI 作为编码通道（会多一份 node_modules，但也是最好的自检手段）。
3. 模型供应商与预算（影响 compaction 阈值与子 agent 的模型档位）。
4. 家人成员的会话是否共享同一个 root conversation（推荐：每人一个 conversation，共享扩展与技能）。
