#!/usr/bin/env bash
# ldm-ratio-matrix.sh — M2 补测：zstd 长程匹配（LDM）在真实语料上的压缩比矩阵。
#
# 用 ldm-ratio 对语料逐块 compress_with_params，扫 chunk ∈ {8,16,32,64}MiB × long ∈ {off,on}
# @ level 19，量「>8MiB 封存块开 LDM 相比不开，比值提升多少」。同基准对照：唯一变量是 --long。
# 不改系统、不碰源数据（ldm-ratio 不落盘，纯内存压缩求和）。结果贴进报告。
#
# 用法：
#   bash bench/scripts/ldm-ratio-matrix.sh <语料目录> [max_bytes] [level]
# 例（先小 cap 试跑，再放大）：
#   bash bench/scripts/ldm-ratio-matrix.sh bench/datasets/claude-projects 33554432
#   bash bench/scripts/ldm-ratio-matrix.sh bench/datasets/claude-projects 0   # 0 = 不限（全语料，慢）
set -euo pipefail

IN="${1:?用法: ldm-ratio-matrix.sh <语料目录> [max_bytes] [level]}"
CAP="${2:-268435456}"   # 默认 256MiB 控时（level 19 慢）
LVL="${3:-19}"

# max_bytes=0 视为「不限」：ldm-ratio 的 --max-bytes 取一个极大值放行全语料。
if [ "$CAP" = "0" ]; then CAP=1099511627776; fi   # 1TiB，实为不限

FUSE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../fuse" && pwd)"
BIN="$FUSE_DIR/target/release/ldm-ratio"

[ -x "$BIN" ] || { echo "先构建：( cd fuse && cargo build --release --bin ldm-ratio )"; exit 1; }
[ -d "$IN" ]  || { echo "语料目录不存在：$IN"; exit 1; }

echo "# LDM 压缩比矩阵  语料=$IN  max_bytes=$CAP  level=$LVL"
for CS_MIB in 8 16 32 64; do
  CS=$(( CS_MIB * 1024 * 1024 ))
  # off：不传 --long；on：传 --long。同 chunk、同 level，唯一变量是 LDM。
  "$BIN" --input "$IN" --chunk-size "$CS" --level "$LVL" --max-bytes "$CAP"
  "$BIN" --input "$IN" --chunk-size "$CS" --level "$LVL" --max-bytes "$CAP" --long
done
