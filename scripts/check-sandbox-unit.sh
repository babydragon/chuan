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

# ---------------------------------------------------------------------------
# 第二条：授权表里标了 rw 的路径，在"这套 unit 加固 + bwrap"下**真的能写**。
#
# 为什么不能只 grep：`ProtectSystem=strict` 会把整棵树挂成只读，`ReadWritePaths` 是唯一的
# 放开方式；而 bwrap 的 `--bind` 只继承源的 `MS_RDONLY`，user namespace 里也清不掉。
# 结果是"fstab 没错、挂载是 rw、`mode = "rw"` 也写了，但就是写不进去"——
# 2026-11-16 板上为此绕了一大圈（先怀疑 NAS，又怀疑 bwrap）。这里真写一次，提前暴露。
#
# 用法：bash scripts/check-sandbox-unit.sh [chuan.toml]（不给参数就试 /etc/chuan/chuan.toml）
# ---------------------------------------------------------------------------
CFG="${1:-/etc/chuan/chuan.toml}"
if [[ ! -f $CFG ]]; then
  echo "skip: 没有 $CFG，跳过 rw 路径的可写探测"
  exit 0
fi
svc_user=$(awk -F= '/^\[Service\]/ { s = 1; next } /^\[/ { s = 0 } s && /^User=/ { print $2 }' "$UNIT" | head -1)
if [[ -z ${svc_user:-} ]] || ! id -u "$svc_user" >/dev/null 2>&1; then
  echo "skip: unit 的服务用户（${svc_user:-未声明}）在本机不存在，跳过 rw 可写探测"
  exit 0
fi
mapfile -t RW_PATHS < <(awk '!/^[[:space:]]*#/ && /mode[[:space:]]*=[[:space:]]*"rw"/ { split($0, a, "\""); print a[2] }' "$CFG")
# unit 里真实的 ReadWritePaths（`-` 前缀去掉）：用来判断这条路径**现在**有没有被放开。
# 注意不能"测的时候顺手把 ReadWritePaths 加上"——那样永远通过，等于没测。
mapfile -t UNIT_RW < <(awk -F= '/^\[Service\]/ { s = 1; next } /^\[/ { s = 0 }
                               s && /^ReadWritePaths=/ { sub(/^ReadWritePaths=/, ""); print }' "$UNIT" \
                         | tr ' ' '\n' | sed 's/^-//' | grep -v '^$')
probed=0
for p in "${RW_PATHS[@]}"; do
  [[ -z $p ]] && continue
  if [[ ! -e $p ]]; then
    echo "skip: $p 不存在（未挂载？）"
    continue
  fi
  covered=""
  for r in "${UNIT_RW[@]}"; do
    if [[ $p == "$r" || $p == "$r"/* ]]; then covered=$r; break; fi
  done
  args=("${PROPS[@]}" --property="User=$svc_user")
  [[ -n $covered ]] && args+=(--property="ReadWritePaths=$covered")
  # 同一套加固 + unit 里那条（若有）放开，再让 bwrap --bind 一次：写不进去就说明配置对不上
  out=$(sudo -n systemd-run --quiet --wait --pipe --collect --unit=chuan-rw-check \
          "${args[@]}" \
          bwrap --unshare-all --ro-bind /usr /usr --ro-bind /lib /lib \
                --ro-bind /lib64 /lib64 --ro-bind-try /bin /bin \
                --proc /proc --dev /dev \
                --bind "$p" "$p" \
                /bin/sh -c "touch '$p/.chuan-rw-probe' && rm -f '$p/.chuan-rw-probe'" 2>&1) || {
    echo "FAIL: $p 在授权表里是 rw，但这套 unit 加固下写不进去" >&2
    echo "      输出：$out" >&2
    if [[ -z $covered ]]; then
      echo "      原因：unit 的 ReadWritePaths 没有覆盖它（ProtectSystem=strict 下就是只读）" >&2
      echo "      修法：加一行 ReadWritePaths=-$p（网络挂载再加 RequiresMountsFor=$p）" >&2
    else
      echo "      原因：unit 已放开 $covered，但这条路径仍然只读——检查挂载本身是否以 ro 挂上" >&2
    fi
    exit 1
  }
  probed=$((probed + 1))
done
echo "rw 路径可写探测通过（$probed 条）"
