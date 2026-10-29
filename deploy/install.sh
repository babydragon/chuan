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
#   CHUAN_USER      服务用户名（默认 chuan）
#   CHUAN_GROUP     服务组名（默认 chuan）
#   CHUAN_ROOT      安装根（默认空=真实系统）；打包/测试时指向临时目录
#
# 幂等：可以反复执行。新装会创建目录，已有安装会**校正**目录与凭据文件权限。
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
      if [[ -r "$0" ]]; then sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'
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

# ---------- 路径与身份 ----------
# CHUAN_ROOT：把安装根指到临时目录（打包/测试用，正常安装留空）
# CHUAN_USER / CHUAN_GROUP：服务用户（默认 chuan）
R="${CHUAN_ROOT:-}"
SVC_USER="${CHUAN_USER:-chuan}"
SVC_GROUP="${CHUAN_GROUP:-chuan}"
BIN="${R}/usr/local/bin/chuan"
ETC="${R}/etc/chuan"
SECRETS="$ETC/secrets"
STATE="${R}/var/lib/chuan"
UNITDIR="${R}/etc/systemd/system"

stat_mode()  { stat -c '%a' "$1" 2>/dev/null; }
stat_owner() { stat -c '%U:%G' "$1" 2>/dev/null; }
# stat 输出 `750`，配置里写的是 `0750`：必须按八进制数值比，否则每次都说"校正"。
same_mode()  { [[ $((8#$1)) -eq $((8#$2)) ]]; }

# 建目录；已存在就校正权限（幂等：可以反复执行，用来修历史安装）
ensure_dir() {
  local p=$1 mode=$2 owner=$3 group=$4 cur_m cur_o
  if [[ -d $p ]]; then
    cur_m=$(stat_mode "$p"); cur_o=$(stat_owner "$p")
    if ! same_mode "$cur_m" "$mode" || [[ $cur_o != "$owner:$group" ]]; then
      if [[ $DRY_RUN -eq 1 ]]; then
        echo "  ~ 校正 $p：$cur_m $cur_o → $mode $owner:$group"
      else
        chown "$owner:$group" "$p" && chmod "$mode" "$p"
        echo "  ~ 校正 $p：$cur_m $cur_o → $mode $owner:$group"
      fi
    fi
  else
    run install -d -m "$mode" -o "$owner" -g "$group" "$p"
  fi
}

ensure_file() {
  local p=$1 mode=$2 owner=$3 group=$4 cur_m cur_o
  [[ -f $p ]] || return 1
  cur_m=$(stat_mode "$p"); cur_o=$(stat_owner "$p")
  if ! same_mode "$cur_m" "$mode" || [[ $cur_o != "$owner:$group" ]]; then
    if [[ $DRY_RUN -eq 1 ]]; then
      echo "  ~ 校正 $p：$cur_m $cur_o → $mode $owner:$group"
    else
      chown "$owner:$group" "$p" && chmod "$mode" "$p"
      echo "  ~ 校正 $p：$cur_m $cur_o → $mode $owner:$group"
    fi
  fi
  return 0
}

# 1. 服务专用用户
if [[ -z $R ]]; then
  if ! id -u "$SVC_USER" >/dev/null 2>&1; then
    run useradd --system --home /var/lib/chuan --shell /usr/sbin/nologin "$SVC_USER"
    [[ $DRY_RUN -eq 1 ]] || say "已创建用户 $SVC_USER"
  fi
  if getent group lp >/dev/null 2>&1; then
    run usermod -aG lp "$SVC_USER"
  fi
fi

# 2. 二进制
say "安装 $BIN_SRC → $BIN"
run install -Dm755 -o root -g root "$BIN_SRC" "$BIN"

# 3. 目录（新装建、旧装校正）
# /etc/chuan 必须与 unit 的 ConfigurationDirectoryMode 一致（0750），组为服务组：
# 否则 daemon 读不到 0640 的配置，systemd 每次启动还会告警。
ensure_dir "$ETC"            0750 root "$SVC_GROUP"
# secrets 目录要让服务用户"进得去"（x 位）；文件权限见第 5 步
ensure_dir "$SECRETS"        0750 root "$SVC_GROUP"
ensure_dir "$ETC/playbooks"  0750 root "$SVC_GROUP"
ensure_dir "$ETC/prompts"    0750 root "$SVC_GROUP"
# 运行期新增的服务写这里；必须让服务用户可写，否则重启就丢
ensure_dir "$ETC/services.d" 0770 root "$SVC_GROUP"
# 角色目录：agent 的 role 工具会往里写，必须让服务用户可写（与 services.d 同理）
ensure_dir "$ETC/roles"      0770 root "$SVC_GROUP"
# 权限申请草稿（capability 工具只写这里，不改配置）
ensure_dir "$ETC/capabilities" 0770 root "$SVC_GROUP"
ensure_dir "$STATE"          0750 "$SVC_USER" "$SVC_GROUP"
ensure_dir "$STATE/backups"  0750 "$SVC_USER" "$SVC_GROUP"
# daemon 运行期自建的目录：存在就顺手校正
# timers：宿主 lane 里 agent 放监控脚本的地方（[host.path] 默认已授权）
ensure_dir "$STATE/timers" 0750 "$SVC_USER" "$SVC_GROUP"
for d in workspaces inbox skills media files rt; do
  [[ -d "$STATE/$d" ]] && ensure_dir "$STATE/$d" 0750 "$SVC_USER" "$SVC_GROUP"
done

# 4. 配置（不覆盖已有）
CFG="$(asset chuan.toml.example)" || die "缺少 chuan.toml.example"
if [[ -f "$ETC/chuan.toml" ]]; then
  say "$ETC/chuan.toml 已存在，保持不变（只校正权限）"
  ensure_file "$ETC/chuan.toml" 0640 root "$SVC_GROUP"
else
  run install -Dm640 -o root -g "$SVC_GROUP" "$CFG" "$ETC/chuan.toml"
  [[ $DRY_RUN -eq 1 ]] || say "已生成 $ETC/chuan.toml（按需修改 allowed_users / model / 路径）"
fi

# 5. 凭据文件权限校正
#    - unit 里 LoadCredential 引用的（以及 *.cred）：systemd 以 root 读 → 0400 root:root
#    - 其余（service 的 token_file）：daemon 以服务用户读 → 0440 root:<group>
declare -A ROOT_ONLY=()
UNIT_SRC="$(asset chuan.service || true)"
if [[ -n $UNIT_SRC ]]; then
  while IFS= read -r line; do
    case $line in
      LoadCredential=*)
        spec=${line#LoadCredential=}
        ROOT_ONLY[${spec%%:*}]=1
        ;;
    esac
  done < "$UNIT_SRC"
fi
if [[ -d $SECRETS ]]; then
  while IFS= read -r f; do
    [[ -f $f ]] || continue
    base=$(basename "$f")
    if [[ -n "${ROOT_ONLY[$base]:-}" || $base == *.cred ]]; then
      ensure_file "$f" 0400 root root
    else
      ensure_file "$f" 0440 root "$SVC_GROUP"
    fi
  done < <(find "$SECRETS" -maxdepth 1 -type f 2>/dev/null)
fi

# 6. systemd 单元
for u in chuan.service chuan-backup.service chuan-backup.timer; do
  f="$(asset "$u")" || die "缺少 $u"
  if [[ -z $R ]]; then
    run install -Dm644 -o root -g root "$f" "/etc/systemd/system/$u"
  else
    run install -Dm644 -o root -g root "$f" "$UNITDIR/$u"
  fi
done
if [[ -z $R ]]; then
  run systemctl daemon-reload
  # 宿主执行（D22 L1）：`systemctl --user` 需要 user manager；journalctl 需要 journal 组。
  # 两者都只在真实系统上做，--root 安装（打包/交叉）时跳过。
  if command -v loginctl >/dev/null 2>&1; then
    run loginctl enable-linger "$SVC_USER"
  else
    say "没有 loginctl：跳过 enable-linger（宿主 lane 的 user systemd 将不可用）"
  fi
fi

cat <<NEXT

下一步（未自动执行）：
  1) 放密钥（systemd 以 root 读，服务用户看不到）：
       install -m 0400 -o root -g root <token>   /etc/chuan/secrets/telegram_token
       install -m 0400 -o root -g root <api-key> /etc/chuan/secrets/zai_key
     其它服务凭据（service 的 token_file）用 0440 root:$SVC_GROUP，否则 daemon 读不到：
       install -m 0440 -o root -g $SVC_GROUP <token> /etc/chuan/secrets/<name>
  2) 编辑 /etc/chuan/chuan.toml（allowed_users、model、路径）
  3) 启用：
       systemctl enable --now chuan.service
       systemctl enable --now chuan-backup.timer
  4) 看日志： journalctl -u chuan -f

目录与权限（本脚本会建好并校正已有安装）：
  /etc/chuan                0750 root:$SVC_GROUP
  /etc/chuan/secrets        0750 root:$SVC_GROUP   （文件见上）
  /etc/chuan/services.d     0770 root:$SVC_GROUP   （运行期加的服务写这里）
  /etc/chuan/roles          0770 root:$SVC_GROUP   （子 agent 角色，agent 可写、热加载）
  /etc/chuan/capabilities   0770 root:$SVC_GROUP   （权限申请草稿，agent 只写这里）
  /etc/chuan/playbooks      0750 root:$SVC_GROUP
  /var/lib/chuan            0750 $SVC_USER:$SVC_GROUP
  /var/lib/chuan/timers     0750 $SVC_USER:$SVC_GROUP  （宿主 lane 的监控脚本；[host.path] 已授权）

宿主执行（D22，P4.6-A）：
  - 本脚本已 enable-linger：`systemctl --user` 才能用（user timer 跑在独立 slice，
    不吃 chuan.service 的 MemoryMax）
  - unit 里已加 SupplementaryGroups=systemd-journal（journalctl 要读系统日志）
  - 授权表就在本配置文件（改完热加载，不用重启）；默认表见 chuan.toml 的 [host] 段

代理说明：
  - 下载/安装阶段：--proxy 或 CHUAN_PROXY / https_proxy
  - 运行阶段：写在 /etc/chuan/chuan.toml 的 [proxy] url（Telegram、web_fetch、
    沙盒脚本、装包都用它，不需要在 systemd 里再加 Environment）
NEXT

if [[ -z $R ]]; then
  if [[ $ENABLE -eq 1 ]]; then
    run systemctl enable --now chuan.service
    run systemctl enable --now chuan-backup.timer
    say "已启用 chuan.service 与 chuan-backup.timer"
  fi
  # enable --now 对已在运行的服务不会重启：升级后必须显式 restart 才会用上新二进制
  if systemctl is-active --quiet chuan.service 2>/dev/null; then
    say "chuan.service 正在运行（可能是旧进程）：升级后请执行 systemctl restart chuan"
  fi
fi
