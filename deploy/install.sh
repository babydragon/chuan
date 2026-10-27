#!/usr/bin/env bash
# chuan（川）安装脚本
#
# 两种用法：
#   1) 从 release 一键装（推荐）：
#        curl -fsSL https://github.com/<owner>/chuan/releases/latest/download/install.sh | sudo bash
#      或下载 tar 包解开后：sudo ./install.sh
#   2) 从仓库目录装（开发用，自动用 target/release/chuan）：
#        sudo deploy/install.sh
#
# 选项：
#   --no-enable   只安装文件，不 systemctl enable/--now
#   --dry-run     只打印要做什么
#
# 环境变量：
#   CHUAN_VERSION   指定版本（默认 latest，可写 v0.1.0）
#   CHUAN_REPO      覆盖仓库（默认 owner/repo 见下）
#   BIN_SRC         直接指定二进制路径
set -euo pipefail

REPO="${CHUAN_REPO:-babydragon/chuan}"
VERSION="${CHUAN_VERSION:-latest}"
DRY_RUN=0
ENABLE=1

for arg in "$@"; do
  case "$arg" in
    --no-enable) ENABLE=0 ;;
    --dry-run)   DRY_RUN=1 ;;
    -h|--help)
      if [[ -r "$0" ]]; then sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      else echo "用法：install.sh [--no-enable] [--dry-run]（CHUAN_VERSION=v0.1.0 指定版本）"
      fi
      exit 0 ;;
    *) echo "未知参数：$arg" >&2; exit 2 ;;
  esac
done

die() { echo "错误：$*" >&2; exit 1; }
say() { echo "==> $*"; }
run() {
  if [[ $DRY_RUN -eq 1 ]]; then echo "  + $*"; else "$@"; fi
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" >/dev/null 2>&1 && pwd || echo .)"

# ---------- 找资产目录（release 解包 = 与本脚本同级；仓库 = deploy/） ----------
asset() {
  for d in "$SCRIPT_DIR" "$SCRIPT_DIR/deploy"; do
    [[ -f "$d/$1" ]] && { echo "$d/$1"; return 0; }
  done
  return 1
}

# ---------- 没有二进制就去下载 release ----------
if [[ -z "${BIN_SRC:-}" || ! -f "${BIN_SRC:-/nonexistent}" ]]; then
  if [[ -f "$SCRIPT_DIR/chuan" ]]; then
    BIN_SRC="$SCRIPT_DIR/chuan"
  elif [[ -f "$SCRIPT_DIR/../target/release/chuan" ]]; then
    BIN_SRC="$SCRIPT_DIR/../target/release/chuan"
  else
    [[ $EUID -eq 0 || $DRY_RUN -eq 1 ]] || die "需要 root"
    case "$(uname -m)" in
      riscv64)          ARCH=riscv64 ;;
      x86_64|amd64)     ARCH=x86_64 ;;
      aarch64|arm64)    ARCH=aarch64 ;;
      *) die "不支持的架构 $(uname -m)（可用 release 资产：riscv64 / x86_64）" ;;
    esac
    if [[ "$VERSION" == "latest" ]]; then
      URL="https://github.com/$REPO/releases/latest/download/chuan-$ARCH-linux-gnu.tar.gz"
    else
      URL="https://github.com/$REPO/releases/download/$VERSION/chuan-$VERSION-$ARCH-linux-gnu.tar.gz"
    fi
    command -v curl >/dev/null || die "需要 curl"
    command -v tar  >/dev/null || die "需要 tar"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    say "下载 $URL"
    FILE="$(basename "$URL")"
    curl -fsSL --retry 3 -o "$TMP/$FILE" "$URL" || die "下载失败（版本不存在或网络不通）"
    if curl -fsSL --retry 3 -o "$TMP/$FILE.sha256" "$URL.sha256" 2>/dev/null; then
      ( cd "$TMP" && sha256sum -c "$FILE.sha256" ) || die "校验失败，包可能损坏"
      say "校验通过"
    else
      echo "（没有 sha256 文件，跳过校验）"
    fi
    mkdir -p "$TMP/pkg"
    tar -xzf "$TMP/$FILE" -C "$TMP/pkg"
    say "解开到 $TMP/pkg"
    ARGS=()
    [[ $DRY_RUN -eq 1 ]] && ARGS+=(--dry-run)
    [[ $ENABLE -eq 0 ]] && ARGS+=(--no-enable)
    exec bash "$TMP/pkg/install.sh" ${ARGS[@]+"${ARGS[@]}"}
  fi
fi

[[ -f "$BIN_SRC" ]] || die "找不到二进制 $BIN_SRC"
[[ $EUID -eq 0 || $DRY_RUN -eq 1 ]] || die "需要 root"

say "安装 $BIN_SRC → /usr/local/bin/chuan"

# 1. 服务专用用户
if ! id -u chuan >/dev/null 2>&1; then
  run useradd --system --home /var/lib/chuan --shell /usr/sbin/nologin chuan
  say "已创建用户 chuan"
fi
if getent group lp >/dev/null 2>&1; then
  run usermod -aG lp chuan
fi

# 2. 二进制与目录
run install -Dm755 "$BIN_SRC" /usr/local/bin/chuan
run install -d -m 0755 /etc/chuan
run install -d -m 0700 -o root -g root /etc/chuan/secrets
run install -d -m 0750 -o chuan -g chuan /var/lib/chuan
run install -d -m 0750 -o chuan -g chuan /var/lib/chuan/backups

# 3. 配置（不覆盖已有）
CFG="$(asset chuan.toml.example)" || die "缺少 chuan.toml.example"
if [[ -f /etc/chuan/chuan.toml ]]; then
  say "/etc/chuan/chuan.toml 已存在，保持不变"
else
  run install -Dm640 -o root -g chuan "$CFG" /etc/chuan/chuan.toml
  say "已生成 /etc/chuan/chuan.toml（按需修改 allowed_users / model / 路径）"
fi

# 4. systemd
for u in chuan.service chuan-backup.service chuan-backup.timer; do
  f="$(asset "$u")" || die "缺少 $u"
  run install -Dm644 "$f" "/etc/systemd/system/$u"
done
run systemctl daemon-reload

cat <<'NEXT'

下一步（未自动执行）：
  1) 放密钥（root 只读）：
       install -m 0400 -o root -g root <token>   /etc/chuan/secrets/telegram_token
       install -m 0400 -o root -g root <api-key> /etc/chuan/secrets/zai_key
     若用别的 provider，改 unit 里的 LoadCredential 名与 CHUAN_KEY_FILE
  2) 编辑 /etc/chuan/chuan.toml（allowed_users、model、路径）
  3) 启用：
       systemctl enable --now chuan.service
       systemctl enable --now chuan-backup.timer
  4) 看日志： journalctl -u chuan -f
NEXT

if [[ $ENABLE -eq 1 ]]; then
  run systemctl enable --now chuan.service
  run systemctl enable --now chuan-backup.timer
  say "已启用 chuan.service 与 chuan-backup.timer"
fi
