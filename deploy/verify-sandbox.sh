#!/usr/bin/env bash
# 验证目标机沙盒能力（修正版）
# 用法： bash verify-sandbox.sh
# 只读检查 + /tmp 临时目录；不修改系统

set -u
HOST_WS="$(mktemp -d /tmp/sbx-verify.XXXXXX)"
trap 'rm -rf "$HOST_WS"' EXIT

ok()   { printf '  \033[32m[ OK ]\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m[FAIL]\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m[WARN]\033[0m %s\n' "$*"; }
info() { printf '         %s\n' "$*"; }
sec()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

BWRAP_OK=0; CGROUP_OK=0

##############################################################################
sec "0. 基本环境"
info "kernel : $(uname -r)   arch: $(uname -m)"
info "distro : $(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME")"
info "libc   : $(ldd --version 2>/dev/null | head -1)"
info "systemd: $(systemctl --version 2>/dev/null | head -1)"
[ "$(uname -m)" = "riscv64" ] && ok "目标架构" || warn "非 riscv64（仅预演）"

##############################################################################
sec "1. user namespace"
r=1
[ -r /proc/sys/kernel/unprivileged_userns_clone ] && { v=$(cat /proc/sys/kernel/unprivileged_userns_clone); [ "$v" = 1 ] && ok "unprivileged_userns_clone=1" || { bad "unprivileged_userns_clone=0"; r=0; }; }
[ -r /proc/sys/kernel/apparmor_restrict_unprivileged_userns ] && { v=$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns); [ "$v" = 0 ] && ok "apparmor_restrict_unprivileged_userns=0" || { bad "apparmor_restrict_unprivileged_userns=1"; r=0; }; }
[ "$r" = 1 ] && ok "无 sysctl 级 userns 限制（max_user_namespaces=$(cat /proc/sys/user/max_user_namespaces 2>/dev/null)）"

##############################################################################
sec "2. bubblewrap"
if ! command -v bwrap >/dev/null 2>&1; then
  bad "未安装"; info "sudo apt install -y bubblewrap"; exit 0
fi
ok "bwrap $(bwrap --version 2>&1 | awk '{print $2}')  $(ls -l "$(command -v bwrap)" | awk '{print $1}')"

##############################################################################
sec "3. 冒烟：只读根"
bwrap --ro-bind / / --dev /dev --proc /proc true 2>&1 && ok "可启动" || bad "无法启动"

##############################################################################
sec "4. 全隔离 + tmpfs 挂载点（修正版）"
# 关键：--ro-bind / / 之后根是只读的，挂载点必须建在 tmpfs 内
if bwrap --unshare-all --die-with-parent --ro-bind / / --proc /proc --dev /dev \
        --tmpfs /tmp --bind "$HOST_WS" /tmp/ws --chdir /tmp/ws --clearenv \
        --setenv PATH /usr/local/bin:/usr/bin:/bin \
        sh -c 'echo started' 2>&1; then
  ok "--unshare-all 可用（user/pid/net/ipc/uts/cgroup）"
  BWRAP_OK=1
else
  bad "--unshare-all 失败（上面是原因）"
fi

##############################################################################
sec "5. 生产挂载集（最小可见面）"
# 真实使用时不能 ro-bind /，否则 /home、/var 全部对沙盒可见
MNT=()
MNT+=(--ro-bind /usr /usr)
for d in /lib /lib64 /bin /sbin /etc; do
  if [ -L "$d" ]; then MNT+=(--symlink "$(readlink "$d")" "$d")
  elif [ -d "$d" ]; then MNT+=(--ro-bind "$d" "$d"); fi
done
if bwrap --unshare-all --die-with-parent "${MNT[@]}" --proc /proc --dev /dev \
        --tmpfs /tmp --dir /ws --bind "$HOST_WS" /ws --chdir /ws \
        sh -c 'echo started' 2>"$HOST_WS/e5"; then
  ok "白名单挂载集可用"

  out=$(bwrap --unshare-all --die-with-parent "${MNT[@]}" --proc /proc --dev /dev \
        --tmpfs /tmp --dir /ws --bind "$HOST_WS" /ws --chdir /ws \
        sh -c 'ls /home 2>/dev/null | head -3; test -d /home && echo HOME_VISIBLE || echo HOME_HIDDEN' 2>/dev/null)
  echo "$out" | grep -q HOME_HIDDEN && ok "/home 对沙盒不可见（密钥与个人文件不暴露）" || { bad "/home 仍可见：$out"; info "生产挂载集必须显式白名单，不能 ro-bind / /"; }

  bwrap --unshare-all --die-with-parent "${MNT[@]}" --proc /proc --dev /dev \
        --tmpfs /tmp --dir /ws --bind "$HOST_WS" /ws --chdir /ws \
        sh -c 'touch /etc/should-not-exist 2>/dev/null && echo ETC_WRITABLE || echo ETC_RO' 2>/dev/null \
        | grep -q ETC_RO && ok "/etc 只读" || bad "/etc 可写"

  bwrap --unshare-all --die-with-parent "${MNT[@]}" --proc /proc --dev /dev \
        --tmpfs /tmp --dir /ws --bind "$HOST_WS" /ws --chdir /ws \
        sh -c 'touch /ws/ok && echo WS_OK' 2>/dev/null | grep -q WS_OK \
        && ok "workspace 可写" || bad "workspace 不可写"

  ifs=$(bwrap --unshare-all --die-with-parent "${MNT[@]}" --proc /proc --dev /dev \
        --tmpfs /tmp -- sh -c 'tail -n +3 /proc/net/dev 2>/dev/null | cut -d: -f1 | tr -d " \t" | sort | tr "\n" " "' 2>/dev/null)
  ifs=${ifs:-?}
  if [ "$ifs" = "lo " ] || [ "$ifs" = "lo" ]; then ok "只有 lo（netns 内正常，无外部连通）"
  else warn "非预期接口：$ifs"; fi

  bwrap --unshare-all --die-with-parent "${MNT[@]}" --proc /proc --dev /dev \
        --tmpfs /tmp -- sh -c 'command -v curl >/dev/null && curl -s --max-time 3 https://example.com >/dev/null && exit 0 || exit 1' 2>/dev/null \
        && bad "沙盒内可访问外网" || ok "沙盒内无法访问外网"

  out=$(bwrap --unshare-all --die-with-parent "${MNT[@]}" --proc /proc --dev /dev \
        --tmpfs /tmp --clearenv --setenv PATH /usr/bin:/bin -- sh -c 'env | wc -l' 2>/dev/null | tr -d ' ')
  [ "${out:-9}" = "1" ] && ok "环境变量已清空（仅 PATH）" || info "沙盒内环境变量数：${out:-?}（--clearenv 后再 --setenv PATH 属正常）"
else
  bad "白名单挂载集失败：$(head -3 "$HOST_WS/e5")"
fi

##############################################################################
sec "6. cgroup v2 与内存硬限"
if [ "$(stat -fc %T /sys/fs/cgroup 2>/dev/null)" = "cgroup2fs" ]; then
  ok "cgroup v2 unified"
  info "controllers: $(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null)"
  if systemctl --user show-environment >/dev/null 2>&1; then
    ok "user systemd manager 可用"
    if command -v python3 >/dev/null 2>&1; then
      systemd-run --user --scope --quiet -p MemoryMax=200M -p MemorySwapMax=0 \
        python3 -c 'a = bytearray(600*1024*1024); print("ALLOCATED")' >"$HOST_WS/m.out" 2>"$HOST_WS/m.err"
      rc=$?
      if grep -q ALLOCATED "$HOST_WS/m.out" 2>/dev/null; then
        bad "600MB 分配成功 → 内存限制未生效"
      elif [ $rc -ne 0 ]; then
        ok "600MB 分配被杀死（rc=$rc）→ 内存硬限有效"
        CGROUP_OK=1
      else
        warn "结果不明确"
      fi
    else
      warn "无 python3，跳过"
    fi
  else
    warn "user manager 不可用（需 loginctl enable-linger）"
  fi
else
  bad "非 cgroup v2"
fi

##############################################################################
sec "7. 降级路径（bwrap 不可用时才用）"
info "LSM: $(cat /sys/kernel/security/lsm 2>/dev/null || echo '需 root 读取')"
info "kernel $(uname -r) 的 Landlock 状态见上；内核 ≥5.13 即有基础支持"
info "seccomp: $(grep -c seccomp /proc/self/status 2>/dev/null) 项"

##############################################################################
sec "结论"
[ "$BWRAP_OK" = 1 ]  && ok "沙盒：可用 → 采用「能力越界才审批」的宽松策略" || bad "沙盒：不可用 → 转 systemd 沙箱或收紧策略"
[ "$CGROUP_OK" = 1 ] && ok "资源限制：cgroup 内存硬限已验证生效" || warn "资源限制：未能验证 → 需备用方案"
echo
echo "如第 5 节有 FAIL，请把该节输出一并贴回。"
