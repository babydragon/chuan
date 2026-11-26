接一个需要 OAuth 的第三方服务（provider 清单 + 授权 + 服务注册）

适用：某个 HTTP/MCP 服务要求 OAuth 授权（授权码 + PKCE），而你希望**主人只在通道里点两下**
就接进来。安装阶段已经准备好 OpenTunnel 与 `chuan-oauth-mcp` 时，全程不需要 SSH、不需要
`systemctl restart`。

先确认三件事（缺一项就先说清楚缺什么，不要硬试）：

1. `mcp(server="oauth", tool="auth_status")` 能调通 → helper 在；
2. 服务方的授权元数据（通常是一个 `/.well-known/oauth-protected-resource/...` 地址）；
3. 服务方要求的 scope。**从只读开始**；写权限等主人明确要了再加。

## 1. 写清单草稿

在 `/ws/oauth/<id>.toml` 写（`id` 用来区分 provider，小写字母/数字/-/_/.）：

```toml
schema = 1
id = "<id>"
kind = "mcp"                      # 或 oauth2（静态 client）
resource = "https://service.example.com/mcp"
discovery = "https://service.example.com/.well-known/oauth-protected-resource"

[client]
mode = "dcr"                      # 支持动态注册时用 dcr；否则 static + client_id
token_endpoint_auth = "none"      # 服务方要求密钥时用 client_secret_file
pkce = "S256"

[authorize]
scopes = ["read", "offline_access"]

[token]
refresh = true

[callback]
mode = "opentunnel"               # 有 tunnel 就自动回调；否则 manual
path = "/cb"
port = 8765

[output]
token_file = "/var/lib/chuan/oauth/<id>/access_token"
header = "Authorization: Bearer {token}"
```

规则（违反会被工具拒绝，不要绕）：

- 清单里**不能出现密钥字面量**：只能写 `client_secret_file` 这类路径；
- `output.token_file` 必须落在 `/var/lib/chuan/oauth/` 下——那是 helper 的状态目录；
- URL 必须 `https://`（只有本机测试才允许 `allow_private = true` 用 http）。

## 2. 安装清单

```
oauth_manifest(action="install", path="/ws/oauth/<id>.toml", _approval={...})
```

主人批准后文件落到 `/etc/chuan/oauth/providers.d/<id>.toml`。**不用重启**：helper 每次调用都重读。
覆盖已有清单会先备份到 `.backup/`。

顺带说清楚影响面：这次只加一份"怎么拿 token"的配置，还没有任何权限；真正的授权在下一步由主人点。

## 3. 授权（把链接交给主人）

```
mcp(server="oauth", tool="auth_begin", args={"provider": "<id>"})
```

- 返回里有 `url`：用普通消息把链接发给主人，附一句"点完同意就行，不用复制回调地址"；
- **不要把 token、code、回调 URL 里的参数发到聊天里**，也不要在聊天里索要它们；
- 返回 `needs_bootstrap=true`：没有公网回调。告诉主人两条路——等他把 tunnel 建好，
  或把清单改成 `mode = "manual"` 后重新授权（浏览器跳到 `127.0.0.1` 失败是正常的）。

然后每 30–60 秒问一次 `mcp(server="oauth", tool="auth_status", args={"provider":"<id>"})`，
`state` 变 `connected` 就继续。授权链接 10 分钟内有效，过期就重新 `auth_begin`。

## 4. 接进核心（此时再注册服务）

```
service(action="add", name="<服务名>", kind="mcp",
        base="https://service.example.com/mcp",
        token_file="/var/lib/chuan/oauth/<id>/access_token")
```

- **顺序不能反**：先授权、后 `service add`。token 不存在时握手会失败（"连接失败，配置未写入"），
  这是正常的，**不要重启任何服务**，重新授权后再 add 一次即可；
- 成功后会立即注册并写入 `services.d/`，下次启动自动加载；
- 不写 `chuan.toml`——服务定义属于增量条目，放 `services.d/`（D48）。

## 5. 收尾

- 告诉主人现在能用什么（例如"可以查域名列表了"），以及 scope 是只读；
- 要加写权限：覆盖清单（改 `[authorize] scopes`）→ 重新 `auth_begin` → 主人再点一次 →
  `service add` 同名重加即可（会覆盖片段）；
- 到期与刷新：helper 自动刷新，剩余不足 10 分钟就换；连续失败会写日志。
  真的失效（`state=expired`、`invalid_grant`）才需要重新授权。

## 失败对照

| 现象 | 处理 |
|---|---|
| `needs_bootstrap=true` | 没有 OpenTunnel 回调；改 `manual` 或等 tunnel |
| 授权页说 redirect_uri 不合法 | 清单的 callback 与服务方登记不一致；`opentunnel` 模式要固定 route |
| `state=expired` 且刷新失败 | 重新 `auth_begin`（refresh token 已被服务方撤销） |
| `401` / `insufficient_scope` | scope 不够：加 scope → 覆盖清单 → 重新授权 |
| `service add` 握手失败 | 先确认 token 文件存在、`auth_status` 是 `connected` |

不要建议主人 SSH、改 `chuan.toml` 或重启服务来完成本流程；这些步骤本来就是给通道里的
agent 用的。唯一例外：OpenTunnel 组件本身没装/没有 tunnel 时，需要主人做一次安装级操作。
