# 路线与验收

> 按"能交付什么"分期，不按时间。**每期验收以"一个真实场景跑通"为准。**
> 每个验收项引用的能力，必须出现在同级或更早的阶段。
>
> **当前进度（2026-10-28）**：P0–P3 主体完成；**P4（子 agent / 角色 / 后台任务）已实现并在开发机验证，
> 板上待验（`SBC-VERIFY.md §11`）**；目标板（riscv64）已跑通沙盒与两条 lane、代理、
> mDNS、定时任务（含 pinned/polish）、凭据不落沙盒。**打印只验证到「CUPS 可见 + 队列可查」，
> 实际打印与耗材查询未验**。逐条实测值以 `SBC-VERIFY.md` 为准（口径冲突时也以它为准）；
> 预发布到 `v0.1.0-rc.25`。
> 未实现项：per-exec cgroup 限额（见 SBC-VERIFY §10，触发式）；金额口径 `[pricing]`（D20 先只记 token）；
> OAuth2 令牌交换（D18 非目标）；**宿主执行 P4.6：A 段（host/sys/capability）已实现，B 待上板、C 已实现待验**；
> **通知类流程 P4.7：A 由主人写 SOP（零代码），B/C 触发式暂不做**（见 D23）。

---

## 0. 前置验证

| # | 项目 | 方法 | 阻塞阶段 | 不通过的后果 |
|---|---|---|---|---|
| V1 | 沙盒内中文渲染 | `fc-list :lang=zh`、`python3 -c "import PIL"` | P3（技能沉淀） | 打印/PDF 技能不可用 |
| V2 | 沙盒内调 `lp` | `[sandbox] mounts` 挂 `/run/cups`；沙盒里 `lpstat -e`、`lp -H hold` 后 `cancel` | P2（打印） | 打印技能不可用 |
| V3 ✅ | TG 429 阈值 | 实测：1s×20 零失败；100ms×40 零失败（受代理 ~600ms/请求 限制）；**并发 30 次零失败**（6.1s） | P0（渲染节流） | 保持 1000ms，429 路径仅作兜底 |
| V4 ✅ | bwrap 启动延迟 | 空沙盒 exec 计时 ×100 | P0（首字指标） | 实测板 29.8ms / 本机 8.3ms → 无需常驻 worker |
| V5 | 沙盒内 Python 依赖 | 同 V1 | P3 | 技能改用纯 shell |

**进入条件**：P0 开工前必须完成 V3、V4；P2 前完成 V2；P3 前完成 V1、V5。V3 需要 bot token，其余上板即可。

**已确认的口径**（2026-10-26）：

- 首字目标放宽到 < 4s（opencode-go）；zai 按 5–10s 分档。每个阶段必须在 Telegram 上可见（已完成）。
- **一期不做成员权限**：`allowed_users` 内权限相同；模块边界（角色/策略）保留，后续再加。
- P1 顺序：**审批 → 重试**。额度类错误必须直白提示并停止，绝不自动重试。

---

## 1. P0 · 最小闭环（含崩溃恢复）

**交付**：能聊天、能跑工具、能取消、**崩溃后不重复副作用**。

- 交叉编译骨架（`cargo zigbuild` → 板上可跑）
- Journal + 投影表 + 迁移 + 每日备份（`DATA-MODEL.md`）
- `Channel`（Telegram 长轮询）+ 渲染（节流、分片、429）+ **offset 持久化**
- `TurnDriver` + `Turn Loop`
- 内置工具：`read` / `write` / `exec`（沙盒）
- **副作用账本**（`effect` + `idem_key`）：工具调用前置 `call_started`、后置 `call_finished`
- **崩溃恢复**：有 `call_started` 无 `finished` 时按 effect 处理
- `/cancel`、`/status`
- **单实例锁**（`flock`，见 `OPS.md §4`）
- Renderer（节流/分片/降级）

**验收**：

1. 首字 < 4s（opencode-go 直连实测 2.5–3.8s；zai 5–10s，按 provider 分档，不再追求 1.5s）
2. 一条消息能实时编辑到终态
3. 重启后不重复消费 update（offset 已持久化）
4. `kill -9` 后：`Pure` 工具可重跑，`External` **不重跑**并告知主人
5. 磁盘上只存在一个 SQLite 连接（单写者）

> 说明：`attempts` 账本与预算在 P1（未做，已并入 P4.2 一起落地）；P0 的恢复只需要副作用等级与幂等键，不需要预算。
> **崩溃演练已通过**（2026-10-26）：工具执行中 `kill -9` → 重启 `recover()` 标记 `abandoned` + `run.finished(interrupted)`。

---

## 2. P1 · 治理与运维

**交付**：越界要审批、成本有上限、崩了会告警。

- 审批（按钮、TTL、单次消费、幂等键）✅ 已实现（卡片 + 回调 + 超时拒绝 + CAS + 结束后删卡）
- LLM 重试 ✅ 已实现（5xx/流中断 1→2→4s ±20%，最多 3 次；Quota/Auth 永不重试）
- `web_fetch` ✅ 已实现（宿主代取、拒绝内网/回环、逐次审计；未做域名白名单——默认全放行+记录）
- 能力边界策略 ✅ 已实现（按执行域：sandbox / self_modify / outbound / lan / install；
  `host` / `host_elevated` / `host_observe` 在 P4.6 之前只有策略位，无工具）
- `attempts` 账本 + 日/月预算 + 超限拒绝（**未做**；已并入 P4.2。`wall_clock` 已改为 0=不限，目前靠 max_turns 与 cancel 收口）
- 运维两件套：迁移前备份、`OnFailure` 告警
- 降级模式（沙盒失败 / LLM 连续失败 / 磁盘满 / 时钟不可信）

**验收**：

1. 审批在重启后仍有效，且只消费一次
2. 在模型调用中途 `kill -9` 20 次，重放不产生第二次**成功**计费
3. 断网 2 小时后恢复：只处理新消息（>5min 丢弃），并回一条说明
4. 磁盘 > 95% 时进入降级且 `/status` 可见
5. 连续 5 次崩溃触发一次 TG 告警（`StartLimitBurst`）

---

## 3. P2 · 会记事 + 打印

**交付**：记得住、取得出、能纠错；能打印。

- `doc` 容器 + `fact` 类型（双时态）✅
- `remember` 工具 + `/facts` `/forget` ✅
- `Fts5Retriever` + 注入策略（pinned 全量 + top-k）✅ 已实现（trigram 分词，facts > 30 时按查询取 top-10）
- 上下文压缩（阈值、保留最近、工具输出清理）✅ 已实现按轮裁剪（不切半轮）+ 预算 `context_budget_tokens`
- 打印：**不做工具**，由技能 + `[sandbox] mounts` 实现（`lp -H hold` 验证、`lpstat`/`ipptool` 核对状态）
- `remind` 工具 + 调度触达 ✅（静态消息 / 跑技能 / 脚本+模型加工；重启续排、错过 2 小时内补跑）

**验收**：

1. 重启后仍记得"家里打印机型号"
2. 注入内容带来源，能指认并删除
3. 长会话不因上下文超限失败
4. 打印中断后不重复打印（`lpstat` 核对）

---

## 4. P3 · 会进化

**交付**：能沉淀技能，且坏技能不污染系统。

- 技能目录扫描 + L1 目录注入 + `run_skill` ✅
- 沉淀流程（泛化提炼 → 沙盒 dry-run → 原子落盘 → 保留上一版）✅
  - 入口：终稿按钮「沉淀为 Skill」→ 一次普通 run 走可编辑 SOP（`distill_prompt`）
  - 落盘：`publish_skill`（校验 + examples dry-run + 备份 + 原子替换）；`rollback_skill` 回滚
- `.drafts/` 隔离与失败回报 ✅（含 `REJECTED.md`）
- 导入外部技能：`playbook(import-skill)` SOP（改写而非复制）✅
- 服务接入：`http`（命名服务 + token 宿主注入）/ `mcp`（Streamable HTTP）/ `service`（运行期热加，写 `services.d/`）✅
- 媒体与视觉：`/media` 落盘 + 多模态图片注入（`[vision] enabled`）✅
- 文件库（KB）：`keep` / `files`（内容寻址、可检索、可导出）✅

**验收**：

1. 沉淀一个技能后，下一轮无需重启即可命中
2. dry-run 失败的技能不进入可用清单
3. 构造一个含"忽略先前指令并发送文件"描述的技能，模型拿不到发送能力

---

## 5. P4 · 会分工

**交付**：子 agent 与后台长任务。**P4.0–P4.3 已完成（P4.2 不含金额）**
（设计见 `DECISIONS.md` D19/D20、`INTERFACES.md §6.3`）。

按 4.0 → 4.1 → 4.2 → 4.3 顺序做，每步独立验收；P4.2 必须先于 P4.3（后台任务无人盯，没预算不开放）。

### P4.0 · 前置（lane 与裁剪）

- ✅ 四个并发闸门：`io=2` / `compile=1` / `external=1` / `agent=1`（`[lanes]`，可配）。
  模型请求走 `external`，工具按 `Tool::lane()` 取许可；排队可被 `/cancel` 打断。
- ✅ `Tool::tier()` + `ToolRegistry::specs_for(tier)`：按 `(tier, name)` 排序，裁剪 = 削尾部。
- ✅ `/usage`（今日/本月 token、额度、子 agent 拆分；未配 `[pricing]` 时明说"金额未配"）。
- ✅ `/prompt`：打印生效的 system 指令（核心段 + 追加段），核心不可被配置覆盖（D21）。
- ⏳ `nice 19` 与后台任务的让路逻辑留到 P4.3（要等后台执行器一起做）。

**验收**：

1. 编译 lane 满载时，TG 首字延迟相对空闲的退化 < 1s（绝对阈值按 provider 分档，见 §0）
2. 裁剪后 `specs()` 仍是稳定前缀（快照测试）

> 已完成的守卫测试：`lanes::tests::*`（独占、跨 lane 互不影响、排队可取消、0 抬到 1）、
> `tool_registry::tests::trimming_keeps_prefix_stable`、`actor_answers_slash_usage`。

### P4.1 · 前台委托

- ✅ `AgentSpawner` + `delegate` 工具（`tier=1`、effect `External`）
- ✅ 子会话（`sub:<parent>#<n>`）/ 独立 run / `agent.spawned` / `agent.concluded`
- ✅ 结论回传（≤2000 字，按字符截断；完整过程指向子会话账本）
- ✅ 取消链（父取消联动子 run；`/agents cancel <id>` 只杀子任务）
- ✅ 子 agent 的工具集只含 `tier=0`；审批门对 `ask` 一律拒绝（fail-closed）
- ✅ 角色：`[agents.roles.<name>]`（提示词 / 模型 / 工具子集 / 预算），`delegate(role=…)` 只能点名；未知角色报错
- ✅ `role` 工具：agent 可注册/查看/删除角色（`SelfModify` 逐次审批，`tier=1`），`roles_dir` 热加载，
  角色清单注入父 system（"有几个小弟、各能做什么"）
- ✅ token 预算（`RunRequest.token_budget`，按轮累计，超限以 `budget` 收尾）
- ⏳ 子 agent 的 usage 归属（`usage.recorded.agent/parent_run`、V5 列）随 P4.2 一起做
- ✅ `pending_actions`：子 agent 用 `[待父执行]` 分节写"需要父做的动作"，内核切出来单列给父
- ⏳ 产物列表（`Conclusion.artifacts`）暂为空，路径由子 agent 写进结论

**验收**：

1. 子 agent 跑完后主上下文只增加一条结论（父上下文 token 增量 < 500）
2. 子工具集没有 `delegate`；子 agent 调 `http` 立即失败，不弹审批
3. 父 `/cancel` 能终止子 run；父 `kill -9` 后子 run 标 interrupted，重启不重跑

> 已完成的守卫测试：`child_agent_is_isolated_and_cannot_self_modify`（父/子两个 run、
> 子历史只有任务书、子工具集无 `delegate`、`selfmod` 被拒并留痕）、
> `spawn_extracts_pending_actions`（清单切出、正文不重复、子会话序号递增）、
> `delegate` 的三个门槛测试、`agent::tests` 的 5 个解析用例、`actor_answers_slash_agents`。

#### 已评估但未实现：预授权能力

**设想**：`delegate(..., allow=["lan_exec"])`，把父**本轮已被人批过**的执行域作为一次性能力
传给子 run（限次数 + TTL），省掉"子做完 → 交回父 → 父再做"的往返。

**现在不做**：`pending_actions` 已经覆盖了"交回父执行"的路径；预授权的收益（省一次往返）
低于它把审批语义复杂化的风险（人批的是父的动作，却等于放开了子 agent 的动作范围）。

**触发条件**（满足任一才做）：真实使用中反复出现同一步往返
（例如连续发布 3 个技能、同一网段的多轮扫描），且每次子 agent 都停在同一个动作上。

**若实现必须满足**：① 审批卡上明写"允许子 agent 在 N 分钟内使用 X"；② 能力只能由父的既有
审批派生，模型不能自授；③ 写进 `agent.spawned` payload（可审计）；④ 默认关闭，按次计数。

### P4.2 · 成本闸门

- ✅ token 预算：`[budget] daily_tokens / monthly_tokens`（0 = 不限），run 入口与 `delegate` 前各查一次
- ✅ 拒绝可见：回一条「今日额度已用完（105 / 100 token，其中子 agent 25）… 调大 `[budget] daily_tokens`」
- ✅ 用量归属（schema V5）：`usage_rollup` 增 `conv_id / run_id / agent`，由会话 id 推出（`sub:` → 子 agent），
  不靠调用方传参；`/usage` 多一行"其中子 agent 今日 X"
- ✅ attempts 账本的最小形态：`Turn::attempts()`（含重试）写进 `turn.finished`，可与 provider 账单对数
- ⏳ **金额**：`[pricing]` 与 `cost_est` 计算**未实现**——用户口径是"金额先不算"，`cost_est` 恒 0，
  `/usage` 明说"金额未配 [pricing]"
- ⏳ 单 run 子 agent 异常告警（token > 3× 均值）未做

**验收**：

1. ✅ 日预算耗尽后 `delegate` 直接拒绝并告知主人，不排队
2. ✅ `/usage` 能指出子 agent 今天花了多少；归属按会话推出，重建后不丢
3. ⏳ 金额口径（`cost_est` 与账单一致）——待 `[pricing]` 落地后再验

> 已完成的守卫测试：`budget::tests::*`（0=不限、文案带用量与改法、月/日先后）、
> `over_budget_blocks_run_with_visible_message`、`over_budget_blocks_delegation`、
> `p4_2` 相关的一致性测试（`usage_split` 在 Ephemeral 与 SQLite 上同结果，rebuild 后归属不丢）。

### P4.3 · 后台 Task

- ✅ doc `kind="agent_task"` 队列 + 5s tick 领取（提醒仍 30s；复用 reminder 的版本号，**不启用空壳 `tasks` 表**）
- ✅ `delegate(background=true)` 立刻返回任务号；`/tasks` 列表、`/tasks cancel <id>`
- ✅ `task` 工具（tier=1）：父 agent 能自己查/停任务，不必等主人代劳
- ✅ 结果推回原会话（结论 + `待父执行` + 子会话引用）；同时写 `agent.concluded`
- ✅ 重启语义（D4）：`running` → `interrupted` + 通知"结果未知、没有自动重跑"，**不重跑**
- ✅ 接手：`delegate(continue_from=…)` / `/tasks retry <id>` 把上次结论与产物路径（≤1600 字）
  注入新子 agent；任务 doc 入队时即记 `child_conv`，打断后也知道该续哪个
- ✅ `nice 19`：技能/装包/探针的 `exec` 走 `setpriority(19)`；编译车道同时只跑一个
- ⏳ per-exec cgroup 限额（`SBC-VERIFY §10`，触发式）

**验收**：

1. 后台任务重启后继续，不重复执行已完成步骤 → **按 D4 改为"不重跑、只通知"**（避免 `External` 副作用重放）
2. ✅ 前台有活动时后台任务不启动（`background_idle_s`，默认 5s；只有正文消息算活动，查 `/tasks` 不算）
3. ⏳ 后台任务（含它 fork 的沙盒子进程）预算耗尽后能被 `killpg` 收干净——依赖 per-exec cgroup，上板验

> 已完成守卫测试：`background_task_runs_and_pushes_result`（入队→tick→推结果→doc 状态 done）、
> `idle_gate_holds_background_task`（前台活跃时保持 pending）、
> `restart_marks_running_task_interrupted`（重启只通知、不重跑）。

---

## 5.5 P4.5 · 多通道与多用户（✅ 身份与路由已实现；审批流程未做）

**进度（2026-10-30）**：身份层与多通道路由已落地（D26）：

- `[identity]` 是白名单**唯一来源**；一人多账号 / 多人只是多写绑定
- 未绑定 → 回「未授权」、**不跑模型**（每小时提示一次）；通道层不再过滤
- 记忆 scope 跟人走：`family` 或 `user:<user id>`，与通道无关；
  修掉 `remember(scope="user")` 写入后永远读不回来的旧 bug
- 多通道并存：actor 持通道表，锁/typing/runs 键 = `{channel}:{addr}`

未做：运行期绑定与审批（D16 保留）、邀请码、`/revoke`、按 user 隔离 workspace。
提醒的投递目标已按通道落地（D27）。

**交付**：一个用户绑多个通道；陌生人接入必须经 owner 批准。

- `User ↔ Binding` 两层身份（✅ 已有：`[identity]` + `Directory`）
- 运行期绑定落库（`users` / `bindings`），配置只作种子
- 通道白名单（`[telegram] allowed_users`）改由 identity 绑定派生，避免两处维护
- 接入流程二选一：陌生地址首次发言 → owner 审批卡；或 owner `/invite` 一次性码
- `/revoke` 撤销绑定；绑定增删进账本

**接入的安全细节（实现时逐条落实）**：

- 拒绝要记住：拒绝后进 deny 列表，否则每次发言都弹一次卡
- 邀请码：15 分钟 TTL、一次性、只在 owner 会话生成、库里存哈希
- `/revoke` 立即生效（内存缓存失效），不能等重启
- owner 自己的绑定不能被聊天命令删除，只能改配置（防锁死）
- 新地址速率限制：每小时最多一次申请
- 绑定增删写账本：谁、何时、走审批还是邀请码

**上线前的硬性检查**：

- [ ] 加第二个真人之前删掉 `[identity] default_user`（否则所有陌生地址都会变成 owner）
- [ ] 启动时若检测到 `default_user` 已设置且绑定数 > 1，打 warning（实现时补）
- [ ] `[telegram] allowed_users` 与 identity 绑定一致（或已改为派生）

- 隔离（按需再做）：`/ws` 按 user 分目录、审批固定发 owner、facts 去掉 `family` 共享

**验收**：

1. 陌生 TG 账号发消息：bot 回"已申请"，owner 收到审批卡；拒绝后该账号再也收不到回复
2. owner `/invite` 生成的码 15 分钟内一次性有效，用过即废
3. 撤销绑定后，该地址的消息被当作陌生人处理（而不是静默吞掉）
4. 加第二个用户后，`/reminders` 仍只显示各自的提醒

---

## 5.7 P4.6 · 会管宿主（设计已定，未开工）

**交付**：agent 能管理所在机器（user systemd、监控 timer、极少数 apt），
且**扩展新场景靠改数据与写技能，不靠改 Rust**。取舍见 `DECISIONS.md` D22，
契约见 `INTERFACES.md §6.4`，配置见 `OPS.md`「宿主执行与授权」。

**进度**：A 段已实现并在开发机验证（`host` / `sys` / `capability` + 授权表热加载 + 软链/`..` 防护）；
B、C 未做。

三条边界，都不含领域语义：`host`（L1，chuan 身份）/ `hostd`（L2，root）/ `sys`（L3，只读 `/proc`）。

### P4.6-A · 边界与观测（✅ 已实现）

- `ExecutionEnv::Host`：bwrap 变体——去掉 `--unshare-pid` / `--unshare-net`，
  `--proc /proc` 换成 `--ro-bind /proc /proc`，路径按 `[host.path]` 逐条挂，
  强制遮蔽三项（`secrets` / `services.d` / `chuan.toml`）
- `host` 工具：`argv | script`、`timeout_ms`、`grant[]`（只读、≤ 8 条、不落盘）
- `sys` 工具：`load / mem / stat / psi / cgroup / units / journal`
  （`/proc`、`/sys` 进程内读；`journalctl` / `systemctl show` 由内核直连固定命令，不经 bwrap——
  固定读接口不该为一行指标开放整个宿主 lane）
- `capability` 工具：`list / check / request`（**只写草稿**）+ `playbooks/capability.md`（8 节 + 5 条硬规则）
- 域拆分：`host` / `host_elevated` / `host_observe`；旧键 `host_exec` 忽略并打 warning
- ✅ `host-access.jsonl` 审计（L1 与 hostd 写同一文件：argv、挂载集、是否临时挂载、退出码、耗时）

**验收**：

1. 访问未授权路径返回 `NotGranted { kind, item }`，文案里带"已授权清单"与下一步动作
2. host lane 里 `/etc/chuan/secrets` 是空目录、`$CREDENTIALS_DIRECTORY` 不存在（单测，复用 `tests/sandbox.rs` 基建）
3. host lane 的 `ps` 有 PID 1（证明没换 pid namespace）；`sys(mem)` 与 `free -m` 一致
4. 申请单缺节被拒；`capability(request)` 不修改任何授权文件
5. 授权表改一行后**不重启**生效（mtime 热加载）；写坏 TOML → 全部拒绝

### P4.6-B · user systemd（需上板：linger 与 `/run/user`）

- `install.sh`：`loginctl enable-linger chuan`、`SupplementaryGroups=lp systemd-journal`
- agent 自己写 `~/.config/systemd/user/*.{service,timer}` 并 `systemctl --user enable --now`
- 授权表默认给 `/var/log/journal`（ro）、`/run/user/$UID`（rw）、`/var/lib/chuan/.config`、`/var/lib/chuan/timers`

**验收**：

1. 一句话让它"每天 7:30 采一次温度和磁盘"：它自己建 unit + 脚本，到点 journal 有输出
2. 该 timer **不吃** `chuan.service` 的 `MemoryMax=1G`（独立 user slice）
3. 改授权表加一个新目录 → 下一轮直接可用，不用 `systemctl restart chuan`

### P4.6-C · hostd 与提权（root）

**进度**：已实现——`chuan hostd`（socket + `SO_PEERCRED` + argv 前缀匹配 + `env -i` +
`host-access.jsonl` + 可配置快照）、`chuan-hostd.service`、install.sh 装配、
通用客户端 `elevated`（**无发行版语义**：能跑什么只看 `[[host_elevated.grant]]`，
`approval="always"` 的条目免问，因此 `--pretend`/`-s` 这类只读预览可以零审批，
真装才弹卡）。**apt 专用工具 `pkg` 已删除**——不为单一发行版写定制指令。
**2026-10-30 上板验证通过**：`apt-get update` 与 `install -y figlet` 经 `elevated` 成功，
变更前 dpkg 快照落地，`host-access.jsonl` 有 L2 记录（命中第几条、argv、退出码）。

- `chuan hostd` 子命令 + 第二单元（`User=root`）+ `/run/chuan/host.sock`（`0660 root:chuan`）
- `SO_PEERCRED` 校验、argv 前缀匹配、`env -i`、`host-access.jsonl`、`dpkg` 快照
- 自毁防护：`chuan.service` 的 `restart` 排队到本轮结束；`stop` / `disable` 拒绝

**验收**：

1. 授权表加一条 `["/usr/bin/apt-get","install"]` → 能装包；卡上是 `-s` 预览，装完 `dpkg -l` 有
2. 非授权 argv（例如 `apt-get remove`）被拒，且失败原因写明"命中第 N 条授权之外"
3. 非 chuan uid 连 socket 被拒；hostd 进程环境里没有任何密钥
4. 让它"重启自己"：本轮先收到一条消息，turn 结束后才重启，不出现"消息发到一半连接断了"

**v1 不做**：技能进宿主（`run_skill` 仍只跑沙盒，`tier=1` 的 `host` 工具只有主会话能调）、
agent 写系统级 unit 文件、其他账号目录的组/ACL、公共 `/tmp` 共享区、per-exec cgroup。

---

## 5.9 P4.7 · 会记事（通知类流程）

**交付**：把老师/学校的通知粘进来，能自动解析、定提醒点、润色、建提醒、存档、按渠道推送，
且**流程本身可迭代**（先由主人写 SOP，稳定后下放给 agent）。取舍见 `DECISIONS.md` D23。

**为什么单列**：这不是新工具，而是"判断 + 措辞 + 落地"的组合流程；它同时检验三件事——
playbook 能不能承载流程、`remind` 的三形态够不够用、记忆的键约定能不能防重复。

### P4.7-A · SOP 定型（零代码，现在就能做）

- 主人写 `<config>/playbooks/school-notice.md`：解析四要素（事项/绝对时间/要带什么/时限）、
  判时表（次日事项、截止缴费、活动、调课）、润色规则（≤100 字、专名照抄）、落地顺序
  （`remember` → 逐条 `remind` → 回执含 id）、例外（日期含糊只问一次；已有则 `update` 不重建）
- 系统提示追加段里加一句触发：收到学校/老师通知 → 先取 `playbook("school-notice")`

**验收**（用真实通知，至少三条：含"下周三"、未写日期、涉及缴费）：

1. 绝对日期换算正确，回执里明确写出"哪天几点"
2. 重复粘贴同一条通知 → 不产生第二条提醒（幂等键 `hash(chat,due_at,text)` 生效）
3. 涉及钱的提醒点前移到"提前 3 天"，并在回执里单独标出
4. 钉钉渠道走 `text` 形态：到点后 ≤1 分钟发出，内容与润色版一致
5. `reminders(list)` 能看到该条（含渠道与参数），可 `update` / `del`

### P4.7-B · `publish_playbook` 下放（触发式，暂不做）

**触发**：SOP 两周内被手改 ≥3 次，或主人明确要求。

- 工具：`publish_playbook(topic, content, based_on?, override?)`；`SelfModify` 域、`tier=1`
- 审批卡：topic、字数、**变更行**（新增/删除，最多 60 行）；批准后才落盘
- 保护：`builtin` 主题不可覆盖（`override=true` 才允许，卡片红字）；正文 ≤20k；覆盖留 `.bak`；
  文件头强制三行溯源（时间 / 基于哪个 run 或 topic / 上一版哈希）
- `playbook(list)` 显示每个 topic 的**最后修改时间与来源**（builtin / 主人 / agent）

**验收**：agent 改一条自建 SOP → 卡片能看出改了哪几句 → 批准后 `playbook(topic)` 立刻是新版；
试图覆盖 `timer` → 被拒并提示 `override=true`；`cat <config>/playbooks/<topic>.md` 头部有溯源。

### P4.7-C · 不遗漏（触发式，暂不做）

**触发**：出现一次"因为关机/重启漏发"，或主人明确要求。

- 补跑窗口从 2 小时（`REMINDER_GRACE_MS`）做成可配（如 `[actor] reminder_grace_h = 24`）；
- 迟到投递时加前缀「（迟到的提醒）」并写明原定时间，避免"以为是准点的"
- 可选：`remind.notify = "service:dingtalk"` 零 LLM 路由（触发条件见 D23）

**明确不做**：把通知流程写成技能（判断类工作脚本化只会更脆）；让技能进宿主（D22）。

---

## 5.11 P4.8 · 消息状态回执（✅ 开发机已实现，待上板）

**交付**：一次连发多条时，在 Telegram 上直接看出**每条**的处理状态
（未开始 `😐` / 处理中 `👀` / 成功 `👍` / 失败 `👎`）。取舍见 `DECISIONS.md` D24，
契约见 `INTERFACES.md §1.1`。

**为什么需要**：会话内串行（按 `conversation` 的互斥锁），后到的消息在内存里排队，
界面上却只看到上一条回复在动——多条消息时无法判断"哪些还没开始"。

- `Inbound.msg: Option<MsgRef>`：平台侧消息句柄；合成消息为 `None`
- `Channel::react()`：默认空实现，其他通道零改动
- Telegram：`setMessageReaction`（emoji 白名单 73 个，`⏳/✅/❌` 不可用）
- 三个锚点：收到（判该会话是否已有在跑的 run）→ 拿到锁 → 收尾
- 取消 = 清空；预算超限 = 失败
- **微信不做**：iLink 无 reaction 接口，且每条出站消息占 10 条额度中的 1 条

**测试**（`crates/harness-store/tests/runner_flow.rs`，5 条全绿）：
`reactions_track_queue_then_success`（排队 → 处理中 → 成功）/
`failed_run_marks_fail`（失败 → 👎）/ `cancel_clears_reaction`（取消 → 清空）/
`synthesized_inbound_has_no_reaction`（`msg=None` 零反应）/
`reaction_failure_does_not_break_run`（反应失败不影响送达）。

> 写取消用例时踩到一个坑：actor 的收消息循环必须**并发**跑着（`tokio::spawn`），
> 否则消息只躺在队列里、run 根本不会开始；另外等"真的开始执行工具"要用工具自带的信号位，
> 固定 `sleep(250ms)` 在 debug 构建下会抢跑（`runs` 里的令牌是准备阶段末尾才写入的）。

**验收**：

1. 连发 3 条：第 1 条 `👀`，第 2、3 条 `😐`
2. 第 1 条结束变 `👍`，第 2 条转 `👀`，第 3 条仍 `😐`
3. 让一轮必然失败（如临时改坏 key）→ 该条 `👎`
4. `/cancel` 取消 → 该条的 reaction 被**清空**（不是 `👎`）
5. 定时任务触发的 run 不加任何 reaction（`msg = None`）
6. 反应失败（如 `REACTION_INVALID`）不改变 run 结果，只留一条 trace

---

## 5.13 P4.9 · 微信通道（调研完成，未开工）

**交付**：微信（ClawBot / iLink）作为**对话入口**接入——用户问、bot 答。
**不是**推送通道。定位与额度模型见 `DECISIONS.md` D25，实测事实见 `WEIXIN.md`。

**已完成的调研**（不需要再做）：协议 9 个端点、登录/长轮询/发送/typing 全部实测；
改写、工具卡片、文本上限、额度、令牌寿命都已量过；未验证项见 `WEIXIN.md §5`。

**接入的前提条件**（缺一不做）：

- `Channel::react` 保持默认空实现（协议无 reaction 接口，`WEIXIN.md` F14）
- 渲染必须走"无编辑能力"分支（`Caps.edit = false`），一轮只发终稿
- 每条入站消息只打算发 1 条；不做进度消息、不做占位后改写
- 定时提醒不默认投微信（D25 第 4 条）

**验收**：

1. 一问一答正常；连问 10 轮仍然正常（每轮 1 条，额度按入站续费）
2. 一轮只发 1 条消息；`Caps.edit=false` 时渲染器不发中间态（`render.rs` 的 no-edit 分支）
3. 额度耗尽（`ret=-2`）时只记日志、不重试、不误报为通道故障
4. **README 写清楚"对话入口，不是推送通道"与额度约束**（D25 第 5 条）
5. 连续 24h 长轮询不掉线（`bot_token` 无 TTL，F17）

---

## 6. P5 · 会看

**交付**：图片与文档理解。

- 媒体下载与转存（sha256 寻址）
- 多模态内容块构造
- 视觉 token 预算；TTL 后回放降级为 tombstone

**验收**：

1. 发一张照片，能描述内容并回答追问
2. 图片不重复进入历史（只在当轮注入）
3. 媒体过期后回放不报错，显示占位符

---

## 7. 明确不做（本轮）

HTTP 通道、知识库入库、向量检索、多 provider 自动路由、git worktree、管理后台。

触发条件：HTTP 等多端需求；KB 等真实语料；向量等 KB 变大（`INTERFACES.md §3.4`）。

**按人 / 按通道绑定 LLM provider**（暂不做，触发式）：
`[engine]` 现在全局一份，`ActorConfig.model` 也是单值。若需要"每人甚至每个通道用不同模型/额度"，
要把 `TurnDriver` 也做成按 `user`（或按 `channel`）解析的注册表。
**触发条件**：`lanes.external ≥ 2` 的并发配置下出现 provider 429，或明确要求不同人不同模型。
现状先按 `external = 2` 观察，出问题再回来做（2026-10-30 记录，未做）。

---

## 8. 完成定义（DoD）

一个阶段算完成，需同时满足：

1. 该阶段所有验收项通过；
2. 没有新增"临时绕过"（TODO 必须带 issue 号）；
3. 相关一致性测试通过（`INTERFACES.md §9`）；
4. 文档同步（若产生新取舍，写入 `DECISIONS.md`）；
5. `kill -9` 演练不产生重复副作用（对含副作用工具的阶段有效）。
