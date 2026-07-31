#!/usr/bin/env bash
# 安装 scrollz harness 的 systemd user 单元。**只安装，不启用**。
# 启用是独立动作：systemctl --user enable --now scrollz-harness.timer
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
LOGDIR="${XDG_STATE_HOME:-$HOME/.local/state}/scrollz-harness"

mkdir -p "$DEST" "$LOGDIR"
for unit in scrollz-harness.service scrollz-harness.timer; do
    install -m 0644 "$SRC/$unit" "$DEST/$unit"
    echo "installed $DEST/$unit"
done
systemctl --user daemon-reload
echo
echo "已安装，**尚未启用**。检查："
echo "  systemctl --user cat scrollz-harness.service"
echo "  systemctl --user start scrollz-harness.service   # 手工跑一轮"
echo "启用定时器（这一步才会开始无人值守）："
echo "  systemctl --user enable --now scrollz-harness.timer"
