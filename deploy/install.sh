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
#   --proxy URL   下载走代理（也认 CHUAN_PROXY / https_proxy / http_proxy）
#
# 环境变量：
#   CHUAN_VERSION   指定版本（默认 latest，可写 v0.1.0）
#   CHUAN_REPO      覆盖仓库（默认 owner/repo 见下）
#   CHUAN_BASE_URL  覆盖下载源（默认 https://github.com，可换镜像）
#   BIN_SRC         直接指定二进制路径
set -euo pipefail

REPO="${CHUAN_REPO:-babydragon/chuan}"
VERSION="${CHUAN_VERSION:-latest}"
DRY_RUN=0
ENABLE=1
PROXY="${CHUAN_PROXY:-${https_proxy:-${http_proxy:-}}}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-enable) ENABLE=0; shift ;;
    --dry-run)   DRY_RUN=1; shift ;;
    --proxy)     [[ $# -ge 2 ]] || { echo "--proxy 需要参数" >&2; exit 2; }
                 PROXY="$2"; shift 2 ;;
    --proxy=*)   PROXY="${1#--proxy=}"; shift ;;
    -h|--help)
      if [[ -r "$0" ]]; then sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
      else echo "用法：install.sh [--no-enable] [--dry-run] [--proxy URL]（CHUAN_VERSION=v0.1.0 指定版本）"
      fi
      exit 0 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
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
    BASE="${CHUAN_BASE_URL:-https://github.com}"
    if [[ "$VERSION" == "latest" ]]; then
      REL="$BASE/$REPO/releases/latest/download"
      URL="$REL/chuan-$ARCH-linux-gnu.tar.gz"
    else
      REL="$BASE/$REPO/releases/download/$VERSION"
      URL="$REL/chuan-$VERSION-$ARCH-linux-gnu.tar.gz"
    fi
    command -v curl >/dev/null || die "需要 curl"
    command -v tar  >/dev/null || die "需要 tar"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    say "下载 $URL"
    FILE="$(basename "$URL")"
    CURL=(curl -fsSL --retry 3)
    [[ -n "$PROXY" ]] && CURL+=(-x "$PROXY")
    "${CURL[@]}" -o "$TMP/$FILE" "$URL" || die "下载失败（版本不存在或网络不通；可用 --proxy 指定代理）"
    # 校验：release 里只有一个 SHA256SUMS，里面既有带版本号的包也有稳定别名
    if "${CURL[@]}" -o "$TMP/SHA256SUMS" "$REL/SHA256SUMS" 2>/dev/null \
       && grep -q " $FILE\$" "$TMP/SHA256SUMS"; then
      ( cd "$TMP" && grep " $FILE\$" SHA256SUMS | sha256sum -c - ) || die "校验失败，包可能损坏"
      say "校验通过（SHA256SUMS）"
    else
      echo "（取不到 SHA256SUMS，跳过校验）"
    fi
    mkdir -p "$TMP/pkg"
    tar -xzf "$TMP/$FILE" -C "$TMP/pkg"
    say "解开到 $TMP/pkg"
    ARGS=()
    [[ $DRY_RUN -eq 1 ]] && ARGS+=(--dry-run)
    [[ $ENABLE -eq 0 ]] && ARGS+=(--no-enable)
    # 内层脚本来自已发布的包，可能是旧版（不认识 --proxy），故只用环境变量透传；
    # 它本身只装本地文件、不联网。用 bash 而不是 exec，好让 trap 清掉临时目录。
    [[ -n "$PROXY" ]] && export CHUAN_PROXY="$PROXY"
    status=0
    bash "$TMP/pkg/install.sh" ${ARGS[@]+"${ARGS[@]}"} || status=$?
    # 必须在这里退出：否则会继续往下走「本地安装」分支，而下载路径没有 BIN_SRC，
    # set -u 会报 "BIN_SRC: 未绑定的变量"（rc.1/rc.2 的真实事故）。
    exit "$status"
  fi
fi

[[ -f "$BIN_SRC" ]] || die "找不到二进制 $BIN_SRC"
[[ $EUID -eq 0 || $DRY_RUN -eq 1 ]] || die "需要 root"

say "安装 $BIN_SRC → /usr/local/bin/chuan"

# 1. 服务专用用户
if ! id -u chuan >/dev/null 2>&1; then
  run useradd --system --home /var/lib/chuan --shell /usr/sbin/nologin chuan
  [[ $DRY_RUN -eq 1 ]] || say "已创建用户 chuan"
fi
if getent group lp >/dev/null 2>&1; then
  run usermod -aG lp chuan
fi

# 2. 二进制与目录
run install -Dm755 "$BIN_SRC" /usr/local/bin/chuan
# /etc/chuan 必须与 unit 的 ConfigurationDirectoryMode 一致（0750），
# 且组为 chuan：否则 daemon 读不到 0640 的配置，systemd 每次启动还会告警。
run install -d -m 0750 -o root -g chuan /etc/chuan
run install -d -m 0700 -o root -g root /etc/chuan/secrets
# 示例配置默认引用 /etc/chuan/playbooks；daemon 无权在 root 目录下创建，先建好
run install -d -m 0750 -o root -g chuan /etc/chuan/playbooks
run install -d -m 0750 -o chuan -g chuan /var/lib/chuan
run install -d -m 0750 -o chuan -g chuan /var/lib/chuan/backups

# 3. 配置（不覆盖已有）
CFG="$(asset chuan.toml.example)" || die "缺少 chuan.toml.example"
if [[ -f /etc/chuan/chuan.toml ]]; then
  say "/etc/chuan/chuan.toml 已存在，保持不变"
else
  run install -Dm640 -o root -g chuan "$CFG" /etc/chuan/chuan.toml
  [[ $DRY_RUN -eq 1 ]] || say "已生成 /etc/chuan/chuan.toml（按需修改 allowed_users / model / 路径）"
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

代理说明：
  - 下载/安装阶段：--proxy 或 CHUAN_PROXY / https_proxy
  - 运行阶段：写在 /etc/chuan/chuan.toml 的 [proxy] url（Telegram、web_fetch、
    沙盒脚本、装包都用它，不需要在 systemd 里再加 Environment）
NEXT

if [[ $ENABLE -eq 1 ]]; then
  run systemctl enable --now chuan.service
  run systemctl enable --now chuan-backup.timer
  say "已启用 chuan.service 与 chuan-backup.timer"
fi
