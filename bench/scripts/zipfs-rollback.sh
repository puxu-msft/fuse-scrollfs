#!/usr/bin/env bash
# zipfs-rollback.sh — 回滚 zipfs-cutover：卸载挂载、删空 backing 视图、还原源备份。零丢失。
# 用法：zipfs-rollback.sh <目标目录>  （cutover 时备份在 <目标>.zipfs-orig）
set -uo pipefail
TARGET="${1:?需目标目录}"; ORIG="$TARGET.zipfs-orig"
[ -d "$ORIG" ] || { echo "[rollback] 无备份 $ORIG，无法回滚" >&2; exit 1; }
fusermount3 -u "$TARGET" 2>/dev/null || fusermount -u "$TARGET" 2>/dev/null || true
for _ in $(seq 1 50); do mountpoint -q "$TARGET" || break; sleep 0.1; done   # 轮询等卸载完成
rmdir "$TARGET" 2>/dev/null || { echo "[rollback] $TARGET 非空（仍挂载？）" >&2; exit 1; }
mv "$ORIG" "$TARGET"            # 还原源
rm -f "$TARGET.zipfs.pid"
echo "[rollback] 已还原 $TARGET（zipfs backing 保留，可手动删）"
