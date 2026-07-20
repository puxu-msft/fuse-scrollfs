#!/usr/bin/env bash
# umount-b0.sh — 卸载 mount-b0.sh 挂起的 B0 透传挂载点。
#
# 用法:
#   bash bench/scripts/umount-b0.sh                 # 卸载默认挂载点
#   MNT=/path/mnt bash .../umount-b0.sh             # 指定挂载点
#
# 行为:
#   - 优先 fusermount3 -u（普通用户即可，无需 sudo），回退 fusermount -u。
#   - 卸载后若守护进程仍在（PID 文件），尝试 SIGTERM 收尾。
#   - 未挂载则幂等返回成功，不报错。
#
# 安全: 只针对明确的 MNT 与 PID 文件，无通配符、无 sudo、无破坏性命令。

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

MNT="${MNT:-$BENCH_DIR/.mnt/b0}"
PIDFILE="$BENCH_DIR/.mnt/b0.pid"

log()  { printf '[umount-b0] %s\n' "$*"; }
warn() { printf '[umount-b0] WARN: %s\n' "$*" >&2; }
die()  { printf '[umount-b0] ERROR: %s\n' "$*" >&2; exit 1; }

[ -n "$MNT" ] || die "MNT 为空，拒绝继续"

if mountpoint -q "$MNT" 2>/dev/null; then
  log "卸载: $MNT"
  if command -v fusermount3 >/dev/null 2>&1; then
    fusermount3 -u "$MNT" || warn "fusermount3 -u 失败，尝试 fusermount。"
  fi
  if mountpoint -q "$MNT" 2>/dev/null && command -v fusermount >/dev/null 2>&1; then
    fusermount -u "$MNT" || warn "fusermount -u 也失败（可能仍被占用，fuser/lsof 排查）。"
  fi
  if mountpoint -q "$MNT" 2>/dev/null; then
    die "卸载未成功: $MNT 仍是挂载点。"
  fi
  log "已卸载: $MNT"
else
  log "$MNT 当前未挂载，跳过。"
fi

# ── 收尾守护进程（若 PID 文件存在且进程仍活）─────────────────────
if [ -f "$PIDFILE" ]; then
  pid="$(cat "$PIDFILE" 2>/dev/null || true)"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    log "守护进程仍在 (pid=$pid)，发送 SIGTERM 收尾。"
    kill -TERM "$pid" 2>/dev/null || true
  fi
  rm -f -- "$PIDFILE"   # 明确单文件，无通配符
fi

log "完成。"
