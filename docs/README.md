# 家庭智能管家 Harness · 文档索引

> 运行在单块 RISC-V SBC（riscv64 / 8GB）上的个人助理 harness。Rust 单二进制，Telegram 与微信两个入口。
> 核心特征：**账本即真相的状态模型** + **沙盒化执行** + **可沉淀技能**。
>
> 当前状态：P0–P3 主体已实现；**已在 riscv64 目标板上跑通**（沙盒隔离、两条 lane、代理、
> mDNS、定时任务含 pinned/polish、凭据不落沙盒）。打印仅到「CUPS 可见」，实际打印/耗材未验。
> P4 已完成：车道闸门、前台委托、角色、token 预算与用量归属、后台任务与接手；金额口径未做（D20）。
> **P4.6（宿主执行）**：A 段已实现（`host` / `sys` / `capability` + 数据化授权，D22）；
> B（user timer 上板）与 C（`chuan hostd` 提权）未做。
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
> 预发布到 `v0.1.0-rc.31`，正式版待定。

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
