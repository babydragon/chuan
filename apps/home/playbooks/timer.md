# 建/管 systemd user timer（timer）

> 适用于"每天/每小时跑一次、不需要 LLM、不需要 root"的活：采温度、磁盘、负载、
> 抓一次网关状态、清理旧文件。**不要**用 `remind` 做这类事——那要走模型、要花钱，
> 而且服务重启就断。
>
> 你的 user manager 已经由 `loginctl enable-linger` 拉起来了，timer 跑在**独立 slice**，
> 不吃 `chuan.service` 的 `MemoryMax=1G`。

## 位置与命名（固定，别自创）

| 东西 | 路径 |
|---|---|
| 脚本 | `/var/lib/chuan/timers/<name>.sh`（可写，已在 `[host.path]`） |
| service | `/var/lib/chuan/.config/systemd/user/chuan-<name>.service` |
| timer | `/var/lib/chuan/.config/systemd/user/chuan-<name>.timer` |

`<name>` 只用 `[a-z0-9-]`，≤ 32 字。前缀 `chuan-` 是留给"哪些是 agent 建的"的边界。

## service 模板

```ini
[Unit]
Description=chuan timer: <name>

[Service]
Type=oneshot
ExecStart=/var/lib/chuan/timers/<name>.sh
# 脚本自己负责超时；systemd 再兜一层
TimeoutStartSec=300
# 一次跑飞不要拖垮主板：上限给够但不放任
MemoryMax=128M
# 与主服务同级的加固
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/var/lib/chuan/timers
StandardOutput=journal
StandardError=journal
```

## timer 模板

```ini
[Unit]
Description=chuan timer: <name>

[Timer]
OnCalendar=*-*-* 07:30:00
# 机器关机错过的那次，开机后补跑一次
Persistent=true
# 别在整点一起挤
RandomizedDelaySec=300
Unit=chuan-<name>.service

[Install]
WantedBy=timers.target
```

`OnCalendar` 常用写法：`hourly`、`*-*-* 07:30:00`、`Mon..Fri 08:00`、`*-*-* 03:00/6:00`（每 6 小时）。

## 步骤（一次做完，别拆成五轮）

```bash
# 1) 写脚本并自测（自己先跑通，不要指望 systemd 帮你发现语法错）
cat > /var/lib/chuan/timers/<name>.sh <<'EOF'
#!/bin/sh
set -eu
echo "temp=$(cat /sys/class/thermal/thermal_zone0/temp)"
echo "disk=$(df -h / | tail -1)"
EOF
chmod +x /var/lib/chuan/timers/<name>.sh
/var/lib/chuan/timers/<name>.sh        # ← 必须这一步先过

# 2) 写两个 unit（同上模板）
mkdir -p /var/lib/chuan/.config/systemd/user

# 3) 生效并立刻跑一次
systemctl --user daemon-reload
systemctl --user enable --now chuan-<name>.timer
systemctl --user start chuan-<name>.service     # 手动跑一次，别等 7:30

# 4) 验三件事
systemctl --user list-timers chuan-<name>.timer --no-pager   # NEXT 时间对不对
journalctl --user -u chuan-<name>.service -n 20 --no-pager    # 有没有输出/报错
systemctl --user show chuan-<name>.service -p Result -p ExecMainStatus
```

## 硬规则

1. **脚本里不放 sudo**：user timer 是普通用户，`sudo` 要么要密码、要么被
   `NoNewPrivileges` 挡住。需要 root 的活走系统 timer（见下）或 `hostd`（未实现）。
2. **脚本里不调模型**：那是 `remind` 的活。timer 只做确定性动作。
3. **输出写 journal**，不要自己 `>> /tmp/xxx.log`（`PrivateTmp` 下重启就没）。
4. **失败要能看见**：脚本 `set -eu`，退出码非 0 会记在 `Result=exit-code`；
   要主动通知就写 journal 并在里面打 `ERROR:` 前缀，主人可以看到再让你处理。
5. **一次一个事**：一个 timer 只做一件事，不要写"大一统监控脚本"。

## 从别处迁移过来时逐条核对

| 要核对的 | 为什么 |
|---|---|
| 脚本属主和可执行位 | 从 `/root` 或别的账号搬过来，chuan 往往没有读/执行权 |
| 脚本里的绝对路径 | 常见 `/root/xxx`、`~/xxx`；`~` 在 user manager 里是 `/var/lib/chuan` |
| 环境变量（PATH / 代理 / TZ） | systemd 的 PATH 很短；代理要显式写，或从 `/etc/environment` 读 |
| 是否依赖 root | `smartctl`、`systemctl restart`、读 `/root` → 只能留在系统 timer |
| 原有的输出/告警方式 | 邮件、`wall`、写文件；迁过来后统一改成 journal，通知交给主 agent |
| 原来的调度语义 | cron 的 `*/5` ≠ `OnCalendar=*:0/5`；`@reboot` 在 user timer 里用 `OnBootSec=` |
| 迁移后先禁用旧的 | 新的一次都没跑通之前，**不要**删旧的；两边并存时把新脚本的输出打在 journal，对一天 |

## 不该迁的（留在系统 timer）

- 需要 root 的采集（`smartctl -a /dev/sda`、`/proc/1/...` 之外的受限路径）；
- 服务生命周期（重启别人、`systemctl disable`）；
- 与系统启动顺序强相关（`After=network-online.target` 在 user manager 里语义不同）。

这类只能"读+管"：你用 `sys(units)` 看状态、用 `host` 改脚本内容，unit 本身由主人写。
