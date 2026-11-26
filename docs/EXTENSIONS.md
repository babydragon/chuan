# 扩展机制：通道适配器与 OAuth 旁路

> **状态：设计，未实现。** 本文定义两件事：新通道怎么接（D44）、需要用户授权的第三方服务怎么接（D45）。
> 结论先写：**内核不引入插件 ABI**——平台代码、OAuth 流程都在旁路进程里；内核只保留协议、边界与审计。
>
> 非目标：WASM、原生动态库（`.so`）、`npm require` 式插件、插件热加载、插件市场。
> 术语：**适配器**（adapter）指实现通道协议的独立进程；**helper** 指 OAuth 旁路进程。

---

## 0. 结论

| 扩展对象 | 形态 | 是否编进 `chuan` | 换一家要不要改 Rust |
|---|---|---|---|
| 通道（微信/飞书/…） | 独立进程 + UDS + JSON-RPC 2.0 子集 | 只编协议实现，不编平台代码 | 不要 |
| 工具（第三方服务） | MCP（远端已有；补本地 HTTP/UDS 形态）+ 技能 + `[[service]]` | 不要 | 不要 |
| 需要用户授权的服务（OAuth） | 独立 helper + 中继回调 + `token_file` | 不要 | 不要 |
| 领域能力（打印、某家 API） | 技能 / playbook（D17） | 不要 | 不要 |

判据仍是那一句：**换一个环境/一家服务还要改 Rust 吗**。要，就说明它不属于内核。

首个真实适配器改为**微信**：把现有进程内实现拆成旁路进程（§1.8）。钉钉调研保留在
[`DINGTALK.md`](DINGTALK.md)，但因目标群属于学校组织、无法建应用，**暂不采用**。
适配器源码放仓库根的 `adapters/`，不编进 workspace。

---

## 1. 通道适配器（D44）

### 1.1 进程模型

```
平台 ──长连接/Webhook──> 适配器进程（独立 uid / 独立 unit）
                              │  UDS（核心监听，适配器连入）
                              ▼
                        chuan（Channel trait 的 socket 实现）
```

- 适配器**不是**沙盒里的插件：它持有该平台的凭据（bot token / app secret），必须能出网。
- 适配器也**不是**内核的一部分：它读写不了 Journal、DB、配置、其他通道的凭据，更不能调工具。
- 内核侧只多一个实现：`Channel` trait 的 socket 版本。Telegram/微信继续留在进程内，直到协议被第二个通道验证。
- 适配器源码放 `adapters/<id>/`（不进 Cargo workspace、不参与 `cargo zigbuild`），开发与部署约定见 [`adapters/README.md`](../adapters/README.md)。

### 1.2 进程、权限与 socket

| 项 | 约定 |
|---|---|
| 用户 | 每个适配器一个系统用户，如 `chuan-ch-<id>`，与 `chuan` 不同 uid；独立 uid 时需把 `chuan` 加入该用户的主组（否则改不了 socket 组，配置会拒绝启动） |
| socket | 核心创建并监听 `/run/chuan/channels/<id>.sock`；同 uid 时 0600，配了独立 `uid` 时 `chuan:<适配器主组> 0660`；目录 0755，父目录必须可穿越 |
| 对端校验 | 配置了 `uid` 时，核心 accept 后用 `SO_PEERCRED` 校验对端 uid，否则断开并记日志 |
| 文件面 | 适配器只可写自己的 state/media 目录；`ProtectSystem=strict`、`ProtectHome=yes`、`PrivateTmp=yes`、`NoNewPrivileges=yes` |
| 资源 | 单元级 `MemoryMax`、`CPUQuota`；内核侧每通道入站限流（**待实现**） |
| 凭据 | `LoadCredential[Encrypted]=` 放适配器自己的凭据目录，内核完全不经手 |
| 安装 | **只有主人**；生产建议钉二进制路径 + sha256（当前只校验 `uid` 与 socket 权限，sha256 待补） |

**为什么由核心监听**：如果适配器监听，本机任何能写那个目录的进程都可以抢注 socket 冒充通道；核心监听 + 对端 uid 校验把这条路堵死。

### 1.3 协议 v1

传输：`SOCK_STREAM` UDS，**NDJSON**（一行一个 JSON-RPC 2.0 对象）。UTF-8，单行上限 1MB。

#### 握手

适配器连上后必须先握手，超时 5s：

```json
{"jsonrpc":"2.0","id":1,"method":"chuan.hello","params":{
  "protocol":1,
  "channel":"feishu",
  "adapter_version":"0.1.0",
  "caps":{"edit":true,"buttons":true,"markdown":false,"max_text":4000,"min_edit_interval_ms":1000}
}}
```

```json
{"jsonrpc":"2.0","id":1,"result":{"protocol":1,"ok":true}}
```

- `channel` 必须与 socket 配置的 id 一致，否则 `-32002 channel_mismatch`。
- 协议版本不匹配返回 `-32001 unsupported_version`，适配器不得降级重试。
- 握手成功后核心才开始向它投递出站请求。

#### 入站（适配器 → 核心，通知，不需要响应）

```json
{"jsonrpc":"2.0","method":"chuan.inbound","params":{
  "conversation":"oc_xxx",
  "actor":"ou_xxx",
  "kind":"text|command|media|callback",
  "text":"...",
  "dedupe_key":"event_id",
  "callback_id":null,
  "msg":"om_xxx",
  "attachments":[{"path":"/var/lib/chuan-ch-feishu/media/a1b2","name":"photo.jpg","mime":"image/jpeg","bytes":12345}]
}}
```

- `received_at` 由**核心**生成，不信适配器时钟。
- `msg` 是平台消息句柄（字符串，可为 null）；核心只用它挂状态标记/编辑。
- `kind` 只允许 `text / command / media / callback`；`scheduled` / `resume` 是内核合成的，适配器不得发送。
- `actor` 只报平台内原始 id；**身份映射、角色、白名单都在核心配置里**。

#### 出站（核心 → 适配器，请求/响应）

| 方法 | 参数 | 结果 |
|---|---|---|
| `chuan.deliver` | `conversation`, `view` | `{"msg":{"channel","id"}}` |
| `chuan.update` | `conversation`, `msg`（字符串）, `view` | `{"outcome":"edited\|not_modified\|too_old\|rejected","retry_after_ms"?}` |
| `chuan.ack` | `callback_id`, `text` | `{}` |
| `chuan.send_file` | `conversation`, `path`, `name`, `mime`, `caption` | `{"msg":{...}}` |
| `chuan.prompt` | `conversation`, `prompt` | `{"msg":{...}}` |
| `chuan.clear_buttons` | `conversation`, `msg` | `{}` |

- `view` 的结构与 `View` 一致：`shard/is_last/blocks/status/buttons`；`Block` 用相邻标签
  `{"type":"paragraph","value":"…"}` / `{"type":"code","value":{"lang":…,"text":…}}`；
  分片、节流、转义在核心，适配器只渲染。
- 同一会话的请求**按序**投递，核心保证同一会话不并发两条出站请求。
- 适配器 30s 不响应 → 该通道标记 `degraded`，请求记失败；恢复后继续。
- `update` 返回 `rejected` 且带 `retry_after_ms` 时，核心按它退避，不自行猜测。

#### 错误

标准 JSON-RPC：`-32600 invalid_request`、`-32601 method_not_found`、`-32602 invalid_params`。
自定义：`-32001 unsupported_version`、`-32002 channel_mismatch`、`-32003 media_rejected`、`-32004 rate_limited`、`-32005 internal`。

#### 附件

1. 适配器把平台文件下载到自己的 `media/` 目录，报 `path/name/mime/bytes`。
2. 核心 `canonicalize` 后校验路径必须在该适配器的 media 根下；否则 `-32003 media_rejected`。
3. 核心**重新计算大小与摘要**（不信任适配器报的值），超过 `[channel] max_media_mb`（默认 20）拒绝。
4. 通过后拷进 `<media_dir>/<digest>`，再以 `Attachment` 进入正常流程。

#### 去重与重连

- `dedupe_key` 是平台事件 id；核心沿用现有去重（同一通道内重复即丢）。
- 适配器断线重连要带指数退避（1s 起、上限 60s）；能按平台游标补投就补投，不能补投就接受丢失，**不要编造**。
- 核心重启后 socket 重建，适配器自然重连；适配器不需要感知核心版本。

### 1.4 安全模型

**先说结论：适配器是该通道的信任根。** 它收到什么就能伪造什么，协议层无法用密码学阻止——因为它拿到的正是“主人从平台发来的消息”。把它当成“第二个 bot token 持有者”来治理。

| 威胁 | 能否拦 | 处理 |
|---|---|---|
| 伪造 `inbound` 冒充主人 | 不能（等价于平台账号被接管） | 限制 blast radius：只影响该通道；身份映射在核心；审计留痕 |
| 伪造审批回调 | 不能（同上） | 可选配置“只允许哪些通道发起审批回调”；审批本身只认主人身份 |
| 偷看别的通道 / Journal / 配置 | 能 | 独立 uid、独立 unit、无宿主挂载 |
| 用任意宿主路径当附件 | 能 | media 根校验 + 重算摘要 + 大小上限 |
| 抢注 socket 冒充通道 | 能 | 核心监听 + `SO_PEERCRED` uid 校验 + 目录权限 |
| 刷爆 token / 内存 | 能 | 入站限流（每会话 30/min、每通道 300/min）、单条上限、单元 `MemoryMax` |
| agent 自行安装适配器 | 能 | 安装是主人动作；agent 无 `self_modify` 权限改通道配置 |

**审计字段**：每条入站记 `channel / actor / kind / bytes / dedupe_key`；每条出站记 `msg id / 结果`。正文默认不写日志（Journal 里已有），只记哈希与大小，便于对账又不扩大泄露面。

### 1.5 配置与部署（草案）

```toml
[channels]
enabled = ["telegram", "echo"]        # 第一个是默认通道

[[channels.adapter]]
id = "echo"
socket = "/run/chuan/channels/echo.sock"   # 默认值；可省略
# uid = "chuan-echo"                       # 独立用户；需把 chuan 加进该用户主组
media_dir = "/var/lib/chuan/echo-media"    # 不接受附件就删掉
max_media_mb = 20
```

> 当前版本只支持 `id / socket / uid / media_dir / max_media_mb`；`binary` 与 `sha256` 钉住还没实现。

```ini
# chuan-channel-echo.service
[Unit]
Description=chuan channel adapter (echo)
After=network-online.target

[Service]
User=chuan-echo
Group=chuan-echo
ExecStart=/usr/bin/python3 /usr/local/lib/chuan/adapters/echo/echo_tui.py --socket /run/chuan/channels/echo.sock
Environment=CHUAN_CHANNEL_SOCKET=/run/chuan/channels/echo.sock
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
NoNewPrivileges=yes
MemoryMax=128M
CPUQuota=50%
ReadWritePaths=/var/lib/chuan-ch-feishu

[Install]
WantedBy=multi-user.target
```

适配器的 UI 凭据、媒体目录都在自己的 `/var/lib/chuan-ch-feishu` 下；`chuan backup` 不包含它。

### 1.6 新增一个适配器（清单）

1. 实现协议 v1（任意语言；推荐用平台官方 SDK 的 Python/Go 版本）。
2. 建独立用户与 unit，凭据走 `LoadCredentialEncrypted`。
3. 配置加 `[[channels.adapter]]`；`[channels] enabled` 加 id；`[identity]` 给人的 `channels` 加 `"<id>:<平台 id>"`。
4. `systemctl restart chuan`（不重编、不发版）。
5. 验收：文本 / 图片 / 文件、按钮或文本降级、断线重连、限流、日志前缀。
6. 记录到 `docs/`（接入事实与坑），**不把平台代码写进 `crates/`**。

### 1.7 验收

- `Echo` 适配器（测试桩）跑通全协议；重复 `dedupe_key` 不重复入队；`update` 的四种结果都正确映射。
- 断线 30s 后自动重连；核心重启后恢复。
- 路径越界附件被拒；超限被拒。
- 错误 uid 连接被拒（`SO_PEERCRED`）。
- 入站限流生效，且不会把正常长消息误杀。

**实现状态（M0，2026-11-24）**：协议 v1 + `SocketChannel` + `Echo` 适配器已实现。
核心侧在 `crates/harness-channels/src/socket.rs`，Echo 在 `adapters/echo/`；
`cargo test -p harness-channels --test socket_adapter` 覆盖握手、入站、投递。
未做：入站限流、MCP 基础设施、微信拆分（见 §1.8 与 §6）。

### 1.8 首个真实平台：微信（从进程内拆出）

**为什么不是钉钉**：钉钉需要在学校组织里创建并发布企业内部应用，目标群不受主人控制；
微信是已经在跑、已经上板验证的通道，更适合验证协议。

**形态**：`adapters/weixin/` 独立进程。第一阶段**复用现有 Rust 实现**
（`harness-channels::weixin`）作为库，外面套协议层，不重写 iLink 客户端。
适配器 id 必须仍是 `weixin`（`[identity]` 的绑定与会话键 `weixin:<addr>` 不变）；
核心检测到同名 adapter 时**不得同时启用进程内实现**（fail closed），否则两个 poller 会重复收消息。
理由：现有代码约 2.4k 行（扫码登录、AES-128-ECB 媒体、CDN 上传、游标与 `context_token` 语义），
重写风险大于收益；协议的中立性由 `Echo` 适配器 + 将来的新平台验证，不必拿生产通道冒险。

**必须一起迁移的状态**（否则会重复或发不出）：

| 状态 | 现在在哪 | 迁移后 |
|---|---|---|
| 长轮询游标 `wx.buf` / `wx.buf:<bot_id>` | 核心 SQLite（`StoreOffsets`） | 适配器自己的 `state.json` |
| 会话 `context_token`（`wx.buf:ctx:<from>`） | 核心 SQLite | 适配器 `state.json` |
| 平台凭据 | `[weixin] credential_file` | 同一份格式，适配器 CLI 读写 |
| 入站媒体 | 核心 `media_dir` | 适配器自己的 media 目录，核心校验后拷入 `/media` |

迁移方式：加一个一次性导出命令（如 `chuan weixin-export-state <file>`），适配器启动时
`--import-state` 读取；**不要让适配器直接读核心 DB**（单写者与 schema 耦合）。
不迁移的后果：游标空会重放最近 `stale_after_s` 窗口内的消息；`context_token` 丢失会让
主动提醒/发送失败，直到用户再次给机器人发消息。

**登录**：适配器提供 `weixin-adapter login`，复用 `weixin::login`，写同一份凭据；
核心的 `chuan weixin-login` 在迁移期保留，切完标记弃用。

**切换（单实例，不能双跑）**：

1. 停 `chuan`；
2. 导出状态；
3. 启动微信适配器，确认长轮询连上；
4. 核心配置改为 `[channels] enabled = ["telegram"]` + `[[channels.adapter]] id = "weixin"`；
5. 启动 `chuan`，验证单聊/群、媒体、提醒、审批短码；
6. 回滚：停适配器、删掉 `[[channels.adapter]]`、恢复 `enabled = ["telegram","weixin"]`、重启核心
   （进程内实现保留一个版本）。

**风险**：一个微信号同一时刻只能有一个长轮询；适配器与核心都要有单实例锁与健康检查，
切换窗口内不能同时起两个 poller。

**验收**：与现有行为对齐——文本/图片/文件、typing、`context_token`、出站 10 条/入站额度、
stale 丢弃、断线重连、`dedupe_key` 去重。

### 1.9 多通道投递与账号 1:N（D46）

- 一个 user 可以绑多条 `channel:addr`（D26 已实现）；会话键仍是 `{channel}:{addr}`。
- **默认投递回发起通道**：审批卡、状态、结果都跟发起会话走。
- 显式跨通道：`remind` 的 `channel` 参数已实现（D27，走审批）；新增 `notify`（设计）复用同一套解析——
  从 user 的绑定里找目标通道，找不到就报错，不猜。
- 跨通道发送必须审批（卡片写“发给谁 / 哪个通道 / 内容摘要”）。
- `send_file` 仍只发当前会话，避免变成“任意文件发给任意人”的出口；跨通道文件另立设计。
- 不做“广播到所有通道”，除非消息里显式列出目标。

---

## 2. OAuth 旁路（D45）

### 2.1 角色划分

```
模型 ──auth_begin/status/revoke──> helper（独立 unit，持 client_secret + refresh token）
                                      │  回调：公网中继 / 手动粘贴
                                      └─ 原子写 access token ──> token_file
模型 ──http(service=…, path=…)──> chuan ──读 token_file──> 服务 API
```

- **内核不实现 OAuth**（D17）：它只认 `token_file` 里的静态凭据，现有 `http` 服务每次请求重读该文件，所以刷新对内核零改动。
- **模型不参与令牌生命周期**：只能看到“待授权 / 已连接 / 已失效”和一条公开的授权链接。
- **helper 持有全部机密**：`client_secret`、`code_verifier`、refresh token。授权码与令牌永不进上下文、永不进日志。

### 2.2 配置（草案）

helper 自己的配置文件（`/etc/chuan/oauth.toml`，root 只读）：

```toml
[relay]
base = "https://auth.example.com"
claim_key_file = "/etc/chuan/secrets/oauth-claim"

[[provider]]
name = "provider"
authorize_url = "https://provider.example/oauth/authorize"
token_url = "https://provider.example/oauth/token"
client_id = "chuan"
client_secret_file = "/etc/chuan/secrets/provider-client-secret" # 公共客户端可省略
scopes = ["read", "offline"]
extra_authorize = { access_type = "offline" }
service = "provider"                                            # 对应 chuan 的 [[service]] 名
token_file = "/var/lib/chuan-oauth/provider-default/access_token"
```

chuan 侧（现有能力，不改代码）：

```toml
[[service]]
name = "provider"
kind = "http"
base = "https://api.provider.example"
token_file = "/var/lib/chuan-oauth/provider-default/access_token"
header = "Authorization: Bearer {token}"
```

> 若某个服务更适合由 MCP 暴露高层工具（如“搜邮件”而不是“发 HTTP”），helper 也可以同时是一个本地 MCP 服务；两条路可以并存。
>
> MCP 已改为**每次请求重读** `token_file`（2026-11-24），所以 OAuth 轮换不需要重启核心；
> 若某个 MCP 服务自己持有令牌，也可以不经过 `token_file`。

### 2.3 四种回调形态

| 形态 | 手机可达 | 需要公网 | 授权码进聊天 | 结论 |
|---|---|---|---|---|
| **中继**（CF Worker）+ helper 轮询 | 任意网络 | 是 | 否 | 推荐，见 2.4 |
| **OpenTunnel** 临时公网 route | 任意网络 | 是（第三方） | 否 | 可选，见 2.6.1；riscv64 支持待实测 |
| 板子直收回调（局域网） | 必须同网 | 否 | 否 | 需要 HTTPS/证书；提供方对 redirect URI 的规则要先实测，不推荐作为默认 |
| 手动粘贴（终端） | 任意网络 | 否 | 否 | 无公网兜底，见 2.6 |

**明确不做**：把回调 URL 粘回聊天。授权码是凭据材料，进聊天就会落 Journal、进模型上下文；PKCE 只能保证它不能被第三方兑换，不能保证它不落日志。

### 2.4 公网中继（CF Worker）

**中继只传 code，不碰 verifier，也不持任何密钥。**

路由：

- `GET /cb?code&state`
  - 以 `state` 为键写入 Durable Object（强一致，保证只能取一次），值 `{code, claim_hash, exp}`，TTL 10 分钟；
  - 返回极简 HTML：“授权完成，可以回到聊天了。”不写日志、不渲染 query。
- `POST /take`（`Authorization: Bearer <claim>`）
  - 校验 `claim` 的哈希与 state 匹配；
  - 原子取出并删除，返回 `{"code":"…"}`；已取过或过期返回 404。

部署：`wrangler` + 自定义域名（或 `*.workers.dev`）；DO 用 alarm 清理过期项。**Worker 不需要任何 secret**——claim key 由 helper 生成，只存在板上和这一次请求里。

安全：

- PKCE（S256）保证中继即使泄露 `code` 也换不到 token（verifier 从未离开板子）。
- `state` 防 CSRF；`claim` 防别人抢先取码；一次性删除防重放。
- 板子**不开任何入站端口**，NAT 后可用。

代价：多一个外部依赖（不是“第二台机器”，但属于新增基础设施，需主人拍板）。

### 2.5 授权时序

1. 模型调 `auth_begin(provider, account?)`。
2. helper 生成 `state`（32B）、`code_verifier`（43–128 字符）、`claim`（32B），把 `{state, verifier, claim_hash, provider, account, created_at}` 写入 `pending/`（0600）；`verifier` 只留在板子上。
3. helper 拼授权 URL（`authorize_url` + `client_id` + `scopes` + `redirect_uri` + `state` + PKCE challenge + 额外参数），返回 `{url, expires_at}`。
4. 模型把 URL 作为普通文本发到当前会话（公开值，不需要新通道能力）。
5. 用户点开 → 同意 → 提供方跳到 `https://auth.example.com/cb?code&state`。
6. 中继存 code；helper 每 2s 轮询 `/take`，最长 5 分钟。
7. helper 用 `code + verifier`（+ `client_secret`，若为机密客户端）换 token；校验返回的 `scope`；原子写 token 文件与 `access_token`。
8. 状态变 `connected`；模型调用 `auth_status` 得到结果后继续。

超时 / 拒绝 / `state` 不匹配：删除 pending，状态 `expired`，给一句“链接已失效，需要重新发起”。

### 2.6 手动粘贴（无公网）

1. 提供方登记 Desktop 客户端的 loopback 回调（`http://127.0.0.1:<port>/`）。
2. helper 打印/推送授权 URL；用户在任何浏览器完成同意。
3. 浏览器跳到 `http://127.0.0.1:<port>/?code=…`，必然打不开——从地址栏复制整条 URL。
4. 在板子上执行 `chuan auth finish`，**从 stdin 读**粘贴内容（不要当命令行参数，避免 shell history），helper 校验 `state` 并用 verifier 兑换。

限制：多一步复制粘贴；部分企业策略会禁用 loopback；提供方对“桌面客户端”的可用 scope 可能更窄。

### 2.6.1 OpenTunnel 临时回调（可选）

[OpenTunnel](https://opentunnel.xyz/) 是 anomalyco 的「盲 TLS 隧道」：本机跑一个客户端，
拿到 `https://<route>.<tunnel-id>.opentunnel.xyz`，TLS 在**本机**终止，中继只转发密文。

**它不是一个“跑一次就退出”的 CLI**：一个 `opentunnel` 二进制 + 一个**常驻客户端进程**
（维持桥接 WebSocket 与本地 TLS 证书）。`route add` 会启动后台服务；在 Linux 有 systemd 时
注册用户级 `opentunnel-<profile>.service`，没有时退化成普通后台进程。也可以用
`opentunnel serve` 前台运行，由 helper 自己拉起/杀掉——这样不必装 systemd 单元。
无论哪种，公网 endpoint 只在客户端进程存活时存在。

用于 OAuth 回调的最小流程：

```bash
# helper 先在 127.0.0.1:<port> 起一次性回调监听（只认 /cb、state 一次性、5 分钟 TTL）
opentunnel route add 127.0.0.1:<port> --name auth
# 输出 https://auth.<tunnel-id>.opentunnel.xyz → 作为 redirect_uri
# 收到 code / 超时后：
opentunnel route remove auth
```

- 想让地址稳定（提供方要求预登记 redirect URI）就固定 `--name auth`；`route remove` 只撤掉路由，
  不删除隧道身份；`down` 断开所有路由；`delete --yes` 会永久丢弃 hostname。
- 想每次随机、用完即弃：不加 `--name`，route 名随机；但提供方必须支持动态 redirect URI
  （Cloudflare MCP 的 DCR 可以；Google 不行）。
- **不是认证**：拿到 URL 的人都能访问回调；靠 `state` + PKCE + 一次性 + 短 TTL + 只绑定
  loopback 来收口。收到结果立刻 `route remove`，回调服务随即退出。
- 首次的隧道身份由 `route add` / `up` 创建；`serve` 只连接已有身份。所以 helper 要么让
  `route add` 的后台服务跑到回调结束，要么先 `route add` 一次、`down`、再用 `serve` 前台接管。
- 代价：多一个常驻/按需启动的第三方 CLI 与后台服务；**官方预编译只列 Linux x64/arm64**。
  2026-11-24 在本机用 `cargo zigbuild --release --target riscv64gc-unknown-linux-gnu.2.28`
  **能编过**（5.2MB、GLIBC_2.28 基线），但尚未在目标板上运行。
- **发布方式**：chuan CI 在 tag 上额外构建 riscv64 的 `opentunnel-cli`（pin 到实测提交），
  作为独立可选资产 `chuan-opentunnel-riscv64-linux-gnu.tar.gz`；主 chuan 包不含它。
  `install.sh --with-opentunnel` 才下载，`--bootstrap-oauth-tunnel` 才创建 tunnel/证书。
  失败不阻断 chuan 安装/发布；MIT 许可文件随资产分发。
- **2026-11-24 本机 x86 实测（通过）**：
  - 第一次 tunnel 的证书签发停在 `challenge` 超过 14 分钟（challenge token 轮换过），
    属服务端瞬时故障/陈旧 pending（同类问题见 anomalyco/opencode#54624）；
  - 删掉 pending 状态、新建 tunnel 后，证书约 1 分钟到 `Ready`；
  - 用 `serve` 前台托管 + route `auth`，`https://auth.<id>.opentunnel.xyz/cb?code=…&state=…`
    实际收到回调；ZeroSSL 证书有效（CN 为 tunnel hostname）；
  - 全程没有安装 systemd 单元（库创建 identity + `serve` 前台）。
  结论：**OpenTunnel 可用于临时 OAuth 回调**；但 helper 必须能在证书长期 `challenge` 时
  丢弃 pending / 用新 profile 重试，不能无限 resume 一个坏 tunnel。

### 2.7 令牌存储与刷新

目录：`/var/lib/chuan-oauth/<provider>-<account>/`（目录 `0750 helper:chuan`，核心只需能进入并读 `access_token`）

| 文件 | 权限 | 内容 |
|---|---|---|
| `tokens.json` | 0600 helper | `refresh_token`、`access_token`、`expires_at`、`scope`、`refresh_expires_at?` |
| `access_token` | 0640 helper:chuan | 纯文本 access token，`token_file` 指向它 |
| `pending/<state>.json` | 0600 helper | verifier、claim、创建时间；完成/失败即删 |
| `status.json` | 0644 | `connected / expired / revoked`、最近错误码（不含令牌） |

刷新策略：

- 定时器每 10 分钟检查；`expires_at - now < 10min` 就刷新（access token 通常 1 小时）。
- 单进程 + 互斥，绝不并发刷新；刷新成功原子替换 `tokens.json` 与 `access_token`。
- 提供方返回新的 `refresh_token` 时一并替换；`refresh_token_expires_in` 存在就记下来并提醒主人。
- 网络错误：指数退避，**不**标记失效；`invalid_grant` / `invalid_client`：标记 `expired`，停止重试，等下一次交互提示重新授权。
- 刷新失败不影响已签发的 access token 继续用到过期。

多账号：按 `provider + account` 分目录；chuan 侧用不同 `[[service]]` 名（如 `provider-work` / `provider-personal`）各指一个 `token_file`。

### 2.8 模型可见面

| 工具 | 返回 | 约束 |
|---|---|---|
| `auth_begin(provider, account?)` | `{url, expires_at}` | URL 只含公开值；同一 provider/account 同时只允许一个 pending |
| `auth_status(provider, account?)` | `{state, expires_in?}` | 永不返回令牌或令牌片段 |
| `auth_revoke(provider, account?)` | `{ok}` | 调用提供方撤销端点并删除本地令牌 |

审批与文案：发起授权本身不改系统，不需要审批卡；真正授予数据访问的是用户在浏览器里点的“同意”。helper 的安装/启用属于主人配置，agent 不能自行完成。

### 2.9 失败与运维

| 现象 | 处理 |
|---|---|
| 用户没完成授权 | pending 5 分钟过期，状态 `expired`，提示重新发起 |
| 提供方拒绝 / scope 变更 | 状态 `expired`，不重试；重新发起会带上新 scope |
| refresh `invalid_grant` | 撤销、改密、过期或超出令牌上限；提示重新授权 |
| 令牌文件被删 | 状态 `revoked`，按未授权处理 |
| 中继 404 / 超时 | 提示“没收到授权结果”，保留 pending 到 TTL |
| 日志 | 只记状态与错误码；code / token / verifier 一律 `[redacted]` |
| 备份 | `/var/lib/chuan-oauth` **不进入** `chuan backup`；恢复后重新授权 |

### 2.10 验收

- 单元：PKCE 生成/校验、`state` 一次性、claim 哈希比较、刷新边界（提前 10 分钟）、`invalid_grant` 映射。
- 集成：本地 mock provider + mock 中继，跑通 begin → callback → exchange → refresh；断言日志中无 code/token。
- 手工：手机（不同网络）完成一次授权，全程 < 2 分钟；令牌文件权限正确；核心 `http` 调用成功。
- 失效演练：手工把 `tokens.json` 的 refresh token 改坏 → 状态变 `expired`，不产生重试风暴。

**实现状态（2026-11-26，Rust 内置手动/OpenTunnel 回调）**：`chuan oauth-mcp` 已完成
DCR、PKCE（S256）、授权码交换、refresh、`token_file` 和本地 MCP `serve`；实测 scope
`user:read account:read dns.read offline_access`，refresh 成功，且 chuan 的 `McpServer`
用该 token 成功执行 `tools/list`、`whoami`、`GET /zones`。本地 MCP 服务已实现
（`auth_begin / status / refresh`；`auth_revoke` 默认关闭），并已用 chuan 的 `McpServer` 调用验证。
OpenTunnel 自动回调已在 Rust 版接入；relay 未做。

### 2.11 oauth-mcp：把 helper 做成 MCP 服务

helper 既是**CLI**（begin/finish/status/refresh/test-mcp），也能以**本地 MCP 服务**运行
（`serve --port 8790`，本机 token）。已实现的 MCP 工具：`auth_begin / auth_status / auth_refresh`；
`auth_revoke` 默认不注册（等 per-tool 角色策略）。工具规划：

| 工具 | 作用 |
|---|---|
| `auth_begin(provider, account?)` | 生成链接与 pending 状态 |
| `auth_status(provider, account?)` | 返回 `pending / connected / expired / revoked` |
| `auth_revoke(provider, account?)` | 撤销并删本地令牌 |
| `auth_list()` | 列出已配置 provider 与状态（不含令牌） |

provider 是**数据**，不是代码分支。两类：

1. `oauth2`：静态 `authorize_url` / `token_url` + `client_id` / `client_secret` + PKCE（普通第三方）。
2. `mcp`：按 MCP 授权规范发现资源元数据与授权服务器，必要时做 **DCR（动态客户端注册）**；
   Cloudflare 属于这一类（见 2.12）。

与 chuan 的衔接两种：

- **A. `token_file` 交接**：helper 刷新后原子写；chuan 的 MCP 客户端**每次请求重读**（已实现）。
- **B. 本地代理**：chuan 连 `oauth-mcp` 的 `/mcp/<provider>`，helper 注入 token 再转发到上游；
  不动 chuan 的 OAuth，但代理要处理 Streamable HTTP / SSE 与取消。复杂度更高。

建议先 A：改动小，能验证 Cloudflare 端点；只有在 MCP 客户端兼容性不成立时才做 B。

**前置条件**：当前 MCP 子工具没有 per-tool 的域/角色策略（整个 `mcp` 工具是 `Outbound`，成员可用）。
在补上“某个 MCP 工具仅主人可调”之前，`auth_revoke` 这类破坏性动作不要暴露给成员；
可以先只暴露 `begin/status`，或先做原生工具再迁到 MCP。

运维：MCP 在启动时拉一次 `tools/list`；helper 必须先于 `chuan` 可用，否则要重启核心才能看到工具。

### 2.12 第一个 provider：Cloudflare

- 端点 `https://mcp.cloudflare.com/mcp`；工具 `search / execute / docs`，Code Mode 在
  Dynamic Worker 里执行，约 1k token 常量开销，不随 API 端点数量膨胀。
- 2026-11-24 匿名探测：`POST initialize` 返回 `401`，`WWW-Authenticate` 指向
  `/.well-known/oauth-protected-resource/mcp`，scope 为 `user:read account:read`；
  说明端点走 MCP OAuth，API Token 路径仍待带 token 实测。
- 授权服务器元数据（`/.well-known/oauth-authorization-server`）：
  - `authorization_endpoint = https://mcp.cloudflare.com/authorize`
  - `token_endpoint = https://mcp.cloudflare.com/token`
  - `registration_endpoint = https://mcp.cloudflare.com/register`（支持 DCR）
  - `code_challenge_methods_supported = ["S256"]`
  - `grant_types_supported = ["authorization_code", "refresh_token"]`
  - `token_endpoint_auth_methods_supported` 含 `none`（公共客户端 + PKCE 可行）
  - scope 里除 `user:read / account:read` 外还有按产品划分的 `dns.read`、`workers-scripts.read` 等
- OAuth 授权时会跳转到 Cloudflare，**选账号与权限**；也支持把 **API Token** 放在
  `Authorization: Bearer`（CI/自动化）。
- 作为 oauth-mcp 的 `mcp` 类 provider：资源元数据 → 授权服务器元数据 → DCR（拿 client_id）
  → 授权码 + PKCE（S256）+ `resource=https://mcp.cloudflare.com/mcp` → 刷新；
  令牌只对该 MCP 端点有效。DCR 意味着 **redirect_uri 可以每次注册**，适合 OpenTunnel
  的临时地址；但更稳的做法是固定 route 名并复用 client。
- **兼容性验证顺序**：
  1. 先用最小权限 API Token 让 chuan 直接连（`[[service]] kind="mcp"` + `token_file`），
     验证 Streamable HTTP 与 `initialize/tools/list/tools/call` 是否通；
  2. 再做 oauth-mcp 的 DCR/OAuth 版本，验证刷新与 `token_file` 交接。
- **2026-11-24 实测（API Token 路径）**：chuan 的 `McpServer` 成功加载 5 个工具
  （`docs / search / execute / whoami / submit_feedback`），`whoami` 返回用户身份，
  `execute` 用 `GET /zones` 读到 5 个 zone。**MCP 客户端兼容性已确认**；OAuth 仍待实现。
- 走通后补一页 `docs/CLOUDFLARE-MCP.md`：建 token/选权限、能做什么、审计与限额。

---

## 3. 与既有决策的关系

- **D9**：Channel 仍是替换型扩展点；本设计把它的“实现”从编译期 trait 扩展到进程协议，trait 本身不变。
- **D12**：仍不做插件热加载；改配置后重启对应 unit 即可。
- **D17**：平台语义、提供方 API 语义留在适配器/技能；内核只给协议与边界。
- **D18 / 不变量 14**：凭据不进沙盒、不进上下文；helper 与适配器各自持凭据，内核不经手。
- **D41**：同样是“宿主侧 UDS + 单行 JSON”的思路；通道协议是它的双向版本。
- **D46**：多通道投递默认回发起通道；显式跨通道（`remind.channel` 已实现，`notify` 待做）且必须审批。

---

## 4. 里程碑（核心稳定后再排）

| 阶段 | 内容 | 验收 |
|---|---|---|
| M0 ✅ | 协议 v1 + `Echo` 适配器 + socket 权限/附件校验 | `socket_adapter` 测试全绿；Echo 在本地核心跑通文本（按钮/审批待实测） |
| M1 | 微信状态导出（游标 + `context_token`）与适配器 CLI | 导出/导入后不重放、能主动发 |
| M2 | 微信适配器 v1（`adapters/weixin/`，复用 Rust 实现） | 单聊/群、文本/图片/文件、typing、提醒与审批短码对齐 |
| M3 | 切换一周 + 回滚演练 | 进程内实现保留一个版本，可一键回退 |
| M4 | oauth-mcp（本地 MCP）+ 中继 + 手动粘贴兜底 | 2.10 全部通过；`auth_*` 可被模型调用 |
| M5 | Cloudflare 作为首个 `mcp` provider（API Token 兼容性 → OAuth/DCR） | CF 能查/改 Worker 与 DNS；补 `CLOUDFLARE-MCP.md` |
| M6 | MCP per-tool 策略、token 每请求读、工具目录刷新 | `auth_revoke` 可安全开放；轮换不需重启 |

## 5. 待定

1. 微信适配器第一阶段**复用现有 Rust 实现**（推荐）还是用 Python 重写？重写要把扫码登录、
   AES 媒体、CDN、游标语义全部复刻，风险与工期都高；建议先复用，语言中立性由 `Echo` 与新平台验证。
2. 状态导出命令的形态：`chuan weixin-export-state <file>` 还是由适配器只读打开 SQLite 导出？
   （推荐前者，避免适配器依赖核心 schema。）
3. 进程内微信实现保留几个版本用于回滚？建议至少一个正式版本。
4. oauth-mcp 放哪：建议 `adapters/oauth-mcp/`（不进 workspace），避免它变成内核的一部分。
5. 回调选哪种：CF Worker 中继、OpenTunnel 临时 route、还是手动粘贴？（CF 的 DCR 让
   OpenTunnel 也可行；riscv64 的 OpenTunnel CLI 支持待实测。）
6. Cloudflare 先走 API Token 验证兼容性，还是直接做 OAuth/DCR？
6. MCP per-tool 策略的形态：服务端在 `tools/list` 里声明 `owner_only`，还是配置里按
   `server/tool` 白名单？

## 6. 基础设施补齐清单

| # | 缺口 | 为什么现在补 | 归属 |
|---|---|---|---|
| 1 | 核心只认 `telegram` / `weixin`，没有动态通道 id | 适配器注册后要能启动、路由、审计 | 配置 + actor |
| 2 | 没有 socket 版 `Channel` 实现与 supervisor | 协议落地的载体 | 内核 + `Channel` |
| 3 | 适配器注册/握手/`SO_PEERCRED`/限流/附件校验 | 安全边界 | 内核 |
| 4 | 微信游标与 `context_token` 导出/导入 | 切适配器不重放、能主动发 | 应用 + 适配器 |
| 5 | 微信登录 CLI 从核心迁到适配器 | 适配器独立运维 | 适配器 |
| 6 | MCP 子工具的域/角色策略（`owner_only`） | `auth_revoke`、CF 危险操作不能对成员开放 | `mcp.rs` + 策略 |
| 7 ✅ | MCP `token_file` 每次请求重读 | OAuth access token 轮换 | `mcp.rs`（已实现） |
| 8 | 本地 MCP 传输（127.0.0.1 / UDS）+ 本机 token | oauth-mcp 与 chuan 同机通信 | `mcp.rs` |
| 9 | MCP 工具目录刷新（当前启动拉一次） | helper 晚启动/重启后不必重启核心 | `mcp.rs` |
| 10 | oauth-mcp 的 provider 发现与 DCR | Cloudflare 这类 MCP OAuth | oauth-mcp |
| 11 | 打包：适配器 unit、安装脚本、升级/回滚说明 | 不重编核心就能上线 | deploy |
| 12 | 测试：`Echo` 一致性、微信行为对齐、mock OAuth/DCR | 没有回归就没有切换信心 | tests |

依赖顺序：1–3 → 4–5 → 6–9 → 10 → 11–12。

---

## 附录 A · 提供方差异（示例，不是默认）

- **测试发布状态**：某些提供方对“外部 + 测试”应用签发的 refresh token 只有 7 天；授权次数与测试用户数也有限制。把应用发布到生产状态通常解除 7 天限制，验证是另一条轴（影响警告页与用户上限）。**发布后必须重新授权一次才拿到长效令牌**，并以 token 响应里的 `refresh_token_expires_in` 是否出现作为判据（出现 `604799` 即 7 天）。
- **长期不用**：部分提供方会让长期未使用的 refresh token 失效（如 6 个月）。
- **改密/撤销**：用户改密码或手动撤销会立刻作废 refresh token。
- **令牌上限**：同一用户对同一客户端可持有的 refresh token 有数量上限，超额会使最旧的失效；helper 应复用已存令牌，不要每次重新授权。
- **受限 scope**：读取全部邮件/全部云盘通常属于受限范围，生产验证成本高；优先选非受限的细粒度 scope（如“仅本应用创建/用户挑选的文件”）。
