# OAuth provider manifest（设计，未实现）

目标：以后接一个新的 OAuth 服务，**不改 `oauth-mcp` 代码**。agent 搜文档、写一份
manifest；主人批准后，helper 按 manifest 完成授权、刷新、写 `token_file`。

边界：manifest 是**数据**，不是代码。不执行脚本/模板表达式；遇到非标准流程就明确拒绝，
不要用“自定义 hook”把内核变成执行器。

---

## 1. 谁能写、谁批准

- agent 可以起草 manifest、跑校验、打印计划；
- **只有主人能安装**到 `<config>/oauth/providers.d/<id>.toml`（与 `services.d` 同级，
  走 `self_modify` 审批）；
- manifest 里不出现任何密钥值，只出现 `client_secret_file` 这类路径；
- 审批卡展示：授权/令牌端点、redirect URI、申请的 scope、token 写到哪、是否启用 refresh。

---

## 2. manifest 草案

```toml
schema = 1
id = "google-workspace"
display = "Google Workspace"
kind = "oauth2"                  # oauth2 | mcp
docs_url = "https://developers.google.com/workspace/guides/workspace-cli"
fetched_at = "2026-11-26"

[client]
mode = "static"                  # static | dcr | public
client_id = "...apps.googleusercontent.com"
client_secret_file = "/etc/chuan/oauth/google.secret"  # 公共客户端可省略
token_endpoint_auth = "client_secret_post"             # none | basic | post
pkce = "S256"

[authorize]
url = "https://accounts.google.com/o/oauth2/v2/auth"
scopes = [
  "openid",
  "email",
  "https://www.googleapis.com/auth/gmail.readonly",
  "https://www.googleapis.com/auth/drive.file",
]
extra = { access_type = "offline", prompt = "consent" }

[token]
url = "https://oauth2.googleapis.com/token"
refresh = true
revoke_url = "https://oauth2.googleapis.com/revoke"

[callback]
mode = "opentunnel"              # manual | opentunnel | relay | loopback
path = "/cb"
port = 8765

[output]
service = "google"
token_file = "/var/lib/chuan/oauth/google/access_token"
header = "Authorization: Bearer {token}"

[verify]
url = "https://www.googleapis.com/oauth2/v3/userinfo"
expect_status = 200
```

MCP 类 provider（Cloudflare 这种）用发现而不是手写端点：

```toml
kind = "mcp"
resource = "https://mcp.cloudflare.com/mcp"
discovery = "https://mcp.cloudflare.com/.well-known/oauth-protected-resource/mcp"
```

helper 读资源元数据 → 授权服务器元数据 → DCR → PKCE → 刷新；scope 由 manifest 的
`scopes` 或 `auth_begin --scopes` 给。

---

## 3. 字段的硬约束

| 字段 | 约束 |
|---|---|
| `authorize.url` / `token.url` | 必须 HTTPS；内网/回环地址需要主人显式 `allow_private = true` |
| `client.mode` | 只能 `static` / `dcr` / `public`；不支持自定义注册协议 |
| `pkce` | `S256` 或 `none`；`plain` 拒绝 |
| `callback.mode` | 只能四种；`redirect_uri` 由 helper 按模式生成，manifest 不能填任意 URL |
| `scopes` | 逐个字符串；不允许通配符；安装时展示给主人 |
| `token_file` | 必须在 helper 允许的目录下（默认 `/var/lib/chuan/oauth/`） |
| `extra` | 只允许 query/form 的字符串/布尔/数字，不允许嵌套脚本 |
| 密钥 | 只能 `*_file`；值不进 manifest、不进日志、不进上下文 |

**不支持的**：自定义签名算法、非标准 grant、需要执行 JS 的流程、动态修改 token JSON 结构。
这些需要 helper 代码，不能用 agent 现编的 manifest 硬凑。

---

## 4. agent 的工作流

1. 搜官方文档 + 拉 discovery 元数据（能拉到就优先用）；
2. 按 schema 写 `providers.d/<id>.toml` 草稿，带 `docs_url` / `fetched_at`；
3. `oauth-mcp manifest validate <file>`：
   - schema、URL、scope、callback、token_file 路径；
   - 有 discovery 的，和实际元数据比对（endpoint/issuer/scope 支持）；
4. `oauth-mcp manifest plan <file>`：
   - 打印“会打开哪个域名、申请哪些 scope、回调用哪种模式、token 写到哪、是否刷新”；
5. 主人批准后落盘；
6. `auth_begin --provider <id>`：生成链接，经 channel 发出；
7. 回调（OpenTunnel/手动/中继）→ helper 换 token → 写 `token_file` → `verify` 请求；
8. 失败时返回结构化原因（端点不匹配/scope 被拒/回调超时），不要猜。

agent 不能自己批准 manifest，也不能把 `client_secret` 写进 manifest 或聊天。

---

## 5. 回调模式

- `manual`：授权后把失败页 URL 粘到 `finish` 的 stdin；
- `opentunnel`：稳定 route `auth`，公网 HTTPS 回本机 `127.0.0.1:<port>/cb`；
- `relay`：CF Worker 存 code，helper 主动轮询；
- `loopback`：浏览器与 helper 同机时的 127.0.0.1 回调。

manifest 只选模式，不写死公网 URL；实际 URL 由 OpenTunnel identity / relay 配置生成。
没有 bootstrap 时 `auth_begin` 返回 `needs_bootstrap`，模型通过 channel 告诉用户。

---

## 6. keyring / token 存储

- manifest 不控制加密方式；helper 统一处理：
  - refresh token：AES-256-GCM 加密，key 优先 systemd credential，回退 0600 key 文件；
  - access token：写 `token_file`（0600/tmpfs），因为 chuan 的 `http`/MCP 客户端要读；
  - 可选 OS keyring；SBC 上不假设 Secret Service 一定存在；
- 日志只记状态/错误码，token 一律 redact；
- `client_secret_file` 由主人放，权限 0400/0440，helper 只读。

---

## 7. 为什么不直接用 gwscli

- 官方 `gwscli` 只提供 x86_64/ARM64 预编译，**没有 riscv64**；K1 跑不了。
- 它用 Desktop app + loopback 回调，要求浏览器和 CLI 同机；SBC + 手机场景不适用。
- 它绑定 Google Workspace MCP，不适合作为通用 OAuth 抽象。
- 结论：借鉴它的**交互体验**（按 action/service 选 scope、增量授权、加密存储），
  不把它的二进制或流程当依赖。

---

## 7.1 实现状态（2026-11-26）

Rust 版已内置为 `chuan oauth-mcp`：

- `manifest validate/plan`
- `--providers-dir` 指定 manifest 目录；部署时由单元传 `<config>/oauth/providers.d`，
  不写时退回状态目录下的 `<dir>/providers.d`（兼容旧安装）
- `begin / finish / status / refresh`（手动粘贴回调）
- 本地 MCP `serve`（`auth_begin / auth_status / auth_refresh`，`auth_revoke` 默认不注册）
- manifest `callback.mode = "opentunnel"` 时，`auth_begin` 会起本地回调监听 + `opentunnel serve`，
  回调后自动换 token 并停掉 `serve`（Rust 版已接；尚未在本机做完整浏览器授权回归）

尚未实现：relay、加密存储/security hardening。
Python 参考实现已删除；只保留 Rust `chuan oauth-mcp` 与 manifest 文件。

### 7.2 加密迁移：什么时候需要重新授权

加密是**存储格式变化**，不是授权变化。只要 provider/client/refresh token 不变，
可以原地迁移，不需要重新走浏览器：

```text
tokens.json（明文）
  → 读出 refresh/access/endpoint/client/resource/scope
  → 用新 master key 加密成 tokens.enc
  → 原子替换、删除明文
  → helper/服务重启
```

当前 `tokens.json` 已带 `schema/provider/client_id/token_endpoint/resource/scope`，
未来加密直接包同一份语义即可。迁移命令应支持 `--dry-run` 和“校验后再删明文”。

**不需要重新授权**：

- 只加 AES-GCM/`systemd-creds`/keyring；
- master key 轮换（用旧 key 解密、新 key 加密）；
- OpenTunnel hostname/route 换了，但 OAuth client 和 refresh token 没变；
- access token 过期——用 refresh token 换即可。

**需要重新授权**：

- 换 OAuth client（client_id / 应用）；
- refresh token 已被撤销、过期、改密、超出 provider 限制；
- 新增 scope 需要用户同意（增量授权也要点一次）；
- master key 丢失且没有可用备份（token 无法解密）；
- 把 token 目录复制到另一台机器，而 key 绑定原机器/TPM。

所以现在只要**不改 client_id、不换 provider 账号、不在加密迁移时丢 token**，
以后的安全加固就是原地升级，不会让用户重新授权。

## 8. 静态服务 token 要不要一起加密？

值得，但要先分清威胁模型。

- **加密 at rest 能防**：备份/同步、误把 `secrets` 目录挂进沙盒、误拷贝、日志/快照、磁盘被拿走。
- **加密 at rest 不能防**：同 uid 的运行期进程、agent 拿到 `host` 执行权限且路径被授权、
  已经解密后的 `token_file` 被读走。运行时边界仍然是“不挂载、不授权路径、不进上下文”。

建议分两类：

| 凭据 | 存法 | 谁解密 | 轮换 |
|---|---|---|---|
| 静态 HTTP/MCP token（现有大部分） | `systemd-creds encrypt` 成 `.cred`；`token_file` 指向 `$CREDENTIALS_DIRECTORY/<name>`（即 `/run/credentials/chuan.service/<name>`） | **systemd** 在 unit 启动时解密 | 重新加密 + `systemctl restart chuan` |
| OAuth refresh token | helper 的 `tokens.enc`（AES-256-GCM） | **oauth-mcp helper**，master key 来自 systemd credential | helper 自动刷新；key 轮换时 rekey |
| OAuth access token | helper 写 `access_token`（0600 或 `/run` tmpfs） | chuan 的 `http`/MCP 直接读 | 每小时刷新 |

静态 token 走 systemd credential 的好处：

- 内核/服务代码**不用实现加密格式**，现有 `token_file` 接口不变；
- `.cred` 在磁盘上是密文，误挂载 `/etc/chuan/secrets` 不会泄露；
- key 由 systemd host key/TPM 持有，不在 chuan 进程环境里。

代价：**轮换要 restart chuan**；对每天/每小时变化的 token 不合适。变化的 token 走
OAuth helper 或一个小的本地 secrets broker，不要硬塞进 systemd credential。

不建议做“chuan 自己读 `token_file` 然后 AES 解密”的通用层：key 必须在 chuan 进程里，
host lane/同 uid 仍可能拿到解密后的内容，收益主要是 at-rest，和直接让 systemd 解密相比
多写一套格式。只有在“必须热轮换、又不能用 helper”时才考虑。

### 迁移

1. `chuan secrets set <name>`（root，从 stdin 读，不回显）：
   - `systemd-creds encrypt --name=<name>` 写 `/etc/chuan/secrets/<name>.cred`；
   - 删除/不保留明文；
   - 打印“需要重启 chuan”。
2. 配置里把对应 `token_file` 改成 `/run/credentials/chuan.service/<name>`；
   unit 增加 `LoadCredentialEncrypted=<name>:/etc/chuan/secrets/<name>.cred`。
3. 重启后在日志里确认服务启动；失败时不要回退到明文。
4. `chuan backup` 仍然排除 `secrets` 与 `/run/credentials`。

**不要做的事**：把 `/run/credentials` 或 `/etc/chuan/secrets` 挂进沙盒、写进
`[host.path]`、放进技能/playbook；沙盒默认已遮蔽 secrets，这条不能破。

## 9. 安全加固迁移计划（未实现）

原则：**加密是存储升级，不是重新授权**。迁移只改文件格式和 key 来源，不改
`client_id`、provider、账号或 refresh token。

### 阶段 0：现在（已完成）

- `tokens.json` 固定 `schema=1`，保留 `provider / client_id / token_endpoint /
  resource / scope / refresh_token / expires_at`；
- 这些字段以后直接作为加密信封的明文载荷，不再变更含义。

### 阶段 1：加密存储 + 迁移命令

1. 新格式 `tokens.enc`：magic/version + nonce + AES-256-GCM 密文；AAD 绑定
   `provider|client_id`；
2. key 来源：优先 systemd credential，开发机回退 0600 key 文件；
3. 新命令：

```bash
chuan oauth-mcp migrate --dry-run   # 只读旧 tokens.json，打印会做什么
chuan oauth-mcp migrate             # 加密 + 回读校验 + 原子替换
chuan oauth-mcp rekey               # 旧 key 解密、新 key 加密
chuan oauth-mcp status              # 不打印 token，只报加密/授权状态
```

4. 迁移顺序必须是：读旧文件 → 加密到临时文件 → 用新 key 回读解密并校验
   `provider/client_id` → 原子 rename → 最后删除明文；任何一步失败都保留明文并报错。
5. 迁移窗口内可读旧明文，但**只读不写**；写入一律新格式。

### 阶段 2：systemd credential

1. 生成 32B master key，`systemd-creds encrypt --name=chuan_oauth_master`；
2. `chuan-oauth-mcp.service` 加 `LoadCredentialEncrypted=chuan_oauth_master:...`；
3. helper 从 `$CREDENTIALS_DIRECTORY` 读取，不写环境变量；
4. 静态 HTTP/MCP token 改走 `LoadCredentialEncrypted`，`token_file` 指向
   `/run/credentials/chuan.service/<name>`；轮换 = 重新加密 + `systemctl restart chuan`。

### 阶段 3：移除明文

- 一个正式版本后，`tokens.json` 只保留导入兼容；
- 新写入只接受 `tokens.enc`；日志/错误里不再出现 token 片段；
- 文档明确：key 丢失 = 重新授权；跨机器复制 token 目录 = 重新授权。

### 验收

- 对一份现有明文 token 跑 `migrate --dry-run`，不改文件；
- 正式迁移后 `status` 正常，`refresh` 成功，访问 token_file 内容更新；
- helper 重启后仍能解密；
- 错误 key / 损坏密文 → 明确报错，不静默覆盖；
- 迁移失败可恢复备份，原 refresh token 仍可用；
- 全流程不触发浏览器授权（client_id 不变）。

### 回滚

- 迁移前保留 `tokens.json.bak`（0600）一个版本；
- 新格式读取失败时回退旧文件并提示，不自动删备份；
- 回滚只需恢复备份 + 重启 helper，不需要重新授权（除非 key 已丢失）。

## 10. 实现顺序

1. helper 增加 `manifest validate/plan` 与 provider 目录；
2. 把现有 Cloudflare 配置迁成一份 manifest（验证 schema 够用）；
3. 支持 `oauth2` static client + `opentunnel`/`manual` 回调；
4. 加 `mcp` 发现 + DCR（Cloudflare 已是第一条）；
5. 最后再做 keyring/加密存储与 provider 级测试。
