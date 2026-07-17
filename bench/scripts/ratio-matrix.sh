#!/usr/bin/env bash
# ratio-matrix.sh — T3「块大小×等级×字典」压缩比矩阵驱动。
#
# 用 ratio-bench 把真实语料经实际 Store+Core 路径写入临时后端，扫一组配置，输出压缩比矩阵。
# 不改系统、不碰源数据（ratio-bench 用临时目录，drop 自清）。结果贴进 REPORT.md。
#
# 用法：
#   bash bench/scripts/ratio-matrix.sh <语料目录> [max_bytes]
# 例：
#   bash bench/scripts/ratio-matrix.sh bench/datasets/claude-projects 134217728
set -euo pipefail

IN="${1:?用法: ratio-matrix.sh <语料目录> [max_bytes]}"
CAP="${2:-134217728}"   # 默认 128MiB 控时

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RB="$REPO_DIR/target/release/scrollz"
BIN="$REPO_DIR/target/release/ratio-bench"

[ -x "$BIN" ] || { echo "先构建：( cargo build --release -p scrollz-bench --bin ratio-bench )"; exit 1; }
[ -x "$RB" ]  || { echo "先构建：( cargo build --release -p scrollz --bin scrollz )"; exit 1; }
[ -d "$IN" ]  || { echo "语料目录不存在：$IN"; exit 1; }

DICT="$(mktemp -t scrollz-matrix-dict.XXXXXX)"
trap 'rm -f "$DICT"' EXIT

echo "# 训练 512K 字典（64KiB 切块，对齐块粒度）"
"$RB" train-dict --input "$IN" --output "$DICT" --max-dict 524288 --chunk-size 65536 --max-sample-bytes "$CAP" 2>/dev/null | tail -1

for BE in shadow container; do
  echo "## backend=$BE"
  for CS in 65536 262144 1048576; do
    for LVL in 3 19; do
      "$BIN" --input "$IN" --backend "$BE" --chunk-size "$CS" --level "$LVL" --max-bytes "$CAP"
    done
  done
  echo "### 64KiB + 512K 字典"
  for LVL in 3 19; do
    "$BIN" --input "$IN" --backend "$BE" --chunk-size 65536 --level "$LVL" --dict "$DICT" --max-bytes "$CAP"
  done
done
