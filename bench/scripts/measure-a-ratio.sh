#!/usr/bin/env bash
# measure-a-ratio.sh — 测量条件 A（btrfs + zstd）的真实压缩比。
#
# 为什么单独一个脚本：btrfs 的压缩比只能用 `compsize` 读（du 显示的是逻辑大小，
# 看不到压缩后的磁盘占用），而 compsize 走 SEARCH_V2 ioctl，**需要 root**。
# 因此主对照（无 sudo）测不到这格，由本脚本补齐。
#
# 用法：
#   bash bench/scripts/measure-a-ratio.sh          # 默认测已有子集（~678MB，快）
#   DATASET=~/.claude/projects bash bench/scripts/measure-a-ratio.sh   # 测完整目标负载（8.7G，慢、写盘多）
#
# 参数（环境变量）：
#   MNT      btrfs 挂载点      默认 /mnt/zipfs-btrfs
#   DATASET  要测的数据源(只读) 默认 bench/datasets/claude-projects
#
# 行为：把 DATASET 复制进 btrfs 的一个探针子目录 → sync → sudo compsize → 算比值 → 删探针。
# 安全：set -euo pipefail；探针目录路径显式，清理无通配符；源 DATASET 只读不动。

set -uo pipefail

MNT="${MNT:-/mnt/zipfs-btrfs}"
DATASET="${DATASET:-bench/datasets/claude-projects}"
PROBE="$MNT/.ratio-probe"   # 专属探针目录，避免碰其它数据

log()  { printf '[measure-a] %s\n' "$*"; }
die()  { printf '[measure-a] ERROR: %s\n' "$*" >&2; exit 1; }

# ── 前置检查 ───────────────────────────────────────────────────
[ -d "$DATASET" ] || die "数据源不存在: $DATASET"
mountpoint -q "$MNT" 2>/dev/null || die "$MNT 不是挂载点（先 bash bench/scripts/setup-btrfs.sh）"
findmnt -no FSTYPE "$MNT" 2>/dev/null | grep -qx btrfs || die "$MNT 不是 btrfs"
command -v compsize >/dev/null 2>&1 || die "缺 compsize（apt install btrfs-compsize 或 brew）"

# 可写性
probe_test="$MNT/.measure-a-write-test.$$"
( : > "$probe_test" ) 2>/dev/null || die "$MNT 不可写（需 sudo chown \$(id -u):\$(id -g) $MNT）"
rm -f -- "$probe_test"

LOGICAL=$(du -sb "$DATASET" | cut -f1)
log "数据源: $DATASET （逻辑 $(du -sh "$DATASET" | cut -f1)）"

# ── 清旧探针（显式路径，无通配符）──────────────────────────────
if [ -e "$PROBE" ]; then
  log "清理旧探针目录: $PROBE"
  rm -rf -- "$PROBE"
fi
mkdir -p "$PROBE"

# ── 写入 + 落盘 ────────────────────────────────────────────────
log "复制数据进 btrfs（cp -a，源只读）..."
cp -a "$DATASET"/. "$PROBE"/ 2>/dev/null || log "（cp 有个别项失败，多为 symlink，不影响压缩比测量）"
sync; sleep 1; sync

# ── compsize（需 root）─────────────────────────────────────────
log "运行 compsize（需 sudo，按提示输密码）..."
echo "──────────────────────────────────────────────"
sudo compsize "$PROBE"
RC=$?
echo "──────────────────────────────────────────────"

# ── 用 compsize 的 TOTAL 行算逻辑/物理比 ───────────────────────
if [ "$RC" -eq 0 ]; then
  # compsize TOTAL 行形如: TOTAL  35%  240M  678M  678M
  read -r DISK UNCOMP < <(sudo compsize -b "$PROBE" 2>/dev/null \
      | awk '/^TOTAL/{print $3, $4}')
  if [ -n "${DISK:-}" ] && [ -n "${UNCOMP:-}" ] && [ "$DISK" -gt 0 ]; then
    awk -v u="$UNCOMP" -v d="$DISK" 'BEGIN{
      printf "\n[measure-a] A(btrfs zstd:3) 压缩比 = 逻辑 %.1f MiB / 物理 %.1f MiB = %.2fx\n",
             u/1048576, d/1048576, u/d}'
  fi
else
  log "compsize 返回非零（可能未授权 sudo），上方输出供参考"
fi

# ── 清理探针 ───────────────────────────────────────────────────
log "清理探针目录: $PROBE"
rm -rf -- "$PROBE"
sync
log "完成。把上面的「压缩比 = ...x」填进 CONSOLIDATED.md §3 的 A 行即可。"
