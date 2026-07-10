#!/usr/bin/env bash
# crash-test-container.sh — container 后端 fsync 后崩溃 smoke（docs/05 §5 / 任务 3.3）。
#
# ContainerStore（布局 V）把 durability 100% 委托 redb（fsync/flush → 合并挂起写为一个
# `Durability::Immediate` redb 事务 commit）。本 smoke 只验**委托正确**：fsync 返回成功的行，
# 守护被 kill -9 后重开 redb 容器必全部存活——**不重测 redb 引擎本身的崩溃恢复**（非目标，§1）。
# 与 crash-test.sh（shadow 端到端）互补：那条验 archive 字节流提交协议，本条验 container→redb 委托。
#
# 进程级 kill -9（非掉电），只需 /dev/fuse（门控同 crash-test.sh）。安全：只用自建 mktemp 工作区，
# cleanup 只卸自己的挂载、rm -rf 仅 mktemp 目录（case 守卫）；绝不通配 rm、绝不动系统挂载。
#
# 用法：bash bench/scripts/crash-test-container.sh [行数] [kill前秒数]
# 退出码 0=PASS 或 SKIP，非 0=FAIL。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BIN="${BIN:-$REPO_DIR/target/release/zipfs}"
LINES="${1:-40000}"
KILL_AFTER="${2:-1.5}"
CHUNK_SIZE="${CHUNK_SIZE:-65536}"

log()  { printf '[crash-ctr] %s\n' "$*"; }
skip() { printf '[crash-ctr] SKIP：%s\n' "$*"; exit 0; }

WORK="$(mktemp -d -t zipfs-crashctr-XXXXXX)"
CONTAINER="$WORK/container.redb"   # container 后端 backing = 单个 redb 容器文件
MNT="$WORK/mnt"; MNT2="$WORK/mnt2"
PROGRESS="$WORK/acked.log"
mkdir -p "$MNT" "$MNT2"
DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill -9 "$DAEMON_PID" 2>/dev/null
  for m in "$MNT" "$MNT2"; do fusermount3 -u "$m" 2>/dev/null || fusermount -u "$m" 2>/dev/null || true; done
  case "$WORK" in
    /tmp/zipfs-crashctr-*|"${TMPDIR:-/tmp/}"zipfs-crashctr-*) rm -rf "$WORK" 2>/dev/null ;;
  esac
}
trap cleanup EXIT
fail() { printf '[crash-ctr] FAIL：%s\n' "$*" >&2; exit 1; }

[ -x "$BIN" ] || skip "未找到 zipfs 二进制：$BIN（先 cargo build --release -p zipfs）"
[ -c /dev/fuse ] || skip "/dev/fuse 不存在，FUSE 不可用"

mount_ctr() {
  "$BIN" --backend container --backing "$CONTAINER" --mountpoint "$1" --chunk-size "$CHUNK_SIZE" \
    >"$WORK/daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 50); do
    mountpoint -q "$1" 2>/dev/null && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || { cat "$WORK/daemon.log" >&2; return 1; }
    sleep 0.1
  done
  return 1
}
wait_unmounted() {
  for _ in $(seq 1 50); do
    mountpoint -q "$MNT" 2>/dev/null || return 0
    fusermount3 -u "$MNT" 2>/dev/null || fusermount -u "$MNT" 2>/dev/null || true
    sleep 0.1
  done
  return 1
}

log "工作区：$WORK（container=$CONTAINER chunk=$CHUNK_SIZE）"
mount_ctr "$MNT" || fail "首次挂载 container 失败"

# 写者：逐行 append+fsync，每行 fsync 成功记 acked（fsync → redb Immediate commit）。
python3 - "$MNT/session.jsonl" "$PROGRESS" "$LINES" <<'PY' &
import os, sys
path, progress, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
payload = "A" * 32
try:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    pf = open(progress, "a", buffering=1)
    for i in range(n):
        os.write(fd, ('{"seq":%d,"payload":"%s"}\n' % (i, payload)).encode())
        os.fsync(fd)
        pf.write("%d\n" % i); pf.flush(); os.fsync(pf.fileno())
except Exception as e:
    sys.stderr.write("writer 终止：%s\n" % e)
PY
WRITER_PID=$!

sleep "$KILL_AFTER"
log "kill -9 守护 PID=$DAEMON_PID（崩溃于写中途）"
kill -9 "$DAEMON_PID" 2>/dev/null || fail "kill 守护失败（可能已退出）"
wait "$WRITER_PID" 2>/dev/null
DAEMON_PID=""
wait_unmounted || fail "stale mount 未卸载干净"

ACKED=-1
[ -s "$PROGRESS" ] && ACKED="$(tail -n1 "$PROGRESS")"
log "崩溃前已 fsync 确认到 seq=$ACKED"
[ "$ACKED" -ge 0 ] || fail "崩溃前无已确认行，无法验证 durability"

log "重开 redb 容器，验证委托 durability…"
mount_ctr "$MNT2" || fail "重挂失败——redb 容器不可重开"

python3 - "$MNT2/session.jsonl" "$ACKED" <<'PY' || fail "恢复校验失败（见上）"
import os, sys, json, time
path, acked = sys.argv[1], int(sys.argv[2])
payload = "A" * 32
data = None
deadline = time.time() + 5.0
while time.time() < deadline:
    try:
        with open(path, "rb") as f: data = f.read()
        break
    except OSError: time.sleep(0.1)
if data is None:
    sys.stderr.write("重开后会话文件始终不可读（委托 durability 失败）\n"); sys.exit(1)
lines = data.split(b"\n")
if lines and lines[-1] == b"":
    lines = lines[:-1]
else:
    sys.stderr.write("撕裂行：末行无换行结尾\n"); sys.exit(1)
for i, raw in enumerate(lines):
    if raw != ('{"seq":%d,"payload":"%s"}' % (i, payload)).encode():
        sys.stderr.write("第 %d 行字节不匹配（损坏/错位）：%r\n" % (i, raw[:80])); sys.exit(1)
    json.loads(raw)
survived = len(lines)
if survived - 1 < acked:
    sys.stderr.write("委托 durability 违反：恢复 %d 行，但已 fsync 确认到 seq=%d\n" % (survived, acked)); sys.exit(1)
print("[crash-ctr] 重开 redb 恢复 %d 行，全部字节完好且连续；覆盖已确认 seq 0..%d" % (survived, acked))
PY

log "PASS：container fsync 后 kill，redb 重开数据在（委托正确）"
exit 0
