# 目标板端到端验证清单（riscv64）

> 开发与联调都在 x86_64 开发机上完成；这份清单只记录**只能在目标板确认**的项。
> 每项做完改成 `[x]`，并写下实测结论（版本 / 耗时 / 报错原文）。
> 原则：**实测优先于推断**；没测过的一律留空，不写"应该可以"。
>
> **口径**：本文件是板上状态的唯一权威。`README.md` / `ROADMAP.md` 里的概览若与此冲突，
> 以本文件为准——概览只允许写这里已勾的结论。

参考硬件：RISC-V SBC（riscv64，8GB 内存，Debian 系发行版，systemd）。
数值阈值按这块板定：常驻内存目标 <150MB，沙盒冷启动 <60ms。

---

## 0. 编译与依赖

- [ ] `cargo zigbuild --target riscv64gc-unknown-linux-gnu --release` 通过（含 rusqlite bundled / rustls）
- [x] `bwrap --version` ≥ 0.11，且两组参数都被接受（**2026-10-27 实测**：离线 `exec` 与联网 `lan_exec` 均可执行，两组参数都被接受）：
      离线：`--unshare-all`
      联网：`--unshare-user --unshare-ipc --unshare-pid --unshare-uts --unshare-cgroup-try`
      （⚠️ 0.11 若没有 `--unshare-cgroup-try`，联网 lane 要换写法）
- [ ] cgroup v2 unified，`memory` / `pids` 控制器可写（systemd `Delegate=yes`）——**细则见 §10，触发式暂缓**
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
      自 rc.4 起联网 lane 自动挂载，不再需要写配置。注意队列名 `B7520DW` 不是主机名，
      `lpstat -v` 给的是 `ipp://BRW4CD577B1850A.local:631/ipp/print`）
- [ ] `http://192.168.x.x` 被 `web_fetch` 拒绝并留痕

- [x] 沙盒里读不到凭据：`/etc/chuan/secrets` 在沙盒内是空的只读 tmpfs
      （**2026-10-28 实测**：`ls` 为空、`cat` 报文件不存在、`touch` 报只读；
      宿主上仍是 0750 root:chuan + 各文件按用途 0400/0440）

## 3. 出网与审计

- [x] `web_fetch` 出网与 TLS 通（**2026-10-27 实测**：对 `openai.com` 拿到 403 响应，
      证明 DNS/TLS/代理链路可用）
- [ ] `web_fetch` 真正取到页面正文（同上一条的 403 来自源站 Cloudflare：
      `server: cloudflare` + `cf-mitigated: challenge`，换浏览器 UA 同样 403；未换源验证）
- [ ] 大响应（>12KB JSON）变成"结构 + 预览"，全文落 `/ws/.tool-results/`
- [ ] `net-access.jsonl` 落盘、权限 `600`、达到上限后轮转
      （**2026-10-27 部分**：`tail` 能读到 403 那条记录，字段齐全；权限与轮转未验）
- [ ] 拒绝内网的记录也写进日志

## 4. 通道与部署

- [x] systemd 启动（**2026-10-27 实测**：`enable --now`、多次 `restart` 均正常；
      rc.4 之前还验过 243/CREDENTIALS 的失败路径）
- [ ] `flock` 单实例（未验：并发启动第二个实例应被拒；装脚本后没做过这个实验）
- [ ] 重启后 offset 不丢（**部分**：多次重启后未出现重复回复，但没做"发消息→立刻重启"的对照）
- [ ] `LoadCredentialEncrypted` 注入密钥（未验；出厂 unit 用的是明文 `LoadCredential=`
      + `0400 root:root`，加密路径需要自己改 unit 才启用）
- [ ] `env -i` 的宿主执行看不到密钥（未验；`host`/`host_elevated` 尚未实现，见 §12）
- [ ] Telegram 走代理可用（**部分**：收发一直正常；但没单独验证"是否经过代理"——
      `[proxy] url` 已配置，Telegram 也可能直连可达，两者无法区分）
- [x] 时区 / `[app] utc_offset` 与系统一致（**2026-10-27 实测**：16:30 / 16:45 定时任务
      按本地时间准点投递；09:00 的一次性提醒在停服后 09:09 补跑，说明按 +08:00 计算）
- [x] 中文与表格渲染（**2026-10-27/28 实测**：整轮对话与工具轨迹的中文、列表、
      代码块均正常；未单独构造 CJK 双宽对齐的边界用例）

## 5. 技能

- [x] `publish_skill` 在板上 dry-run（**2026-10-27 实测**：`machine-info` 技能发布成功，
      examples 通过后才落盘）
- [x] 安装后**无需重启**即可被模型调用（同上：发布后直接被定时任务调用，中间没有重启）
- [ ] `rollback_skill` 回到上一版
- [ ] `network: true` 的技能真的有网；同一脚本在离线 lane 里失败
- [ ] 导入外部技能：`playbook(import-skill)` → 改写 → 安装
- [ ] 打印技能：`ipptool` 取 `marker-levels`、`lp -H hold` 后 `cancel` 不浪费纸
      （**部分**：`lpstat -r/-e/-p -d` 与 mDNS 解析已验证，见 §2；耗材与挂起/取消没做完）

## 6. 定时任务

- [x] 建一条 `weekdays 09:00` 静态提醒，重启后仍在
      （**2026-10-27 实测**：建好后立即 `systemctl restart chuan`，到点自行投递）
- [x] 错过 1 小时内补跑一次（**2026-10-27 实测**：stop 后过了触发点再 start，立刻补投）
- ⏭️ **按设计放弃验证**：关机错过 3 小时不补跑。>2h 的补跑窗口只用于防"断电一周后
  开机被过期提醒刷屏"，家里长时间断电不是常态；这条路径与 2h 内的补跑共用代码。
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

- [x] MCP 服务（`kind = "mcp"`）握手成功（**2026-10-27 实测**：dida 握手通过并注册成功）
- [x] `mcp(server, tool, args)` 真实调用一次（**2026-10-27 实测**：
      `mcp(server="dida", tool="get_user_preference")` 正常返回，鉴权由宿主注入）
- [ ] `tools/list` 数量与远端一致（开发机对 dida 见过 57 个；板上未逐项核对）
- [x] `service(action="add")` 运行期加服务，**不重启**即可用；配置落到 `services.d/`
      （**2026-10-27 实测**：dida MCP 通过 `service add` 握手成功并写入片段；
      **2026-10-28 补**：`services.d/*.toml` 启动时会被加载——本机用 `demo` 片段验证过，
      且 `services.d` 必须是 `0770 root:chuan`，否则重启会丢）
- [ ] `service` 拒绝 `secrets/` 之外的 `token_file`
- [ ] 视觉：发一张带文字的截图，模型能读出内容；`[vision] enabled` 关闭时明确说"看不到"

## 9. 性能

- [ ] 首字延迟（目标 < 4s，直连模型）——未测；需要"发消息到出现第一个字"的端到端计时
- [ ] 单轮完整耗时——未测；现有数据只有工具侧耗时（见下条），不含模型往返
- [x] `exec` 调用耗时 / 沙盒冷启动（**2026-10-27 实测**：`exec ls /home` 工具侧 39ms，
      `exec uname -m` 505ms 含首次启动；开发机约 30ms）
- [x] 常驻内存 RSS（**2026-10-27 实测**：`ps aux` → RSS 7.1MB / VSZ 92MB，
      远低于 <150MB 目标；未做长跑观察，`VmHWM` 峰值待补）

---

## 10. per-exec cgroup 限额（P4 前置 · 触发式暂缓）

**现在不验，也不实现。** 触发条件（满足任一）：

1. 需要在沙盒里**编译或安装带 C 扩展的 Python 模块**（`pip install numpy/scipy/pillow` 之类），
   或跑 `cargo` / `npm` 构建；
2. 出现过一次"某个 `exec` 把服务拖进 OOM"或 TG 在编译期间完全无响应；
3. P4.3 后台任务上线之前（长任务无人盯，必须有硬限）。

粒度为**一次 `exec` 一个 cgroup**（`exec-<call_id>/`），理由：

- 与现有取消边界一致：`sandbox.rs` 的 `setsid()` + `kill(-pid)` + `--unshare-pid` 都是按单次 exec 切的；
- 归因：`memory.events` 的 `oom_kill` 能直接记到对应 `tool.call_finished`；
- 生命周期：exec 结束即可 `cgroup.kill` + `rmdir`，不留记账残渣；
- 子 agent 不需要自己的 cgroup：它的每次 exec 天然各有一组，agent 级并发由 `agent` lane 管。

**验证项**（做的时候逐条勾，记录实测值）：

- [ ] unit 加 `Delegate=yes` 后，daemon 能在 `/sys/fs/cgroup/<service>/` 下建子组并写 `cgroup.subtree_control`
- [ ] `cgroup.controllers` 含 `memory` `pids`；`memory.max` / `pids.max` / `memory.swap.max` 可写
- [ ] `cgroup.kill` 可用（kernel ≥5.14）；不可用时降级为遍历 `cgroup.procs` 发 SIGKILL 并记录降级
- [ ] 单次 exec 建 `exec-<call_id>/`：bwrap 启动后 `cgroup.procs` 有 PID，孙进程也在同一组
      （**顺序**：spawn 前建组 → `sh -c 'echo $$ > <cg>/cgroup.procs; exec bwrap …'`；
      不能 spawn 后再写，`--unshare-pid` 之后 fork 出来的会漏）
- [ ] `memory.max=512M`：`python3 -c "x=bytearray(600*1024*1024)"` 被杀，服务不 OOM，
      该次 `tool.call_finished` 里能看到 `oom_kill`
- [ ] `/tmp` 的 256MB tmpfs **计入本组内存**：写满 `/tmp` 只限住本组，服务 `MemoryMax=1G` 不受影响
- [ ] `pids.max=64`：fork 炸弹被截断，TG 仍能响应（不靠服务级 1G 兜底）
- [ ] 编译 lane 的技能用 768M 跑一次 `pip install`（带 C 扩展）：成功并记录 `memory.peak`
- [ ] 连续 50 次 exec 后 `/sys/fs/cgroup/<service>/` 下无 `exec-*` 残留
- [ ] 取消/超时路径：`cgroup.kill` 后 `cgroup.procs` 为空，**包括 `setsid` 逃逸的孙进程**
      （这是 `killpg` 唯一漏掉的形态）
- [ ] 无 `memory` 控制器时进入降级：`/status` 可见，错误信息明说"无内存限额"，不静默放行

**实现要点（做的时候照这个走）**

1. 服务 cgroup 路径从 `/proc/self/cgroup` 读，不写死 `system.slice`。
2. 组内限额：`memory.max=512M`、`pids.max=64`、`memory.swap.max=0`；编译 lane 单独 768M。
3. `/tmp` tmpfs 与进程内存共用同一额度，512M 里要给那 256M 留位置。
4. 收尾顺序：`cgroup.kill` → `kill_group(pid)` 兜底 → 读 `memory.peak` / `memory.events` 写账本 → `rmdir`。
5. 降级必须**可见**（D5：沙盒降级必须收紧），不允许静默。

---


## 11. 子 agent 与后台任务（P4）

> 逐条**怎么引出场景**（聊天话术 + 观察面 + 失败信号）见 `VERIFY-P4.md`；
> 这一节只保留可直接勾的断言。

**板上验证入口**：Telegram 对话 + 库里的账本（没有 CLI 通道）。
查库：`sqlite3 /var/lib/chuan/chuan.db`（按你的 `[storage] db` 改）。

- [ ] 前台委托：让它"派一个子 agent 读 /ws 下的大文件并给结论"
      → 父回复是一条结论；`select conv_id,count(*) from journal where conv_id like 'sub:%' group by 1;` 有子会话
- [ ] 子 agent 工具集：让它派子 agent 去"发布技能"或"调 http"
      → 子 agent 应立刻收到"已拒绝执行"，**不弹审批卡**（`select name,status,preview from tool_calls where conv_id like 'sub:%';`）
- [ ] 结论截断：让子 agent 写一段很长的结论 → 父上下文只看到 ≤2000 字 + 子会话引用
- [ ] `[待父执行]`：子 agent 停在"需要发布技能"→ 父收到清单并能接着做（审批卡出现）
- [ ] 角色注册：`role add coder …` → 审批卡上能看到**工具、模型、提示词开头**；批准后
      `ls ~/.config/chuan/roles/` 有 `coder.toml` + `coder.md`；**不重启**再用 `role=coder` 能生效
- [ ] 角色热加载：直接改 `roles/coder.md` → 下一次 delegate 就带新提示词（不用重启）
- [ ] 角色越权：把 `publish_skill` 写进角色 tools → 实际工具集里没有它（三层收窄）
- [ ] 后台任务：让它"后台跑、慢慢做" → 立刻返回任务号；`/tasks` 可见；前台静默（默认 5s）后开跑，完成后推结论
- [ ] 父 agent 管任务：问"那个任务怎样了" → 它调 `task(status)` 而不是编；说"停掉它" → 它调 `task(cancel)`
- [ ] 不抢前台：后台任务 pending 时**连续发正文消息** → 任务不启动；只发 `/tasks` 这类指令不影响启动
- [ ] 重启打断：后台任务运行中 `systemctl restart chuan`
      → 收到"被重启打断"；`/tasks retry <id>` 能带上次进展重排（新任务显示"续 task-xxx"）
- [ ] 接手简报：retry 后子 agent 应复用 `/ws` 里已有产物，而不是从头再来（看它的第一轮工具调用）
- [ ] 用量归属：`select agent,sum(prompt_tokens),sum(completion_tokens) from usage_rollup group by agent;`
      → 有 `main` 与 `agent` 两行；`/usage` 出现"其中子 agent 今日 X"
- [ ] token 预算：`daily_tokens` 设一个很小的值 → 下一条消息直接回"额度已用完"，不调模型
- [ ] `nice 19`：pinned 任务或 `install_deps` 运行期间 `ps -o pid,ni,cmd -C python3` → `ni=19`
- [ ] 派活判断（不滥派）：一句能答的问题（"现在几点"）**不应**派出子 agent
      （`select count(*) from journal where kind='tool.call_started' and json_extract(payload,'$.name')='delegate';`）
- [ ] 派活判断（该派就派）：让它读一个 50KB 的文件并总结 → 应派子 agent，而不是把全文读进主上下文
- [ ] 后台任务期间问答：后台任务运行中继续发消息 → 能正常回复（首字可能被子 agent 当前这一轮挡住）
- [ ] 完成后追问：后台任务完成后问"刚才那个任务结果" → 模型能复述（说明 `agent_task_done` 进了账本）

## 12. 宿主执行与授权（P4.6-A 已实现；B/C 未做）

> 设计见 `DECISIONS.md` D22、`INTERFACES.md §6.4`、配置见 `OPS.md`「宿主执行与授权」。
> 这一节提前列出**只能上板验**的项——它们大多不是"功能对不对"，而是"这台机器上到底允许什么"。
>
> **先跑脚本**：`sudo -u chuan -H bash deploy/verify-host-lane.sh`。它把下面大部分项
> （linger、user manager、ro→rw 重挂、宿主 PID 视图、`/sys`、D-Bus、`journalctl`、
> `systemctl --user`、遮蔽、`env -i`）一次性跑完，并给出 PASS/FAIL 的处置建议。

**前置（安装脚本要落实，先手工验一遍）**：

- [ ] `loginctl enable-linger chuan` 后 user manager 在跑：
      `sudo -u chuan XDG_RUNTIME_DIR=/run/user/$(id -u chuan) systemctl --user is-system-running`
- [ ] `journalctl -u chuan -n 5` 作为 chuan 能读（需 `SupplementaryGroups=systemd-journal`）；
      改 unit 后**必须复核 bwrap 仍能挂 `/proc`**（`ProtectKernel*` 那条老坑，见 §2）

**L1（host lane）**：

- [ ] bwrap 里 `/run/user/<uid>` 是**可写**的（`systemctl --user` 的控制 socket 在那）：
      `touch /run/user/<uid>/probe`
      **2026-10-29 结论**：不需要可写。`systemctl --user` 只**连** socket（`srw-rw-rw-`），
      只读挂载即可；unit 文件写 `/var/lib/chuan/.config`。
      踩过的坑：`ReadWritePaths=/run/user/%U` 在 system 单元里 `%U` 恒为 0 → `/run/user/0` 不存在 →
      `226/NAMESPACE` 起不来（板上实测）。真要可写必须写死 uid，且 bwrap 的 `--bind` 不清除
      继承的 `MS_RDONLY`，所以"挂成 rw"在只读父挂载下本来就不成立
- [ ] 看到的是**宿主进程表**：`ps -e | head` 里有 PID 1 与 `journald`（不是空表、不是新 procfs）
- [ ] **遮蔽实测**（2026-10-29 曾有回归：`masks_for` 写了但没接进 `exec_with`，L1 里能 `cat /etc/chuan/chuan.toml`）：
      `ls -la /etc/chuan/secrets` 应为空、`cat /etc/chuan/chuan.toml` 应读出 0 字节
- [ ] `/etc/chuan/secrets` 是空目录、`$CREDENTIALS_DIRECTORY` 不存在：
      `ls -A /etc/chuan/secrets; echo $CREDENTIALS_DIRECTORY`
- [ ] 能连宿主 loopback 的代理：`curl -x http://127.0.0.1:7890 -sI https://example.com`
- [ ] 未授权路径的报错是**结构化**的（`NotGranted` + 精确路径），不是泛泛的 `Permission denied`

**L2（hostd）**：

- [ ] socket 权限 `0660 root:chuan`；`sudo -u nobody` 连接被拒（`SO_PEERCRED` 生效）
- [ ] 授权表**热加载**：改一行后不重启即生效；把 TOML 写坏 → 全部拒绝（fail-closed，不是回退旧版）
- [ ] `apt-get install` 条目：审批卡上是 `-s` 预览，真落后 `dpkg -l <pkg>` 存在
- [ ] hostd 子进程 `env -i`：`/run/credentials/chuan.service` 不可读、环境里无 `*_KEY` / `*_FILE`
- [ ] `host-access.jsonl`：权限 `0640`，每行含 `grant_index` 与展开后的 argv

**L3（`sys`）与自毁防护**：

- [ ] `sys(metric="mem")` 与 `free -m` 数值一致；`sys(pid=…)` 对**别的 uid** 的进程只给公开字段
      （读不到 `smaps_rollup` 时要说清"看不到"，而不是报"进程不存在"）
- [ ] `chuan.service` 的 `restart`：本轮结束后才执行，且先收到一条消息
- [ ] user timer 一分钟实跑一次：journal 有输出，且**不吃** `chuan.service` 的 `MemoryMax=1G`

**2026-10-29 · 首次上板实测（`deploy/verify-host-lane.sh`）**：

| 项 | 结果 |
|---|---|
| 宿主 PID 视图（不 `--unshare-pid`） | ✅ 看到 PID 1 / kthreadd |
| `/sys` 只读挂载 | ✅ `thermal_zone0` 45–46℃ |
| 强制遮蔽 | ✅ tmpfs 后 `secrets` 为空；`/run/credentials` 不可见 |
| `env -i` | ✅ 无 `*_KEY` / `*_TOKEN` 变量 |
| linger / user manager | ✅ `enable-linger` 后 `/run/user/996/systemd/private` 存在 |
| `systemctl --user` 在宿主 lane 里 | ✅ 能连（返回 `offline`，因还没有 user unit） |
| `journalctl` | ⚠️ 未加 `systemd-journal` 组时"insufficient permissions"；加组后正常 |
| ro 父挂载里 bind 成 rw | ❌ bwrap 不清 `MS_RDONLY`；**`systemd-run --property=ReadWritePaths=/run/user/996` 实测 `RW-OK`** → unit 加一行即可，无需动 `ProtectHome` |
| 系统 D-Bus（`systemctl status`）在 bwrap 里 | ✅ **补挂 `/run/systemd/system` 后正常**（此前报 `Host is down` 是因为 `sd_booted()` 找不到该目录；不是 D-Bus 问题） |

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

- ⚠️ **per-exec cgroup 限制未实现**：不变量 8 与 `docs/DESIGN.md` 写的是"cgroup 内存与 pids 硬限"，
  实际代码只有 systemd `MemoryMax=1G` 兜底 + 沙盒 `/tmp` 256MB 上限。缺单次 `exec` 的
  `memory.max` / `pids.max`——一个 fork 炸弹仍能在 1G 内把服务拖垮。
  **粒度已定：一次 `exec`（一次 bwrap 进程树）一个 cgroup**，不是每个子 agent 一个、也不是每个 run 一个
  （理由与做法见 §10）。**当前不实现**：pid namespace + `--die-with-parent` 已能保证"杀得干净"，
  缺的是内存/pids 的**记账与归因**；等真遇到"沙盒里编译/装 Python 模块"这类大内存任务再上。
- ⚠️ **`web_fetch` 遇 Cloudflare 挑战**：403 + `cf-mitigated: challenge` 时模型无从判断，
  会改用 `lan_exec` + `r.jina.ai` 兜底。可考虑在 403 且带该响应头时给一句"该站有反爬，可换源"。

---

## 未验项分组（2026-10-28 复核）

按"为什么还没验"分组（条数以本文勾选为准，这里是归类不是计数）：

| 原因 | 条目 |
|---|---|
| **需要你在板上跑** | `flock` 单实例；offset 对照实验；`env -i` 提权执行；`http://192.168.x.x` 拒绝留痕；大响应落盘；`net-access.jsonl` 权限/轮转；`/py` `/node` 只读；`install_deps(pip)` 与探测拦截；`remember`→重启→命中；`keep`→`send_file` 链路 |
| **需要时间观察** | 首字延迟；单轮完整耗时；`VmHWM` 长跑峰值；`/ws` 清理（要造旧文件） |
| **需要破坏性演练** | `kill -9` 恢复；备份/恢复到新库 |
| **依赖你的设备/场景** | 实际打印与耗材；`network: true` 技能；视觉（当前 `[vision] enabled=false`）；`LoadCredentialEncrypted`（出厂用明文）；Telegram 是否经代理 |
| **代码已测但没在板上单独验** | `cargo zigbuild`（CI 已跑，板上未本地编）；cgroup v2 可写；`node`/`npm`；Debian `dist-packages` 路径；`security: token_file` 校验；`rollback_skill` |
| **触发式暂缓** | per-exec cgroup 全套（§10）：等"沙盒里编译/装带 C 扩展的 Python 模块"或 P4.3 后台任务上线前再做 |
| **明确放弃** | 关机错过 3h 不补跑（见 §6） |
