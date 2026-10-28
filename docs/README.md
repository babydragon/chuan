# 家庭智能管家 Harness · 文档索引

> 运行在单块 RISC-V SBC（riscv64 / 8GB）上的个人助理 harness。Rust 单二进制，Telegram 为主入口。
> 核心特征：**账本即真相的状态模型** + **沙盒化执行** + **可沉淀技能**。
>
> 当前状态：P0–P3 主体已实现；**已在 riscv64 目标板上跑通**（沙盒隔离、两条 lane、代理、
> mDNS、打印链路、定时任务含 pinned/polish、凭据不落沙盒），逐条实测值见 `SBC-VERIFY.md`。
> 预发布到 `v0.1.0-rc.5`，正式版待定。

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

图：`architecture.png`（源 `architecture.dot`，同时导出 `.svg`）。
历史版本与已合并的文档在 `archive/`。

---

## 十条关键结论

1. **分层沿用业界十层模型**（接入/状态/循环/上下文/工具/执行/治理/记忆/编排/可观测），不自造词汇。
2. **Journal 是状态唯一真相**：模型看见过的都必须能从账本重建；docs 是结论的唯一真相。
3. **恢复按崩溃点分级**，不按"轮"重放；副作用分 `Pure`/`Idempotent`/`External` 三级。
4. **权限与隔离分离**：授权轴是"执行域"（sandbox / self_modify / outbound / lan / install / host_exec），不是命令黑名单；沙盒才是边界。
5. **技能是纯函数**：只能读写 workspace，拿不到打印/发送的直连能力；`SKILL.md` 正文永不进上下文。
   领域能力（打印、服务 API）由技能实现，内核只给通用挂载与协议适配（D17）。
   **凭据只在宿主侧**：值不进沙盒、不进上下文，注入发生在宿主构造请求时（D18）。
6. **三个数据平面**：状态（journal）/ 结论（docs）/ 资料（KB），生命周期与备份方式不同。
7. **四个替换型扩展点**：Channel（多通道并存）、Store（换数据库）、Retriever（换检索）、TurnDriver（换 LLM 实现），各有一致性测试。
8. **LLM 客户端自写**（约 400 行），接口冻结为 `TurnDriver`；rig 的 loop 在 journal-first 前提下用不上。
9. **沙盒 = bwrap 白名单挂载**（`/home` 不可见、默认无网、超时 killpg）。内存边界目前是
   **服务级 `MemoryMax=1G` + 沙盒 `/tmp` 256MB**；*per-exec `cgroup` 限额尚未实现*（缺口见 `SBC-VERIFY.md`）。出网由宿主 `web_fetch` / `lan_exec` 代取。
10. **当前不做**：向量、多租户 SaaS、插件热加载、通用 workflow 引擎——都要等第二个应用证明需求。

---

## 文档维护规则

- **单一事实来源**：一个决定只在一处展开，其它地方只引用。架构写 `DESIGN`，契约写 `INTERFACES`，理由写 `DECISIONS`。
- **被推翻的决定不删除**：移到 `DECISIONS.md` 的"已废弃"小节，写清推翻了什么、为什么。历史是判断依据。
- **实测优先于推断**：凡是标 `PLATFORM.md` 的结论，必须来自目标板；未实测的标 ⚠️ 并列入 `ROADMAP.md` 的前置验证。
- 历史版本存 `archive/`。
