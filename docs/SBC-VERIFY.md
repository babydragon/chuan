# 目标板端到端验证清单（riscv64）

> 开发与联调都在 x86_64 开发机上完成；这份清单只记录**只能在目标板确认**的项。
> 每项做完改成 `[x]`，并写下实测结论（版本 / 耗时 / 报错原文）。
> 原则：**实测优先于推断**；没测过的一律留空，不写"应该可以"。

参考硬件：RISC-V SBC（riscv64，8GB 内存，Debian 系发行版，systemd）。
数值阈值按这块板定：常驻内存目标 <150MB，沙盒冷启动 <60ms。

---

## 0. 编译与依赖

- [ ] `cargo zigbuild --target riscv64gc-unknown-linux-gnu --release` 通过（含 rusqlite bundled / rustls）
- [ ] `bwrap --version` ≥ 0.11，且两组参数都被接受：
      离线：`--unshare-all`
      联网：`--unshare-user --unshare-ipc --unshare-pid --unshare-uts --unshare-cgroup-try`
      （⚠️ 0.11 若没有 `--unshare-cgroup-try`，联网 lane 要换写法）
- [ ] cgroup v2 unified，`memory` / `pids` 控制器可写（systemd `Delegate=yes`）
- [ ] `python3 -m venv` 可用（Debian 系需要单独装 `python3-venv`）
- [ ] 沙盒冷启动耗时（开发机约 30ms；板上记录实测值）
- [ ] （可选）`node` / `npm` 在 `/usr/bin` 下；否则 `install_deps(npm)` 会明确报"找不到"

## 1. Python 运行时

- [ ] 首次启动创建 venv（`--system-site-packages`）成功，记录耗时
- [ ] **Debian 的 `/usr/lib/python3/dist-packages` 在 venv 的 `sys.path` 里**
      （与开发机路径不同，必须实测）
- [ ] `apt install python3-numpy` 后 `/py/bin/python3 -c "import numpy"` 直接可用
- [ ] `install_deps(pip)` 实际联网安装一次：记录源、耗时、是否需要代理
- [ ] 已可导入的包会被探测拦截（不重复安装）

## 2. 沙盒与隔离

- [ ] `/ws` 可写；`/inbox` `/skills` `/media` 只读；`/home` `/root` `/var` 不可见
- [ ] `/py` `/node` 只读挂载；`PATH` 含 `/py/bin`
- [ ] `/tmp` 是 tmpfs：写入大文件的 OOM 边界（记录触发点）
- [ ] 超时 `killpg`：`sleep 300` 到点被杀，无残留进程
- [ ] 离线 lane 的 netns 里**只有 `lo`**
- [ ] 联网 lane（`lan_exec` / `network: true` 技能）能看到网卡，且 **DNS 可用**
      （`/etc/resolv.conf` 指向 `/run`，已只挂该子目录）
- [ ] `[sandbox] mounts` 挂 `/run/cups` 后，沙盒内 `lpstat -e` 可见打印机（只读）
- [ ] `http://192.168.x.x` 被 `web_fetch` 拒绝并留痕

## 3. 出网与审计

- [ ] `web_fetch` 取公网页面成功（TLS 根证书齐全；需要代理时生效）
- [ ] 大响应（>12KB JSON）变成"结构 + 预览"，全文落 `/ws/.tool-results/`
- [ ] `net-access.jsonl` 落盘、权限 `600`、达到上限后轮转
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

- [ ] 建一条 `weekdays 09:00` 静态提醒，重启后仍在
- [ ] 关机错过 3 小时不补跑、只排下一次；错过 1 小时内补跑一次
- [ ] pinned（跑技能）任务能投递，且**不经过模型**
- [ ] `skill + polish` 任务：脚本出数据、模型出格式
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
- [ ] `service(action="add")` 运行期加服务，**不重启**即可用；配置落到 `services.d/`
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
