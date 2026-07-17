#!/usr/bin/env bash
# scrollz-mount.sh — 幂等自挂载守护（后台运行 + PID + stale endpoint 清理）。
# 长期运行 scrollz（布局 S）用：起前清残留、已挂则跳过、写 PID、随 WSL/systemd 重挂。
#
# 用法：scrollz-mount.sh <backing-dir> <mountpoint> [chunk_size]
# 环境：SCROLLZ_BIN（默认 target/release/scrollz）、SCROLLZ_LEVEL（默认 3）。
set -uo pipefail

BACKING="${1:?需 backing 目录}"
MNT="${2:?需挂载点}"
CHUNK="${3:-1048576}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${SCROLLZ_BIN:-$SCRIPT_DIR/../../target/release/scrollz}"
LEVEL="${SCROLLZ_LEVEL:-3}"
PID_FILE="$MNT.scrollz.pid"

mkdir -p "$BACKING" "$MNT"

# 已是挂载点 → 幂等跳过（重复调用安全）。
if mountpoint -q "$MNT"; then echo "[scrollz] $MNT 已挂载，跳过"; exit 0; fi
# stale endpoint（守护崩溃残留 ENOTCONN）→ 先卸载。
if ! ls "$MNT" >/dev/null 2>&1; then fusermount3 -u "$MNT" 2>/dev/null || fusermount -u "$MNT" 2>/dev/null || true; fi

"$BIN" --backend shadow --backing "$BACKING" --mountpoint "$MNT" \
  --chunk-size "$CHUNK" --level "$LEVEL" --pid-file "$PID_FILE" &
for _ in $(seq 1 50); do mountpoint -q "$MNT" && { echo "[scrollz] 挂载就绪 $MNT (pid $(cat "$PID_FILE" 2>/dev/null))"; exit 0; }; sleep 0.1; done
echo "[scrollz] 挂载超时" >&2; exit 1
