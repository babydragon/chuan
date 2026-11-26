# oauth-mcp（OAuth 旁路 helper）

**Rust 内置为 `chuan oauth-mcp`**，单二进制，无 Python 依赖。
不是通道适配器，也不是内核的一部分；它只持有 OAuth 凭据。

## 命令

```bash
chuan oauth-mcp manifest validate <file>
chuan oauth-mcp manifest plan <file>

chuan oauth-mcp begin   --dir /tmp/oauth-test [--provider cloudflare-mcp]
chuan oauth-mcp finish  --dir /tmp/oauth-test   # 从 stdin 粘贴回调 URL
chuan oauth-mcp status  --dir /tmp/oauth-test
chuan oauth-mcp refresh --dir /tmp/oauth-test

# 本地 MCP 服务（chuan 从这里拿 auth_* 工具）
chuan oauth-mcp serve --dir /tmp/oauth-test --port 8790
```

`--providers-dir` 指定 provider manifest 目录；部署时是 `/etc/chuan/oauth/providers.d`，
不写则退回 `<dir>/providers.d`（旧安装的兼容路径）。

`serve` 首次会生成 `<dir>/mcp-token`（0600）；chuan 用它作为本地 MCP 的 Bearer token。

**自动刷新**：`serve` 每 60 秒检查一次清单对应的 token，剩余有效期不足 10 分钟就用
refresh token 换新（失败写 journald，同一 provider 至少隔 15 分钟再试）。不需要人工续期；
只有 refresh token 被服务方撤销时才需要重新授权。
工具：`auth_begin`、`auth_status`、`auth_refresh`；`auth_revoke` 默认不注册
（等 per-tool 角色策略落地后再开）。

`begin`/`finish` 这类一次性命令只适合 `manual` 模式。**`opentunnel` 模式必须用常驻的
`serve`**：回调监听与 `opentunnel serve` 都挂在这个进程里，命令行 `begin` 打印完 URL
就退出，进程一退回调就没了。

## 回调

- `callback.mode = "manual"`：`begin` 打印授权 URL；浏览器跳到 127.0.0.1 失败后，
  把地址栏 URL 粘给 `finish` 的 stdin。
- `callback.mode = "opentunnel"`：`auth_begin` 起本地回调监听 + `opentunnel serve`，
  收到 code 后自动换 token、停掉 `serve` 并撤掉公网入口；需要先 bootstrap 出 tunnel/route。
  没有 tunnel 时 MCP 返回 `needs_bootstrap=true`，模型应通过当前通道告诉用户。

bootstrap 的 marker 默认在 `~/.local/state/opentunnel.json`（服务用户 chuan 就是
`/var/lib/chuan/.local/state/opentunnel.json`），可用 `CHUAN_OAUTH_TUNNEL_STATE` 覆盖。

## 用 OpenTunnel 自动回调（完整步骤）

目标：浏览器点完同意就自动完成，不需要复制粘贴回调 URL。

前提：

- OpenTunnel 是**可选组件**，不装不影响 chuan；装不装都是 `manual` 模式的超集。
- 预编译只提供 **riscv64**（CI 从 tag `v0.6.2` 的固定 commit 交叉编译）。其它架构用
  `--opentunnel-bin <自编译的 opentunnel>`。
- 公网回调只在 `opentunnel serve` 存活时存在；授权结束后 helper 会主动撤掉。

### 1. 安装（带 OpenTunnel）

目前最稳的是先解开 tar 再装（`v0.4.0-rc.1` 的一键脚本会丢掉可选开关，rc.2 起才透传）：

```bash
V=v0.4.0-rc.1
curl -fLO "https://github.com/babydragon/chuan/releases/download/$V/chuan-$V-riscv64-linux-gnu.tar.gz"
mkdir -p chuan-pkg && tar xzf "chuan-$V-riscv64-linux-gnu.tar.gz" -C chuan-pkg
sudo CHUAN_VERSION="$V" chuan-pkg/install.sh \
  --with-opentunnel --bootstrap-oauth-tunnel
```

一键装（rc.2 及以后；用实际 tag 替换 `V`）：

```bash
V=v0.4.0-rc.2
curl -fsSL "https://github.com/babydragon/chuan/releases/download/$V/install.sh" \
  | sudo CHUAN_VERSION="$V" bash -s -- \
      --with-opentunnel --bootstrap-oauth-tunnel
```

走代理时就地加 `-x` 和 `https_proxy`（`V` 沿用上面的写法，注意变量要写在 `sudo` 之后）：

```bash
P=http://127.0.0.1:7890
curl -fsSL -x "$P" "https://github.com/babydragon/chuan/releases/download/$V/install.sh" \
  | sudo CHUAN_VERSION="$V" https_proxy="$P" bash -s -- \
      --with-opentunnel --bootstrap-oauth-tunnel
```

- `--with-opentunnel`：装 OpenTunnel 二进制到 `/usr/local/lib/chuan/opentunnel/opentunnel`，
  同时装好并启用 `chuan-oauth-mcp.service`、生成 `services.d/oauth.toml`；
- `--bootstrap-oauth-tunnel`：创建 tunnel 身份与固定 route `auth`，等证书 Ready 后停掉后台服务，
  只留 identity/route（真正授权时再 `serve`）；
- **从解开的 tar 安装时要自己设 `CHUAN_VERSION=<tag>`**：脚本没有版本信息时会去找
  `latest`，而 `latest` 可能还没有 OpenTunnel 资产；
- bootstrap 失败**不阻断** chuan 安装，可以稍后重跑；
- 不需要手工写 provider 清单或重启：清单用 `oauth_manifest` 装，helper 每次调用都重读。

### 2. 确认 tunnel 已创建

```bash
sudo -u chuan cat /var/lib/chuan/.local/state/opentunnel.json
sudo -u chuan cat /var/lib/chuan/.config/opentunnel/chuan.toml
```

marker 里应该是：

```json
{ "hostname": "…", "profile": "chuan",
  "callback": "http://127.0.0.1:8765/cb",
  "url": "https://auth.<hostname>/cb", "created_at": 0 }
```

`url` 就是后面的 `redirect_uri`。没有这个文件或 bootstrap 失败时，从解开的包里重跑
（证书处于 challenge 时脚本会丢掉 pending 自动重试一次）：

```bash
sudo CHUAN_USER=chuan CHUAN_GROUP=chuan \
  OPENTUNNEL_BIN=/usr/local/lib/chuan/opentunnel/opentunnel \
  bash chuan-pkg/bootstrap-oauth-tunnel.sh
```

日志在 `/var/lib/chuan/oauth/opentunnel/bootstrap.log`。

### 2.5 之后的步骤可以由 agent 完成

`--with-opentunnel` 已经装好 helper 单元与 `services.d/oauth.toml`，以下动作都可以在通道里
让 agent 做，不需要 SSH、不需要重启：

| 动作 | 谁做 | 工具 |
|---|---|---|
| 查看/读取已有清单 | agent | `oauth_manifest(action="list")` / `("get", id=…)` |
| 起草 + 安装 provider 清单 | agent（主人审批） | `oauth_manifest(action="install", path="/ws/oauth/<id>.toml")` |
| 发起授权、把链接发给你 | agent | `mcp(server="oauth", tool="auth_begin")` |
| 查看授权进度 | agent | `... tool="auth_status"`（不返回 token） |
| 把服务接进核心 | agent（主人审批） | `service(action="add", kind="mcp", base=…, token_file=…)` |
| 续期 | helper 自动 | 见上面的「自动刷新」 |

顺序要记住：**先授权、后 `service add`**。token 文件不存在时 `service add` 会握手失败并
拒绝写配置（不会留下半截状态），重新授权后再 add 一次即可。
清单装好后 helper 每次调用都重读，所以改清单/加 provider 也不需要重启 helper。

下面的手工步骤保留给"自己动手"或排查场景。

### 3. helper 常驻服务

`--with-opentunnel` 已经把它装好并启用（`chuan-oauth-mcp.service`），确认一下：

```bash
systemctl status chuan-oauth-mcp --no-pager
sudo -u chuan cat /var/lib/chuan/oauth/mcp-token   # 本地 MCP 的 Bearer token，0600
```

旧包或手工安装时，从解开的包里补（单元随 tar 分发）：

```bash
sudo install -Dm644 chuan-pkg/chuan-oauth-mcp.service \
  /etc/systemd/system/chuan-oauth-mcp.service
sudo systemctl daemon-reload
sudo systemctl enable --now chuan-oauth-mcp
```

单元里有三处别删：`HOME=/var/lib/chuan`、`RuntimeDirectory` + `XDG_RUNTIME_DIR`
（`opentunnel` 要一个私有的 0700 runtime 目录，否则拒绝启动）、`ReadWritePaths`
放开 `oauth/` 与 `.local/`（后者是 tunnel 标记和证书续期写入的位置）。
仓库里的同内容文件：`adapters/oauth-mcp/chuan-oauth-mcp.service`。

### 4. provider 改成 opentunnel 回调

内置的 `cloudflare-mcp` 默认是 `manual`。新建一份 manifest 覆盖它（同名文件优先）：

```bash
sudo mkdir -p /etc/chuan/oauth/providers.d
sudo tee /etc/chuan/oauth/providers.d/cloudflare-mcp.toml >/dev/null <<'EOF'
schema = 1
id = "cloudflare-mcp"
kind = "mcp"
resource = "https://mcp.cloudflare.com/mcp"
discovery = "https://mcp.cloudflare.com/.well-known/oauth-protected-resource/mcp"

[client]
mode = "dcr"
token_endpoint_auth = "none"
pkce = "S256"

[authorize]
scopes = ["user:read", "account:read", "dns.read", "offline_access"]

[token]
refresh = true

[callback]
mode = "opentunnel"
path = "/cb"
port = 8765
opentunnel_bin = "/usr/local/lib/chuan/opentunnel/opentunnel"
opentunnel_profile = "chuan"

[output]
service = "cloudflare"
token_file = "/var/lib/chuan/oauth/cloudflare-mcp/access_token"
header = "Authorization: Bearer {token}"
EOF
sudo systemctl restart chuan-oauth-mcp
```

- **provider manifest 是配置**，跟 `services.d` 一样放 `/etc/chuan/oauth/providers.d/`；
  状态（tokens/pending/回调结果）才在 `/var/lib/chuan/oauth/`。单元用
  `--providers-dir /etc/chuan/oauth/providers.d` 指定，没写这个参数时退回
  `<dir>/providers.d`（旧安装的兼容路径）；
- `port` 必须与 bootstrap 的 route 一致（默认 8765）；
- 要写 DNS/Worker 就把 scopes 加上并重新授权（改了 manifest 要重启 helper）；
- `chuan oauth-mcp manifest validate /etc/chuan/oauth/providers.d/cloudflare-mcp.toml`
  可以先自查。

### 5. 接进 chuan（不改 chuan.toml）

服务定义放 `/etc/chuan/services.d/`，文件名就是服务名；启动时加载，**不用改 `chuan.toml`**：

```bash
sudo tee /etc/chuan/services.d/oauth.toml >/dev/null <<'EOF'
kind = "mcp"
base = "http://127.0.0.1:8790/mcp"
token_file = "/var/lib/chuan/oauth/mcp-token"
tools = ["auth_begin", "auth_status", "auth_refresh"]
EOF

sudo tee /etc/chuan/services.d/cloudflare.toml >/dev/null <<'EOF'
kind = "mcp"
base = "https://mcp.cloudflare.com/mcp"
token_file = "/var/lib/chuan/oauth/cloudflare-mcp/access_token"
EOF
```

重启顺序：先 `chuan-oauth-mcp`，再 `chuan`（chuan 启动时拉 MCP 工具目录）。
以后再加 provider 也只是往 `providers.d/` 与 `services.d/` 各放一个文件。

### 6. 授权

在聊天里让 agent 调本地 `oauth` 服务的 `auth_begin`（模型侧是 `mcp` 工具，
`server = "oauth"`, `tool = "auth_begin"`, `arguments = {"provider":"cloudflare-mcp"}`）。
它返回一条公开授权链接，agent 通过当前 channel 发给你。

浏览器打开 → 同意 → Cloudflare 跳到 `https://auth.<hostname>/cb?code=…` →
helper 自动换 token、写 `access_token`、撤掉公网入口。然后：

```bash
sudo -u chuan /usr/local/bin/chuan oauth-mcp status \
  --dir /var/lib/chuan/oauth \
  --providers-dir /etc/chuan/oauth/providers.d \
  --provider cloudflare-mcp                               # state=connected
sudo systemctl restart chuan                            # 重新拉工具目录
```

之后 agent 就能用 `mcp(server="cloudflare", tool="search"/"execute", ...)` 操作 Worker/DNS 等。

## 接进 chuan

见上面第 5 步；`oauth` 服务必须比 chuan 先起来，否则启动时拉不到 `auth_*` 工具。
MCP 的 `token_file` 每次请求重读，helper 刷新后不需要重启核心；但**首次授权完成**要重启
chuan，让它重新拉真实服务的工具目录。

## provider manifest

`providers.d/*.toml` 是数据，不是代码；agent 起草、主人批准后放到
`/etc/chuan/oauth/providers.d/<id>.toml`（配置；`--providers-dir` 可改）。
字段与约束见 `docs/OAUTH-PROVIDERS.md`。

默认内置 `cloudflare-mcp`（DCR + discovery，`manual` 回调）；没有 manifest 文件时也能用。
支持 `oauth2` 静态 client 与 `mcp` discovery/DCR；`allow_private = true` 只用于
本机测试（HTTP loopback），生产不要开。

## 排查（OpenTunnel）

| 现象 | 看哪里 |
|---|---|
| `auth_begin` 返回 `needs_bootstrap=true` | `/var/lib/chuan/.local/state/opentunnel.json` 不存在；重跑 bootstrap，看 `oauth/opentunnel/bootstrap.log` |
| 授权页能开，回调打不开 | `opentunnel serve` 没起来（看 `journalctl -u chuan-oauth-mcp`）；证书/route 不在 `~/.config/opentunnel/chuan.toml` |
| 同意后没有任何反应 | 先看 `/var/lib/chuan/oauth/cloudflare-mcp/callback-result.json` 的 `error`（例如 8765 被占用 → `监听回调端口失败`）；端口要和 bootstrap 的 route 一致 |
| 授权超过 10 分钟 | 一次性回调 TTL 到了，重新 `auth_begin` |
| 结果不明 | `/var/lib/chuan/oauth/cloudflare-mcp/callback-result.json`（`ok` / `error`） |
| 状态 `expired` | `auth_refresh`，或重新授权 |

bootstrap 失败常见原因是证书长期停在 `challenge`（服务端瞬时故障）。脚本已经会丢掉
pending 重试一次；仍失败就稍后重跑，不要无限 resume 同一个坏 tunnel。

## E2E

已用本地 mock OAuth provider 验证完整链路（不依赖 Cloudflare）：

- `auth_begin` 生成授权 URL；
- mock authorize 返回 callback；
- `opentunnel` 模式下回调自动触发 token 交换；
- `auth_status` 变 connected，`access_token` 写出；
- `auth_refresh` 后 access token 从 `access-1` 变为 `access-2`。

临时测试脚本与进程已清理；没有把 mock provider 留进仓库。
真机（riscv64 + 真 OpenTunnel + 真 Cloudflare）待上板确认。

## 安全（当前阶段）

- 明文 token 仍在 0600 文件；**加密存储/security hardening 留到正式版之后**；
- `begin` 只打印公开授权 URL；`finish` 从 stdin 读，避免 shell history/聊天；
- token 不进日志；scope 按最小权限；
- 每次 DCR 是新 client_id，不会改动其他 MCP 客户端。

## 安装（其它可选件）

- riscv64 的 OpenTunnel 由 chuan CI 额外交叉编译，作为独立可选资产
  `chuan-opentunnel-riscv64-linux-gnu.tar.gz`；主 chuan 包不含它；
- MIT 许可与版本随资产分发（`/usr/local/lib/chuan/opentunnel/LICENSE-opentunnel`、`VERSION`）。

## 升级 OpenTunnel

CI 固定在 `v0.6.2` tag，并校验对应 commit。升级时同时改 CI 的 tag/commit、
资产版本，并重新跑 riscv64 `zigbuild`。设备上安装新版 chuan 带
`--with-opentunnel` 即可替换二进制；tunnel identity/证书保留，bootstrap 会跳过。

## 回退到手动回调

把 `/etc/chuan/oauth/providers.d/cloudflare-mcp.toml` 的 `[callback] mode` 改回 `manual`
（或删掉这个文件用内置默认），重启 `chuan-oauth-mcp`，然后按 `begin` / `finish` 的流程走。
tunnel identity 留着不占公网入口，下次还能用。
