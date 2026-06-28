#!/usr/bin/env bash
# mount-bs.sh — 用 zipfs 布局 S（shadow，影子树/每文件压缩包）读写挂载。
#
# BS = FUSE + zstd 分块（每文件 archive），读写。隔离「分块压缩税」（见 docs/00-overview.md §4.1）。
# 二进制来自 fuse/ crate：cargo build --release，产物 fuse/target/release/zipfs。
# 用法：zipfs --backend shadow --backing <dir> --mountpoint <mnt> --chunk-size 65536。
#
# 用法:
#   bash bench/scripts/mount-bs.sh                 # 默认 backing/挂载点，后台挂载，64KiB 块
#   BACKING=/path/on/ext4 MNT=/path/mnt bash .../mount-bs.sh
#   FOREGROUND=1 bash .../mount-bs.sh              # 前台运行（Ctrl-C 卸载，便于调试）
#
# 参数（环境变量）:
#   BACKING     后端 archive 树目录（必须在 ext4 上）  默认 bench/.bs-backing
#   MNT         BS 挂载点                               默认 bench/.mnt/bs
#   BIN         zipfs 二进制路径                        默认 fuse/target/release/zipfs
#   CHUNK_SIZE  逻辑块大小（字节）                      默认 65536（64KiB，§6.1 裁决）
#   FOREGROUND  置 1 则前台阻塞运行；否则后台启动并写 PID 文件
#
# 安全/健壮:
#   - 二进制不存在 → 优雅报错，提示先 `cargo build --release`（不擅自 build）。
#   - backing 目录自动创建（在 bench 内）；挂载点自动创建。
#   - 已是挂载点 → 拒绝重复挂载，提示先 umount-bs.sh。
#   - 不 sudo、不 modprobe：FUSE 普通用户即可挂（/dev/fuse + fusermount3）。

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$BENCH_DIR/.." && pwd)"

BACKING="${BACKING:-$BENCH_DIR/.bs-backing}"
MNT="${MNT:-$BENCH_DIR/.mnt/bs}"
BIN="${BIN:-$REPO_DIR/fuse/target/release/zipfs}"
CHUNK_SIZE="${CHUNK_SIZE:-65536}"
FOREGROUND="${FOREGROUND:-0}"
PIDFILE="$BENCH_DIR/.mnt/bs.pid"

log()  { printf '[mount-bs] %s\n' "$*"; }
warn() { printf '[mount-bs] WARN: %s\n' "$*" >&2; }
die()  { printf '[mount-bs] ERROR: %s\n' "$*" >&2; exit 1; }

# ── 二进制存在性（不擅自 build，优雅提示）──────────────────────
if [ ! -x "$BIN" ]; then
  cat >&2 <<EOF
[mount-bs] ERROR: 未找到 zipfs 二进制: $BIN
  请先在 fuse/ crate 构建:
      ( cd "$REPO_DIR/fuse" && cargo build --release )
  产物应为 fuse/target/release/zipfs。构建后重跑本脚本。
EOF
  exit 1
fi

# ── FUSE 基本能力探测 ──────────────────────────────────────────
[ -c /dev/fuse ] || die "/dev/fuse 不存在——FUSE 不可用，无法挂 BS。"
command -v fusermount3 >/dev/null 2>&1 || command -v fusermount >/dev/null 2>&1 \
  || warn "未找到 fusermount3/fusermount——卸载可能需手动。"

# ── backing 目录（应在 ext4 上）─────────────────────────────────
mkdir -p "$BACKING" || die "无法创建 backing 目录: $BACKING"
log "backing(archive 树): $BACKING（确保在 ext4 上）"

# ── 挂载点幂等 ─────────────────────────────────────────────────
mkdir -p "$MNT" || die "无法创建挂载点: $MNT"
if mountpoint -q "$MNT" 2>/dev/null; then
  die "$MNT 已是挂载点。若要重挂，请先 bash bench/scripts/umount-bs.sh"
fi

# ── 前台模式 ───────────────────────────────────────────────────
if [ "$FOREGROUND" = "1" ]; then
  log "前台挂载（Ctrl-C 卸载）: $BIN --backend shadow --backing $BACKING --mountpoint $MNT --chunk-size $CHUNK_SIZE"
  exec "$BIN" --backend shadow --backing "$BACKING" --mountpoint "$MNT" --chunk-size "$CHUNK_SIZE"
fi

# ── 后台模式 ───────────────────────────────────────────────────
mkdir -p "$(dirname "$PIDFILE")"
log "后台挂载: $BIN --backend shadow --backing $BACKING --mountpoint $MNT --chunk-size $CHUNK_SIZE"
"$BIN" --backend shadow --backing "$BACKING" --mountpoint "$MNT" --chunk-size "$CHUNK_SIZE" \
  >"$BENCH_DIR/.mnt/bs.log" 2>&1 &
bg_pid=$!
printf '%s\n' "$bg_pid" > "$PIDFILE"

for _ in $(seq 1 50); do
  if mountpoint -q "$MNT" 2>/dev/null; then
    log "挂载成功: $MNT (pid=$bg_pid, pidfile=$PIDFILE)"
    log "纳入 run-suite: CONDITIONS=\"... BS=$MNT\" bash bench/scripts/run-suite.sh"
    log "卸载: bash bench/scripts/umount-bs.sh"
    exit 0
  fi
  if ! kill -0 "$bg_pid" 2>/dev/null; then
    warn "守护进程已退出，挂载失败。日志:"
    sed 's/^/[mount-bs]   /' "$BENCH_DIR/.mnt/bs.log" >&2 2>/dev/null || true
    rm -f -- "$PIDFILE"
    die "BS 挂载失败（见上）。"
  fi
  sleep 0.1
done

warn "等待挂载点超时（5s），但守护仍在运行（pid=$bg_pid）。请手动检查: mountpoint $MNT"
exit 1
