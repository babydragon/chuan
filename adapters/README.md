# adapters/ — 外部通道适配器

这里放**通道适配器**的源码与部署资产。它们**不属于 Cargo workspace**，不参与
`cargo build` / `cargo zigbuild`，也不会被编进 `chuan` 二进制。

协议（握手、入站/出站方法、错误码、安全边界）见 [`docs/EXTENSIONS.md §1`](../docs/EXTENSIONS.md)。
设计取舍见 [`docs/DECISIONS.md` D44](../docs/DECISIONS.md)。

## 目录约定

```
adapters/
  README.md              # 本文件
  echo/                  # 协议测试桩（已实现）：终端 TUI，用来验协议
  weixin/                # 首个真实适配器（计划）：复用 harness-channels::weixin 的 Rust 实现
    Cargo.toml
    src/main.rs
    chuan-channel-weixin.service
    README.md            # 平台特有的配置/坑，只记事实
  oauth-mcp/             # OAuth provider manifest 与 unit（helper 已内置进 chuan）
    providers.d/
    chuan-oauth-mcp.service
    README.md
```

> 钉钉调研保留在 `docs/DINGTALK.md`，但因无法在目标组织建应用，**暂不采用**。
> `oauth-mcp` 是宿主旁路服务、不是通道适配器，落点见 `docs/EXTENSIONS.md §5`。

- 每个适配器一个目录，自带 `README.md` 与 systemd 单元；平台特有的事实写在自己的 README，
  不写进 `docs/EXTENSIONS.md`。
- 适配器之间不共享代码；公共部分只有协议，避免“适配器框架”变成第二个内核。

## 如何单独开发

1. **实现协议 v1**（任意语言；微信第一阶段复用 Rust 实现，新平台推荐 Python/Go）：
   - 连上 `/run/chuan/channels/<id>.sock`，先发 `chuan.hello`；
   - 把平台消息映射成 `chuan.inbound`；
   - 实现 `chuan.deliver / update / ack / send_file / prompt / clear_buttons`；
   - 遵守 `caps` 的能力声明（不能编辑就声明 `edit=false`，不要假装成功）。
2. **自己起一个独立 unit**：独立系统用户、`LoadCredentialEncrypted=` 放平台凭据、
   `ProtectSystem=strict`、`ProtectHome=yes`、`PrivateTmp=yes`、`NoNewPrivileges=yes`、
   `MemoryMax`/`CPUQuota`。
3. **在 chuan 配置里注册**：

   ```toml
   [[channels.adapter]]
   id = "echo"
   uid = "chuan-echo"                       # 独立用户；同 uid 调试可省略
   socket = "/run/chuan/channels/echo.sock"
   media_dir = "/var/lib/chuan/echo-media"
   max_media_mb = 20
   ```

4. `systemctl restart chuan`（只重载配置，不重编、不发版）。
5. 在 `[identity]` 里绑定人：`channels = ["dingtalk:<userId>"]`。

## 本地调试

- socket 路径通过环境变量传入（如 `CHUAN_CHANNEL_SOCKET`），不要写死。
- 日志写 stderr，由 journald 收集：`journalctl -u chuan-channel-<id> -f`；
  内核日志里的事件带 `channel:<id>` 前缀。
- 先用 `echo/` 桩跑通协议，再接平台 SDK；平台 SDK 的重连/游标行为不要和协议重连混在一起。

## 验收清单

- 握手成功；协议版本不匹配时明确报错，不降级。
- 文本、图片、文件各一条；超限附件被拒。
- 重复 `dedupe_key` 不重复入队。
- 断线 30s 后自动重连；核心重启后恢复。
- 按钮/卡片不可用时按 `caps` 降级为文本，不静默丢失。
- 平台凭据不出现在日志、不出现在消息正文。
- 适配器进程读不到 `chuan` 的配置、DB、Journal（用独立 uid 验证）。

## 发布

- 拷贝适配器目录与 unit → `systemctl daemon-reload` → 启 unit → 重启 `chuan`。
- 适配器版本与协议版本分开记：协议不兼容才需要改内核；适配器自身升级不需要发 `chuan` 版本。
- 卸载：停 unit、从配置删 `[[channels.adapter]]`、重启 `chuan`；不需要回滚二进制。

## 安全红线

- 适配器**不得**读取 `chuan` 的配置、数据库、Journal、其他通道的凭据；
  它只需要自己的 state/media 目录和平台凭据。
- 附件只报自己 media 目录下的路径；核心会重新校验并拷贝，不信任适配器给的摘要。
- 适配器是该通道的信任根（能伪造主人消息），所以**只能由主人安装**；
  agent 不得自行新增、替换或改配置。
