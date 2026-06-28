#!/usr/bin/env bash
# setup-btrfs.sh — 参数化创建并挂载一个 btrfs loop image（路线 A 的载体）。
#
# 用稀疏镜像 + mkfs.btrfs + mount -o loop,compress-force=zstd:LEVEL。
# 挂载与 mkfs 需要 root（loop 设备 / mount），脚本会显式要求。
#
# 为什么 compress-FORCE 而非 compress（针对本项目目标负载的最佳配置）:
#   目标负载是 ~/.claude/projects 这类 append-only 可压缩 jsonl。btrfs 默认的
#   `compress=zstd` 用采样启发式，会误判跳过大量本可压缩的数据——实测对 676M 子集
#   漏压 212M、整体仅 2.44x。`compress-force` 强制压每个 extent，对此场景才是最佳，
#   也才与 zipfs「逐块强制压缩」apples-to-apples（用 FORCE=0 可退回默认启发式对照）。
#
# 用法（环境变量参数化）:
#   IMG=/path/to/btrfs.img SIZE=20G MNT=/mnt/zipfs-btrfs ZSTD_LEVEL=3 \
#     sudo -E bash bench/scripts/setup-btrfs.sh
#
# 参数（均可用环境变量覆盖，含默认值）:
#   IMG         镜像文件路径        默认 ./bench/results/btrfs.img（相对调用处）
#   SIZE        镜像逻辑容量        默认 20G（truncate 稀疏，不立即占满物理空间）
#   MNT         挂载点              默认 /mnt/zipfs-btrfs
#   ZSTD_LEVEL  zstd 压缩等级       默认 3（btrfs 支持 1..15）
#
# 安全约束:
#   - set -euo pipefail，任一步失败即止。
#   - 挂载前检查 btrfs 已加载；未加载则提示 sudo modprobe btrfs 并退出（绝不擅自 modprobe）。
#   - 不覆盖已存在且非空的挂载点上的已挂载文件系统。

set -euo pipefail

IMG="${IMG:-./bench/results/btrfs.img}"
SIZE="${SIZE:-20G}"
MNT="${MNT:-/mnt/zipfs-btrfs}"
ZSTD_LEVEL="${ZSTD_LEVEL:-3}"
FORCE="${FORCE:-1}"   # 1=compress-force（默认，本负载最佳）；0=compress（btrfs 默认启发式，仅作对照）

log()  { printf '[setup-btrfs] %s\n' "$*"; }
die()  { printf '[setup-btrfs] ERROR: %s\n' "$*" >&2; exit 1; }

# ── 参数校验 ───────────────────────────────────────────────────
[ -n "$IMG" ] || die "IMG 为空，拒绝继续"
[ -n "$MNT" ] || die "MNT 为空，拒绝继续"
case "$ZSTD_LEVEL" in
  ''|*[!0-9]*) die "ZSTD_LEVEL 必须是整数，得到: '$ZSTD_LEVEL'" ;;
esac
if [ "$ZSTD_LEVEL" -lt 1 ] || [ "$ZSTD_LEVEL" -gt 15 ]; then
  die "ZSTD_LEVEL=$ZSTD_LEVEL 超出 btrfs 支持范围 1..15"
fi

# ── root 检查（mkfs/loop/mount 都需要）───────────────────────────
if [ "$(id -u)" -ne 0 ]; then
  die "需要 root 执行 mkfs/mount。请用: sudo -E bash $0"
fi

# ── btrfs 模块必须已加载（不擅自 modprobe）──────────────────────
if ! grep -qw btrfs /proc/filesystems 2>/dev/null; then
  cat >&2 <<'EOF'
[setup-btrfs] ERROR: btrfs 内核模块未加载。
  本脚本遵循「不擅自 modprobe」原则，请先手动加载后重试:
      sudo modprobe btrfs
  （WSL 每次启动都需要重新加载；持久化见 docs/00-overview.md §7。）
EOF
  exit 1
fi

command -v mkfs.btrfs >/dev/null 2>&1 || die "mkfs.btrfs 未安装（sudo apt install btrfs-progs）"

# ── 挂载点幂等检查 ─────────────────────────────────────────────
if mountpoint -q "$MNT" 2>/dev/null; then
  die "$MNT 已是挂载点。若要重建，请先 teardown.sh 卸载。"
fi

# ── 镜像文件处理 ───────────────────────────────────────────────
if [ -e "$IMG" ]; then
  die "镜像已存在: $IMG（拒绝覆盖；如确需重建请手动确认后删除，或换 IMG 路径）"
fi
IMG_DIR="$(dirname "$IMG")"
[ -d "$IMG_DIR" ] || { log "创建镜像目录: $IMG_DIR"; mkdir -p "$IMG_DIR"; }

log "创建稀疏镜像: $IMG (SIZE=$SIZE)"
truncate -s "$SIZE" "$IMG"

log "格式化 btrfs: $IMG"
# -f 允许在已 truncate 出的文件上建 fs；首轮不开 mixed/特殊配置，保持默认。
mkfs.btrfs -f "$IMG" >/dev/null

log "创建挂载点: $MNT"
mkdir -p "$MNT"

if [ "$FORCE" = "1" ]; then
  COMPRESS_OPT="compress-force=zstd:$ZSTD_LEVEL"
else
  COMPRESS_OPT="compress=zstd:$ZSTD_LEVEL"
  log "注意: FORCE=0，用 btrfs 默认启发式 compress（会跳过判为不可压的 extent），仅作对照"
fi

log "挂载: $IMG -> $MNT  (loop, $COMPRESS_OPT)"
mount -o loop,"$COMPRESS_OPT" "$IMG" "$MNT"

# ── 确认结果 ───────────────────────────────────────────────────
if mountpoint -q "$MNT"; then
  log "挂载成功。当前挂载信息:"
  findmnt -no SOURCE,TARGET,FSTYPE,OPTIONS "$MNT" 2>/dev/null | sed 's/^/  /'
  log "压缩比可用 compsize 查看: compsize $MNT"
  log "卸载/清理: IMG=$IMG MNT=$MNT bash bench/scripts/teardown.sh"
else
  die "挂载后校验失败：$MNT 不是挂载点"
fi
