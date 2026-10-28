# 运维基础

> 这一页是"把系统跑起来并维持住"的最小集：配置、密钥、单元、时间、重试、备份、磁盘。
> 设计依据见 `DESIGN.md`，数据结构见 `DATA-MODEL.md`。

---

## 1. 配置

**单一入口**：`~/.config/chuan/chuan.toml`（或 `/etc/chuan/chuan.toml`，用 `--config` 指定）。
优先级：**CLI > 环境变量 > TOML > 内置默认**。

```toml
[app]
name = "home"
# 定时任务用的本地时区偏移（不做 DST）
utc_offset = "+08:00"
timezone = "Asia/Shanghai"

[telegram]
allowed_users = [123456789]
stale_after_s = 300
token_file = "~/.config/chuan/secrets/telegram_token"

[engine]
provider = "zai"                     # echo | zai | opencode-go
model    = "glm-5.3-flash"
api_key_file = "~/.config/chuan/secrets/zai_key"
# base_url = "..."                   # 覆盖预设；以 /chat/completions 结尾则原样用
# proxy = "http://127.0.0.1:7890"    # 模型走代理；不写=直连
# system_prompt_file = "~/.config/chuan/prompts/system.md"

[actor]
max_turns = 8
# 0 = 不限（长任务优先）：卡住靠 单轮 HTTP 超时(180s) / 工具超时 / max_turns / 手工 /cancel 收口
wall_clock_ms = 0
# workspace 清理：/ws 是草稿区，不是档案库；0 = 该维度不设限
workspace_keep_days = 7
workspace_max_mb = 256
context_budget_tokens = 24000
workspace = "~/.local/share/chuan/workspaces"

[storage]
db = "~/.local/share/chuan/chuan.db"
lock = "~/.local/share/chuan/chuan.lock"

[skills]
dir = "~/.local/share/chuan/skills"
# 沉淀 SOP（可选）；默认读 ~/.config/chuan/prompts/distill.md，找不到用内置
distill_prompt_file = "~/.config/chuan/prompts/distill.md"
# 操作规范目录（playbook），可覆盖内置 SOP
playbooks_dir = "~/.config/chuan/playbooks"

[identity]
# 一个人可以有多个通道地址；找不到绑定时用 default_user
default_user = "owner"

[[identity.user]]
id = "owner"
name = "主人"
channels = ["telegram:123456789"]

[policy]
# 审批轴是"执行域"（能力越界），不是命令黑名单：
#   sandbox      沙盒内（/ws 可写、宿主不可见、无网络）→ 问也没用
#   self_modify  改自身（装/回滚技能、改配置）        → 该问
#   outbound     出网：真正的闸是域名白名单          → 白名单内免问
#   host_exec    宿主执行/提权                      → 默认关闭
sandbox = "allow"
self_modify = "ask"
outbound = "allow"
lan = "ask"        # 内网调试（有网卡，局域网+公网都可达）
install = "ask"    # 装依赖（pip/npm，供应链动作）
host_exec = "deny"
# 额外加严的工具名（只能加严，不能放松 host_exec = "deny"）
require_approval = []

# 命名服务（可选）。kind = "http"（默认）或 "mcp"；token 只放凭据文件，权限 0600
# [services.dida]
# kind = "mcp"
# base = "https://mcp.dida365.com"
# token_file = "~/.config/chuan/secrets/dida_token"
# tools = ["create_task", "list_undone_tasks_by_time_query"]

## 视觉（图片理解）

```toml
[vision]
enabled = true     # 默认 false
```

- 开启后，聊天里的图片（jpeg/png/webp）作为多模态内容随当轮消息发给模型，**能被"看见"**；
- 限制：每轮最多 4 张、单张 ≤4MB；历史里只保留 `/media/...` 路径，旧图不会重复上传；
- 隐私：**开这个等于把图片发给模型提供商**。家里有小孩照片，想清楚再开；
- 关闭时模型只拿到文件路径，会明确说"看不到图片内容"。

## 代理（proxy）

板子在国内、GitHub/Telegram/境外 API 都要过代理时，**一处配置全用它**：

```toml
[proxy]
url = "http://127.0.0.1:7890"   # 留空则读 https_proxy / http_proxy
sandbox   = "auto"    # auto = 只在 exec(proxy=true) 时注入 / always / never
web_fetch = "auto"    # auto = 直连失败再试代理 / always / never
services  = "never"   # http/mcp 默认直连（国内服务绕代理更慢）
```

用到它的地方：

| 位置 | 行为 |
|---|---|
| Telegram | `CHUAN_PROXY` 优先，其次 `[proxy].url`，都没有则 reqwest 读环境变量 |
| 模型（LLM） | 只看 `[engine] proxy` / `CHUAN_LLM_PROXY`，与 `[proxy]` 无关 |
| 联网 lane | `lan_exec(proxy=true)` 或 `sandbox="always"` 时注入 `http_proxy/https_proxy/all_proxy` + `no_proxy` |
| 离线 lane | **不注入**：没有网卡，注入了也连不上；`exec` 因此不提供 `proxy` 参数 |
| `web_fetch` | 双客户端；`auto` 先直连、失败再走代理；`proxy=true/false` 可显式覆盖 |
| `install_deps` | **总是**带代理（pip/npm 几乎一定要出网） |
| `http` / `mcp` | 仅 `services="always"` 时走代理 |

**不需要**在 systemd unit 里加 `Environment=https_proxy=...`——配置里写了就够了（除非你只是想让 `[proxy].url` 留空、靠环境变量兜底）。

注意：联网 lane（`lan_exec`、`network: true` 技能）**不隔离 netns**，所以里面的 `127.0.0.1` 就是宿主 loopback，能直接连本地代理；离线 lane（`exec`）连 `lo` 之外什么都没有，DNS 都不通，代理无从谈起。

**安装阶段**的代理是另一回事（下载 GitHub 资产）：见 README 第 0 节，用 `curl -x "$P" ... | sudo https_proxy="$P" bash`，或 `sudo ./install.sh --proxy "$P"`。

## 凭据与路径

| 东西 | 位置 | 谁创建 |
|---|---|---|
| 凭据（token/key） | `<config>/secrets/<name>`，目录 `0750 root:chuan`，文件见下 | **主人**（agent 只使用，永不读取内容） |
| 主配置 | `<config>/chuan.toml` | 主人 |
| 运行期新增服务 | `<config>/services.d/<name>.toml`（目录 `0770 root:chuan`） | agent 通过 `service` 工具写；安装脚本预建，缺了会导致"本进程可用、重启丢失" |
| playbook（SOP/API 说明） | `<config>/playbooks/<topic>.md` | agent 或主人 |
| 技能 | `<skills>/<name>/` | agent 通过 `publish_skill` |
| 文件库（长期） | `<library>/<digest>/<name>` | agent 通过 `keep` |
| 聊天附件 | `<media>/<digest>.<ext>` | 通道自动落盘（30 天） |
| 工作区（沙盒 `/ws`） | `<workspace>/` | agent（7 天清理） |

规则：

- agent 传给 `service` 的 `token_file` 必须是**宿主机绝对路径**（允许 `~/`），
  且 `canonicalize` 后必须落在 `<config>/secrets/` 内——防止把任意宿主文件当 token 发出去；
- **权限分两种**，别一律 `0400 root:root`：
  - `LoadCredential` 注入的（`telegram_token`、模型 key）：`0400 root:root`——systemd 以 root 读，进程拿 fd；
  - `service` 的 `token_file`（dida365、weread…）：`0440 root:chuan`——**daemon 自己以 chuan 身份读**。
    目录本身也必须 `0750 root:chuan`，否则光是 `x` 位缺失就会 `Permission denied`（不是文件权限问题）；
- 凭据内容只由宿主在发请求时读取，**不进上下文、不进日志、不进技能**；
- 带签名的 webhook（钉钉/飞书）用 `sign` 在宿主侧算 HMAC-SHA256，见 README「带签名的 webhook」；
- 工具输出在进上下文与账本前会过一遍泄漏检测（`harness_kernel::leak`），命中打码并记 `warn`；
- 这些路径会按实际配置注入 system prompt，所以可以直接问 agent「key 该放哪」。

`<config>` 默认 `~/.config/chuan`，`<skills>/<library>/<media>` 默认在 `~/.local/share/chuan/` 下。

[library]
# 文件库：keep 存下来的产物（永久）
dir = "~/.local/share/chuan/files"

[sandbox]
# 显式授权给沙盒的宿主挂载（默认空）。例：让技能能用 CUPS
# mounts = [{ src = "/run/cups", dst = "/run/cups", ro = true }]

[net]
# 出网访问记录（JSONL，8MB 自动轮转），net_log 用它做审计
log = "~/.local/share/chuan/net-access.jsonl"

[runtime]
dir = "~/.local/share/chuan/rt"
# venv 带 --system-site-packages：apt 装的 python 包可直接用；
# pip 装进 /py 的包优先命中。大件优先 apt，缺的再用 install_deps。
# pip_index = "https://mirrors.tuna.tsinghua.edu.cn/pypi/web/simple"

[backup]
dir = "~/.local/share/chuan/backups"
keep_days = 7
keep_min = 2
max_total_mb = 256
```

**优先级示例**（已实测）：

| 来源 | provider |
|---|---|
| TOML 写 `zai`，无其它覆盖 | `zai` |
| `CHUAN_PROVIDER=echo` | `echo` |
| `CHUAN_PROVIDER=echo` + `--provider zai` | `zai`（CLI 赢） |

**环境变量覆盖**（保留兼容名）：

| 变量 | 覆盖 |
|---|---|
| `CHUAN_CONFIG` | 配置文件路径 |
| `CHUAN_PROVIDER` / `CHUAN_MODEL` / `CHUAN_BASE_URL` | `[engine]` |
| `CHUAN_KEY_FILE` / `CHUAN_API_KEY` | `[engine]` 密钥 |
| `CHUAN_LLM_PROXY` | `[engine]` 代理 |
| `CHUAN_TELEGRAM_TOKEN` / `_FILE` | Telegram token |
| `CHUAN_ALLOWED_USERS` | 逗号分隔 |
| `CHUAN_DB` / `CHUAN_LOCK` / `CHUAN_WORKSPACE` | 路径 |
| `CHUAN_SYSTEM_PROMPT_FILE` | system prompt（整体替换） |
| `CHUAN_SYSTEM_PROMPT_EXTRA_FILE` | 追加到内置 system prompt 末尾（推荐） |

**命令行**：

```bash
chuan                                   # 用 TOML
chuan --config /path/chuan.toml          # 指定配置
chuan --provider echo                    # 临时覆盖
chuan backup                             # 备份 + 按策略清理
```

**密钥不写进 TOML**：只允许 `token_file` / `api_key_file` 引用（CLI 与环境变量中的明文仅用于临时调试）。

## 2. 密钥

### 2.1 三种方式的安全性排序

| 方式 | 静态保护 | 运行时暴露面 | 结论 |
|---|---|---|---|
| `LoadCredentialEncrypted=` | **加密**（主机/TPM 密钥） | 仅凭据目录，服务停止即销毁 | **最推荐** |
| `LoadCredential=` | 文件 root-only，服务用户不可读 | 凭据目录 fd 路径，不进 environ | 推荐 |
| 文件 0600 + 应用读取 | 依赖文件权限 | 同用户的任意进程可读（含 agent 的 Host 执行） | 可接受 |
| `EnvironmentFile=` / `Environment=` | 依赖文件权限 | **`/proc/<pid>/environ` 同用户可读、子进程继承** | **不推荐** |

关键点：**环境变量不是"放进文件就安全了"**。`EnvironmentFile` 只是让 systemd 把内容读进进程环境，任何同用户进程都能从 `/proc/<pid>/environ` 拿到，且会被子进程继承（沙盒里我们 `--clearenv` 清掉，但宿主侧执行不会）。

### 2.2 推荐写法

```bash
# 一次性：把明文文件加密成凭据（绑定本机）
systemd-creds encrypt --name=telegram_token /etc/chuan/secrets/telegram_token /etc/chuan/secrets/telegram_token.cred
systemd-creds encrypt --name=zai_key        /etc/chuan/secrets/zai_key        /etc/chuan/secrets/zai_key.cred
chmod 600 /etc/chuan/secrets/*.cred
```

```ini
[Service]
LoadCredentialEncrypted=telegram_token:/etc/chuan/secrets/telegram_token.cred
LoadCredentialEncrypted=zai_key:/etc/chuan/secrets/zai_key.cred
# %d = 凭据目录，仅本服务可读，服务停止即销毁
Environment=CHUAN_TELEGRAM_TOKEN_FILE=%d/telegram_token
Environment=CHUAN_KEY_FILE=%d/zai_key
```

没有 TPM 的机器也可以用 `LoadCredential=`（明文文件放 `/etc/chuan/secrets`，root `0400`，服务用户不可读）。

### 2.3 轮换

替换 `.cred` → `systemctl restart chuan`；无需重新编译。Telegram token 泄露时先在 BotFather `/revoke`，再更新凭据。

### 2.4 与应用的关系

应用只在启动时读一次并常驻内存；不会写回、不会进日志、不会进沙盒（`--clearenv`）。`chuan backup` 子命令不需要任何密钥。

## 3. systemd 单元

```ini
[Unit]
Description=chuan home assistant
After=network-online.target
Wants=network-online.target
# StartLimit* 必须在 [Unit] 段，写在 [Service] 不生效（systemd >= 229）
StartLimitIntervalSec=600
StartLimitBurst=5

[Service]
Type=simple
User=chuan
Group=chuan
SupplementaryGroups=lp            # CUPS 打印
ExecStart=/usr/local/bin/chuan --config /etc/chuan/chuan.toml
Restart=always
RestartSec=2
LoadCredential=telegram_token:/etc/chuan/secrets/telegram_token
LoadCredential=llm_api_key:/etc/chuan/secrets/llm_api_key
StateDirectory=chuan
StateDirectoryMode=0750
Environment=MALLOC_ARENA_MAX=2
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/var/lib/chuan
# 崩溃循环熔断：5 分钟内 5 次 → failed（由 OnFailure 告警）
OnFailure=chuan-alert@%n.service
MemoryMax=1G

[Install]
WantedBy=multi-user.target
```

### 加固项与 bwrap 的冲突（板上真实事故）

**不要**给这个 unit 加 `ProtectKernelTunables=yes`、`ProtectKernelLogs=yes`、`ProtectHostname=yes`。

它们会让 systemd 先动服务的 `/proc`（或 UTS namespace），之后 bwrap 在自己的 mount namespace 里挂全新 procfs 被内核拒绝：

```
bwrap: Can't mount proc on /proc: Operation not permitted
```

阴险之处在于 **daemon 照常启动、日志一切正常**，只有第一次 `exec` 才暴露——而且沙盒与联网 lane 会同时失效（两者都走 bwrap）。实测在 x86_64 + systemd 253 上可复现，与架构无关。

用这些替代（收益接近，且与 bwrap 兼容）：

```ini
ProtectSystem=strict
ProtectHome=read-only
ProtectKernelModules=yes
ProtectClock=yes
ProtectControlGroups=yes
```

`scripts/check-sandbox-unit.sh` 会从 unit 里抽出全部加固项，用 `systemd-run` 真跑一次 bwrap 验证——CI 里跑，别只靠 review。

资源限制走 `systemd-run --user --scope -p MemoryMax=`（已验证 `rc=137`）。系统服务内调用它需要 **user manager 在跑**：`loginctl enable-linger chuan`。若不可用，降级为 `prlimit`（对 Python 不可靠，需在 `/status` 标记）。

`Delegate=yes` 只在需要自建子 cgroup 时使用，当前不用。

---

## 4. 单实例

`/var/lib/chuan/chuan.lock`（`flock` 独占，非阻塞）。拿不到锁直接退出并记录。

理由：单写者 SQLite + 调度器要求绝对单实例；`flush` 时若不慎启动第二个进程，会出现双写与重复推送。

---

## 5. 启动顺序

```
1. 读配置 → 校验（含 store/retriever 组合合法性）
2. 单实例锁
3. store.migrate（失败则拒绝启动）
4. 孤儿回收（扫描 CHUAN_TURN= 标记的残留进程）
5. 打开 channel（长轮询开始）
6. 启动调度器（扫描到期任务）
7. 就绪日志 + 自检报告
```

任一阶段失败 → 非零退出，让 systemd 决定重启；**不要带病运行**。

---

## 6. 时间与时区

| 用途 | 规则 |
|---|---|
| 存储 | 一律 **UTC epoch 毫秒** |
| 展示 | 按 `app.timezone`（IANA，如 `Asia/Shanghai`） |
| 自然语言时间 | 由模型解析 + 显式时区注入 prompt，**不自己写解析器** |
| 租约 | 存储用**墙钟 UTC ms**（要跨重启）；进程内另持单调钟计算已过时长 |
| 提醒 | 存 `due_at_utc`；到点判定用 `now >= due_at`，重启即补 |
| 墙钟跳变 | 若 `now` 比上次记录小 > 5 分钟：记 **Log**（不进 Journal）；把现有租约按回跳量延长，避免被误判过期；不重发已发提醒 |

**RTC 注意**：SBC 可能无电池，开机时间可能是 1970。启动时若 `now < 2024-01-01`，等待 NTP 同步（最多 30s）或标记 `clock_untrusted`，在此期间不发定时消息。

---

## 7. 重试与退避

| 场景 | 次数 | 退避 | 计入预算 |
|---|---|---|---|
| LLM 5xx / 超时 | 3 | 1s → 2s → 4s，±20% jitter | ✅（每次成功或失败都记 usage/attempt） |
| LLM 流中断（已收到部分） | 1 | 立即重发整轮，丢弃半截文本 | ✅ |
| TG 429 | — | 按 `retry_after` 全局等待，不做次数上限 | 否 |
| 工具超时 | 0 | 不重试（由模型决定重新调用） | 否 |
| 任务租约丢失 | 由调度器 | 按 `miss_policy`（补跑/跳过） | ✅ |

**流中断的判定**：收到 `finish_reason` 之前连接关闭 = 中断；已收到 = 正常结束。半截文本永不落库。

### 7.1 Token 计数

- **真实计费以 provider 返回的 `usage` 为准**（`usage.recorded` 事件的唯一来源）。
- 压缩阈值需要**估算**：起步用 `字节数 / 3.5`（中英混排）并留 15% 余量；不引入 tiktoken（riscv64 上依赖与性能未知）。
- 估算误差通过"压缩阈值取保守值（如 context 的 70%）"吸收，不追求精确。

---

## 8. 备份与恢复

| 项 | 做法 |
|---|---|
| 每日备份 | `VACUUM INTO /var/lib/chuan/backup-YYYYMMDD.db`，保留 7 份 |
| 迁移前 | 备份到 `backup-pre-migrate-<version>.db`，失败则拒绝启动 |
| 恢复 | 停服务 → 用备份覆盖 → 启动（会跑迁移，因为备份可能是旧版本） |
| 恢复演练 | 每季度一次：在 `/tmp` 起一个实例指向备份，验证能读到最近 10 条会话 |

**不支持二进制回滚**：旧版代码读不了新 schema。所以升级前必须确认备份可读。

每日备份由 timer 触发：

保留策略走环境变量（SBC 上磁盘比天数更紧，两者同时约束）：

| 变量 | 默认 | 含义 |
|---|---|---|
| `CHUAN_BACKUP_DIR` | `/var/lib/chuan/backups` | 备份目录 |
| `CHUAN_BACKUP_KEEP_DAYS` | 7 | 超过天数删除（但至少保留 `KEEP_MIN` 份） |
| `CHUAN_BACKUP_KEEP_MIN` | 2 | 最近 N 份永不删 |
| `CHUAN_BACKUP_MAX_MB` | 256 | 总量上限，超出从最旧删起 |

```ini
# chuan-backup.service
[Service]
Type=oneshot
ExecStart=/usr/local/bin/chuan backup
Environment=CHUAN_BACKUP_DIR=/var/lib/chuan/backups
Environment=CHUAN_BACKUP_KEEP_DAYS=7
Environment=CHUAN_BACKUP_KEEP_MIN=2
Environment=CHUAN_BACKUP_MAX_MB=256

# chuan-backup.timer
[Timer]
OnCalendar=*-*-* 03:30:00
Persistent=true
[Install]
WantedBy=timers.target
```

`chuan backup` **可在守护进程运行时执行**（VACUUM INTO 读快照，不抢单实例锁）。
迁移前另有一次自动备份：`<db>.pre-migrate-v<旧版本>.db`。

---

## 9. 磁盘与寿命

| 项 | 设置 |
|---|---|
| WAL | `journal_size_limit=32MB`，每 5 分钟 `wal_checkpoint(TRUNCATE)` |
| SQLite 缓存 | `cache_size=-8000`（8MB） |
| 备份 | 保留 7 份，超出删除最旧 |
| workspace | TTL 24h，启动与每小时各清理一次 |
| artifacts | 按 sha256 去重；90 天未引用则删除（journal 保留 tombstone） |
| 告警 | 磁盘使用 > 85% 发 TG 告警 |

---

## 10. 日志字段

所有日志（journald）至少包含：

```
ts, level, component, event,
conv_id, turn_id (opt), run_id, call_id (opt),
lane (io|compile|external|model),
outcome (ok|error|timeout|cancelled),
latency_ms, tokens_in, tokens_out, cost_est
```

- **不记录**消息正文、工具参数、密钥；需要正文时记 `sha256` 与长度。
- 保留期由 journald `SystemMaxUse=200M` 控制。
- Trace 与 Log 的分工：Trace 存执行结构（可回放），Log 存运维事件。

---

## 11. 降级模式

| 触发 | 行为 |
|---|---|
| 沙盒自检失败 | 拒绝执行 `exec`，只保留只读工具；TG 告知主人 |
| LLM 连续失败 N 次 | 进入只读（不调模型），保留定时提醒 |
| 磁盘 > 95% | 停止备份、停止 media 下载，只保留文本 |
| 时钟不可信 | 暂停定时消息，等待同步 |

降级必须**可见**（`/status` 与启动日志），不允许静默。
