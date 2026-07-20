#!/usr/bin/env bash
# run-suite.sh — 对一组「条件 → 挂载点」运行全部 fio job，输出 JSON 到 results/<日期>/<条件>/。
#
# 条件映射通过 CONDITIONS 环境变量传入，格式: "名称=挂载点" 以空格或换行分隔。
# 例:
#   CONDITIONS="C0=/mnt/scrollz-c0 A=/mnt/scrollz-btrfs B0=bench/.mnt/b0 B2=/mnt/scrollz-b2" \
#     bash bench/scripts/run-suite.sh
#
# B0 挂载点由 mount-b0.sh 准备（FUSE 透传二进制），卸载用 umount-b0.sh。
#
# 不传则用一组默认占位映射（多数挂载点不存在 → 会被优雅跳过并显式 log）。
#
# 行为:
#   - 某条件挂载点不存在/不可写 → 打印 "SKIP <条件>: <原因>" 并继续，绝不静默截断。
#   - 每个 fio job 前尝试冷缓存: sync + drop_caches（需 root）。无权限则告警并降级为热缓存。
#   - 结果落 results/<UTC日期>/<条件>/<job>.json，纯 JSON 便于 collect.py 解析。
#
# 依赖: fio。可选: sudo（冷缓存）。

set -uo pipefail
# 注意：不用 set -e。单个 fio 失败不应让整批条件中断——失败会被记录并继续。

# ── 路径定位 ───────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIO_DIR="$BENCH_DIR/fio"
RESULTS_ROOT="$BENCH_DIR/results"

# 日期占位目录（UTC，便于跨时区归档一致）。可用 RUN_TAG 覆盖。
RUN_TAG="${RUN_TAG:-$(date -u +%Y%m%d-%H%M%S)}"
OUT_DIR="$RESULTS_ROOT/$RUN_TAG"

# fio job 跑的顺序（写在前，给随机读/写留下数据文件）。
FIO_JOBS=(seq-write rand-read rand-write)

# 轮数（默认 1）。用户要求基准默认单轮、单项目标 ~1-5 分钟，减少测试量。
# 需要稳定性统计时再加轮数：ROUNDS=3 bash run-suite.sh。每轮结果落 <RUN_TAG>/r<N>/<条件>/。
ROUNDS="${ROUNDS:-1}"

# 默认条件映射（占位；真实运行请通过 CONDITIONS 覆盖）。
# B0 指向 mount-b0.sh 的默认挂载点（bench/.mnt/b0）；未挂载则按现有逻辑优雅跳过。
# B0 的卸载走 umount-b0.sh（FUSE 透传，非 btrfs，不用 teardown.sh）。
DEFAULT_CONDITIONS="C0=$BENCH_DIR/.mnt/c0 A=/mnt/scrollz-btrfs B0=$BENCH_DIR/.mnt/b0 B2=/mnt/scrollz-b2"
CONDITIONS="${CONDITIONS:-$DEFAULT_CONDITIONS}"

log()  { printf '[run-suite] %s\n' "$*"; }
warn() { printf '[run-suite] WARN: %s\n' "$*" >&2; }
die()  { printf '[run-suite] ERROR: %s\n' "$*" >&2; exit 1; }

command -v fio >/dev/null 2>&1 || die "fio 未安装（sudo apt install fio）"
[ -d "$FIO_DIR" ] || die "缺少 fio job 目录: $FIO_DIR"

# ── 冷缓存能力探测（一次性）────────────────────────────────────
DROP_CACHES_MODE="none"   # none | root | sudo
if [ "$(id -u)" -eq 0 ]; then
  DROP_CACHES_MODE="root"
elif command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
  DROP_CACHES_MODE="sudo"
fi
if [ "$DROP_CACHES_MODE" = "none" ]; then
  warn "无 root / 无免密 sudo —— 冷缓存项降级为热缓存（结果会偏乐观，已显式告警）。"
fi

drop_caches() {
  # 冷缓存: 先 sync，再写 3 到 drop_caches。失败不致命，降级为热缓存。
  case "$DROP_CACHES_MODE" in
    root)
      sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null \
        && log "  冷缓存: drop_caches OK" \
        || warn "  drop_caches 写入失败，本项为热缓存" ;;
    sudo)
      sync; sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null \
        && log "  冷缓存: drop_caches OK (sudo)" \
        || warn "  sudo drop_caches 失败，本项为热缓存" ;;
    none)
      sync; log "  热缓存模式（无 drop_caches 权限）" ;;
  esac
}

mkdir -p "$OUT_DIR"
log "结果目录: $OUT_DIR"
log "条件映射: $CONDITIONS"
log "轮数: $ROUNDS（默认 1；加轮数用 ROUNDS=N）"

ran_any=0
for round in $(seq 1 "$ROUNDS"); do
  # 单轮（默认）不加子目录，保持向后兼容；多轮时每轮落 r<N>/ 子目录。
  if [ "$ROUNDS" -gt 1 ]; then
    round_out="$OUT_DIR/r$round"
    log "########## ROUND $round / $ROUNDS ##########"
  else
    round_out="$OUT_DIR"
  fi
  for pair in $CONDITIONS; do
    name="${pair%%=*}"
    mnt="${pair#*=}"

    if [ -z "$name" ] || [ -z "$mnt" ] || [ "$name" = "$pair" ]; then
      warn "无法解析条件项 '$pair'（应为 名称=挂载点）—— 跳过"
      continue
    fi

    # ── 挂载点可用性检查（不存在则优雅跳过）──────────────────────
    if [ ! -d "$mnt" ]; then
      log "SKIP $name: 挂载点目录不存在 ($mnt)"
      continue
    fi
    if ! mountpoint -q "$mnt" 2>/dev/null; then
      # 允许用普通目录跑（如 C0 裸 ext4 子目录），但提示它不是独立挂载点。
      warn "$name: $mnt 不是独立挂载点，按普通目录处理（C0 可接受；A/B 通常应是挂载点）"
    fi

    # fio 需要在目标里建数据文件 → 必须可写。
    testfile="$mnt/.run-suite-write-test.$$"
    if ! ( : > "$testfile" ) 2>/dev/null; then
      log "SKIP $name: 挂载点不可写 ($mnt)"
      continue
    fi
    rm -f -- "$testfile"   # 明确单文件，无通配符

    cond_out="$round_out/$name"
    # fio 工作目录: 在挂载点下开专属子目录，避免污染挂载点根。
    workdir="$mnt/fio-work"
    mkdir -p "$cond_out" "$workdir"

    log "=== 条件 $name @ $mnt (workdir=$workdir) ==="
    ran_any=1

    for job in "${FIO_JOBS[@]}"; do
      jobfile="$FIO_DIR/$job.fio"
      if [ ! -f "$jobfile" ]; then
        warn "  缺少 job 文件: $jobfile —— 跳过该 job"
        continue
      fi
      outjson="$cond_out/$job.json"
      log "  运行 fio: $job -> $outjson"
      drop_caches
      # FIO_EXTRA：可选额外 fio 命令行参数（空格分隔），用于首轮加运行时上限等，
      # 例如 FIO_EXTRA="--runtime=30" 把每个 job 封顶 30s（size 先到则提前结束）。
      # 不改 job 文件本身，保证模板可复现。
      # shellcheck disable=SC2086
      # DIR 供 fio job 的 directory=${DIR} 使用。
      if DIR="$workdir" fio "$jobfile" ${FIO_EXTRA:-} \
          --output-format=json \
          --output="$outjson" >/dev/null 2>"$cond_out/$job.stderr"; then
        log "  完成: $job"
      else
        warn "  fio 失败: $name/$job（详见 $cond_out/$job.stderr）"
      fi
    done

    # 留一份压缩比快照线索（du）。btrfs 真实压缩比另用 compsize（见 README）。
    du -sh "$workdir" 2>/dev/null | sed 's/^/[run-suite]   workdir 占用: /' || true
  done
done

if [ "$ran_any" -eq 0 ]; then
  warn "没有任何条件可运行（全部挂载点缺失/不可写）。请检查 CONDITIONS 与挂载状态。"
fi

log "全部条件处理完毕。汇总: python3 bench/scripts/collect.py $OUT_DIR"
