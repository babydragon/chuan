#!/usr/bin/env bash
# chuan.service 的加固项会不会把 bwrap 打死 —— 真实跑一次，不是 grep。
#
# 背景：ProtectKernelTunables=yes（以及 ProtectKernelLogs=yes）会让 systemd 先动
# 服务的 /proc，之后 bwrap 再挂一个全新的 procfs 就被内核拒绝：
#   bwrap: Can't mount proc on /proc: Operation not permitted
# 结果：沙盒与联网 lane 全部失效，但 daemon 本身照常启动，只有真正跑命令时才暴露。
#
# 做法：从 deploy/chuan.service 里抽出 [Service] 的加固项，用 systemd-run 起一个
# 临时服务跑 bwrap，断言成功。没有 systemd-run / 不是 root 时跳过（本地开发机）。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNIT="$REPO_ROOT/deploy/chuan.service"

skip() { echo "skip: $*"; exit 0; }

[[ -f "$UNIT" ]] || { echo "找不到 $UNIT" >&2; exit 1; }
command -v bwrap >/dev/null || skip "本机没有 bwrap"
command -v systemd-run >/dev/null || skip "没有 systemd-run"
sudo -n true 2>/dev/null || skip "需要免密 sudo 才能起临时 systemd 服务"

# 只取会改变 namespace/权限的项；User/Group/Environment 之类跳过
mapfile -t PROPS < <(
  awk -F= '
    /^\[Service\]/ { insvc = 1; next }
    /^\[/          { insvc = 0 }
    insvc && /^(NoNewPrivileges|PrivateTmp|PrivateDevices|ProtectSystem|ProtectHome|ProtectKernelTunables|ProtectKernelModules|ProtectKernelLogs|ProtectClock|ProtectHostname|ProtectProc|ProcSubset|ProtectControlGroups|RestrictSUIDSGID|RestrictNamespaces|RestrictRealtime|LockPersonality|MemoryDenyWriteExecute|RemoveIPC|PrivateUsers)=/ {
      printf "--property=%s=%s\n", $1, $2
    }
  ' "$UNIT"
)

[[ ${#PROPS[@]} -gt 0 ]] || skip "unit 里没有可测的加固项"

out=$(sudo -n systemd-run --quiet --wait --pipe --collect --unit=chuan-sandbox-check \
        "${PROPS[@]}" --property=User=nobody \
        bwrap --unshare-all --ro-bind /usr /usr --ro-bind /lib /lib \
              --ro-bind /lib64 /lib64 --proc /proc --dev /dev /usr/bin/echo ok 2>&1) || {
  echo "FAIL: unit 的加固项让 bwrap 无法在沙盒里挂 /proc" >&2
  echo "      输出：$out" >&2
  echo "      最常见的原因：ProtectKernelTunables=yes / ProtectKernelLogs=yes（见本脚本头部注释）" >&2
  exit 1
}
[[ "$out" == *ok* ]] || { echo "FAIL: bwrap 输出异常：$out" >&2; exit 1; }

echo "unit 加固项与 bwrap 兼容（${#PROPS[@]} 项）"
