让技能调用一个有鉴权的服务（服务网关，D41）

适用：**技能脚本**要调一个有鉴权的 HTTP 接口（路由器/设备管理、第三方 API），需要多步编排
（读 → 等 → 写 → 回读）。如果只是查一次、不需要脚本，直接用 `http(service=…)` 更省事，不要走本文。

核心边界：**密码/令牌不进沙盒，也不进上下文**。技能只拿到一个本地 unix socket，
目的地、方法、路径、动作与鉴权头全部由 `[services.*]` 决定。

## 0. 先问清楚，不要猜

- `base`：只到 `scheme://host[:port]`，例如 `https://device.example.com`；
- 鉴权：请求头模板（`Authorization: Bearer {token}` / `X-Auth: Basic {token}`）与凭据文件格式；
- 要用的动作：每个动作的 method、path，以及是否靠请求头传动作（如 `X-Action: GetStatus`）。

拿不到这些就先问主人；**不要**凭直觉拼 REST 路径，也不要拿 404 去试。

## 1. 凭据文件（主人完成；你只给步骤）

文件内容**一行**，就是模板里 `{token}` 要替换成的值：

- `Bearer`：直接写令牌；
- `Basic`：写 `base64("user:password")`，**不要**带 `Basic ` 前缀（前缀在 header 模板里）。

```bash
read -rsp '密码: ' P; echo
umask 077
printf 'admin:%s' "$P" | base64 -w0 > /etc/chuan/secrets/<名字>
unset P
sudo chown root:chuan /etc/chuan/secrets/<名字>
sudo chmod 0440 /etc/chuan/secrets/<名字>
sudo -u chuan head -n1 /etc/chuan/secrets/<名字>   # 能读到才算对
```

不要要求主人把密码发到聊天里，也不要把任何凭据写进 `/ws` 或技能参数。

## 2. 注册服务（你来做；会弹审批卡）

```
service(action="add", name="device",
        base="https://device.example.com",
        token_file="/etc/chuan/secrets/<名字>",
        header="X-Auth: Basic {token}",
        extra_headers={"Content-Type": "text/plain"},
        expose_to_skills=true,
        allow_methods=["POST"],
        allow_paths=["/api/"],
        header_allow={"X-Action": ["GetStatus", "SetMode"]},
        _approval={why="<一句话：接哪个设备、做什么>", impact="向设备发指令"})
```

要点：

- `allow_paths` 以 `/` 结尾按**前缀**匹配，否则精确匹配；
- `header_allow` 是**头名 + 允许取值**（动作类接口就靠它）；表外一律拒绝；
- 三张白名单任一为空都会拒绝开放，不会退化成"全放开"；
- 只给模型用 `http` 调用时**不要**开 `expose_to_skills`；
- 主人批准后立即生效，并写进 `services.d/`，**重启不丢**。审批卡会列出
  base / 方法 / 路径 / 可设置的头（动作）/ 凭据文件 / 鉴权头模板。

## 3. 先验证单步，再写技能

```
http(service="device", method="POST", path="/api/",
     headers={"X-Action": "GetStatus"}, body={})
```

返回正常再继续。4xx 先看服务端的错误文本，不要换着路径乱试。

## 4. 写技能

`skill.json` 加一项（与第 2 步的服务名一致）：

```json
{ "services": ["device"], "network": false }
```

脚本从 stdin 读参数，`_gateway` 是服务名 → socket 路径；一次连接发一行、读一行：

```python
import json, socket, sys

args = json.load(sys.stdin)
GW = args["_gateway"]["device"]

def call(action, body=None):
    req = {"method": "POST", "path": "/api/",
           "headers": {"X-Action": action}, "body": body or {}}
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(GW)
    s.sendall((json.dumps(req) + "\n").encode())
    buf = b""
    while chunk := s.recv(65536):
        buf += chunk
    s.close()
    out = json.loads(buf)
    if "error" in out:
        raise RuntimeError(out["error"])
    return json.loads(out["body"])
```

约束：每服务每次执行 ≤64 次转发、请求体 ≤64KB、响应 ≤1MB、单请求 20s；
`network: true` 与 `services` **不能同时声明**。

**examples 必须是离线自检**（解析参数、拼 payload、边界情况）。发布时的 dry-run
不启动网关，样例里真去连服务一定失败。

## 5. 安装（第二张审批卡）

```
publish_skill(name="…", services=["device"], _approval={…})
```

`services` 参数必须与 `skill.json` 里的完全一致；卡上会重复展示同一份授权摘要。

## 6. 排错对照

| 现象 | 原因 |
|---|---|
| `没有对技能开放（expose_to_skills…）` | 服务没开入口，或主人还没批准 |
| `allow_methods / allow_paths 为空` | 开了入口却没给白名单 |
| `没有对技能开放路径 …` | path 不在清单里（注意前缀是否以 `/` 结尾） |
| `取值不在允许清单里` | 动作头的值没写进 `header_allow` |
| `不能由技能设置` | 脚本想自己带 `Authorization`/`Cookie`/`Host` 等（禁止） |
| `读取凭据失败` | 文件路径不对，或服务用户读不到 |
| 技能拿不到 `_gateway` | `skill.json` 没写 `services`，或安装时没传 `services` |
| `network=true 与 services 不能同时` | 去掉 `network`（网关本来就不需要网卡） |

## 7. 收回

```
service(action="remove", name="device", _approval={…})
```

立即失效；已安装的技能还在，但调用会报"服务没有配置"。删除不会删凭据文件。
