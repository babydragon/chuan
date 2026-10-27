# chuan（川）· 家庭智能管家 Harness

跑在单板机（RISC-V SBC / riscv64 / 8GB）上的个人助理。Telegram 交互，能执行命令、读写工作区、定时提醒，并支持把调试成功的脚本沉淀为技能。

- **设计文档**：`docs/README.md`（架构 / 契约 / 决策 / 路线）
- **许可证**：MIT
- **运维手册**：`docs/OPS.md`（配置 / 密钥 / systemd / 备份 / 降级）
- **实测基线**：`docs/PLATFORM.md`

---

## 0. 安装（release）

```bash
# RISC-V 单板机或 x86_64 Linux，需要 root + systemd
curl -fsSL https://github.com/babydragon/chuan/releases/latest/download/install.sh | sudo bash
```

或手动下载 `chuan-<tag>-<arch>-linux-gnu.tar.gz` 解开后 `sudo ./install.sh`。
脚本只装文件、**不自动启动**；放密钥与启用步骤会打印在末尾。

**下载需要代理时**（GitHub 慢或被墙）：外层 `curl` 取脚本、内层 `curl` 下 tar 包是两次独立请求，都要给代理：

```bash
P=http://127.0.0.1:7890
curl -fsSL -x "$P" https://github.com/babydragon/chuan/releases/latest/download/install.sh \
  | sudo https_proxy="$P" bash
```

- 外层 `-x "$P"`：取 install.sh 本身；
- `sudo https_proxy=...`：sudo 会清空环境变量，所以要**显式写在 sudo 后面**，最终由内层 `curl` 继承；
- 也可以先下 tar 包再本地装：`curl -x "$P" -LO <tar.gz> && tar xzf <tar.gz> && sudo ./install.sh --proxy "$P"`；
- `install.sh` 认 `--proxy URL` / `CHUAN_PROXY` / `https_proxy` / `http_proxy`，任一个即可；`CHUAN_BASE_URL` 可换镜像源。

> 运行阶段（Telegram、web_fetch、沙盒脚本、装包）用配置里的 `[proxy] url`，**不需要**在 systemd unit 里加 `Environment=`，见 [代理（proxy）](#代理proxy)。

从源码构建：

```bash
cargo zigbuild --release --target riscv64gc-unknown-linux-gnu   # 需要 zig
sudo deploy/install.sh
```

### 版本与发布

| 你想做的 | 做法 | 结果 |
|---|---|---|
| 试一下、不发布 | push 到分支 / 开 PR / 手动 Run workflow | 只有 Actions artifact（14 天），**不产生 Release** |
| 预发布（RC、beta、调试版） | 打 tag `v0.2.0-rc.1` | Release 标记为 **pre-release**；**不会**成为 `latest` |
| 正式发布 | 打 tag `v0.1.0` | Release 成为 `latest`，`/releases/latest/download/` 指向它 |

装预发布版要显式指定版本：

```bash
curl -fsSL https://github.com/babydragon/chuan/releases/download/v0.2.0-rc.1/install.sh \
  | sudo CHUAN_VERSION=v0.2.0-rc.1 bash
```

> 环境变量要放在 `sudo` 后面（`sudo VAR=x bash`），写在 `curl` 前面是无效的。
> 二进制按 glibc 2.28 基线构建，目标系统 ≥ 2.28 即可。

---

## 1. 它能做什么

| 能力 | 说明 |
|---|---|
| 对话 | 流式输出，同一条消息里原地更新，不是"处理中…" |
| 工具 | `exec`（bwrap 沙盒）· `read` `write`（仅工作区）· `remind` · `web_fetch` · `send_file` · `keep` `files` · `playbook` · `install_deps` · `run_skill` `publish_skill` · `http` `mcp` `service` |
| 过程可见 | 状态行显示 `第 N 轮 · 思考/执行`，工具轨迹带参数与耗时 |
| 安全 | 沙盒无网络、`/home` 与宿主 `/tmp` 不可见、cgroup 硬限、超时 killpg |
| 指令 | `/new` `/cancel` `/status` `/help`，已在 Telegram 注册可补全 |

家庭成员默认只能会话级使用；宿主/提权执行需要主人显式授权并逐条审批（见 `docs/DECISIONS.md` D15）。

---

## 2. 快速开始

### 2.1 构建

```bash
# 本机调试
cargo build --release

# 交叉编译到目标板（需要 cargo-zigbuild + zig）
cargo zigbuild --release --target riscv64gc-unknown-linux-gnu --bin chuan
```

### 2.2 准备密钥

密钥只放在文件里，**不要**写进配置或聊天：

```bash
mkdir -p ~/.config/chuan/secrets && chmod 700 ~/.config/chuan/secrets
printf '%s' '<telegram-bot-token>' > ~/.config/chuan/secrets/telegram_token
printf '%s' '<llm-api-key>'       > ~/.config/chuan/secrets/zai_key
chmod 600 ~/.config/chuan/secrets/*_key ~/.config/chuan/secrets/telegram_token
```

生产环境建议改用 systemd `LoadCredential`（见 §5）。

### 2.3 配置

`~/.config/chuan/chuan.toml`（完整示例见 `deploy/chuan.toml.example`）：

```toml
[telegram]
allowed_users = [123456789]          # 只有这些人能用
token_file = "~/.config/chuan/secrets/telegram_token"

[engine]
provider = "zai"                     # echo | zai | opencode-go
model    = "glm-5.3-flash"
api_key_file = "~/.config/chuan/secrets/zai_key"

[storage]
db = "~/.local/share/chuan/chuan.db"
```

**优先级**：CLI > 环境变量 > TOML > 内置默认。

### 2.3.1 让 agent 处理本地文件

沙盒里只有两个路径：`/ws`（工作区，可写）和 `/inbox`（只读）。

```bash
cp ~/some.pdf ~/.local/share/chuan/inbox/   # 投递给 agent
```

宿主 `/tmp`、`/home` **不会**挂进沙盒——这是刻意的隐私边界。数量多时建议用 `cp` 而不是挂载。

### 2.4 运行

```bash
chuan --config ~/.config/chuan/chuan.toml
```

首次启动会：迁移数据库 → 恢复上次未完成的调用 → 注册指令 → 开始长轮询。

### 2.5 对话

给 bot 发任意消息。常用指令：

| 指令 | 作用 |
|---|---|
| `/new` | 开启新会话（保留长期记忆） |
| `/cancel` | 取消当前任务 |
| `/status` | 空闲/运行中、会话 epoch |
| `/help` | 指令列表 |
| `/facts` | 查看长期记忆 |
| `/forget <id>` | 删除一条记忆 |
| `/approve <id>` | 无按钮通道时确认审批 |
| `/deny <id>` | 无按钮通道时拒绝审批 |

### 2.6 部署为 systemd 服务

```bash
cargo build --release
sudo ./deploy/install.sh --no-enable     # 装二进制/目录/单元，不启动
# 放密钥（root 只读）+ 改 /etc/chuan/chuan.toml
sudo systemctl enable --now chuan
sudo systemctl enable --now chuan-backup.timer
```

单元要点：`Restart=always`、`StartLimitBurst=5`、`ProtectSystem=strict`、`ProtectHome=read-only`、`ReadWritePaths=/var/lib/chuan`、`MemoryMax=1G`、`LoadCredential` 注入密钥（不进 environ）。

---

## 2.7 技能（Skill）

一个技能就是一个目录：

```
skills/<name>/
├── skill.json     # 机器清单（注入上下文）
├── run.py         # 入口脚本
└── SKILL.md       # 给人看的说明（不进上下文）
```

`skill.json`：

```json
{
  "name": "echo",
  "description": "把输入文本原样回显",
  "params": {"type":"object","properties":{"text":{"type":"string"}},"required":["text"]},
  "entry": "run.py",
  "interpreter": "python3",
  "timeout_ms": 10000,
  "network": false,
  "examples": [
    { "args": { "text": "hi" }, "expect_contains": "hi" }
  ]
}
```

`examples` 是验收样例：`publish_skill` 会逐条在沙盒里跑，全部通过才允许安装。

模型通过 `run_skill(name, args)` 调用；脚本从 **stdin 读 JSON**、往 **stdout 写结果**、退出码 0 表示成功。
技能在沙盒里执行，只能访问 `/ws`（工作区）与只读的 `/skills`；`network: true` 才允许出网（当前实现默认断网）。

新增/修改技能后**无需重启**：目录 mtime 变化会自动重新扫描。

### 沉淀闭环

一次改动过文件的 run 结束后，终稿会带一个「🧪 沉淀为 Skill」按钮：

```
点击 → 一次普通 run（按可编辑 SOP 执行）
     → 泛化脚本，写 /ws/publish/<name>/{skill.json,入口脚本,SKILL.md}
     → publish_skill(name)
          ├─ 校验 name / interpreter / entry
          ├─ 用 skill.json 的 examples 在沙盒里 dry-run
          │    失败 → 草稿留档 .drafts/<name>/REJECTED.md，不污染可用技能
          └─ 通过 → 备份旧版到 .backup/ → 原子替换 → 热加载
```

- 没有 `examples` 的技能**一律拒绝安装**——不可验证的东西不进系统。
- `rollback_skill(name)` 退到上一版；回滚前的版本也会留档，所以回滚可逆。
- 流程本身是文本：改 `~/.config/chuan/prompts/distill.md`（或 `[skills] distill_prompt_file`）即可调整提炼步骤、验收要求，**不用改代码**。


### 导入外部技能

不用指令，直接把内容发进来：

```
把这个技能整理进来：<粘贴说明或脚本>
用 inbox 里那个目录复刻一个技能
```

模型会先 `playbook("import-skill")` 取规范，再按「读意图 → 判断能否做成 CLI 技能（纯提示词型会被劝退）→
盘点依赖 → 重写并补 examples → `publish_skill`」走，安装时会弹一次确认。

SOP 是普通文本，放 `~/.config/chuan/playbooks/import-skill.md`（`distill.md` 同理对应沉淀流程）；
改流程只改文件，不用重编译。

### 让 agent 自己接服务（service）

主人只做一件事：**把 key 放进 `~/.config/chuan/secrets/`**。剩下的由 agent 完成：

```
你：滴答的 key 放好了，帮我接上
agent：service(action="add", name="dida", kind="mcp",
                base="https://mcp.dida365.com",
                token_file="~/.config/chuan/secrets/dida_token")
     → 当场握手；失败就按错误改参数再试
     → 成功后写 services.d/dida.toml，本次立即生效、下次启动也加载
```

- 连不上就不写配置（不把坏配置留盘）；
- 工具太多可以带 `tools` / `catalog` 控制注入量；
- `service(action="remove", name=…)` 删配置并卸载；
- 属于 `self_modify` 域：**创建时会要确认**（因为它会建立对外连接并改自身配置）；
- **凭据路径必须是宿主机绝对路径**（或 `~/`），且**只能放在 `<config>/secrets/` 下**——防止把
  `~/.ssh/id_rsa` 之类随便什么文件当 token 发到远端；相对路径、`..`、目录外文件一律拒绝；
- 永远不会读 token 内容进上下文。

### MCP over HTTP（mcp）

支持 Streamable HTTP 形态的 MCP 服务（不做 stdio），和 `http` 共用 `[services]` 表，多一个 `kind`：

```toml
[services.dida]
kind = "mcp"
base = "https://mcp.dida365.com"
token_file = "~/.config/chuan/secrets/dida_token"
```

启动时做 `initialize → tools/list`，把远端工具**动态注入 system prompt**（和技能目录同一套做法）：

```
MCP 服务 `dida`（用 mcp(server="dida", tool=…, args=…) 调用）：
- list_tasks（参数：status, limit）：列出任务
- create_task（参数：title, due, projectId）：创建任务
```

- 模型只给 `server/tool/args`；**token 由宿主注入**，不进上下文、不进日志；
- **目录形态可配**（`catalog`）：
  - `names`（默认）：只列工具名，57 个工具约 800 字，够模型选；
  - `full`：名字 + 60 字描述，适合工具少的服务；
  - `summary`：只报数量，一个工具名都不列（最省，代价是先查一次）；
- 参数细节按需查：`mcp(server="dida", tool="__list__")`，支持 `query` 过滤
  （如 `query="task"` 只列任务相关）；
- 工具太多可以用 `tools = ["create_task", …]` 只暴露一部分；
- 不跟随重定向；响应按 `text/event-stream` 或 JSON 两种解析；
- 服务连不上只告警，不阻塞启动（其它工具照常可用）。

### 大响应的处理

`http` / `web_fetch` 用同一套策略，**按内容类型决定给什么**：

| 类型 | 进上下文 | 落盘 |
|---|---|---|
| JSON ≤12KB | 全文 | — |
| JSON >12KB | 结构（键/数组长度/首项键）+ 1.2k 预览 | `.json` |
| HTML | 去 `<script>/<style>/<标签>` 的正文，≤8k 字 + 标题 | `.html` |
| text/markdown | ≤8k 字 | `.txt` |
| CSV | 表头 + 前 20 行 | `.csv` |
| 图片 | **不进**：尺寸（PNG/JPEG/GIF 可读）+ 大小 + 路径 | `.png/.jpg/.gif` |
| 音视频 / 二进制 | **不进**：大小 + 路径 | `.mp4/.bin` |

落盘在 `/ws/.tool-results/<call_id>.<ext>`，模型能用 `exec` 直接处理（python/grep），图片可用 `send_file` 转给你看。
清理跟 workspace 一起走（默认 7 天）。

### 代理（proxy）

目标板上的代理写在配置里（或沿用标准环境变量）：

```toml
[proxy]
url = "http://127.0.0.1:7890"   # 留空则用 https_proxy / http_proxy
sandbox   = "auto"   # auto = 只在 exec(proxy=true) 时注入；always / never
web_fetch = "auto"   # auto = 直连失败再试代理；always / never
services  = "never"  # http/mcp 默认直连；国内服务绕代理更慢
```

- 注入的是 `http_proxy` / `https_proxy` / `all_proxy` / `no_proxy`（`no_proxy` 覆盖 localhost 与私网）；
- **联网 lane 不隔离 netns**，所以沙盒里的 `127.0.0.1` 就是宿主，能直接连本地代理；离线 lane 没有网络，代理无意义；
- 模型侧也给了选择权：`exec(proxy=true)` 与 `web_fetch(proxy=true|false)`，不传则按上面的默认策略；
- 代理地址会注入 system prompt（固定内容），所以脚本里不用猜，也不该把地址写死进技能或记忆。

### 第三方 HTTP 服务（http）

带鉴权的 API 用命名服务调用，**token 由宿主注入，模型看不到也写不进 URL**：

```toml
[services.dida]
base = "https://api.dida365.com"
token_file = "~/.config/chuan/secrets/dida_token"
# header = "Authorization: Bearer {token}"   # 默认即此

[services.weread]
base = "https://i.weread.qq.com"
token_file = "~/.config/chuan/secrets/weread_token"
```

模型只给路径：

```
http(service="dida", method="POST", path="/open/v1/task", body={...})
```

- 只能访问该服务配置的 host（path 里出现协议/域名直接拒绝），不跟重定向；
- 审计日志记服务名、方法、路径、状态、耗时，**不记 header、不记 body**，错误信息里的 token 会被抹掉；
- 服务本身的 API 说明放 `playbook`（例如 `playbook("dida-api")`），不进内核。

### 文件库（keep / files）

`/ws` 是草稿区（7 天清理），**要留下的东西用 `keep` 存进文件库**：

```
keep(path="/ws/out/采购清单.xlsx", tags=["家庭","采购"])
files(action="list", query="采购")
files(action="export", id="file-xxxx")   → 取回 /ws，再 send_file 发给你
files(action="del", id="file-xxxx")
```

- 落盘 `~/.local/share/chuan/files/<摘要>/<名字>`，**按内容去重**，永久保存；
- 索引是 doc（`kind=file`），带名字/标签/大小/时间，可检索；
- 三个数据平面里的"资料（KB）"从此有了最小实现：`/ws` 草稿 → `skills` 能力 → `files` 资料。

### 把文件发给用户（send_file）

模型用 `send_file(path, caption)` 把 `/ws` 里的文件发到当前会话：图片走 `sendPhoto`，其余走 `sendDocument`。
只允许发工作区内的文件，单文件 50MB；没有文件能力的通道会降级成一句“文件在 X”。

典型用法就是**打印前的预览**：生成 PDF → `send_file` 给你看 → 你确认后再打。

### 沙盒的额外挂载（把宿主服务给技能用）

默认沙盒里只有 `/ws` `/inbox` `/skills` `/media` `/py` `/node`。要让技能调用宿主服务（打印、D-Bus…），
由**主人**在配置里显式授权：

```toml
[sandbox]
mounts = [
  { src = "/run/cups", dst = "/run/cups", ro = true },   # 打印/查打印机状态
]
```

- `ro = true` 只读；`/ws` `/etc` `/proc` 等保留挂载点不能被覆盖（启动时校验并跳过非法项）；
- 这是**能力授权**：挂 `/run/cups` = 沙盒里任何代码都能提交/取消打印任务；挂 `docker.sock` 等于容器逃逸。
  所以只能人来写，模型只能建议；
- 设计取向：**内核不提供领域工具**（不做 `print`），只提供这种通用口子；打印、发现、查墨量由技能实现，
  换到没有 CUPS 的环境时改技能即可。

### 图片与文件（media）

聊天里发来的图片/文档，通道**只负责下载落盘**，不解析内容：

```
~/.local/share/chuan/media/<digest>.<ext>    ← 宿主（按内容去重，30 天清理）
        ↓ 只读挂载
/media/<digest>.<ext>                        ← 沙盒
```

用户的每条消息后面会附一段：

```
附件（沙盒内只读，路径可直接给 exec 用）：
- /media/abc123.pdf（application/pdf，1200 KB，原名 通知.pdf）
```

**视觉（`[vision] enabled`）**：开启后图片会**同时**作为多模态内容发给模型（最多 4 张、单张 ≤4MB），
所以它能直接看图；历史里只保留路径，旧图不重复上传。默认关闭，因为**开了就等于把照片发给模型厂商**。

**要不要识别、怎么识别，由模型自己决定**：PDF 让它 `exec` 里用 `pdftotext`/pypdf，图片目前只能当文件处理
（还没有视觉能力，模型会直说“看不到图片内容”）。这样通道不绑定任何解析逻辑，以后接 webui / 飞书只是换个下载实现。

### 身份与通道（identity）

用户（User）与通道（Channel）是分开的，一个人可以绑多个通道地址：

```toml
[identity]
default_user = "owner"

[[identity.user]]
id = "owner"
name = "主人"
channels = ["telegram:123456789"]      # 以后再加 "webui:u1"、"feishu:ou_xxx"
```

- 入站消息用 `(channel, addr)` 解析成 user；查不到就落到 `default_user`，再没有就退化成地址本身。
- **提醒记在 user 名下**：`/reminders` 只列自己的，投递默认走这个人绑定的通道。
- **暂不做隔离**：`/ws`、技能目录、facts 的 `family` scope 仍是全局共享；`user:<actor>` scope 已按人分开。
- 要做多用户隔离时动三处：`/ws` 挂载按 user 分目录、审批卡固定发给 owner、facts 去掉 family 共享。结构上都留了口子。

**账号从哪来（预留，未实现）**

现在是**配置即授权**：`[identity]` 里写了谁，谁就能用；改配置 = 加人。`default_user` 是兜底，
加第二个真人前必须删掉，否则新地址会直接继承 owner 权限。

以后要做自助接入，分两层（见 `docs/DECISIONS.md` D16）：

- **种子**：`[identity]` 里的绑定 = 引导（bootstrap），是唯一的信任起点，只能改文件；
- **运行期**：`users` / `bindings` 落库；陌生地址第一次说话 → 给 owner 弹一张"是否接入"的卡，
  批准才写绑定；owner 也可以 `/invite` 发一次性邀请码给家人。撤销用 `/revoke`。

原则：**任何新地址都不能自助获得权限**，必须由 owner 批一次。

### 出网取数与审计（web_fetch / net_log）

沙盒没有网络，外部资料一律走 `web_fetch`（宿主代取）：

- 只允许 `http/https`；**拒绝回环与内网地址**（内网调试走 `lan_exec`，不在这里开口子）；
- 单次 2MB 上限、20s 超时、最多 5 跳重定向（每跳都重新查地址）；
- **每次访问（含被拒绝的）都写一条 JSONL**：时间、URL、域名、query 长度、状态码、字节、耗时、原因；
- `net_log(days, limit)` 按域名聚合并给最近明细，模型据此做审计。

`[policy] outbound = "allow"`（默认）表示不逐次确认，靠事后审计兜底。可以直接问：

```
告诉我本周访问过的域名，按次数排序，看看有没有带 token 的 query
```

### 定时任务（remind）

```
每天早上 9 点，把 Hacker News 热榜前 10 条整理给我
每周一、四 18:30 提醒我倒垃圾
```

模型会把这句话安排成一条提醒：`schedule` 支持 `daily` / `weekdays` / `weekly`（选周几）/ `once`（指定日期时间）。
到点后**不是发一条静态文本，而是重新触发一次普通 run** —— 所以"取热榜 + 摘要"仍然走工具和模型。

- 三种形态（优先级 `message` > `skill` > 纯 prompt）：
  - **静态提醒** `message`：到点只发这句话，**不跑模型**（如“明天记得穿红色校服”）；
  - **pinned** `skill`：到点跑已安装技能，stdout 原样发回（格式固定、时延稳定）；
- **pinned + polish** `skill` + `polish`：脚本只拉数据，stdout 交给模型按指令加工（翻译/分类/排版）——"数据靠脚本、表达靠模型"；
  - **prompt**：到点把 text 当用户消息重新跑一次（适合需要临场判断的开放任务，时延 30–60s 起）。
- 审批卡上会先写**解析后的触发时间**（`静态提醒 · 2026-10-29 19:30（周四）`），参数放在后面供核对；
- 模型收到的每条消息都带一行 `（现在：2026-10-26 周一 17:40（UTC+08:00））`，所以它能自己把“本周五”算成具体时间；
- 只支持**固定时区偏移**（`[app] utc_offset`，默认 `+08:00`），不做夏令时；
- 关机错过的任务：2 小时内补跑一次，超过就跳过，只排下一次；
- 创建要确认（`self_modify = "ask"`）；查看/取消：`/reminders`、`/reminders del <id>`。

### 装依赖（install_deps）

`pip` / `npm` 装进运行时目录，装完在**离线**沙盒里可用：

```
/var/lib/chuan/rt/
├── py/      venv  → 沙盒里挂 /py（技能用 /py/bin/python3）
└── node/    node_modules → 沙盒里挂 /node
```

模型调 `install_deps(ecosystem, packages)`，跑在「有网 + 只写 /rt」的 lane 里，逐条确认。
**装之前会先探测**：能 `import` 到（apt 装的或已在 `/py` 里）就直接返回，不联网、不重复装。
venv 建的时候带 `--system-site-packages`：**apt 装在系统里的 Python 包也能直接用**。
所以在 riscv64 上优先用 `apt install python3-xxx` 装大件（numpy、Pillow 这类通常有预编译），
`install_deps(pip)` 只用来补 apt 里没有或需要新版本的包——`/py` 里的会优先于系统包命中。
`[runtime] pip_index` 可以换成国内源。

### 内网调试（lan_exec）

需要访问打印机、网关管理页、Home Assistant 这类设备时，模型会用 `lan_exec`：保留网卡，只挂 `/ws`，
看不到 /home 与凭据，命令**逐条确认**（`[policy] lan = "ask"`）。

⚠️ 技术上做不到"只通内网不通公网"：有网卡就两者都可达，所以这条 lane 靠审批兜底，不是靠网络边界。
网上导入的技能跑在离线的 `exec` 里，**调不到 `lan_exec`**——这两条路是隔开的。

### workspace 清理

`/ws` 是**草稿区**，不是档案库（真正的结论在账本与技能目录里）。所以会定期清理：

- 启动时清一次，之后每 6 小时一次；
- 超过 `[actor] workspace_keep_days`（默认 7 天）的文件删掉；
- 总量超过 `[actor] workspace_max_mb`（默认 256MB）时，从最旧的删到限额内；
- 只动 workspace 内部，不跟随符号链接，删完顺手清空目录，根目录保留。

两个值都可以设 `0` 表示该维度不设限。要长期保留的东西别放 `/ws`——放进 `skills/` 或写进记忆。

---

## 3. 配置参考

| 段 | 关键项 |
|---|---|
| `[app]` | `name`、`timezone`（展示用）、`utc_offset`（定时任务，`+08:00`） |
| `[telegram]` | `allowed_users`、`token_file`、`stale_after_s` |
| `[engine]` | `provider`、`model`、`base_url`、`api_key_file`、`proxy`（模型专用，默认直连）、`system_prompt_file`、`max_retries`、`retry_backoff_ms` |
| `[actor]` | `max_turns`、`wall_clock_ms`（0=不限）、`workspace`、`workspace_keep_days`、`workspace_max_mb`、`inbox`、`context_budget_tokens` |
| **`[proxy]`** | `url`（留空沿用 `https_proxy`）、`sandbox`、`web_fetch`、`services` |
| **`[vision]`** | `enabled`（图片走多模态，会上传 provider） |
| `[policy]` | `sandbox`/`self_modify`/`outbound`/`lan`/`install`/`host_exec`（`allow`/`ask`/`deny`）、`require_approval`（额外加严） |
| **`[sandbox]`** | `mounts`（把宿主服务只读挂进沙盒，如 `/run/cups`） |
| **`[services.<name>]`** | `kind`（`http`/`mcp`）、`base`、`token_file`、`header`、`tools`、`catalog` |
| `[runtime]` | `dir`、`pip_index`、`npm_registry` |
| `[skills]` | `dir`、`playbooks_dir`、`distill_prompt_file` |
| **`[media]`** | `dir`、`keep_days` |
| **`[library]`** | `dir`（文件库，永久） |
| **`[net]`** | `log`（出网审计 JSONL） |
| **`[identity]`** | `default_user` + `[[identity.user]]`（user ↔ 多通道） |
| `[storage]` | `db`、`lock` |
| `[backup]` | `dir`、`keep_days`、`keep_min`、`max_total_mb` |

预设 provider：

| provider | base_url | 默认模型 |
|---|---|---|
| `echo` | —（离线回显，用于验证链路） | — |
| `zai` | `https://open.bigmodel.cn/api/coding/paas/v4` | `glm-5.3-flash` |
| `opencode-go` | `https://opencode.ai/zen/go/v1` | `deepseek-v4.1-flash` |

> 模型调用默认**直连**（不继承 `https_proxy`）；Telegram 仍需代理时用 `CHUAN_PROXY` / unit 里的 `Environment=https_proxy=...`。

---

## 4. 日常运维

```bash
# 备份（守护进程运行中也可执行）
chuan backup            # 目录与保留策略来自 [backup]

# 看日志
journalctl -u chuan -f

# 重载配置
systemctl restart chuan
```

数据库迁移前会自动备份为 `<db>.pre-migrate-v<旧版本>.db`。

---

## 5. 凭据管理（部署）

> 这一节是**部署约定**，与实现无关。以后接打印机、日历、MCP server 等新依赖时，同一套规则继续用。

### 5.1 五条原则

1. **一个依赖一份凭据**，独立命名（`telegram_token`、`llm_zai`、`printer_api`…），便于单独轮换与吊销。
2. 凭据只允许存在两处：**加密凭据文件**（静态）+ **服务进程内存**（运行时）。
   不得出现在：TOML、命令行参数、聊天记录、git、workspace、备份、日志。
3. 注入只走 **systemd 凭据**；不要用 `Environment=` / `EnvironmentFile=`（内容会进 `/proc/<pid>/environ` 并被子进程继承）。
4. **最小可达**：哪个工具需要哪份凭据，就只给它那一份。宿主/提权执行一律 `env -i`，不挂载凭据目录。
5. **可轮换**：替换密文 → `systemctl restart`；泄露时先在源头吊销（BotFather `/revoke`、控制台删 key）再替换。

### 5.2 目录与权限

```
/etc/chuan/secrets/
├── telegram_token.cred   # root:root 0600
├── llm_zai.cred
└── printer_api.cred
```

明文文件只用于一次性加密，加密后删除。目录 `0700 root`，服务用户不可读。

### 5.3 加密与注入

```bash
# 一次性：加密（绑定本机；有 TPM 时自动使用）
systemd-creds encrypt --name=telegram_token /tmp/tg.plain /etc/chuan/secrets/telegram_token.cred
systemd-creds encrypt --name=llm_zai        /tmp/zai.plain /etc/chuan/secrets/llm_zai.cred
shred -u /tmp/tg.plain /tmp/zai.plain
```

```ini
[Service]
LoadCredentialEncrypted=telegram_token:/etc/chuan/secrets/telegram_token.cred
LoadCredentialEncrypted=llm_zai:/etc/chuan/secrets/llm_zai.cred

# %d = 本服务的凭据目录（仅本服务可读，停止即销毁）
Environment=CHUAN_TELEGRAM_TOKEN_FILE=%d/telegram_token
Environment=CHUAN_KEY_FILE=%d/llm_zai
```

没有 TPM 的机器用 `LoadCredential=`（明文放 `/etc/chuan/secrets`，`0400 root`），安全性次之但仍然隐藏于服务用户。

### 5.4 未来工具凭据

| 依赖 | 凭据名 | 注入方式 |
|---|---|---|
| 打印机（带鉴权的网络打印机） | `printer_api` | 由**打印技能**使用；需要鉴权时经 `[sandbox] mounts` 或后续的 `http` 服务注入，不进沙盒环境 |
| MCP server | `mcp_<name>` | 宿主侧读取，调用时由内核注入 header；token 永不进上下文 |
| 日历 / 邮箱 / 智能家居 | `cal_*`、`mail_*` | 同上，一个依赖一份 |
| 技能（Skill） | **不发放** | 技能是纯函数，只能碰 workspace；需要外部能力时由宿主代劳 |

### 5.5 审计与排障

```bash
# 权限自检：应无任何输出（没有组/其他人可读的文件）
find /etc/chuan/secrets -perm /077 -ls

# 确认明文已清除
grep -rIl 'sk-' /etc/chuan /var/lib/chuan 2>/dev/null

# 备份必须排除 secrets：备份只含 DB，不含凭据
```

### 5.6 与 agent 权限的关系

`Host` / `HostElevated`（见 `docs/DECISIONS.md` D15）**不能**读取凭据目录：子进程用 `env -i` 启动、不挂载凭据、不继承 `*_FILE` 环境变量。某个工具确实需要凭据时，由内核以 fd 传入、用完即关。**能给 agent 执行命令的权限，绝不能顺带给出读密钥的权限。**

## 6. 排障

| 现象 | 原因 / 处理 |
|---|---|
| `409 Conflict: can't use getUpdates while webhook is active` | 该 bot 还挂着 webhook：`curl -X POST https://api.telegram.org/bot<token>/deleteWebhook` |
| `400 MissingSessionID` | `opencode-go` 缺 `x-opencode-session`（程序已自动加；自定义 provider 需注意） |
| 首字很慢（5–10s） | 模型侧排队；zai 比 opencode-go 慢且波动大 |
| 工具不执行、只说步骤 | 检查 system prompt 与模型是否支持 tool calling |
| `another instance is running` | 已有进程持锁；`pkill -x chuan` 或检查 systemd |

---

## 7. 开发

```bash
cargo test --workspace          # 51 个测试
python3 scripts/check-layering.py   # 依赖守卫：kernel 不得依赖 IO 实现
```

分层与契约见 `docs/DESIGN.md`、`docs/INTERFACES.md`；改契约必须同步文档（`AGENTS.md §6`）。
