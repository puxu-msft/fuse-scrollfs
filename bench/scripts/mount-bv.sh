#!/usr/bin/env bash
# mount-bv.sh — 用 scrollz 布局 V（container，redb 全包容器）读写挂载。
#
# BV = FUSE + zstd 分块，redb 容器（单文件 ACID B-tree 当变长 blob 分配器），读写。
# 见 docs/00-overview.md §4.1、docs/01-scrollz-design.md §6.1。
# 用法：scrollz --backend container --backing <redb 容器文件> --mountpoint <mnt> --chunk-size 65536。
#
# 关键差异（与 BS）：container 的 --backing 是【redb 容器文件路径】（不存在则创建），
# 不是目录。挂载点照常是目录。
#
# 用法:
#   bash bench/scripts/mount-bv.sh                 # 默认容器/挂载点，后台挂载，64KiB 块
#   BACKING=/path/on/ext4/scrollz.redb MNT=/path/mnt bash .../mount-bv.sh
#   FOREGROUND=1 bash .../mount-bv.sh              # 前台运行（Ctrl-C 卸载，便于调试）
#
# 参数（环境变量）:
#   BACKING     redb 容器文件路径（必须在 ext4 上）   默认 bench/.bv-backing/scrollz.redb
#   MNT         BV 挂载点                              默认 bench/.mnt/bv
#   BIN         scrollz 二进制路径                       默认 target/release/scrollz
#   CHUNK_SIZE  逻辑块大小（字节）                     默认 65536（64KiB，§6.1 裁决）
#   FOREGROUND  置 1 则前台阻塞运行；否则后台启动并写 PID 文件
#
# 安全/健壮:
#   - 二进制不存在 → 优雅报错，提示先 `cargo build --release`（不擅自 build）。
#   - 容器父目录自动创建（在 bench 内）；挂载点自动创建。
#   - 已是挂载点 → 拒绝重复挂载，提示先 umount-bv.sh。
#   - 不 sudo、不 modprobe：FUSE 普通用户即可挂。

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$BENCH_DIR/.." && pwd)"

BACKING="${BACKING:-$BENCH_DIR/.bv-backing/scrollz.redb}"
MNT="${MNT:-$BENCH_DIR/.mnt/bv}"
BIN="${BIN:-$REPO_DIR/target/release/scrollz}"
CHUNK_SIZE="${CHUNK_SIZE:-65536}"
FOREGROUND="${FOREGROUND:-0}"
PIDFILE="$BENCH_DIR/.mnt/bv.pid"

log()  { printf '[mount-bv] %s\n' "$*"; }
warn() { printf '[mount-bv] WARN: %s\n' "$*" >&2; }
die()  { printf '[mount-bv] ERROR: %s\n' "$*" >&2; exit 1; }

# ── 二进制存在性 ───────────────────────────────────────────────
if [ ! -x "$BIN" ]; then
  cat >&2 <<EOF
[mount-bv] ERROR: 未找到 scrollz 二进制: $BIN
  请先构建 scrollz（crates/scrollz）:
      ( cd "$REPO_DIR" && cargo build --release -p scrollz )
  产物应为 target/release/scrollz。构建后重跑本脚本。
EOF
  exit 1
fi

# ── FUSE 基本能力探测 ──────────────────────────────────────────
[ -c /dev/fuse ] || die "/dev/fuse 不存在——FUSE 不可用，无法挂 BV。"
command -v fusermount3 >/dev/null 2>&1 || command -v fusermount >/dev/null 2>&1 \
  || warn "未找到 fusermount3/fusermount——卸载可能需手动。"

# ── 容器父目录（应在 ext4 上）──────────────────────────────────
BACKING_DIR="$(dirname "$BACKING")"
mkdir -p "$BACKING_DIR" || die "无法创建容器父目录: $BACKING_DIR"
log "backing(redb 容器文件): $BACKING（不存在则由 scrollz 创建）"

# ── 挂载点幂等 ─────────────────────────────────────────────────
mkdir -p "$MNT" || die "无法创建挂载点: $MNT"
if mountpoint -q "$MNT" 2>/dev/null; then
  die "$MNT 已是挂载点。若要重挂，请先 bash bench/scripts/umount-bv.sh"
fi

# ── 前台模式 ───────────────────────────────────────────────────
if [ "$FOREGROUND" = "1" ]; then
  log "前台挂载（Ctrl-C 卸载）: $BIN --backend container --backing $BACKING --mountpoint $MNT --chunk-size $CHUNK_SIZE"
  exec "$BIN" --backend container --backing "$BACKING" --mountpoint "$MNT" --chunk-size "$CHUNK_SIZE"
fi

# ── 后台模式 ───────────────────────────────────────────────────
mkdir -p "$(dirname "$PIDFILE")"
log "后台挂载: $BIN --backend container --backing $BACKING --mountpoint $MNT --chunk-size $CHUNK_SIZE"
"$BIN" --backend container --backing "$BACKING" --mountpoint "$MNT" --chunk-size "$CHUNK_SIZE" \
  >"$BENCH_DIR/.mnt/bv.log" 2>&1 &
bg_pid=$!
printf '%s\n' "$bg_pid" > "$PIDFILE"

for _ in $(seq 1 50); do
  if mountpoint -q "$MNT" 2>/dev/null; then
    log "挂载成功: $MNT (pid=$bg_pid, pidfile=$PIDFILE)"
    log "纳入 run-suite: CONDITIONS=\"... BV=$MNT\" bash bench/scripts/run-suite.sh"
    log "卸载: bash bench/scripts/umount-bv.sh"
    exit 0
  fi
  if ! kill -0 "$bg_pid" 2>/dev/null; then
    warn "守护进程已退出，挂载失败。日志:"
    sed 's/^/[mount-bv]   /' "$BENCH_DIR/.mnt/bv.log" >&2 2>/dev/null || true
    rm -f -- "$PIDFILE"
    die "BV 挂载失败（见上）。"
  fi
  sleep 0.1
done

warn "等待挂载点超时（5s），但守护仍在运行（pid=$bg_pid）。请手动检查: mountpoint $MNT"
exit 1
