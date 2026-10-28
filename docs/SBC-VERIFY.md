# 目标板端到端验证清单（riscv64）

> 开发与联调都在 x86_64 开发机上完成；这份清单只记录**只能在目标板确认**的项。
> 每项做完改成 `[x]`，并写下实测结论（版本 / 耗时 / 报错原文）。
> 原则：**实测优先于推断**；没测过的一律留空，不写"应该可以"。

参考硬件：RISC-V SBC（riscv64，8GB 内存，Debian 系发行版，systemd）。
数值阈值按这块板定：常驻内存目标 <150MB，沙盒冷启动 <60ms。

---

## 0. 编译与依赖

- [ ] `cargo zigbuild --target riscv64gc-unknown-linux-gnu --release` 通过（含 rusqlite bundled / rustls）
- [x] `bwrap --version` ≥ 0.11，且两组参数都被接受（**2026-10-27 实测**：离线 `exec` 与联网 `lan_exec` 均可执行，两组参数都被接受）：
      离线：`--unshare-all`
      联网：`--unshare-user --unshare-ipc --unshare-pid --unshare-uts --unshare-cgroup-try`
      （⚠️ 0.11 若没有 `--unshare-cgroup-try`，联网 lane 要换写法）
- [ ] cgroup v2 unified，`memory` / `pids` 控制器可写（systemd `Delegate=yes`）
- [x] `python3 -m venv` 可用（**2026-10-27 实测**：默认未装，`apt install python3-venv` 后可用）
- [x] 沙盒冷启动耗时（**2026-10-27 实测**：`exec ls /home` 工具侧 39ms，含 `ls` 本身；开发机约 30ms）
- [ ] （可选）`node` / `npm` 在 `/usr/bin` 下；否则 `install_deps(npm)` 会明确报"找不到"

## 1. Python 运行时

- [x] 首次启动创建 venv（`--system-site-packages`）成功（**2026-10-27 实测**：装好 `python3-venv` 后 21s，riscv64 上 ensurepip 解包慢属正常）
- [ ] **Debian 的 `/usr/lib/python3/dist-packages` 在 venv 的 `sys.path` 里**
      （与开发机路径不同，必须实测）
- [ ] `apt install python3-numpy` 后 `/py/bin/python3 -c "import numpy"` 直接可用
- [ ] `install_deps(pip)` 实际联网安装一次：记录源、耗时、是否需要代理
- [ ] 已可导入的包会被探测拦截（不重复安装）

## 2. 沙盒与隔离

- [x] `/ws` 可写；`/inbox` `/skills` `/media` 只读；`/home` `/root` `/var` 不可见
      （**2026-10-27 实测**：`touch /ws/a` → ok；写 `/inbox` → `Read-only file system`；
      `ls /var /root` → `No such file or directory`；沙盒内 `id` = `uid=996(chuan) gid=989(chuan)`；
      `/skills` `/media` 只读未单独验）
- [ ] `/py` `/node` 只读挂载；`PATH` 含 `/py/bin`
- [x] `/tmp` 是 tmpfs 且**有上限**（**2026-10-27 实测**：修复前 `df -h /tmp` = 3.9G（内存一半），
      能写到把服务拖进 OOM；已加 `--size` 限 256MB，`tmp_has_size_cap` 守卫；上限值待新包复核）
- [x] 超时 `killpg`：`sleep 300` 到点被杀，无残留进程
      （**2026-10-27 实测**：`timeout_ms=2000` → `timeout after 2000ms`；之后 `ps -ef` 无 `sleep` 残留）
- [x] 离线 lane 的 netns 里**只有 `lo`**（**2026-10-27 实测**：`ip -o link` 只有 `lo`；
      `curl` 直连 exit=6、走代理 exit=7，与"无网卡"一致）
- [x] 联网 lane（`lan_exec` / `network: true` 技能）能看到网卡，且 **DNS 可用**
      （**2026-10-27 实测**：`getent hosts github.com` → `20.205.243.166`，2.5s）
- [x] `[sandbox] mounts` 挂 `/run/cups` 后，沙盒内 `lpstat -e` 可见打印机（只读）
      （**2026-10-27 实测**：`lpstat -r` → `scheduler is running`；`lpstat -e` → `B7520DW` `T435W`；
      默认队列 `B7520DW`；离线 `exec` 里即可用，说明走的是 socket 不是网络）
- [x] 联网 lane 能解析 `.local`（mDNS）（**2026-10-27 实测**：`getent hosts BRW4CD577B1850A.local`
      → `10.84.1.136`，2.0s。sandbox 原有的二进制靠 `[sandbox] mounts` 加 `/run/avahi-daemon` 打通；
      rc.4 起联网 lane 自动挂载，不再需要写配置。注意队列名 `B7520DW` 不是主机名，
      `lpstat -v` 给的是 `ipp://BRW4CD577B1850A.local:631/ipp/print`）
- [ ] `http://192.168.x.x` 被 `web_fetch` 拒绝并留痕

- [x] 沙盒里读不到凭据：`/etc/chuan/secrets` 在沙盒内是空的只读 tmpfs
      （**2026-10-28 实测**：`ls` 为空、`cat` 报文件不存在、`touch` 报只读；
      宿主上仍是 0750 root:chuan + 各文件按用途 0400/0440）

## 3. 出网与审计

- [ ] `web_fetch` 取公网页面成功（TLS 根证书齐全；需要代理时生效）
      （**2026-10-27 部分**：`openai.com` 返回 403 —— 响应头 `server: cloudflare` +
      `cf-mitigated: challenge`，换浏览器 UA 同样 403，是源站反爬而非代理/请求头问题）
- [ ] 大响应（>12KB JSON）变成"结构 + 预览"，全文落 `/ws/.tool-results/`
- [ ] `net-access.jsonl` 落盘、权限 `600`、达到上限后轮转
      （**2026-10-27 部分**：`tail` 能读到 403 那条记录，字段齐全；权限与轮转未验）
- [ ] 拒绝内网的记录也写进日志

## 4. 通道与部署

- [ ] systemd 启动、`flock` 单实例、重启后 offset 不丢
- [ ] `LoadCredentialEncrypted` 注入密钥；`env -i` 的宿主执行看不到密钥
- [ ] Telegram 走代理可用
- [ ] 时区 / `[app] utc_offset` 与系统一致
- [ ] 中文与表格渲染（CJK 双宽对齐、`<pre>` 块）

## 5. 技能

- [ ] `publish_skill` 在板上 dry-run（examples 跑通才落盘）
- [ ] 安装后**无需重启**即可被模型调用
- [ ] `rollback_skill` 回到上一版
- [ ] `network: true` 的技能真的有网；同一脚本在离线 lane 里失败
- [ ] 导入外部技能：`playbook(import-skill)` → 改写 → 安装
- [ ] 打印技能：`lpstat` 查状态、`ipptool` 取 `marker-levels`、`lp -H hold` 后 `cancel` 不浪费纸

## 6. 定时任务

- [x] 建一条 `weekdays 09:00` 静态提醒，重启后仍在
      （**2026-10-27 实测**：建好后立即 `systemctl restart chuan`，到点自行投递）
- [x] 错过 1 小时内补跑一次（**2026-10-27 实测**：stop 后过了触发点再 start，立刻补投）
- [ ] ~~关机错过 3 小时不补跑~~（**按设计放弃验证**：>2h 的补跑窗口只用于防"断电一周后
      开机被过期提醒刷屏"，家里长时间断电不是常态；代码路径与 2h 内的补跑共用，风险低）
- [x] pinned（跑技能）任务能投递，且**不经过模型**
      （**2026-10-27 实测**：16:30:12 投递，`usage_rollup` 该时刻**无任何记录**；
      输出为技能 stdout 原样排版、整秒投递、无「第 N 轮」状态行）
- [x] `skill + polish` 任务：脚本出数据、模型出格式
      （**2026-10-27 实测**：16:45:23 有 `glm-5.3-flash` 用量记录 11980/248，
      输出是模型改写的小结——与 pinned 那条形成对照，证明两条路径确实不同）
- [ ] 审批卡上显示解析后的触发时间

## 7. 记忆、文件与恢复

- [ ] `remember` → 重启 → 注入命中；facts > 30 时 FTS 检索生效
- [ ] `keep` → `files list` → `files export` → `send_file` 全链路
- [ ] `/ws` 清理按 7 天 / 256MB 生效（`touch -d` 造旧文件）
- [ ] `kill -9` 演练：重启后不重复副作用，遗留 run 补记 `interrupted`
- [ ] 备份 / 恢复到新库

## 8. 服务与视觉

- [ ] MCP 服务（`kind = "mcp"`）握手成功，`tools/list` 数量与远端一致
- [ ] `mcp(server, tool, args)` 真实调用一次（读操作）
- [x] `service(action="add")` 运行期加服务，**不重启**即可用；配置落到 `services.d/`
      （**2026-10-27 实测**：dida MCP 通过 `service add` 握手成功并写入片段；
      **2026-10-28 补**：`services.d/*.toml` 启动时会被加载——本机用 `demo` 片段验证过，
      且 `services.d` 必须是 `0770 root:chuan`，否则重启会丢）
- [ ] `service` 拒绝 `secrets/` 之外的 `token_file`
- [ ] 视觉：发一张带文字的截图，模型能读出内容；`[vision] enabled` 关闭时明确说"看不到"

## 9. 性能

- [ ] 首字延迟（目标 < 4s，直连模型）
- [ ] 单轮完整耗时 / `exec` 调用耗时
- [ ] 常驻内存 RSS（`ps_mem` 或 `/proc/<pid>/status`）

---

## 已在开发机验证、板上只需复核环境差异的项

| 项 | 开发机结论 | 板上要看什么 |
|---|---|---|
| 沙盒隔离 | ✅ 白名单挂载、`/home` 不可见、`lo` only | bwrap 0.11 参数兼容 |
| venv 读系统包 | ✅ `include-system-site-packages=true` | Debian `dist-packages` 路径 |
| 联网 lane | ✅ DNS、pip、`ipptool` 均可 | 代理、编译耗时 |
| 视觉 | ✅ 红/蓝图能正确识别 | 模型 provider 是否支持 |
| 定时任务 | ✅ 触发 / 续排 / 重启恢复 | 时区与 RTC |
| 文件库 | ✅ keep/list/export/del | 板载存储容量 |

---

## 实测记录

### 2026-10-27 · 首次上板（riscv64 / Debian 系 / 8GB）

| 项 | 实测值 / 现象 | 结论 |
|---|---|---|
| 安装 | `install.sh` 成功；venv/配置/unit 三处问题见下 | 已修 |
| systemd 启动 | 首次 243/CREDENTIALS（缺 `/etc/chuan/secrets/*`） | 预期，放密钥后正常 |
| venv | `apt install python3-venv` 后创建成功，耗时 21s | 正常 |
| 沙盒冷启动 | `exec ls /home` 工具侧 39ms | 优于阈值 |
| `/home` 隔离 | `No such file or directory` | 通过 |
| 离线 lane 网络 | `curl` DNS 失败（exit=6）；走代理 exit=7 | 通过（无网卡） |
| 联网 lane | `curl -x 127.0.0.1:7890 https://google.com` → HTTP/2 200，4.1s | 通过 |
| 联网 lane DNS | `getent hosts github.com` → 20.205.243.166 | 通过 |

**上板过程中发现并已修的问题**（都不在开发机上暴露）：

1. `install.sh` 下载分支缺 `exit` → 装完报 `BIN_SRC: 未绑定的变量`（退出码非零，安装本身已完成）；
2. 示例配置把可选的 `distill_prompt_file` 当默认值，指向不存在的文件 → 启动即退出；
3. `/etc/chuan` 建成 0755 与 unit 的 `ConfigurationDirectoryMode=0750` 不一致；`playbooks_dir` 目录 daemon 无权创建；
4. **`ProtectKernelTunables=yes` 让 bwrap 挂不了 `/proc`** → daemon 正常启动、沙盒与联网 lane 全部失效；
5. 离线 `exec` 曾提供 `proxy` 参数并注入代理环境变量，实际必然失败（无网卡）→ 已从离线 lane 移除；
6. venv 可用性只判断 `bin/python3` 存在，`ensurepip` 缺失留下的半成品会被永久沿用。

### 待实现 / 已知缺口

- ⚠️ **per-sandbox cgroup 限制未实现**：不变量 8 与 `docs/DESIGN.md` 写的是"cgroup 内存与 pids 硬限"，
  实际代码只有 systemd `MemoryMax=1G` 兜底 + 沙盒 `/tmp` 256MB 上限。缺单次 `exec` 的
  `memory.max` / `pids.max`——一个 fork 炸弹仍能在 1G 内把服务拖垮。实现方式待定
  （per-exec `systemd-run --scope` 需要 user manager；或直接写 cgroup v2）。
- ⚠️ **`web_fetch` 遇 Cloudflare 挑战**：403 + `cf-mitigated: challenge` 时模型无从判断，
  会改用 `lan_exec` + `r.jina.ai` 兜底。可考虑在 403 且带该响应头时给一句"该站有反爬，可换源"。
