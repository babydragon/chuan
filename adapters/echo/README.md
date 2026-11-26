# Echo 适配器

一个**最小终端通道**，只做两件事：把核心发来的 `View` 打到终端，把用户输入当作入站文本。
它存在的目的不是聊天，而是验证 D44 的进程协议（握手、入站、投递、按钮/回调、附件拒绝）。

- 协议：`docs/EXTENSIONS.md §1`
- 核心侧实现：`crates/harness-channels/src/socket.rs`
- 不是产品通道：没有原地刷新、没有富文本编辑；流式输出就是追加行。

## 前置

核心配置里启用它（示例）：

```toml
[channels]
enabled = ["telegram", "echo"]        # echo 可以只用于本地调试

[[channels.adapter]]
id = "echo"
socket = "/run/chuan/channels/echo.sock"
# 调试时通常与 chuan 同用户，所以不写 uid；生产要独立用户再写 uid。
# media_dir = "/var/lib/chuan/echo-media"   # 不写 = 不接受附件

[[identity.user]]
id = "owner"
channels = ["telegram:123456789", "echo:local"]
```

`enabled` 里第一个通道是默认通道；把 `echo` 放后面，普通回复仍走 Telegram。
要让 Echo 成为唯一入口（本地调试），只写 `enabled = ["echo"]`，并确认 `[identity]` 有 `echo:local`。

## 运行

```bash
python3 adapters/echo/echo_tui.py --socket /run/chuan/channels/echo.sock
```

可选参数：`--conversation tui`、`--actor local`、`--channel echo`。
输入文字回车发送；出现选项时输入数字；`/quit` 或 Ctrl-C 退出。按 `q` 不会退出（避免误触）。

核心先启动（它会创建 socket），Echo 再连。核心重启后 Echo 需要重开（v1 不做自动重连）。

## 验证清单

- 文本：发一句，模型回复出现在终端。
- 流式：`caps.edit=false`，核心只发终稿（没有原地改写）。
- 按钮：审批/选择出现 `1) … 2) …`，输入数字触发回调。
- 附件：Echo 不发附件；如果手工发，核心应按 `media_dir` 校验（未配置则拒绝）。
- 断线：关掉 Echo，核心日志出现“适配器连接结束”；重开即可再连。
