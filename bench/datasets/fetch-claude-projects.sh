#!/usr/bin/env bash
# fetch-claude-projects.sh — 把 ~/.claude/projects 的【只读副本】取到 bench/datasets/claude-projects/。
#
# 旗舰真实数据集（见 docs/00-overview.md §4.4 与 01-scrollz-design.md §1.1）：
# 8.7GB / jsonl·txt·json / 双峰大小 / 追加写为主 / 跨会话高冗余 / zstd:3 实测 31x。
#
# 用法:
#   bash bench/datasets/fetch-claude-projects.sh                 # 默认代表性子集（约 1-2GB）
#   bash bench/datasets/fetch-claude-projects.sh --full          # 取全部（约 8.7GB）
#   bash bench/datasets/fetch-claude-projects.sh --size-cap 3G   # 自定子集上限
#   SRC=/some/other/projects bash .../fetch-claude-projects.sh   # 覆盖源路径
#
# 子集选择策略（确定性，绝不随机）：
#   覆盖双峰特征——既要「小文件密集」的 project 目录，又要至少一个「含巨型 jsonl」的目录。
#   1. 强制纳入恰好一个【巨文件锚点】：所有顶层 project 目录中，含「最大单文件」的那个目录
#      （确定性：按目录内最大文件字节数降序，并列再按目录名升序，取第一个）。
#   2. 其余目录按【总大小升序、并列按目录名升序】依次纳入，直到累计大小逼近 size-cap。
#      升序优先保证小文件密集的目录优先进入，贴合「双峰」里小文件那一峰。
#   3. 巨文件锚点的大小先计入预算；若它本身已超 cap，仍纳入它（旗舰数据集必须含巨文件特征），
#      并显式告警 cap 被锚点撑破，绝不静默截断。
#
# 安全约束（源是用户真实 Claude 记录，绝对只读）：
#   - 只用 cp -a / rsync -a 读取源，永不修改 / 删除 / 移动源数据。
#   - 目的地清理（覆盖旧副本）时严格校验路径非空、位于 bench/datasets 下、非通配符。

set -uo pipefail

# ── 路径定位 ───────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DATASETS_DIR="$SCRIPT_DIR"
DEST="$DATASETS_DIR/claude-projects"

SRC="${SRC:-$HOME/.claude/projects}"

# 默认子集上限（字节）。--full 时忽略。可被 --size-cap 覆盖。
DEFAULT_CAP="2G"

log()  { printf '[fetch-claude] %s\n' "$*"; }
warn() { printf '[fetch-claude] WARN: %s\n' "$*" >&2; }
die()  { printf '[fetch-claude] ERROR: %s\n' "$*" >&2; exit 1; }

# ── 参数解析 ───────────────────────────────────────────────────
MODE="subset"        # subset | full
CAP_HUMAN="$DEFAULT_CAP"
while [ $# -gt 0 ]; do
  case "$1" in
    --full)      MODE="full"; shift ;;
    --size-cap)  CAP_HUMAN="${2:-}"; [ -n "$CAP_HUMAN" ] || die "--size-cap 需要参数"; shift 2 ;;
    --size-cap=*) CAP_HUMAN="${1#*=}"; shift ;;
    -h|--help)
      sed -n '2,30p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) die "未知参数: $1（见 --help）" ;;
  esac
done

# 人类可读容量 -> 字节（支持 K/M/G 后缀，十进制 1000 进制，贴近 du --si 直觉）。
to_bytes() {
  local v="$1" num unit
  case "$v" in
    *[!0-9KMGTkmgt.]*) die "无法解析容量: '$v'（用如 2G / 1500M / 800000000）" ;;
  esac
  num="${v%[KMGTkmgt]}"
  unit="${v#"$num"}"
  case "$unit" in
    ''|[0-9]) printf '%.0f' "$v" ;;
    K|k) printf '%.0f' "$(awk "BEGIN{print $num*1000}")" ;;
    M|m) printf '%.0f' "$(awk "BEGIN{print $num*1000000}")" ;;
    G|g) printf '%.0f' "$(awk "BEGIN{print $num*1000000000}")" ;;
    T|t) printf '%.0f' "$(awk "BEGIN{print $num*1000000000000}")" ;;
    *)   die "未知容量单位: '$unit'" ;;
  esac
}

human() { numfmt --to=si --suffix=B "$1" 2>/dev/null || printf '%sB' "$1"; }

# ── 源校验（绝对只读，先确认源存在且可读）────────────────────────
[ -d "$SRC" ] || die "源目录不存在: $SRC"
[ -r "$SRC" ] || die "源目录不可读: $SRC"

# 选择拷贝工具：优先 rsync（增量、保留属性），回退 cp -a。两者均纯读源。
COPY_TOOL=""
if command -v rsync >/dev/null 2>&1; then
  COPY_TOOL="rsync"
elif command -v cp >/dev/null 2>&1; then
  COPY_TOOL="cp"
else
  die "rsync 与 cp 均不可用，无法拷贝"
fi

# 单个目录的字节大小（apparent/实占以 du -sb 为准，与压缩比口径无关，仅用于选择预算）。
dir_bytes() { du -sb "$1" 2>/dev/null | cut -f1; }
# 目录内最大单文件字节数（确定性巨文件锚点判据）。
dir_max_file() { find "$1" -type f -printf '%s\n' 2>/dev/null | sort -rn | head -1; }

# ── 目的地清理（覆盖旧副本，重重设防）──────────────────────────
clean_dest() {
  [ -e "$DEST" ] || return 0
  # 必须位于 bench/datasets 下、basename 恰为 claude-projects、非链接。
  case "$DEST" in
    "$DATASETS_DIR"/claude-projects) : ;;
    *) die "DEST 路径越界，拒绝删除: '$DEST'" ;;
  esac
  [ -L "$DEST" ] && die "DEST 是符号链接，拒绝删除: '$DEST'"
  [ -d "$DEST" ] || die "DEST 存在但不是目录，拒绝删除: '$DEST'"
  log "清理旧副本（单目录，无通配符）: $DEST"
  rm -rf -- "$DEST"
}

# ── 拷贝一个源目录到目的地（保留相对结构）─────────────────────
# $1 = 源顶层 project 目录的【绝对路径】；目的地为 $DEST/<basename>。
copy_one() {
  local s="$1" base out
  base="$(basename "$s")"
  out="$DEST/$base"
  if [ "$COPY_TOOL" = "rsync" ]; then
    # -a 保留属性；末尾不加 / 让 rsync 在 out 下重建该目录。只读源。
    rsync -a "$s" "$DEST/" >/dev/null 2>&1 || warn "rsync 拷贝失败: $base"
  else
    cp -a "$s" "$out" 2>/dev/null || warn "cp 拷贝失败: $base"
  fi
}

mkdir -p "$DATASETS_DIR"
clean_dest
mkdir -p "$DEST"

log "源: $SRC"
log "目的: $DEST"
log "拷贝工具: $COPY_TOOL"

# ── full 模式：整树只读拷贝 ────────────────────────────────────
if [ "$MODE" = "full" ]; then
  log "模式: --full（整树拷贝，约 8.7GB，耗时与磁盘相关）"
  if [ "$COPY_TOOL" = "rsync" ]; then
    rsync -a "$SRC"/ "$DEST"/ || die "rsync 整树拷贝失败"
  else
    cp -a "$SRC"/. "$DEST"/ || die "cp 整树拷贝失败"
  fi
  total_files=$(find "$DEST" -type f 2>/dev/null | wc -l)
  total_bytes=$(dir_bytes "$DEST")
  log "完成（full）: $total_files 文件, $(human "$total_bytes")"
  exit 0
fi

# ── subset 模式：确定性挑选顶层 project 目录 ───────────────────
CAP_BYTES="$(to_bytes "$CAP_HUMAN")"
log "模式: subset（代表性子集，上限 $(human "$CAP_BYTES")）"

# 枚举源下的顶层 project 目录（确定性按名字排序），跳过空目录。
mapfile -t TOP_DIRS < <(find "$SRC" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort)
[ "${#TOP_DIRS[@]}" -gt 0 ] || die "源下没有任何 project 目录: $SRC"

# 为每个目录算 (max_file_size, total_bytes)。空目录（无文件）跳过，不浪费名额。
declare -a CAND_DIR CAND_TOTAL CAND_MAXF
for d in "${TOP_DIRS[@]}"; do
  mf="$(dir_max_file "$d")"
  [ -n "$mf" ] || continue          # 无文件，跳过
  tb="$(dir_bytes "$d")"
  [ -n "$tb" ] || continue
  CAND_DIR+=("$d"); CAND_TOTAL+=("$tb"); CAND_MAXF+=("$mf")
done
[ "${#CAND_DIR[@]}" -gt 0 ] || die "源下没有任何含文件的 project 目录"

# 1) 巨文件锚点：max_file 最大者（并列按目录名升序，因 TOP_DIRS 已排序，先出现者胜）。
anchor_idx=-1; anchor_max=-1
for i in "${!CAND_DIR[@]}"; do
  if [ "${CAND_MAXF[$i]}" -gt "$anchor_max" ]; then
    anchor_max="${CAND_MAXF[$i]}"; anchor_idx="$i"
  fi
done
ANCHOR_DIR="${CAND_DIR[$anchor_idx]}"
ANCHOR_BYTES="${CAND_TOTAL[$anchor_idx]}"
log "巨文件锚点: $(basename "$ANCHOR_DIR")（最大单文件 $(human "$anchor_max"), 目录 $(human "$ANCHOR_BYTES")）"

# 2) 其余目录按 total 升序、并列按名字升序排序。构造 "total\tidx" 再排序。
declare -a REST_ORDER
{
  for i in "${!CAND_DIR[@]}"; do
    [ "$i" = "$anchor_idx" ] && continue
    printf '%020d\t%s\t%d\n' "${CAND_TOTAL[$i]}" "${CAND_DIR[$i]}" "$i"
  done | sort -k1,1n -k2,2
} > /tmp/.fetch-claude-rest.$$ 2>/dev/null || true
while IFS=$'\t' read -r _tb _dir idx; do
  [ -n "$idx" ] && REST_ORDER+=("$idx")
done < /tmp/.fetch-claude-rest.$$
rm -f -- "/tmp/.fetch-claude-rest.$$"

# ── 预算分配：先纳锚点，再升序填充 ─────────────────────────────
declare -a SELECTED=("$anchor_idx")
acc="$ANCHOR_BYTES"
if [ "$ANCHOR_BYTES" -gt "$CAP_BYTES" ]; then
  warn "巨文件锚点 $(human "$ANCHOR_BYTES") 已超 size-cap $(human "$CAP_BYTES")；旗舰数据集必须含巨文件特征，仍纳入并撑破 cap（显式告警，非静默截断）。"
fi

declare -a SKIPPED
for idx in "${REST_ORDER[@]}"; do
  tb="${CAND_TOTAL[$idx]}"
  if [ $((acc + tb)) -le "$CAP_BYTES" ]; then
    SELECTED+=("$idx")
    acc=$((acc + tb))
  else
    SKIPPED+=("$idx")
  fi
done

# ── 执行拷贝 ───────────────────────────────────────────────────
log "选中 ${#SELECTED[@]} 个 project 目录，预计 $(human "$acc")："
for idx in "${SELECTED[@]}"; do
  printf '[fetch-claude]   + %-60s %s\n' "$(basename "${CAND_DIR[$idx]}")" "$(human "${CAND_TOTAL[$idx]}")"
done
for idx in "${SELECTED[@]}"; do
  copy_one "${CAND_DIR[$idx]}"
done

# ── 跳过清单（显式打印，不静默）────────────────────────────────
if [ "${#SKIPPED[@]}" -gt 0 ]; then
  skipped_bytes=0
  log "跳过 ${#SKIPPED[@]} 个目录（超 cap 后未纳入）："
  for idx in "${SKIPPED[@]}"; do
    skipped_bytes=$((skipped_bytes + CAND_TOTAL[idx]))
    printf '[fetch-claude]   - %-60s %s\n' "$(basename "${CAND_DIR[$idx]}")" "$(human "${CAND_TOTAL[$idx]}")"
  done
  log "跳过合计: $(human "$skipped_bytes")（如需全部用 --full）"
fi

# ── 实际落地统计 ───────────────────────────────────────────────
real_files=$(find "$DEST" -type f 2>/dev/null | wc -l)
real_bytes=$(dir_bytes "$DEST")
log "完成（subset）: $real_files 文件, $(human "$real_bytes") 落在 $DEST"
log "用于 run-suite 时，把它拷进各条件挂载点的 fio-work 或直接对其跑真实负载（grep -r / git status 等，见 overview §4.3）。"
