#!/usr/bin/env bash
# 验证「宿主执行（P4.6）」在目标机上的可行性——只读检查，不改系统
#
# 用法（**以服务账号跑**，结果才有意义）：
#   sudo -u chuan -H bash deploy/verify-host-lane.sh
# 在开发机预演（非 chuan 账号）也能跑，只是 user manager 相关的项会 WARN。
#
# 它回答的是「这台机器上到底允许什么」，对应 SBC-VERIFY §12：
#   1) linger 与 user manager 起没起
#   2) bwrap 能不能把「只读父挂载」里的子目录重挂成可写（/run/user 的关键）
#   3) 不换 pid namespace 时能不能看到宿主进程
#   4) /sys 能不能只读挂进来
#   5) 只挂一个 dbus socket 时 systemctl status 能不能用
#   6) 只挂 /var/log/journal 时 journalctl 能不能读
#   7) systemctl --user 的最小挂载集
#   8) 强制遮蔽（secrets / credentials）是不是真的看不见
#   9) env -i 的宿主执行里有没有密钥

set -u
T="$(mktemp -d /tmp/host-lane.XXXXXX)"
trap 'rm -rf "$T"' EXIT

ok()   { printf '  \033[32m[ OK ]\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m[FAIL]\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m[WARN]\033[0m %s\n' "$*"; }
info() { printf '         %s\n' "$*"; }
sec()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

UID_NOW="$(id -u)"
ME="$(id -un)"
BWRAP_OK=0
FAILED=()

# 生产形态的底座：只读 /usr /etc，宿主 /proc 与 /sys，最小 /dev，无 net 隔离
# （与 INTERFACES §6.4 的 L1 一致：--unshare-user/ipc/uts/cgroup，不 unshare pid/net）
bwrap_host() {
  local args=(
    --die-with-parent --new-session --cap-drop ALL
    --unshare-user --unshare-ipc --unshare-uts --unshare-cgroup-try
    --ro-bind /usr /usr
    --ro-bind /etc /etc
    --ro-bind /proc /proc
    --ro-bind /sys /sys
    --dev /dev
    --size 268435456 --tmpfs /tmp
  )
  # 合并 usr 的系统上 /lib /bin 等是符号链接，不能直接 ro-bind
  local d link
  for d in /lib /lib64 /bin /sbin; do
    [ -e "$d" ] || continue
    if [ -L "$d" ]; then
      link="$(readlink "$d")"
      args+=(--symlink "$link" "$d")
    else
      args+=(--ro-bind "$d" "$d")
    fi
  done
  bwrap "${args[@]}" "$@"
}

##############################################################################
sec "0. 基本环境"
info "kernel : $(uname -r)   arch: $(uname -m)"
info "distro : $(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME")"
info "systemd: $(systemctl --version 2>/dev/null | head -1)"
info "PID 1  : $(ps -p 1 -o comm= 2>/dev/null)（非 systemd 时 D-Bus/unit 相关项不可信）"
info "账号   : $ME (uid=$UID_NOW)$( [ "$ME" = chuan ] && echo '  ← 服务账号，结果可直接采用' || echo '  ← 非 chuan，user manager 项仅供参考' )"
if ! command -v bwrap >/dev/null 2>&1; then
  bad "未安装 bubblewrap"; exit 0
fi
ok "bwrap $(bwrap --version 2>&1 | awk '{print $2}')"
BWRAP_OK=1

##############################################################################
sec "1. linger 与 user manager"
if command -v loginctl >/dev/null 2>&1; then
  l="$(loginctl show-user "$ME" -p Linger --value 2>/dev/null)"
  [ -z "$l" ] && l="$(loginctl show-user "$ME" 2>/dev/null | sed -n 's/^Linger=//p')"
  [ -f "/var/lib/systemd/linger/$ME" ] && l="yes"
  info "Linger=${l:-<未知>}"
  [ "$l" = "yes" ] && ok "已开 linger" || warn "未开 linger：sudo loginctl enable-linger $ME（然后重跑本脚本）"
else
  warn "没有 loginctl"
fi
info "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-<未设置>}"
if [ -S "/run/user/$UID_NOW/systemd/private" ]; then
  ok "/run/user/$UID_NOW/systemd/private 存在（user manager 在跑）"
else
  warn "/run/user/$UID_NOW/systemd/private 不存在（user manager 没起）"
fi
if systemctl --user is-system-running >/dev/null 2>&1; then
  ok "systemctl --user 可用：$(systemctl --user is-system-running 2>&1 | head -1)"
else
  warn "systemctl --user 不可用：$(systemctl --user is-system-running 2>&1 | head -1)"
fi

##############################################################################
sec "2. 关键项：bwrap 能否把「只读父挂载」重挂为可写"
info "场景：daemon 的 ProtectHome=read-only 让 /run/user 只读；host lane 要把它 bind 成 rw"
mkdir -p "$T/ro/src" "$T/ro/dst"
echo x > "$T/ro/src/f"
cat > "$T/nested.sh" <<'NESTED'
#!/usr/bin/env bash
set -u
src="$1"; dst="$2"
mount --bind "$src" "$dst" || { echo "BIND-FAIL"; exit 4; }
mount -o remount,bind,ro "$dst" || { echo "REMOUNT-RO-FAIL"; exit 5; }
if touch "$dst/should-fail" 2>/dev/null; then echo "RO-NOT-EFFECTIVE"; exit 6; fi
rm -f "$dst/should-fail" 2>/dev/null
if bwrap --bind "$dst" "$dst" -- /bin/sh -c "echo y >> $dst/f" 2>"$3"; then
  echo "RW-OK"
else
  echo "RW-FAIL"
fi
NESTED
chmod +x "$T/nested.sh"
r="$(unshare -rm --propagation private "$T/nested.sh" "$T/ro/src" "$T/ro/dst" "$T/nested.err" 2>&1 | tail -1)"
case "$r" in
  RW-OK)      ok "ro 父挂载里可以 bind 出可写目录 → /run/user 无需额外配置" ;;
  RW-FAIL)    warn "bind 后仍是只读（bwrap 不会清除继承来的 MS_RDONLY）——这是前提，不是缺陷"
              info "2026-10-29 板上已验证：unit 加 ReadWritePaths=/run/user/%U 后 systemd 会先把它重挂成 rw，"
              info "复核命令：sudo systemd-run --wait --pipe --uid=chuan --property=ProtectHome=read-only \\"
              info "           --property=ReadWritePaths=/run/user/\$UID bash -c 'touch /run/user/\$UID/probe && echo RW-OK'" ;;
  RO-NOT-EFFECTIVE) warn "自检没造出只读挂载，本项结论不可信（unshare/mount 受限？）" ;;
  *)          warn "无法判定：$r"; info "$(cat "$T/nested.err" 2>/dev/null | tail -3)" ;;
esac

##############################################################################
sec "3. 宿主 PID 视图（不 unshare-pid）"
out="$(bwrap_host -- /bin/sh -c 'ps -e -o pid=,comm= 2>/dev/null | head -5' 2>&1)"
if printf '%s' "$out" | grep -qE '^ *1 '; then
  ok "看得到 PID 1（宿主进程表）"
  info "$(printf '%s' "$out" | head -3 | tr '\n' '|')"
else
  bad "看不到宿主进程：$out"
  FAILED+=("宿主 PID 视图")
fi

##############################################################################
sec "4. /sys 只读挂载"
if bwrap_host -- /bin/sh -c 'cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null || cat /proc/loadavg' >/dev/null 2>"$T/sys.err"; then
  ok "/sys 可只读挂入"
  t="$(bwrap_host -- /bin/sh -c 'cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null' 2>/dev/null)"
  [ -n "$t" ] && info "thermal_zone0 = ${t}（毫摄氏度）"
else
  bad "/sys 挂载失败：$(tail -2 "$T/sys.err")"
  FAILED+=("/sys ro-bind")
fi

##############################################################################
sec "5. systemctl status（挂 /run/systemd/system + /run/dbus）"
if [ -S /run/dbus/system_bus_socket ]; then
  # 单文件 ro-bind 之后，它在沙盒里还是不是 socket？
  info "宿主 dbus: $(systemctl is-active dbus 2>/dev/null)  socket: $(ls -l /run/dbus/system_bus_socket 2>/dev/null | awk '{print $1,$3,$4}')"
  info "沙盒内 uid: $(bwrap_host -- /bin/sh -c 'id -u' 2>/dev/null)（应与宿主一致；变成 0 说明 userns 映射改变了身份）"
  t="$(bwrap_host --dir /run --dir /run/dbus \
        --ro-bind /run/dbus/system_bus_socket /run/dbus/system_bus_socket \
        -- /bin/sh -c 'test -S /run/dbus/system_bus_socket && echo SOCK || echo NOT-SOCK' 2>/dev/null)"
  [ "$t" = "SOCK" ] && ok "socket 在沙盒里仍是 socket" || warn "单文件 bind 后不是 socket：$t"

  # 对照：宿主上（不套 bwrap）能不能查——用来区分「板子 D-Bus 有问题」与「bwrap 有问题」
  if systemctl status chuan --no-pager -n 0 >/dev/null 2>&1; then
    ok "宿主上（无 bwrap）systemctl status 可用 → 问题在沙盒侧"
    host_ok=1
  else
    warn "宿主上（无 bwrap）也查不了 → 先解决 D-Bus/polkit：$(systemctl status chuan --no-pager -n 0 2>&1 | head -1)"
    host_ok=0
  fi

  # 2026-10-29 板上定位：systemctl 在连 bus 之前先做 sd_booted()，即
  #   access("/run/systemd/system/") —— 只挂 /run/dbus 时它返回假，systemctl 直接
  #   打 "System has not been booted with systemd as init system" 并返回 -EHOSTDOWN（Host is down）。
  # 补挂这个目录即可；它是 unit 符号链接目录，不是控制通道。
  bwrap_host --dir /run --dir /run/systemd \
    --ro-bind /run/systemd/system /run/systemd/system \
    --ro-bind /run/dbus /run/dbus \
    -- systemctl status chuan --no-pager -n 0 >"$T/st.out" 2>"$T/st.err"
  rc=$?
  if [ $rc -eq 0 ] && grep -q chuan "$T/st.out"; then
    ok "沙盒里 systemctl status 可用（需挂 /run/systemd/system + /run/dbus）"
    info "$(head -2 "$T/st.out" | tr '\n' '|')"
  elif grep -q "could not be found" "$T/st.err"; then
    ok "系统总线已通（本机没有 chuan.service，换任意存在的 unit 再验）"
  else
    bad "失败（rc=$rc）：$(head -2 "$T/st.err")"
    if [ "${host_ok:-0}" = 1 ]; then
      info "宿主上可用 → 差异只在沙盒侧：先确认 /run/systemd/system 与 /run/dbus 都挂了"
    fi
    FAILED+=("systemctl status in bwrap")
  fi
else
  warn "没有 /run/dbus/system_bus_socket"
fi

# 5b：绕过 sd-bus 直接 connect —— 用来区分「传输层」与「sd_booted/上层」
if [ -S /run/dbus/system_bus_socket ] && command -v python3 >/dev/null 2>&1; then
  r="$(bwrap_host --dir /run --dir /run/dbus \
        --ro-bind /run/dbus/system_bus_socket /run/dbus/system_bus_socket \
        -- python3 -c 'import socket,sys
s=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect("/run/dbus/system_bus_socket"); print("CONNECT-OK")
except OSError as e:
    print("CONNECT-FAIL errno=%s %s" % (e.errno, e.strerror))' 2>&1 | tail -1)"
  case "$r" in
    CONNECT-OK)   ok "裸 connect 成功 → 问题在 sd-bus/认证/策略层（H2/H3），不是传输" ;;
    CONNECT-FAIL*) warn "裸 connect 就失败：$r"
                   info "errno 111 = 拒绝了：说明挂进来的不是真正在 listen 的那个 socket（H1）" ;;
    *)            info "无法判定：$r" ;;
  esac
fi

##############################################################################
sec "6. journalctl（只挂 /var/log/journal）"
if [ -d /var/log/journal ]; then
  if bwrap_host --dir /var --dir /var/log \
       --ro-bind /var/log/journal /var/log/journal \
       -- journalctl -n 3 --no-pager >"$T/j.out" 2>"$T/j.err"; then
    ok "能读系统日志（$(wc -l <"$T/j.out") 行）"
  else
    bad "读不到：$(tail -2 "$T/j.err")"
    info "处置：unit 里加 SupplementaryGroups=systemd-journal（或 sudo usermod -aG systemd-journal $ME）"
    info "      加组后要重启服务/重新登录才生效，再重跑本节"
    FAILED+=("journalctl")
  fi
else
  warn "没有 /var/log/journal（可能用 volatile 日志）"
fi

##############################################################################
sec "7. systemctl --user 的最小挂载集"
if [ -d "/run/user/$UID_NOW" ]; then
  out="$(bwrap_host --dir /run --dir "/run/user" --bind "/run/user/$UID_NOW" "/run/user/$UID_NOW" \
          --setenv XDG_RUNTIME_DIR "/run/user/$UID_NOW" \
          -- systemctl --user is-system-running 2>&1 | head -1)"
  if printf '%s' "$out" | grep -qE 'running|degraded|starting|maintenance|offline|unknown'; then
    ok "systemctl --user 在宿主 lane 里可用：$out"
  else
    bad "不可用：$out"
    FAILED+=("systemctl --user in bwrap")
  fi
  if bwrap_host --dir /run --dir "/run/user" --bind "/run/user/$UID_NOW" "/run/user/$UID_NOW" \
       -- systemctl --user list-timers --no-pager >/dev/null 2>&1; then
    ok "list-timers 可用"
  else
    warn "list-timers 失败（可能只是还没有 timer）"
  fi
else
  warn "/run/user/$UID_NOW 不存在 —— 先 sudo loginctl enable-linger $ME，重新登录/重启服务后再跑"
  FAILED+=("/run/user 不存在（linger 未开）")
fi

##############################################################################
sec "8. 强制遮蔽是否真的生效"
if [ -d /etc/chuan/secrets ]; then
  if [ -r /etc/chuan/secrets ] && [ -n "$(ls -A /etc/chuan/secrets 2>/dev/null)" ]; then
    warn "宿主上 secrets 对 $ME 可读（符合预期，所以沙盒/宿主 lane 必须遮蔽）"
  fi
  n="$(bwrap_host --tmpfs /etc/chuan/secrets --remount-ro /etc/chuan/secrets \
        -- /bin/sh -c 'ls -A /etc/chuan/secrets | wc -l' 2>&1)"
  [ "$n" = "0" ] && ok "tmpfs 遮蔽后 secrets 为空目录" || { bad "遮蔽无效：$n"; FAILED+=("secrets mask"); }
else
  warn "没有 /etc/chuan/secrets（未安装？）"
fi
if bwrap_host -- /bin/sh -c 'test -e /run/credentials && echo VISIBLE || echo HIDDEN' 2>/dev/null | grep -q HIDDEN; then
  ok "/run/credentials 在宿主 lane 里不可见（不挂 /run）"
else
  bad "/run/credentials 可见——检查是不是把 /run 整块挂进来了"
  FAILED+=("credentials 可见")
fi

##############################################################################
sec "9. env -i 与凭据"
if env -i /bin/sh -c 'env' 2>/dev/null | grep -qiE '(^|_)(KEY|TOKEN|SECRET|PASSWORD)='; then
  bad "env -i 里仍有密钥变量（不该发生）"
  FAILED+=("env -i 泄漏")
else
  ok "env -i 里没有 KEY/TOKEN/SECRET 变量"
fi
if [ -e "/run/credentials/chuan.service" ]; then
  if [ -r "/run/credentials/chuan.service" ]; then
    warn "服务账号能读 \$CREDENTIALS_DIRECTORY（预期）—— 所以 lane 绝不能挂 /run"
  fi
else
  info "/run/credentials/chuan.service 不存在（服务没跑或未用 LoadCredential）"
fi

##############################################################################
sec "结论"
if [ ${#FAILED[@]} -eq 0 ]; then
  ok "全部关键项通过；可以按 D22 的默认路径表落地"
else
  bad "需要处理（${#FAILED[@]} 项）："
  for f in "${FAILED[@]}"; do info "- $f"; done
  info "对应处置写进 SBC-VERIFY §12 的备注，再决定是否调整默认授权表"
fi
printf '\n把上面整段输出贴回来即可。\n'
