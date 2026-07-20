#!/usr/bin/env bash
# scrollz-cutover.sh — 可逆切换：把现有目录迁到 scrollz 压缩挂载，源备份保留，verify 通过才生效。
# 步骤：mv 源→源.scrollz-orig（备份）→ ingest --verify 灌入 backing → mount 到原路径。回滚见 scrollz-rollback.sh。
# 用法：scrollz-cutover.sh <目标目录> <backing> [chunk_size]
set -uo pipefail
TARGET="${1:?需目标目录}"; BACKING="${2:?需 backing}"; CHUNK="${3:-1048576}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${SCROLLZ_BIN:-$SCRIPT_DIR/../../target/release/scrollz}"
ORIG="$TARGET.scrollz-orig"

[ -e "$ORIG" ] && { echo "[cutover] $ORIG 已存在，疑似已切换；先 rollback" >&2; exit 1; }
mountpoint -q "$TARGET" && { echo "[cutover] $TARGET 已是挂载点" >&2; exit 1; }
# backing 须不存在或为空——回退时会 rm -rf 它，绝不删用户已有非空目录（no-unconscious）。
[ -e "$BACKING" ] && [ -n "$(ls -A "$BACKING" 2>/dev/null)" ] && { echo "[cutover] backing $BACKING 非空，拒绝（防误删）" >&2; exit 1; }
mv "$TARGET" "$ORIG"                       # 备份源（可逆关键）
mkdir -p "$BACKING" "$TARGET"
# verify 通过才算成功；失败则回退（源仍在 $ORIG）。
if ! "$BIN" ingest --src "$ORIG" --backing "$BACKING" --chunk-size "$CHUNK" --verify; then
  echo "[cutover] ingest/verify 失败，回滚" >&2; rmdir "$TARGET" 2>/dev/null; mv "$ORIG" "$TARGET"; rm -rf "$BACKING"; exit 1
fi
"$BIN" --backend shadow --backing "$BACKING" --mountpoint "$TARGET" --chunk-size "$CHUNK" --pid-file "$TARGET.scrollz.pid" &
for _ in $(seq 1 50); do mountpoint -q "$TARGET" && { echo "[cutover] 已挂载，源备份在 $ORIG"; exit 0; }; sleep 0.1; done
echo "[cutover] 挂载超时，杀守护并 rollback" >&2; [ -f "$TARGET.scrollz.pid" ] && kill "$(cat "$TARGET.scrollz.pid")" 2>/dev/null; exit 1
