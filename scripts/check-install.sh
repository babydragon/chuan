#!/usr/bin/env bash
# install.sh 的回归测试（不需要 root、不联网）
#
# 覆盖两类真实事故：
#   1) 下载分支结束后没有 exit，脚本继续走「本地安装」分支 → `BIN_SRC: 未绑定的变量`
#      （set -u），安装其实成功了但退出码非零；
#   2) `for arg in "$@"` 里 shift 导致 `--proxy URL` 参数错位被当成未知参数。
#
# 做法：造一个假的 release 目录（tar + SHA256SUMS），用 file:// 当下载源跑 dry-run。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

REL="$WORK/base/babydragon/chuan/releases/download/v0.0.0-test"   # 与 CHUAN_REPO 默认值一致
mkdir -p "$REL" "$WORK/pkg" "$WORK/client"

# 假包：安装脚本 + 占位二进制（dry-run 不会真的动文件）
cp "$REPO_ROOT/deploy/install.sh" "$WORK/pkg/install.sh"
printf '#!/bin/sh\necho fake\n' > "$WORK/pkg/chuan"
cp "$REPO_ROOT/deploy/chuan.toml.example" "$WORK/pkg/"
cp "$REPO_ROOT/deploy/chuan.service" "$REPO_ROOT/deploy/chuan-backup.service" \
   "$REPO_ROOT/deploy/chuan-backup.timer" "$WORK/pkg/"
chmod +x "$WORK/pkg/chuan" "$WORK/pkg/install.sh"
tar -czf "$REL/chuan-v0.0.0-test-x86_64-linux-gnu.tar.gz" -C "$WORK/pkg" .
cp "$REL/chuan-v0.0.0-test-x86_64-linux-gnu.tar.gz" "$REL/chuan-x86_64-linux-gnu.tar.gz"
( cd "$REL" && sha256sum *.tar.gz > SHA256SUMS )

# 客户端只放脚本，不放二进制 → 走下载分支
cp "$REPO_ROOT/deploy/install.sh" "$WORK/client/install.sh"

fail() { printf 'FAIL: %b\n' "$*" >&2; exit 1; }

run_case() {
  local name="$1"; shift
  local out="$WORK/$name.log"
  set +e
  ( cd "$WORK/client" && \
    env -u https_proxy -u http_proxy -u CHUAN_PROXY \
        CHUAN_BASE_URL="file://$WORK/base" CHUAN_VERSION=v0.0.0-test \
        bash install.sh --dry-run --no-enable "$@" ) > "$out" 2>&1
  local rc=$?
  set -e
  [[ $rc -eq 0 ]] || fail "$name: 退出码 $rc（预期 0）\n$(tail -5 "$out")"
  local n
  n=$(grep -c "→ /usr/local/bin/chuan" "$out" || true)
  [[ "$n" == "1" ]] || fail "$name: 安装步骤出现 $n 次（预期 1，说明下载分支后没有 exit）"
  grep -q "校验通过（SHA256SUMS）" "$out" || fail "$name: 没走 SHA256SUMS 校验"
  # 反引号写在未加引号的 heredoc 里会被当成命令替换真的执行（真实事故：
  # 装完多一行 "Failed to connect to user scope bus"，且那句话变成空）
  grep -q "Failed to connect" "$out" && fail "$name: 结尾提示里有命令替换的残留输出"
  grep -q "已 enable-linger chuan：systemctl --user 才能用" "$out" \
    || fail "$name: 结尾提示里变量没展开（$(grep -m1 "已 enable-linger" "$out")）"
}

# 1. 无代理参数
run_case plain
# 2. --proxy URL（参数解析顺序：在其它参数之后，最容易踩 shift 错位）
run_case proxy_after --proxy http://127.0.0.1:9
# 3. --proxy=URL
run_case proxy_eq --proxy=http://127.0.0.1:9

# 4. --proxy 缺参数要报错退出（不能静默继续）
set +e
( cd "$WORK/client" && bash install.sh --proxy ) > "$WORK/argerr.log" 2>&1
rc=$?
set -e
[[ $rc -eq 2 ]] || fail "--proxy 缺参数应退出码 2，实际 $rc"
grep -q "需要参数" "$WORK/argerr.log" || fail "--proxy 缺参数没有提示"

# 5. 临时目录要清理干净（不能因为改成 bash 调用而泄漏；$WORK 自己不算）
leftover=$(ls -d /tmp/tmp.* 2>/dev/null | grep -v "^$WORK\$" | wc -l || true)
leftover=${leftover//[!0-9]/}
[[ "$leftover" == "0" ]] || fail "临时目录残留 $leftover 个：$(ls -d /tmp/tmp.* | grep -v "^$WORK\$" | head -3)"

# ---------- 真装一次到临时根，验证目录与权限（含"修正历史安装"）----------
if sudo -n true 2>/dev/null; then
  ROOT="$(mktemp -d)"
  G="$(id -gn)"
  U="$(id -un)"
  # 造一份"权限全错"的历史安装
  sudo install -d -m 0755 -o root -g root "$ROOT/etc/chuan"
  sudo install -d -m 0700 -o root -g root "$ROOT/etc/chuan/secrets"
  printf 'x\n' | sudo tee "$ROOT/etc/chuan/secrets/telegram_token" >/dev/null
  printf 'y\n' | sudo tee "$ROOT/etc/chuan/secrets/weread_token" >/dev/null
  printf 'a = 1\n' | sudo tee "$ROOT/etc/chuan/chuan.toml" >/dev/null
  sudo chmod 0644 "$ROOT/etc/chuan/secrets/telegram_token" "$ROOT/etc/chuan/secrets/weread_token" "$ROOT/etc/chuan/chuan.toml"

  sudo env CHUAN_ROOT="$ROOT" CHUAN_USER="$U" CHUAN_GROUP="$G" \
      BIN_SRC="$WORK/pkg/chuan" bash "$REPO_ROOT/deploy/install.sh" --no-enable > "$WORK/real.log" 2>&1 \
    || fail "真装失败：$(tail -5 "$WORK/real.log")"

  mode_of() { stat -c '%a %U:%G' "$ROOT$1"; }
  expect() { local got; got=$(mode_of "$2"); [[ "$got" == "$3" ]] || fail "$1：$2 是 [$got]，期望 [$3]"; }

  expect "配置目录"   /etc/chuan                  "750 root:$G"
  expect "secrets"    /etc/chuan/secrets          "750 root:$G"
  expect "services.d" /etc/chuan/services.d       "770 root:$G"
  expect "playbooks"  /etc/chuan/playbooks        "750 root:$G"
  expect "prompts"    /etc/chuan/prompts          "750 root:$G"
  expect "状态目录"   /var/lib/chuan              "750 $U:$G"
  expect "配置(已有)" /etc/chuan/chuan.toml       "640 root:$G"
  # LoadCredential 引用的凭据：root 独占；其它服务 token：组可读
  expect "telegram"   /etc/chuan/secrets/telegram_token "400 root:root"
  expect "服务 token" /etc/chuan/secrets/weread_token   "440 root:$G"
  expect "二进制"     /usr/local/bin/chuan        "755 root:root"
  expect "unit"       /etc/systemd/system/chuan.service "644 root:root"

  # 幂等：第二次跑不应该再报任何"校正"（stat 的 750 vs 配置的 0750 必须按数值比）
  sudo env CHUAN_ROOT="$ROOT" CHUAN_USER="$U" CHUAN_GROUP="$G" \
      BIN_SRC="$WORK/pkg/chuan" bash "$REPO_ROOT/deploy/install.sh" --no-enable > "$WORK/real2.log" 2>&1 \
    || fail "第二次真装失败"
  if grep -q "^  ~ 校正 " "$WORK/real2.log"; then
    fail "第二次安装仍在报校正（不幂等）：$(grep "^  ~ 校正 " "$WORK/real2.log" | head -3)"
  fi

  sudo rm -rf "$ROOT"
  echo "权限校正 OK（真装到临时根，二次安装无变更）"
else
  echo "skip: 没有免密 sudo，跳过真装校验"
fi

echo "install.sh OK（4 组用例 + 权限校正）"
