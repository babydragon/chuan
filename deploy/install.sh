#!/usr/bin/env bash
# chuan 安装脚本。需要 root。不会自动启用服务，除非加 --enable。
#   sudo ./install.sh                 # 安装并启用
#   sudo ./install.sh --no-enable     # 只安装文件
set -euo pipefail

ENABLE=1
[[ "${1:-}" == "--no-enable" ]] && ENABLE=0

[[ $EUID -eq 0 ]] || { echo "需要 root"; exit 1; }

BIN_SRC="${BIN_SRC:-target/release/chuan}"
[[ -f "$BIN_SRC" ]] || { echo "找不到 $BIN_SRC，先 cargo build --release"; exit 1; }

# 1. 服务专用用户（无登录 shell）
if ! id -u chuan >/dev/null 2>&1; then
  useradd --system --home /var/lib/chuan --shell /usr/sbin/nologin chuan
  echo "created user chuan"
fi
# 打印机权限（可选）
if getent group lp >/dev/null 2>&1; then
  usermod -aG lp chuan
fi

# 2. 二进制与目录
install -Dm755 "$BIN_SRC" /usr/local/bin/chuan
install -d -m 0755 /etc/chuan
install -d -m 0700 -o root -g root /etc/chuan/secrets
install -d -m 0750 -o chuan -g chuan /var/lib/chuan
install -d -m 0750 -o chuan -g chuan /var/lib/chuan/backups

# 3. 配置（不覆盖已有文件）
if [[ ! -f /etc/chuan/chuan.toml ]]; then
  install -Dm640 -o root -g chuan deploy/chuan.toml.example /etc/chuan/chuan.toml
  echo "已生成 /etc/chuan/chuan.toml（请按需修改）"
fi

# 4. systemd 单元
install -Dm644 deploy/chuan.service        /etc/systemd/system/chuan.service
install -Dm644 deploy/chuan-backup.service /etc/systemd/system/chuan-backup.service
install -Dm644 deploy/chuan-backup.timer   /etc/systemd/system/chuan-backup.timer
systemctl daemon-reload

cat <<'NEXT'

下一步（尚未自动执行）：
  1) 放密钥（root 只读）：
       install -m 0400 -o root -g root <token>  /etc/chuan/secrets/telegram_token
       install -m 0400 -o root -g root <api-key> /etc/chuan/secrets/zai_key
  2) 编辑 /etc/chuan/chuan.toml（allowed_users、model、路径）
  3) 启用：
       systemctl enable --now chuan.service
       systemctl enable --now chuan-backup.timer
  4) 看日志： journalctl -u chuan -f
NEXT

if [[ $ENABLE -eq 1 ]]; then
  systemctl enable --now chuan.service
  systemctl enable --now chuan-backup.timer
  echo "已启用 chuan.service 与 chuan-backup.timer"
fi
