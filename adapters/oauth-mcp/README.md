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

`serve` 首次会生成 `<dir>/mcp-token`（0600）；chuan 用它作为本地 MCP 的 Bearer token。
工具：`auth_begin`、`auth_status`、`auth_refresh`；`auth_revoke` 默认不注册
（等 per-tool 角色策略落地后再开）。

## 回调

- `callback.mode = "manual"`：`begin` 打印授权 URL；浏览器跳到 127.0.0.1 失败后，
  把地址栏 URL 粘给 `finish` 的 stdin。
- `callback.mode = "opentunnel"`：`begin` 起本地回调监听 + `opentunnel serve`，
  收到 code 后自动换 token 并停掉 `serve`；需要先 bootstrap 出 tunnel/route。
  没有 tunnel 时 MCP 返回 `needs_bootstrap=true`，模型应通过当前通道告诉用户。

bootstrap 的 marker 默认在 `~/.local/state/opentunnel.json`，可用
`CHUAN_OAUTH_TUNNEL_STATE` 覆盖。

## provider manifest

`providers.d/*.toml` 是数据，不是代码；agent 起草、主人批准后放到
`<dir>/providers.d/<id>.toml`。字段与约束见 `docs/OAUTH-PROVIDERS.md`。

默认内置 `cloudflare-mcp`（DCR + discovery）；没有 manifest 文件时也能用。
支持 `oauth2` 静态 client 与 `mcp` discovery/DCR；`allow_private = true` 只用于
本机测试（HTTP loopback），生产不要开。

## 安装（可选 OpenTunnel）

```bash
sudo ./install.sh --with-opentunnel --bootstrap-oauth-tunnel
```

- riscv64 的 OpenTunnel 由 chuan CI 额外交叉编译，作为独立可选资产；
- `--with-opentunnel` 只装二进制，`--bootstrap-oauth-tunnel` 才创建 tunnel/证书；
- 失败不阻断 chuan 安装；可重跑 `deploy/bootstrap-oauth-tunnel.sh`；
- MIT 许可与版本随资产分发。

## 接进 chuan

```toml
[services.oauth]
kind = "mcp"
base = "http://127.0.0.1:8790/mcp"
token_file = "/var/lib/chuan/oauth/mcp-token"
tools = ["auth_begin", "auth_status", "auth_refresh"]
```

真实服务的 `token_file` 由 helper 写：

```toml
[services.cloudflare]
kind = "mcp"
base = "https://mcp.cloudflare.com/mcp"
token_file = "/var/lib/chuan/oauth/cloudflare-mcp/access_token"
```

MCP 的 `token_file` 每次请求重读，helper 刷新后不需要重启核心。

## E2E

已用本地 mock OAuth provider 验证完整链路（不依赖 Cloudflare）：

- `auth_begin` 生成授权 URL；
- mock authorize 返回 callback；
- `opentunnel` 模式下回调自动触发 token 交换；
- `auth_status` 变 connected，`access_token` 写出；
- `auth_refresh` 后 access token 从 `access-1` 变为 `access-2`。

临时测试脚本与进程已清理；没有把 mock provider 留进仓库。

## 安全（当前阶段）

- 明文 token 仍在 0600 文件；**加密存储/security hardening 留到正式版之后**；
- `begin` 只打印公开授权 URL；`finish` 从 stdin 读，避免 shell history/聊天；
- token 不进日志；scope 按最小权限；
- 每次 DCR 是新 client_id，不会改动其他 MCP 客户端。

## 升级 OpenTunnel

CI 固定在 `v0.6.2` tag，并校验对应 commit。升级时同时改 CI 的 tag/commit、
资产版本，并重新跑 riscv64 `zigbuild`。设备上安装新版 chuan 带
`--with-opentunnel` 即可替换二进制；tunnel identity/证书保留，bootstrap 会跳过。
