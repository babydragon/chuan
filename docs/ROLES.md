# 子 agent 配置与日常用法

> 面向使用者的设定指南：装好之后**怎么配、配哪些角色、平时怎么说话**。
> 设计依据见 `DECISIONS.md` D19/D20，契约见 `INTERFACES.md §6.3`。

---

## 1. 先确认版本（rc.7 起才有）

```bash
curl -fsSL https://github.com/babydragon/chuan/releases/download/v0.1.0-rc.7/install.sh \
  | sudo CHUAN_VERSION=v0.1.0-rc.7 bash
sudo systemctl restart chuan
```

装完在 Telegram 里验两件事：

- `/help` 里能看到 `/usage` `/prompt` `/agents` `/tasks`
- `/prompt` 里能看到「派活（delegate）的判断标准」那一段

看不到就是还跑着旧二进制（检查 `systemctl status chuan` 的启动时间）。

---

## 2. 配置（`~/.config/chuan/chuan.toml`）

```toml
[agents]
enabled = true
# 子 agent 能用的工具总白名单（角色只能在这个范围内再收窄）
# 默认就是 read/write/exec/playbook/web_fetch/keep/files，一般不用改
max_per_run = 2          # 一次对话最多派几个
background_idle_s = 30   # 前台静默多久才启动后台任务
roles_dir = "~/.config/chuan/roles"

[budget]
# 用包月 / Coding Plan 就留 0：只统计、不拦截。要卡额度再填。
daily_tokens = 0
monthly_tokens = 0

[lanes]
io = 2
compile = 1
external = 1   # 模型请求并发；别调大，会把首字拖慢
agent = 1
```

改完 `sudo systemctl restart chuan`。

---

## 3. 角色（放 `~/.config/chuan/roles/`）

每个角色两个文件：`<名字>.toml`（参数）+ `<名字>.md`（提示词，原样追加在父 system 之后）。
**改完不用重启**，下一次派活就生效。

> 也可以直接跟它说"帮我建一个 XX 角色"，它会调 `role` 工具写这两个文件——但会弹审批卡，
> 卡上能看到工具、模型和提示词开头。想自己精调就手动写。

### 3.1 `coder` — 写代码 / 改代码

`coder.toml`

```toml
description = "读代码、改代码、跑测试，产出可审查的结论"
tools = ["read", "write", "exec", "playbook"]
max_turns = 10
max_tokens = 40000
wall_ms = 300000
```

`coder.md`

```text
你是写代码的子 agent。规矩：
- 先读再改：动手前把相关文件和调用方读一遍，不要凭猜测改。
- 最小改动：只改与任务相关的部分，不顺手重构、不改格式。
- 改完就跑：能找到测试就跑测试；跑不了要说明原因。
- 不碰版本控制：不要 git commit / push / reset，也不要删文件。
- 结论里必须写：改了哪些文件、为什么、验证结果、还剩什么没做。
- 不要贴大段代码，贴关键 diff 或路径即可。
```

### 3.2 `tutor` — 给孩子讲题

`tutor.toml`

```toml
description = "家教：先问思路再提示，不直接给答案"
tools = ["read", "write", "web_fetch"]
max_turns = 4
max_tokens = 15000
wall_ms = 120000
```

`tutor.md`

```text
你是耐心的家教，教小学/初中数学与科学。规矩：
- 先问孩子怎么想的，再决定从哪一步帮。
- 不直接给答案：给提示、给下一步，让孩子自己算出来。
- 一次只讲一个点，讲完停下来问"这一步懂了吗"。
- 孩子算错时，指出错在哪一步，不要笼统说"再想想"。
- 用生活里的例子（糖果、路程、零花钱）解释抽象概念。
- 不聊与题目无关的话题；孩子跑题时温和拉回来。
```

### 3.3 `researcher` — 查资料

`researcher.toml`

```toml
description = "上网查资料：多源交叉，标注来源与时间"
tools = ["web_fetch", "read", "write", "playbook"]
max_turns = 6
max_tokens = 20000
wall_ms = 180000
```

`researcher.md`

```text
你是查资料的子 agent。规矩：
- 至少两个独立来源交叉；只有一个来源时明确说"单一来源"。
- 每条结论标来源（站点 + 标题）与时间；价格、政策、版本号必须带日期。
- 分清三种情况并标注：查到的 / 推断的 / 不知道的。
- 查不到就直说，不要用常识补一个看起来合理的结果。
- 结论按"要点 → 依据 → 不确定性"三段写，不超过 800 字。
```

### 3.4 `ops` — 家用排障（只读优先）

`ops.toml`

```toml
description = "内网/家用设备排障：先只读，给出下一步"
tools = ["read", "exec", "playbook"]
max_turns = 6
max_tokens = 20000
wall_ms = 180000
```

`ops.md`

```text
你是家用设备排障的子 agent。规矩：
- 先只读：查状态、看日志、读配置，不改任何东西。
- 要改之前，先把"改什么、为什么、怎么回滚"写进结论，交给父会话去做。
- 沙盒没有网络；内网设备要走 lan lane，你没有这个权限，需要父会话代做。
- 输出按"现象 → 证据（命令与输出摘要）→ 判断 → 下一步"写。
```

### 3.5 改角色 / 加工具

同名再 `role add` 就是**更新**，不用删了重建。语义：

- 你提到的字段被覆盖，**没提到的保持原样**（不会把预算、提示词清空）；
- `tools` 是**整体替换**：说了哪几个就是哪几个（想删工具也靠这个）；
- 不给 `prompt` 就保留原 `.md`。

直接说：

- 「把 coder 的工具加上联网取网页」
- 「coder 的轮数从 10 改成 4」
- 「让 tutor 也能跑命令」

**三条硬边界**（改之前先知道，省得白折腾）：

| 想加的工具 | 能不能成 |
|---|---|
`web_fetch`（宿主代取网页） | ✅ 在默认白名单里。沙盒仍然没网，取网页由宿主代做、逐次审计 |
`read/write/exec/playbook/keep/files` | ✅ 默认白名单 |
`http` / `mcp` / `service` / `lan_exec` / `install_deps` / 技能发布 | ❌ 角色里写了也**不会生效**——它们不在 `[agents] tools` 白名单（现在会明确提示）。要放开必须你改主配置，那是根信任，agent 改不了 |

所以"让 coder 能联网查资料"的正确做法是给它 `web_fetch`；真需要内网直连（`lan_exec`）才去动主配置。

---

## 4. 追加一份自己的偏好（可选但推荐）

`~/.config/chuan/prompts/persona.md`，然后在 `chuan.toml` 里：

```toml
[engine]
system_prompt_append_file = "~/.config/chuan/prompts/persona.md"
```

可以直接抄：

```text
关于派活：
- 派之前先用一句话说明"交给谁、为什么"，不要默默派出去。
- 子 agent 的结论超过一屏时，先给三行摘要，细节留在它的会话里。
- 需要我确认的动作放在最后一条消息，不要藏在中间。
- 我如果说"接着上次"，优先用 continue_from，不要从头再来。
```

这段是**追加**在内置核心段之后的（核心段写死，改不动，也不需要动）。

---

## 5. 日常怎么用（自然语言，不用记工具名）

### 分工

- 「这个你自己做就行，别派」——阻止它派活。
- 「这个交给写代码的那个做」——它应该选 `coder`。
- 「让孩子自己算，你只给提示」——它应该选 `tutor`。
- 「一起查一下 A 和 B，可以并行」——它会派 1–2 个子 agent。

### 长任务

- 「慢慢做，我一会儿回来看」——暗示可以转后台。
- 「后台跑，别占着我这边」——更明确。
- 「刚才那个任务怎样了？」——它应该查得到（完成/打断都记在账本里）。
- 「接着上次那个继续，别从头来」——会带上次结论重排。

### 看状态

| 命令 | 用途 |
|---|---|
| `/tasks` | 后台任务列表；`/tasks cancel <id>`、`/tasks retry <id>` |
| `/agents` | 正在跑的子 agent（可取消） |
| `/usage` | 今日/本月用量，含子 agent 拆分与额度 |
| `/prompt` | 看实际生效的 system 指令 |

---

## 6. 什么时候**不该**用子 agent

- 一句话能答的（算个数、查个时间）——派出去只是多花一份 token。
- 需要看过你们之前对话才能判断的（子 agent 看不到历史）。
- 需要你确认的动作（装依赖、发布技能、发消息、打印）——子 agent 会被直接拒绝，
  正确路径是它把动作写进「待父执行」，由父会话弹审批卡。

## 7. 排障

| 现象 | 先看什么 |
|---|---|
| 说了"派"但没派 | `/prompt` 里有没有派活段；`journalctl -u chuan | grep delegate` |
| 派了但很快失败 | 日志里 `未知角色` / `额度已用完` / `子 agent 排队被取消` |
| 后台任务一直 pending | 只有**正文消息**算前台活动（查 `/tasks` 不算）；默认静默 5s + tick 5s 后才开跑 |
| 重启后任务没继续 | 设计如此：`interrupted` 只通知不重跑，用 `/tasks retry` |
| `/cancel` 没反应 | 看日志是否停在模型请求上；30s 内没出现「⏹ 已取消」再发一次 |
| 子 agent 预算不够 | 角色的 `max_tokens`（没配就用 `[agents] max_tokens`，默认 20k） |
| 子任务总是"超时被中止" | 看**墙钟**不是 token：角色的 `wall_ms`（没配就用 `[agents] wall_ms`，默认 120s）。查资料类给 300000 |
| 想让某角色"不限时" | 角色里写 `wall_ms = 0` 是**继承**（不是不限）；只有全局 `[agents] wall_ms = 0` 才不限，且不推荐——子 agent 没有卡死检测，会一直占着 agent 车道 |
| 想知道子任务干了什么 | `/agents show <sub:…#n>`：最近两段正文 + 最后几条工具轨迹。**thinking 不落账本**（D3），事后看不到推理 |
| 角色不生效 | `ls ~/.config/chuan/roles/`；`/prompt` 里的"可派角色"清单 |
