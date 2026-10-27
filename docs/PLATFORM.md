# 目标平台实测基线

> 实测机：RISC-V SBC / 目标板发行版（Debian 系）。执行 `deploy/verify-sandbox.sh` 得到。
> 状态：**首轮因脚本 bug 失败，修正后重跑已全部通过**（§5 白名单挂载集与 `/home` 不可见均为实测结果）。
> 复现命令见「复现方式」，待补测项见文末表格。

## 硬件与系统

| 项 | 值 | 影响 |
|---|---|---|
| 架构 | riscv64 | 目标三元组 `riscv64gc-unknown-linux-gnu` |
| 内核 | 6.6.63 | Landlock 可用（≥5.13）、cgroup v2、userns |
| 发行版 | 目标板发行版（Debian 系） | 非 Debian 直系（无 `unprivileged_userns_clone` 开关） |
| glibc | 2.41 | zigbuild 用默认 2.28 基线即可（前向兼容） |
| systemd | 257 | `Delegate=yes`、`MemoryMax`、`ProtectSystem=` 全部可用 |
| init | systemd | 可用 `OnFailure=`、`DynamicUser` |

## 沙盒能力

| 能力 | 实测结果 | 结论 |
|---|---|---|
| userns 限制 | 两个 sysctl 均不存在；`max_user_namespaces=29751` | ✅ 无限制 |
| bwrap | 0.11.0，**无 setuid** | ✅ 走非特权 userns |
| `ro-bind /` 冒烟 | 通过 | ✅ |
| `--unshare-all` | ✅ 通过 | ✅ 完整命名空间隔离可用 |
| 生产白名单挂载集 | ✅ 通过 | ✅ 系统目录只读、仅 workspace 可写 |
| `/home` 可见性 | ✅ 不可见 | ✅ 密钥与个人文件不暴露给沙盒 |
| 网络 | ✅ 仅 `lo`，`curl` 无法出网 | ✅ 默认断网成立 |
| 环境变量 | ✅ `--clearenv` 后仅剩显式 setenv | ✅ 宿主环境不泄漏 |
| cgroup v2 | unified；controllers 含 `memory`/`pids`/`cpu`/`io` | ✅ |
| user systemd manager | 可用 | ✅ 可 `systemd-run --user --scope` |
| **内存硬限** | `MemoryMax=200M` 下 600MB 分配 **rc=137** | ✅ **硬限真实生效** |
| Landlock | LSM 列表包含 `landlock` | ✅ 降级路径可用 |
| seccomp | 默认未启用过滤器 | 需要时自行加 |

## 直接结论

1. **完整沙盒方案成立**（`--unshare-all` + 白名单挂载集全部实测通过），不需要退回 systemd 沙箱或 Landlock；后者留作备份。
2. **`RLIMIT_AS` 不是必需品**：cgroup 已验证能杀（rc=137），Python 类任务的内存限制有可靠手段。
3. **生产挂载集必须是白名单，不能 `ro-bind / /`**。实测确认白名单挂载集下 `/home` 不可见；若改成整根只读挂载，`/home` 会对沙盒可见，等于把 SSH 密钥、照片、其它项目全部交出去。
4. bwrap 0.11 不支持部分 0.13 的新参数（如 `--disable-userns`），实现时以 0.11 为准。
5. 沙盒内 `/proc/net/dev` 只有一个 `lo` 是**正确状态**（netns 自带环回），不代表网络可用；判断标准是能否出网。

## 生产挂载集（基准形态）

```bash
bwrap --unshare-all --die-with-parent --new-session --cap-drop ALL \
  --ro-bind /usr /usr \
  --symlink usr/lib /lib  --symlink usr/lib64 /lib64 \
  --symlink usr/bin /bin  --symlink usr/sbin /sbin \
  --ro-bind /etc /etc \
  --proc /proc --dev /dev --tmpfs /tmp \
  --dir /ws --bind "$WORKSPACE" /ws --chdir /ws \
  --clearenv --setenv PATH /usr/local/bin:/usr/bin:/bin --setenv HOME /ws \
  <cmd>
```

（`/lib`、`/bin` 等在 merging-usr 系统上是符号链接，需用 `--symlink` 而非 `--ro-bind`；脚本里按 `readlink` 自动处理。）

**绝不可见**：`/home`、`/root`、`/var`、`/mnt`、`/media`、`/srv`、宿主 `/tmp`（沙盒内是独立 tmpfs）、宿主的环境变量、宿主进程。

**只读投递目录**：`~/.local/share/chuan/inbox` 挂到沙盒 `/inbox`（`--ro-bind`）。用户需要 agent 处理文件时放这里；
宿主的 `/tmp` 永远不会挂载（隐私与误删风险）。

## 复现方式

```bash
ssh <user>@<board> 'bash -s' < deploy/verify-sandbox.sh
```

脚本只读检查 + `/tmp` 临时目录，不修改系统；唯一可能需要 `sudo` 的是安装 `bubblewrap`。
**用将来运行服务的普通用户执行**，不要用 root——userns 与 cgroup 委派行为与身份强相关。

## 待补测（与 `ROADMAP.md §0` 的 V1–V5 对应）

| # | 项 | 判据 |
|---|---|---|
| V1 | 沙盒内中文渲染 | `fc-list :lang=zh`、`python3 -c "import PIL, reportlab"` |
| V2 | 沙盒内调 `lp` | 挂 `/run/cups` 后沙盒里 `lpstat -e` 可见打印机 |
| V3 ✅ | TG 429 阈值 | 1s×20 零失败；并发 30 次零失败（6.1s）；未见 429 |
| V4 ✅ | bwrap 启动延迟 | 已测：板 29.8ms / 本机 8.3ms（均含 2 次 `date` fork，净约 26ms / 5ms） |
| V5 | 沙盒内 Python 依赖 | 同上 |
