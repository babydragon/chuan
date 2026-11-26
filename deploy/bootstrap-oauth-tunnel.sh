#!/usr/bin/env bash
# 一次性创建 OpenTunnel tunnel + 稳定 route（可选，不属于 chuan 硬依赖）。
#
# 在安装时调用：等到证书 ready 后停掉后台服务，只保留 identity/route；
# 真正用 OAuth 时再 `opentunnel serve`，不再证书等待。
#
# 环境变量（install.sh 会传）：
#   CHUAN_USER / CHUAN_GROUP
#   OPENTUNNEL_BIN              opentunnel 可执行文件
#   CHUAN_OAUTH_DIR             状态目录（默认 /var/lib/chuan/oauth/opentunnel）
#   CHUAN_OAUTH_PROFILE         固定 profile（默认 chuan）
#   CHUAN_OAUTH_CALLBACK_PORT   本地回调端口（默认 8765）
set -euo pipefail

SVC_USER="${CHUAN_USER:-chuan}"
SVC_GROUP="${CHUAN_GROUP:-chuan}"
BIN="${OPENTUNNEL_BIN:-/usr/local/lib/chuan/opentunnel/opentunnel}"
DIR="${CHUAN_OAUTH_DIR:-/var/lib/chuan/oauth/opentunnel}"
PROFILE="${CHUAN_OAUTH_PROFILE:-chuan}"
PORT="${CHUAN_OAUTH_CALLBACK_PORT:-8765}"
HOME_DIR="$(getent passwd "$SVC_USER" | cut -d: -f6)"
HOME_DIR="${HOME_DIR:-/var/lib/chuan}"

die() { echo "错误：$*" >&2; exit 1; }
say() { echo "==> $*"; }

[[ $EUID -eq 0 ]] || die "需要 root"
[[ -x "$BIN" ]] || die "找不到 opentunnel：$BIN（用 --opentunnel-bin 指定）"
command -v runuser >/dev/null || die "需要 runuser（util-linux）"

install -d -m 0750 -o "$SVC_USER" -g "$SVC_GROUP" "$DIR"
for d in data state runtime; do
  install -d -m 0700 -o "$SVC_USER" -g "$SVC_GROUP" "$DIR/$d"
done
# 用 chuan 的标准 XDG 目录：`up` 会把 `serve` 注册成 systemd --user 单元，
# 单元不继承自定义 XDG_*；自定义目录会导致服务找不到 identity。
uid="$(id -u "$SVC_USER")"
runtime="/run/user/$uid"
# install.sh 刚 enable-linger 时 user manager 可能还没起来；等几秒再决定是否回退。
for _ in $(seq 1 50); do
  [[ -d "$runtime" ]] && break
  sleep 0.1
done
if [[ ! -d "$runtime" ]]; then
  runtime="$DIR/runtime"
fi
config="$HOME_DIR/.config"
data="$HOME_DIR/.local/share"
state="$HOME_DIR/.local/state"
install -d -m 0750 -o "$SVC_USER" -g "$SVC_GROUP" "$config" "$data" "$state"
install -d -m 0700 -o "$SVC_USER" -g "$SVC_GROUP" "$config/opentunnel"
cat > "$config/opentunnel/$PROFILE.toml" <<EOF
[routes]
auth = "127.0.0.1:$PORT"
EOF
chown "$SVC_USER:$SVC_GROUP" "$config/opentunnel/$PROFILE.toml"
chmod 0600 "$config/opentunnel/$PROFILE.toml"

identity="$data/opentunnel/$PROFILE/tunnel.json"

# marker 是 helper 判断"能不能自动回调"的唯一依据，任何时候都要和 identity 对齐：
# 删掉 marker 后重跑本脚本必须能恢复，否则 helper 会一直报 needs_bootstrap。
# 不用 python3：目标板不一定装；这里只是给 helper 读的几个字段。
write_marker() {
  local host="$1"
  cat > "$state/opentunnel.json" <<EOF
{
  "hostname": "$host",
  "profile": "$PROFILE",
  "callback": "http://127.0.0.1:$PORT/cb",
  "url": "https://auth.$host/cb",
  "created_at": $(date +%s)
}
EOF
  chown "$SVC_USER:$SVC_GROUP" "$state/opentunnel.json"
  chmod 0600 "$state/opentunnel.json"
}

if [[ -f "$identity" ]]; then
  host="$(sed -n 's/.*"hostname": *"\([^"]*\)".*/\1/p' "$identity" | head -1)"
  [[ -n "$host" ]] || die "tunnel.json 里没有 hostname：$identity"
  write_marker "$host"
  say "已存在 tunnel：https://auth.$host/cb（marker 已对齐）"
  exit 0
fi

as_user() {
  runuser -u "$SVC_USER" -- env \
    HOME="$HOME_DIR" \
    XDG_RUNTIME_DIR="$runtime" \
    "$@"
}

log="$DIR/bootstrap.log"
: > "$log"
chown "$SVC_USER:$SVC_GROUP" "$log"
chmod 0600 "$log"

# up 会创建 tunnel 并等证书。证书可能受服务端瞬时故障影响（见 opencode#54624）：
# 失败一次就丢掉 pending，再试一次；仍失败则保留日志，安装继续。
attempt=1
while (( attempt <= 2 )); do
  say "创建 tunnel / 等待证书（第 $attempt 次，最长 10 分钟）"
  if timeout 600 runuser -u "$SVC_USER" -- env \
       HOME="$HOME_DIR" XDG_RUNTIME_DIR="$runtime" \
       "$BIN" --profile "$PROFILE" up >>"$log" 2>&1; then
    break
  fi
  say "第 $attempt 次未完成，清理 pending 后重试"
  as_user "$BIN" --profile "$PROFILE" down >>"$log" 2>&1 || true
  rm -f "$data/opentunnel/$PROFILE/pending.json" \
        "$data/opentunnel/$PROFILE/private-key.pem" \
        "$data/opentunnel/$PROFILE/token" 2>/dev/null || true
  attempt=$((attempt + 1))
done

if [[ ! -f "$identity" ]]; then
  say "tunnel 未就绪；安装继续。可稍后重跑本脚本，日志：$log"
  exit 1
fi

# 只保留 identity/route，不常驻公网入口
as_user "$BIN" --profile "$PROFILE" down >>"$log" 2>&1 || true
host="$(sed -n 's/.*"hostname": *"\([^"]*\)".*/\1/p' "$identity" | head -1)"
[[ -n "$host" ]] || { say "tunnel.json 里没有 hostname；日志：$log"; exit 1; }

write_marker "$host"
say "tunnel 已就绪：https://auth.$host/cb（serve 尚未启动）"
