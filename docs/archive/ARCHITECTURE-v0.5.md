# 家庭智能管家 Harness — 架构设计 v0.5

> 约束：**Rust** · **RISC-V SBC（riscv64 / 8GB / 无第二台机器）** · 无中间件 · Telegram 主通道，CLI 并级。
> v0.4 经过一轮批判性评审后修订。**变更核心：引擎不再依赖 rig 的 loop，改为自持 `TurnDriver` + 自写 OpenAI 兼容客户端。** 理由见 §2。

---

## 0. 本轮修正的关键事实（含我上一版的错误）

| # | 修正 | 影响 |
|---|---|---|
| 1 | **`AgentRun` 在 `rig-agent`，不在 `rig-core`**。v0.4 那 232 crates 的实测**不包含 loop 引擎** | P5 的前提原本没验证过 |
| 2 | 实测：`rig-agent 0.45`（`default-features=false`）= **116 crates**，含 `rig-memory`，无 TLS | 加上 rig-core 才是完整可用面 |
| 3 | `AgentRun::with_validated_history()` 存在，文档明说用于"历史来自外部存储" | "能否重建" = **能，但只到 turn 边界** |
| 4 | 中途恢复不成立：`next_step` 的恢复语义只对**持久化过的 run** 有效，`new()` 一律从 `PreparingRequest` 起 | 崩在工具执行中 → 该批调用不可恢复，只能回填 `NO_RESULT_PROVIDED` 再花一轮重新规划 |
| 5 | 重建会**归零所有防护计数器**：`usage`、`completion_calls`、`invalid_tool_call_retries`、`max_turns`（默认 1） | 重启循环可无限花 API 调用 |
| 6 | rig 的非法调用修复会向历史插入 corrective feedback（`synthetic` 消息） | journal 若只存语义对话，重建后模型会重犯同一错误 |
| 7 | 实测：`rig` 门面 + `agent` feature = 655 crates（lancedb/fastembed/datafusion/rav1e） | 门面否决，与 `rig-agent` 是两回事 |

## 1. 实测矩阵

| 组合 | crates | 结果 |
|---|---|---|
| `reqwest 0.12 + ring + rusqlite(bundled)` + tokio | 151 | ✅ 14.5s，4.0MB，合法 riscv64 ELF |
| `rig-core 0.45`（reqwest 0.13 + aws-lc-sys 0.46） | 232 | ✅ 编译 + 链接通过 |
| `rig-agent 0.45`（无 transport） | 116 | ⬜ 仅生成锁文件，未编译 |
| `rig` 门面 + `agent` | 655 | ❌ 不可用 |

## 2. 引擎选型：自写客户端 + 自持 TurnDriver

**先纠正一个逻辑：在"journal 是唯一真相"的前提下，loop 从来就买不到。**

- rig 的 `AgentRun` 只能从 turn 边界重建，中途状态（`ExecutingTools(ToolBatch, TurnPolicy)`）拿不回来；
- 重建后计数器归零，成本与反滥用语义丢失；
- 它的修复产物（synthetic feedback）必须一并入 journal，否则模型重犯。

也就是说：**用 rig 的 loop，我们仍要自己处理恢复、预算、重试计数——省下的只有 SSE 解析和 turn 推进那 400 行，代价是每年 8–15 人日的升级税**（过去 12 个月 22 个 minor 版本、167 条 breaking；`rust-version 1.97.1 + edition 2024`；每次升级都要重验 riscv64 交叉编译）。而且 0.45 的 headline 恰好是 "runs survive a restart at any point" ——**它最不稳定的地方正是我们要依赖的语义**。

**结论：`engine` 自写，约 300–400 行**，范围严格限定为：

- 构造请求（messages / tools / tool_choice / stream）；
- 解析 SSE：`text` / `reasoning_content` / `tool_calls[].index-arguments` 分片；
- 产出 `usage` / `finish_reason`；错误分类与重试（宿主侧计数）；
- **不做**：turn 编排、工具执行、历史管理、压缩。

依赖仍是已验证的 `reqwest 0.12 + ring`（151 crates），顺带甩掉 aws-lc。

**保留退路**：`engine` 实现 `TurnDriver` trait。若半年后发现自写是负担，rig wire 层或其它客户端可作为第二个 impl 接入，上层零改动。

## 3. kernel 的 trait 形状（turn 粒度，零 rig 类型）

```rust
pub struct ToolCall { pub id: CallId, pub name: String, pub args: serde_json::Value }
pub enum Step {
    Model { turn: u32 },
    Tools { calls: Vec<ToolCall> },
    Done  { text: String, usage: Usage },
}

#[async_trait]
pub trait TurnDriver: Send + Sync {
    /// history 是 journal 原始行；Message[] 构造与 repair 全在 engine 内完成
    async fn begin(&self, spec: TurnSpec, history: &[HistoryRow],
                   tools: &[ToolDef], sink: EventSink) -> Result<Box<dyn Turn>, Fail>;
}

#[async_trait]
pub trait Turn: Send {
    async fn step(&mut self) -> Result<Step, Fail>;          // 请求下一轮
    async fn on_tool_result(&mut self, r: ToolResult) -> Result<Step, Fail>;
    fn snapshot(&self) -> Snap;  // turn / usage / budget_left → 落 journal
}
```

**为什么不用 `ModelClient`**：那层抽象要么把 rig 的 `Message`/`StreamEvent`/`TurnPolicy` 全量复制进 kernel（1200–1800 行转换，每个版本被动改一次），要么让 kernel 直接依赖 rig（`cargo metadata` 守卫失效，单测锁死 rig 版本）。turn 粒度是唯一能让"可测试"与"守得住"同时成立的切法。

## 4. journal = 协议历史，不是语义对话

```sql
messages(seq, turn_id, kind, payload_json, provider_meta, created_at)
-- kind ∈ { user, assistant, tool_result, synthetic }
-- assistant.payload 完整保留 tool_calls
-- synthetic 存 corrective feedback / NO_RESULT_PROVIDED / 摘要块，渲染层按 kind 隐藏
tool_calls(call_id, turn_id, name, args_json, status, effect, idem_key, result_preview)
attempts(turn_id, attempt_no, model, prompt_tokens, completion_tokens, cost_est, created_at)
turns(id, chat_id, user_id, status, lease_until, started_at, ended_at)
approvals(id, call_id, idem_key, decision, expires_at, consumed_at)
tasks / facts / reminders / kv
```

**验收标准**：任意崩溃点恢复后，engine 的 `repair()` 返回 **0 处修复**，且模型不重犯同一非法调用。

## 5. 恢复语义（崩溃点 × 可恢复性）

| 崩溃点 | 恢复动作 |
|---|---|
| 用户消息已落库、未发请求 | 直接开始 |
| 流式中 | 丢弃半截文本，重发该轮（`Pure`） |
| assistant 已提交、tool 未执行 | 从 tool_calls 重新执行 |
| **tool 执行中** | 按 `effect` 决定：`Pure` 重跑 / `Idempotent` 核对后重跑 / `External` 标 `unknown` |
| assistant 已出终稿、TG 未发 | **直接重发**（文本在库里），带「↻ 上次未送达」前缀 + `[不必重发]` 按钮 |

**为什么改**：v0.4 把"终稿发送"也当 `External` 一律不问就标 unknown，会让用户收到"你收到我刚才的回答了吗"——而答案就在库里。这里降级为「至少一次 + 可见去重」，成本 < 50 行。

## 6. 副作用与幂等

| 级别 | 例子 | 规则 |
|---|---|---|
| `Pure` | 读文件、搜索 | 无条件重跑 |
| `Idempotent(key)` | 打印 | **派发前**先落 `dispatching` 行；核对用 `lpstat -W completed -o <printer>` **按 title 过滤**；窗口外（CUPS `PreserveJobHistory` 默认上限 500）一律 unknown，**禁止自动重发** |
| `External` | 发消息到群 | at-most-once；重规划生成的**新 call_id 不得绕过幂等** |

**审批幂等键 = `hash(tool_name + 规范化 args)`，与 `call_id` 解耦**——否则崩溃后重新规划会生成新 call_id，同一个"发报告到家庭群"被批准两次、执行两次。

## 7. crate 与目录

| crate | 职责 | 允许依赖 |
|---|---|---|
| `kernel` | TurnDriver/Turn/Step、Event、Tool、Effect、StateStore、TaskQueue、Approval | serde, tokio, async-trait, thiserror, tracing |
| `engine` | **唯一** HTTP/SSE/provider 代码 | reqwest, rustls(ring), serde |
| `store` | **唯一** rusqlite + 迁移 + 单写者 actor | rusqlite |
| `capabilities` | bwrap、skills、mcp、print、media | kernel traits |
| `channels` | **唯一** teloxide + CLI | teloxide |
| `app` | host / scheduler / config / bin | 全部 |

CI 守卫：`cargo metadata` 检查 `kernel` 依赖树里不得出现 `reqwest`/`teloxide`/`rusqlite`/`rig-*`。

```
src/{kernel,engine,store,capabilities,channels,app}/
skills/<name>/SKILL.md + scripts/
workspaces/<turn_id>/{in,out,scratch}
```

## 8. 通道

```rust
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    fn caps(&self) -> Caps;                 // edit / buttons / markdown / max_len
    async fn run(&self, tx: mpsc::Sender<Inbound>, cancel: CancellationToken) -> Result<()>;
    async fn send(&self, to: ChatId, out: Outbound) -> Result<MsgRef>;
    async fn edit(&self, msg: MsgRef, out: Outbound) -> Result<()>;
    async fn ack_callback(&self, cb: CallbackRef, text: &str) -> Result<()>;
}
```

- **事件禁止携带大 payload**：`EventC` 只带 `turn_id + seq + 引用`，正文从 snapshot 取。否则 `broadcast(1024)` 是**按条数**算容量，一个滞后订阅者可钉住 1024 × 64KB ≈ 64MB，且按订阅者放大。
- `TextDelta` 可丢；`Lagged` → snapshot 重同步；终稿落库必达。慢 transport 不得反压 loop。
- Renderer：1.2s 节流、4096 分片、流式期纯文本、轨迹折叠、429 `retry_after` 全局退避。
- **入站过期丢弃**：按 `message.date` 丢弃 >5min 的更新并回一条「刚才掉线了」；否则断网恢复后全家会收到一串过期回复。
- 白名单静默丢弃；callback 校验 user + 单次 approval；`callback_data ≤ 64B`；offset 落库 + `update_id` 去重。

## 9. 沙盒

- bwrap：`--unshare-all --die-with-parent --new-session --cap-drop ALL`；`/usr /lib /lib64 /etc` 只读；`workspaces/<turn_id>` 可写。
- 资源：systemd `Delegate=yes` + 子 cgroup `memory.max=512M`、`pids.max=64`；rlimit 兜底 `NPROC/NOFILE/FSIZE`；超时 `killpg`；输出各限 64KB。
- **孤儿回收**：`KillMode=control-group` 杀不到嵌套 cgroup，`--die-with-parent` 管不住 `setsid` 出去的孙进程。启动时扫描 `/proc/*/environ` 里的 `CHUAN_TURN=` 标记并清理。
- 网络默认断；`SKILL.md` 申请 + 全局域白名单 + owner 审批三者同时满足才放行。
- 特权能力留宿主：`lp`、TG、LLM、文件下载。

## 10. 技能：纯函数，不自我授权

**这是本轮最重要的一处改动。** v0.4 让 `SKILL.md` 全文在命中后进上下文——那是一段由 LLM 撰写、经泛化提炼、直接进入决策层的散文，**沙盒对它完全无效**（破坏发生在模型决策层，不是进程层）。

| 级别 | 内容 | 规则 |
|---|---|---|
| L1 | `name + description` | 进 system prompt 目录（~100 token） |
| L2 | **仅结构化 manifest**：参数 schema、脚本 `--help` 的机器摘要 | 正文**永不进上下文**；需要细节由模型 `read_file` 取，且以「不可信数据」分隔符包裹 |
| L3 | `scripts/` 执行 | stdin JSON → stdout JSON；代码不进上下文 |

**结构性约束（比任何检测都有效）**：
1. 技能是**纯函数**：workspace in → workspace out。**永远拿不到打印/发送/删除的直连权限**，副作用只能"产出文件 + 请求宿主执行"。
2. **权限不可自授**：`network` 由 manifest 申请 + 全局策略上限 + owner 在审批界面看 diff 才能放行。
3. 新技能的 `External` 副作用默认**逐次审批**，成功 N 次后才降级。
4. 保留 `drafts/` 与回滚（保留上一版）。

**接受残余风险**：「描述文本里嵌一句看似无害的偏好」没有可判定的检测。所以约束的是**后果**而非意图。

## 11. 记忆

- `facts(scope, subject, predicate, value, kind, confidence, valid_from, valid_to, source)`；scope = `user:<id>` | `family`。
- 写：仅「用户显式记住」或模型调 `remember`，零额外 LLM 调用。
- 整理（MVP 后）：idle ≥10min 且未归纳 ≥6 条 → 后台 task 输出严格 JSON 操作集；双时态 supersede 不覆盖；与 pinned 冲突进 `conflicts`。
- 注入：pinned 全量 + FTS5 top-15，带 `[fact:ID]`；推测前缀「（推测）」。
- 纠错：`/memory`、`/forget`。

## 12. 多 agent 与长任务

| 形态 | 用途 | 语义 |
|---|---|---|
| 前台子 agent | 读 100KB 回 2KB 的探索任务 | 父 turn 内 blocking tool，60–120s，只回传结论 |
| 后台 task | 长脚本、技能 dry-run、`cargo check` | durable 行 + lease，返回 `task_id`；可查询/取消/续跑 |

- 拓扑单层，禁止子 agent 再派。
- **并发按 lane 而非按数量**：`io=2`、`compile=1`（`nice 19` + `cpu.weight=20`）、`external=1`；tokio `worker_threads=4`。
  理由：真正稀缺的是 CPU（整板 ≈0.3–0.6 核）。两个 slot 都跑编译会把 tokio timer 与 TG 长轮询饿到秒级抖动，"首字 <1.5s"直接不达标。
- 写操作串行；并行只用于只读。

## 13. 运维（全部前移到 M1）

| 项 | 做法 |
|---|---|
| 迁移 | `PRAGMA user_version` + 前向迁移；改 schema 后旧库自动升级 |
| 备份 | 每日 `VACUUM INTO backup-YYYYMMDD.sqlite`，保留 7 份 |
| WAL 膨胀 | `journal_size_limit` + 定期 `wal_checkpoint(TRUNCATE)`；启动 `PRAGMA quick_check` |
| 崩溃告警 | `OnFailure=` 发 TG 给 owner；10 分钟内 3 次进入降级模式 |
| 日志 | journald + `SystemMaxUse=200M`，不自己写文件 |
| 成本 | `attempts` 表 + 日/月硬上限；超限直接拒绝 |
| 离线 | 断网恢复后丢弃过期更新（§8） |

## 14. RISC-V

| 项 | 事实 / 处置 |
|---|---|
| 目标 | `riscv64gc-unknown-linux-gnu`，`cargo zigbuild --target ...2.39` |
| 已实测 | `ring` ✅、`rusqlite(bundled)` ✅、`aws-lc-sys` ✅（本方案不再需要） |
| 常驻内存 | 估计 35–55MB RSS（tokio+reqwest+rusqlite+teloxide）；二进制 10–18MB |
| 板上依赖 | apt：`python3-pil`、`python3-reportlab`、`fonts-noto-cjk`、`cups-client`、`bubblewrap` |
| 验证 | qemu-user-static + binfmt 做 smoke；最终以实机为准 |

## 15. 风险

| # | 风险 | 处置 |
|---|---|---|
| R1 | 自写 SSE 客户端踩 provider 方言 | 用录制的真实响应做 fixture 测试；只支持 OpenAI 兼容面；`dialect` 开关（`reasoning_content`/`max_tokens`） |
| R2 | 崩在工具执行中，语义只能近似恢复 | 副作用分级 + 幂等键；`External` 宁可不做也不重做 |
| R3 | 目标板发行版 限制 userns → bwrap 不可用 | 上板 30 分钟验证；Landlock+seccomp 兜底 |
| R4 | 技能 = 持久化提示注入 | 纯函数 + 权限不自授 + L2 只进 manifest（§10） |
| R5 | 单板编译慢拖垮交互 | lane 化并发 + `nice`；长任务一律后台 |
| R6 | 成本失控（重试循环） | `attempts` 表 + 宿主自限 + 硬上限 |

## 16. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| M0 | 交叉编译 + 迁移/备份骨架 + bwrap + TG 长轮询 | 交叉产物上板跑通；bwrap 自检过 |
| M1 | 白名单 / 会话 / 流式编辑 / 工具轨迹 / 取消 / **运维三件套** | 首字 <1.5s；`kill -9` 循环触发告警 |
| M2 | 沙盒 exec / CUPS / 提醒 / 图片 | 恶意脚本只伤自己；不重复打印 |
| M3 | journal + 崩溃重启 + 技能热加载 + `run_skill` | 任意崩溃点恢复后 repair=0；新 skill 下一轮可见 |

## 17. 未决

1. 模型供应商与预算（影响 `attempts` 上限与压缩阈值）。
2. 家人成员的会话是否共享一个 root conversation（推荐：每人一个，共享技能与记忆的 `family` scope）。
3. 是否需要 HTTP 通道（当前只做 TG + CLI）。
