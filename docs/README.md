# 家庭智能管家 Harness · 文档索引

> 运行在单块 RISC-V SBC（riscv64 / 8GB）上的个人助理 harness。Rust 单二进制，Telegram 与微信两个入口。
> 核心特征：**账本即真相的状态模型** + **沙盒化执行** + **可沉淀技能**。
>
> 当前状态：P0–P3 主体已实现；**已在 riscv64 目标板上跑通**（沙盒隔离、两条 lane、代理、
> mDNS、定时任务含 pinned/polish、凭据不落沙盒）。打印仅到「CUPS 可见」，实际打印/耗材未验。
> P4 已完成：车道闸门、前台委托、角色、token 预算与用量归属、后台任务与接手；金额口径不做（D20：token 记账 + 服务商窗口兜底）。
> **P4.10（审批与身份）**：**已实现（开发机）**——`ActorRole`、批准人校验、`Blocked` 终态、
> 命令条目表 + `lan_command` + 打印四条目 + 审计、`capability` 提案与内核安装、提醒按形态免审、
> 家人 UX（无卡、无沉淀按钮、重复被挡才通知）。上板项见 `SBC-VERIFY.md §13`。
> **P4.6（宿主执行）**：A 段已实现（`host` / `sys` / `capability` + 数据化授权）；
> C 段（`chuan hostd` 提权通道、`elevated` 工具）**已实现，待上板验证**；B 段（user timer 上板）未做。
> 通道：Telegram + 微信**双通道已上板跑通**（rc.27）；CLI 通道不存在。
> 语音：微信侧用平台自带转写（已通）；TG 侧 ASR 仅到可行性结论（K1 4 线程 RTF 0.435），未接入。
> 逐条见 `ROADMAP.md §5`，板上验证清单见 `SBC-VERIFY.md §11`。
> 逐条实测值见 `SBC-VERIFY.md`——它是唯一权威，概览与此冲突时以它为准。
> **rc.20**：入站消息状态反应（Telegram 表情回执 😐/👀/👍/👎，D24）。
> **rc.21**：修复发布包漏装 `chuan-hostd.service`（提权通道起不来）；
> `host` 逐次审计；`remind` 回执标明「生效形态」；`playbook` 默认目录改为 `<config>/playbooks`。
> **rc.22**：**删除 apt 专用 `pkg`**，改为通用 `elevated`（argv 授权表驱动，无发行版语义）；
> `approval = "always"` 真正生效（预览免问、真装逐条问）；快照改成数据 `snapshot_argv`。
> **rc.23**：`%U` 按服务账号展开（hostd 是 root，曾展开成 `/run/user/0` 导致整表失效）；
> host lane 补 DNS（`/etc/resolv.conf` 软链悬空）；授权表两段独立失效。
> **rc.24**：微信通道（ClawBot/iLink，**只做文本**）、多通道并存（锁键 `{channel}:{addr}`）、
> 身份与记忆按人归属、提醒按通道投递（D25–D27）。
> **不兼容**：`[identity]` 成为白名单唯一来源，通道级 `allowed_users` 与 `default_user` 移除。
> **rc.25**：`weixin-login` 不再要求完整配置（只读 `[weixin]`/`[proxy]`）。
> **rc.26**：微信凭据归属跟随目录（0640），登录后不用再 chown/chmod；登录提示改指 `[identity]`。
> **rc.27**：配置错误以退出码 2 退出且不重试（不再把 StartLimit 打满）；**上板验证双通道已通**。
> **rc.28**：微信多账号——一个凭据文件 `accounts` 数组，每人一个 bot、独立轮询与游标。
> **rc.29**：微信「正在输入」（`typing_ticket` + `sendtyping`，收尾显式关闭）。
> **rc.30**：审批卡面向非程序员——模型填 `_approval{why,impact}`，卡片三段式，
> 微信上回 `1`/`2` 即可批准/拒绝（不用打 id）。
> **rc.31**：微信补齐三处——`context_token` 落盘（重启后仍能主动发）、非文本消息回提示（不静默）、
> 语音走微信自带转写（无转写则说"听不清"）。
> **rc.32**：微信**入站媒体**——图片/PDF 下载 + AES-128-ECB 解密 + 落盘 `/media`
> （图片走视觉，PDF 交给模型解析或打印）。
> **rc.33**：只发附件时**先收着等指令**（微信图片没有 caption，不再抢跑）；10 分钟内有效。
> **rc.34**：微信**出站文件**——`send_file` 走 CDN 上传（AES-128-ECB），图片/PDF 都能发回去。
> **rc.35**：**审批与身份（D30/D31）**——`[identity] owner` + 主体角色；审批只对主人，
> 家人零卡、越界即 `Blocked`；**命令条目表** `commands.d/`（`lan_command`，打印四条目免审批）；
> `capability` 提案 → 主人批准 → 内核原子安装；提醒按形态免审、定时任务遇审批跳过并通知；
> 用户文案集中到 `kernel/copy.rs`。**破坏性**：已有 `[identity]` 必须补 `owner = "<user-id>"`。
> **rc.36**：修复"有条目却不用"——每轮 system 注入**可用命令条目清单**（名字 + 参数域），
> 并明确"能命中条目就用 `lan_command`"；此前模型不知道条目存在，查打印机仍走 `lan_exec` 弹审批卡。
> **rc.37**：修 `supplies` 读不到墨量——`lpstat -l` 不报 marker-levels（板上实测 exit=0、
> 输出为空），默认条目改走 `ipptool -tv …/get-printer-attributes.test`；打印机枚举由安装脚本
> 按 `lpstat -e` 自动填。顺带修 `/prompt` 的动态注入清单漏列"可用命令条目"。
> **rc.38**：① 工具输出超限不再闷截断——全文落 `/ws/.tool-results/<call>.txt`，
> 上下文只留**首 6k + 尾 2k + 确切路径**（先打码再落盘，`msg.tool_result` 记 `spill`）；
> ② `supplies` 改用定向查询 `/etc/chuan/ipptool/supplies.test`：状态、能力、
> `media-supported`（支持纸型）、`sides-supported`、marker-* 都查，但不含
> `media-col-database` 这类几十 KB 的块（要全量排查仍可用 `lan_exec`）。
> **rc.39**：修 `ipptool` 定义在 CUPS 2.4.12 上的两处解析不兼容——`requested-attributes`
> 改**单行逗号分隔**（多行报 `Unexpected token`）、`STATUS` 用 `successful-ok`（`"ok"` 报
> `Bad STATUS code`）；顺带去掉非标准的 `printer-description`。板上已验证能读到
> `media-supported` / `sides-supported` / `marker-levels`。
> **rc.40**：把 rc.39 之后的三处修正并进安装包——`ipptool` 定义改单行逗号列表、
> `STATUS` 用 `successful-ok`、并接受 `successful-ok-ignored-or-substituted-attributes`
> （Brother 墨仓机对不认识的属性会这么回，数据照常给但 ipptool 会判 FAIL）；
> README 的安装说明改为"预发布必须显式版本"（`/releases/latest` 只指向正式版）。
> 首个正式版：`v0.1.0`。
> **0.2.0-rc.1**：知识库（`kb_ingest` / `kb_search` / `kb_read` / `kb_select` / `kb_outline*` /
> `kb_file` / `kb_list` / `kb_forget`）、`publish_playbook` 与 `rollback_playbook`、
> 自改事务统一（原子写 / 备份 / 回滚 / 回收站 / 记账）、`docs_fts` 改 trigram、
> `http(save_to)` 支持大文件落盘。
> **0.2.0-rc.2**（修复）：子 agent 默认工具含 8 个 `kb_*`（不含 `kb_forget`）；
> `playbooks/` 改为 `0770` 并把五个可写子目录写进 unit 的 `ReadWritePaths`（此前是 EROFS）；
> `role` / `service` / `publish_playbook` / `rollback_playbook` 补 `_approval` 且必填；
> 上下文预算改按 token 计（CJK 1 字≈1 token）并扣掉 system 与工具说明；
> 供应商 400"太长"回可操作提示；`delegate` 引导长委托书写进 `/ws`。
> **0.2.0-rc.3**（后台任务闭环）：
> 后台任务**全部**跑完后自动叫醒父 agent 收尾（带原任务 + 结论 + 待办，落到当前 epoch，
> 同批只唤醒一次、不链式）；队列改 **FIFO**；`/agents` 不再留幽灵（Drop 清理）；
> 卡住判定分档（工具执行期间 30 分钟，模型阶段仍 180s）；
> 提示词新增「执行优先 / 收尾前自查」并把**回合预算**写进 system。
> 预发布到 `v0.2.0-rc.3`。

---

## 阅读顺序

| 顺序 | 文档 | 回答什么问题 |
|---|---|---|
| 1 | [`DESIGN.md`](DESIGN.md) | 系统怎么分层、有哪些模块与功能点、关键流程怎么走 |
| 2 | [`INTERFACES.md`](INTERFACES.md) | 四个替换型扩展点（Channel/Store/Retriever/TurnDriver）与两个组合型（Tool/ExecutionEnv）的契约 |
| 3 | [`DATA-MODEL.md`](DATA-MODEL.md) | 层→模块→crate 映射、事件分类、Journal 与投影、恢复判定 |
| 4 | [`DECISIONS.md`](DECISIONS.md) | 每个关键取舍的理由、代价、反悔成本 |
| 5 | [`SELECTION.md`](SELECTION.md) | 依赖什么库、排除了什么、为什么 |
| 6 | [`PLATFORM.md`](PLATFORM.md) | 目标板的实测事实（沙盒、cgroup、glibc、bwrap 版本） |
| 7 | [`ROADMAP.md`](ROADMAP.md) | 阶段划分、验收标准、前置验证清单 |
| 8 | [`OPS.md`](OPS.md) | 配置、密钥、systemd、时间时区、重试、备份、磁盘 |
| 9 | [`GLOSSARY.md`](GLOSSARY.md) | **术语与命名标准的唯一出处**，写代码前先读 |
| 10 | [`SBC-VERIFY.md`](SBC-VERIFY.md) | 只能在目标板确认的项（上板清单），上板时按它逐条勾 |
| 11 | [`ROLES.md`](ROLES.md) | 子 agent 怎么配、有哪些现成角色、平时怎么说话 |
| 12 | [`VERIFY-P4.md`](VERIFY-P4.md) | P4 验证剧本：聊天话术 + 观察面 + 失败信号 |
| 13 | [`WEIXIN.md`](WEIXIN.md) | 微信通道（ClawBot / iLink）**已实测事实**与未验证清单；只记事实，不含取舍 |
| 14 | [`VOICE.md`](VOICE.md) | 语音消息**入站**的方案与成本：两平台事实、现状缺口、最小改动集、未验证项 |
| 15 | [`ASR.md`](ASR.md) | 本地语音识别的可行性（K1/RVV）、模型选型、实测步骤与判据 |

图：`architecture.png`（源 `architecture.dot`，同时导出 `.svg`）。
历史版本与已合并的文档在 `archive/`。

---

## 十条关键结论

1. **分层沿用业界十层模型**（接入/状态/循环/上下文/工具/执行/治理/记忆/编排/可观测），不自造词汇。
2. **Journal 是状态唯一真相**：模型看见过的都必须能从账本重建；docs 是结论的唯一真相。
3. **恢复按崩溃点分级**，不按"轮"重放；副作用分 `Pure`/`Idempotent`/`External` 三级。
4. **权限与隔离分离**：授权轴是"执行域"（sandbox / self_modify / outbound / lan / install / host / host_elevated / host_observe），不是命令黑名单；沙盒才是边界。
   **宿主能力的扩展靠数据不靠代码**：路径授权与 argv 授权表是配置（人手写、热加载），领域做法是技能（D22）。
5. **技能是纯函数**：只能读写 workspace，拿不到打印/发送的直连能力；`SKILL.md` 正文永不进上下文。
   领域能力（打印、服务 API）由技能实现，内核只给通用挂载与协议适配（D17）。
   **凭据只在宿主侧**：值不进沙盒、不进上下文，注入发生在宿主构造请求时（D18）。
6. **三个数据平面**：状态（journal）/ 结论（docs）/ 资料（KB），生命周期与备份方式不同。
7. **四个替换型扩展点**：Channel（多通道并存）、Store（换数据库）、Retriever（换检索）、TurnDriver（换 LLM 实现），各有一致性测试。
8. **LLM 客户端自写**（约 400 行），接口冻结为 `TurnDriver`；rig 的 loop 在 journal-first 前提下用不上。
9. **沙盒 = bwrap 白名单挂载**（`/home` 不可见、默认无网、超时 killpg）。内存边界目前是
   **服务级 `MemoryMax=1G` + 沙盒 `/tmp` 256MB**；*per-exec `cgroup` 限额尚未实现*（缺口见 `SBC-VERIFY.md`）。出网由宿主 `web_fetch` / `lan_exec` 代取。
10. **当前不做**：向量、多租户 SaaS、插件热加载、通用 workflow 引擎——都要等第二个应用证明需求。
    多 agent 只做单层派生、只回传结论（D19），不做网状协作与递归。

---

## 文档维护规则

- **单一事实来源**：一个决定只在一处展开，其它地方只引用。架构写 `DESIGN`，契约写 `INTERFACES`，理由写 `DECISIONS`。
- **被推翻的决定不删除**：移到 `DECISIONS.md` 的"已废弃"小节，写清推翻了什么、为什么。历史是判断依据。
- **实测优先于推断**：凡是标 `PLATFORM.md` 的结论，必须来自目标板；未实测的标 ⚠️ 并列入 `ROADMAP.md` 的前置验证。
- 历史版本存 `archive/`。
