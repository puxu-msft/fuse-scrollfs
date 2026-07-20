#!/usr/bin/env bash
# umount-bv.sh — 卸载 mount-bv.sh 挂起的 BV 布局 V 挂载点。
# 行为/安全同 umount-b0.sh：fusermount3 -u 优先，回退 fusermount；收尾守护进程；幂等。

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

MNT="${MNT:-$BENCH_DIR/.mnt/bv}"
PIDFILE="$BENCH_DIR/.mnt/bv.pid"

log()  { printf '[umount-bv] %s\n' "$*"; }
warn() { printf '[umount-bv] WARN: %s\n' "$*" >&2; }
die()  { printf '[umount-bv] ERROR: %s\n' "$*" >&2; exit 1; }

[ -n "$MNT" ] || die "MNT 为空，拒绝继续"

if mountpoint -q "$MNT" 2>/dev/null; then
  log "卸载: $MNT"
  if command -v fusermount3 >/dev/null 2>&1; then
    fusermount3 -u "$MNT" || warn "fusermount3 -u 失败，尝试 fusermount。"
  fi
  if mountpoint -q "$MNT" 2>/dev/null && command -v fusermount >/dev/null 2>&1; then
    fusermount -u "$MNT" || warn "fusermount -u 也失败（可能仍被占用）。"
  fi
  if mountpoint -q "$MNT" 2>/dev/null; then
    die "卸载未成功: $MNT 仍是挂载点。"
  fi
  log "已卸载: $MNT"
else
  log "$MNT 当前未挂载，跳过。"
fi

if [ -f "$PIDFILE" ]; then
  pid="$(cat "$PIDFILE" 2>/dev/null || true)"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    log "守护进程仍在 (pid=$pid)，发送 SIGTERM 收尾。"
    kill -TERM "$pid" 2>/dev/null || true
  fi
  rm -f -- "$PIDFILE"
fi

log "完成。"
