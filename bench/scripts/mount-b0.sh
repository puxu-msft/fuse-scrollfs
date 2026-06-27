#!/usr/bin/env bash
# mount-b0.sh — 用 zipfs 透传二进制把一个 backing 目录挂成 B0 挂载点。
#
# B0 = FUSE 透传（不压缩），隔离「纯 FUSE 税」（见 docs/00-overview.md §4.1）。
# 二进制来自 fuse/ crate：cargo build --release，产物 fuse/target/release/zipfs。
# passthrough 用法：zipfs --backing <dir> --mountpoint <mnt>（mount2 阻塞至卸载）。
#
# 用法:
#   bash bench/scripts/mount-b0.sh                 # 用默认 backing/挂载点，后台挂载
#   BACKING=/path/on/ext4 MNT=/path/mnt bash .../mount-b0.sh
#   FOREGROUND=1 bash .../mount-b0.sh              # 前台运行（Ctrl-C 卸载，便于调试）
#
# 参数（环境变量）:
#   BACKING     后端目录（必须在 ext4 上；见 §4.5 受控变量）  默认 bench/.b0-backing
#   MNT         B0 挂载点                                       默认 bench/.mnt/b0
#   BIN         zipfs 二进制路径                                默认 fuse/target/release/zipfs
#   FOREGROUND  置 1 则前台阻塞运行；否则后台启动并写 PID 文件
#
# 安全/健壮:
#   - 二进制不存在 → 优雅报错，提示先 `cargo build --release`（不擅自 build）。
#   - backing 目录自动创建（在 bench 内）；挂载点自动创建。
#   - 已是挂载点 → 拒绝重复挂载，提示先 umount-b0.sh。
#   - 不 sudo、不 modprobe：FUSE 普通用户即可挂（/dev/fuse + fusermount3）。

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$BENCH_DIR/.." && pwd)"

BACKING="${BACKING:-$BENCH_DIR/.b0-backing}"
MNT="${MNT:-$BENCH_DIR/.mnt/b0}"
BIN="${BIN:-$REPO_DIR/fuse/target/release/zipfs}"
FOREGROUND="${FOREGROUND:-0}"
PIDFILE="$BENCH_DIR/.mnt/b0.pid"

log()  { printf '[mount-b0] %s\n' "$*"; }
warn() { printf '[mount-b0] WARN: %s\n' "$*" >&2; }
die()  { printf '[mount-b0] ERROR: %s\n' "$*" >&2; exit 1; }

# ── 二进制存在性（不擅自 build，优雅提示）──────────────────────
if [ ! -x "$BIN" ]; then
  cat >&2 <<EOF
[mount-b0] ERROR: 未找到 zipfs 透传二进制: $BIN
  请先在 fuse/ crate 构建（约 18s）:
      ( cd "$REPO_DIR/fuse" && cargo build --release )
  产物应为 fuse/target/release/zipfs。构建后重跑本脚本。
EOF
  exit 1
fi

# ── FUSE 基本能力探测（普通用户挂载需 /dev/fuse + fusermount3）──
[ -c /dev/fuse ] || die "/dev/fuse 不存在——FUSE 不可用，无法挂 B0。"
command -v fusermount3 >/dev/null 2>&1 || command -v fusermount >/dev/null 2>&1 \
  || warn "未找到 fusermount3/fusermount——卸载可能需手动；挂载若失败请检查 fuse 包。"

# ── backing 目录（应在 ext4 上）─────────────────────────────────
mkdir -p "$BACKING" || die "无法创建 backing 目录: $BACKING"
log "backing: $BACKING（确保它在 ext4 后端上，见 §4.5）"

# ── 挂载点幂等 ─────────────────────────────────────────────────
mkdir -p "$MNT" || die "无法创建挂载点: $MNT"
if mountpoint -q "$MNT" 2>/dev/null; then
  die "$MNT 已是挂载点。若要重挂，请先 bash bench/scripts/umount-b0.sh"
fi

# ── 前台模式：阻塞运行 ─────────────────────────────────────────
if [ "$FOREGROUND" = "1" ]; then
  log "前台挂载（Ctrl-C 卸载）: $BIN --backing $BACKING --mountpoint $MNT"
  exec "$BIN" --backing "$BACKING" --mountpoint "$MNT"
fi

# ── 后台模式：启动并等待挂载点就绪 ─────────────────────────────
mkdir -p "$(dirname "$PIDFILE")"
log "后台挂载: $BIN --backing $BACKING --mountpoint $MNT"
"$BIN" --backing "$BACKING" --mountpoint "$MNT" \
  >"$BENCH_DIR/.mnt/b0.log" 2>&1 &
bg_pid=$!
printf '%s\n' "$bg_pid" > "$PIDFILE"

# 等挂载点真正出现（mount2 异步），最多 ~5s。
for _ in $(seq 1 50); do
  if mountpoint -q "$MNT" 2>/dev/null; then
    log "挂载成功: $MNT (pid=$bg_pid, pidfile=$PIDFILE)"
    log "纳入 run-suite: CONDITIONS=\"... B0=$MNT\" bash bench/scripts/run-suite.sh"
    log "卸载: bash bench/scripts/umount-b0.sh"
    exit 0
  fi
  # 守护若已退出，挂载必然失败。
  if ! kill -0 "$bg_pid" 2>/dev/null; then
    warn "守护进程已退出，挂载失败。日志:"
    sed 's/^/[mount-b0]   /' "$BENCH_DIR/.mnt/b0.log" >&2 2>/dev/null || true
    rm -f -- "$PIDFILE"
    die "B0 挂载失败（见上）。"
  fi
  sleep 0.1
done

warn "等待挂载点超时（5s），但守护仍在运行（pid=$bg_pid）。请手动检查: mountpoint $MNT"
exit 1
