#!/usr/bin/env bash
# crash-test.sh — T1 进程级崩溃一致性 harness（kill -9 守护于写中途）。
#
# 验证目标负载的硬可靠性需求（丢/损会话日志不可接受，ROADMAP T1）：
#   1. durability：fsync 返回成功的 append 行，守护被 kill -9 后**必须全部存活**。
#   2. fail-closed：崩溃**绝不**产生损坏/可静默错读的 archive——重挂后每一行字节完好、
#      构成连续前缀（无撕裂行、无空洞、无垃圾尾巴），且重挂本身不报损坏。
#
# 与现有单元级崩溃测试（archive.rs `updater_未提交即崩溃_*` / `reuse_*`）互补：那些测格式层
# 构造性 fail-closed，本脚本测**真实守护进程被硬杀**的端到端路径（FUSE + Core 尾块缓冲 +
# shadow temp+rename + footer sync）。
#
# 用法：bash bench/scripts/crash-test.sh [行数] [kill前秒数]
# 退出码 0=PASS，非 0=FAIL。不改系统、只用自建临时目录、绝不通配 rm。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$BENCH_DIR/.." && pwd)"
BIN="${BIN:-$REPO_DIR/target/release/zipfs}"

LINES="${1:-100000}"          # 写够多让 kill 大概率落在写中途
KILL_AFTER="${2:-1.5}"        # 守护起来后多少秒 kill -9
CHUNK_SIZE="${CHUNK_SIZE:-1048576}"

# 自建唯一临时工作区（drop 时只删自己建的目录，路径已知非空、非通配）。
WORK="$(mktemp -d -t zipfs-crash-XXXXXX)"
BACKING="$WORK/backing"
MNT="$WORK/mnt"
MNT2="$WORK/mnt2"          # 重挂用全新挂载点，避开 kill -9 残留的 stale endpoint（测试假象）
PROGRESS="$WORK/acked.log"    # 写者每次 fsync 成功后追加的「已确认 seq」（落本地盘，非挂载点）
mkdir -p "$BACKING" "$MNT" "$MNT2"

log()  { printf '[crash-test] %s\n' "$*"; }
fail() { printf '[crash-test] FAIL: %s\n' "$*" >&2; cleanup; exit 1; }

DAEMON_PID=""
cleanup() {
  [ -n "$DAEMON_PID" ] && kill -9 "$DAEMON_PID" 2>/dev/null
  for m in "$MNT" "$MNT2"; do fusermount3 -u "$m" 2>/dev/null || fusermount -u "$m" 2>/dev/null || true; done
  # 只删本脚本在 mktemp 下建的唯一目录。
  case "$WORK" in /tmp/zipfs-crash-*|"$TMPDIR"zipfs-crash-*) rm -rf "$WORK" 2>/dev/null ;; esac
}
trap cleanup EXIT

[ -x "$BIN" ] || fail "未找到 zipfs 二进制：$BIN（先 cargo build --release -p zipfs）"
[ -c /dev/fuse ] || { log "SKIP：/dev/fuse 不存在，FUSE 不可用"; exit 0; }

# 确定性行内容：seq + 可压缩 payload，便于逐行核对字节完好。
line_for() { printf '{"seq":%d,"payload":"%s"}\n' "$1" "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; }

mount_shadow() {
  local mnt="$1"
  "$BIN" --backend shadow --backing "$BACKING" --mountpoint "$mnt" --chunk-size "$CHUNK_SIZE" \
    >"$WORK/daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 50); do
    mountpoint -q "$mnt" 2>/dev/null && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || { cat "$WORK/daemon.log" >&2; return 1; }
    sleep 0.1
  done
  return 1
}

# 等挂载点彻底卸载（kill -9 后的 stale mount 需 fusermount3 -u 清理，且要等其完成再重挂，
# 否则同挂载点的二次 mount 会撞上未清干净的 stale 连接，lookup 失败）。
wait_unmounted() {
  for _ in $(seq 1 50); do
    mountpoint -q "$MNT" 2>/dev/null || return 0
    fusermount3 -u "$MNT" 2>/dev/null || fusermount -u "$MNT" 2>/dev/null || true
    sleep 0.1
  done
  return 1
}

log "工作区：$WORK（chunk=$CHUNK_SIZE）"
mount_shadow "$MNT" || fail "首次挂载失败"
log "守护已挂载 PID=$DAEMON_PID，开始 append+fsync 写入（kill 于 ${KILL_AFTER}s 后）"

# 写者：逐行 append + 每行 fsync；fsync 成功后把 seq 记到本地 PROGRESS（守护若被杀，写者随之报错退出）。
python3 - "$MNT/session.jsonl" "$PROGRESS" "$LINES" <<'PY' &
import os, sys
path, progress, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
payload = "A" * 32
try:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    pf = open(progress, "a", buffering=1)
    for i in range(n):
        line = ('{"seq":%d,"payload":"%s"}\n' % (i, payload)).encode()
        os.write(fd, line)
        os.fsync(fd)          # 返回成功即「已确认 durable」
        pf.write("%d\n" % i)  # 记录到本地盘
        pf.flush(); os.fsync(pf.fileno())
except Exception as e:
    sys.stderr.write("writer 终止：%s\n" % e)  # 守护被杀后写/ fsync 报错，正常
PY
WRITER_PID=$!

sleep "$KILL_AFTER"
log "kill -9 守护 PID=$DAEMON_PID（模拟掉电/崩溃于写中途）"
kill -9 "$DAEMON_PID" 2>/dev/null || fail "kill 守护失败（可能已退出）"
wait "$WRITER_PID" 2>/dev/null
DAEMON_PID=""
wait_unmounted || fail "stale mount 未能卸载干净"

# 已确认 durable 的最高 seq（写者 fsync 成功记录的最后一行）。
ACKED=-1
[ -s "$PROGRESS" ] && ACKED="$(tail -n1 "$PROGRESS")"
log "崩溃前已 fsync 确认到 seq=$ACKED"
[ "$ACKED" -ge 0 ] || fail "崩溃前无任何已确认行（写入太慢或挂载异常），无法验证 durability"

# 重挂，验证恢复。
log "重新挂载，验证恢复…"
mount_shadow "$MNT2" || fail "重挂失败——崩溃后 archive 不可打开（违反 fail-closed：应能开为合法前缀）"

RECOVERED="$MNT2/session.jsonl"

# 逐行核验：读回内容必须是 0..S-1 的连续前缀，每行字节完好，且 S-1 >= ACKED（durability）。
# open 对瞬时就绪竞态（FUSE 重挂后首次访问偶发 ENOENT/ENOTCONN）重试——文件在 backing 已 durable，
# 重试几次必出；始终失败才是真 durability 违反。
python3 - "$RECOVERED" "$ACKED" <<'PY' || fail "恢复校验失败（见上）"
import os, sys, json, time
path, acked = sys.argv[1], int(sys.argv[2])
payload = "A" * 32
data = None
deadline = time.time() + 5.0
while time.time() < deadline:
    try:
        with open(path, "rb") as f:
            data = f.read()
        break
    except OSError:
        time.sleep(0.1)        # FUSE 重挂就绪竞态，重试
if data is None:
    sys.stderr.write("重挂后会话文件始终不可读（违反 durability）\n"); sys.exit(1)
# 文件必须以换行结尾且无半行（fail-closed：不得有撕裂尾巴）。
lines = data.split(b"\n")
if lines and lines[-1] == b"":
    lines = lines[:-1]            # 末尾换行后的空段
else:
    # 最后一行没有换行结尾 = 撕裂行（半行落盘），违反 fail-closed。
    sys.stderr.write("撕裂行：恢复文件末行无换行结尾（半行落盘）\n"); sys.exit(1)
survived = len(lines)
for i, raw in enumerate(lines):
    expect = ('{"seq":%d,"payload":"%s"}' % (i, payload)).encode()
    if raw != expect:
        sys.stderr.write("第 %d 行字节不匹配（损坏/错位）：%r\n" % (i, raw[:80])); sys.exit(1)
    json.loads(raw)              # 双保险：每行可解析
if survived - 1 < acked:
    sys.stderr.write("durability 违反：恢复 %d 行，但崩溃前已 fsync 确认到 seq=%d\n" % (survived, acked)); sys.exit(1)
print("[crash-test] 恢复 %d 行，全部字节完好且连续；durability 覆盖已确认的 seq 0..%d" % (survived, acked))
PY

log "PASS：durability（fsync 行全存活）+ fail-closed（零损坏/撕裂/空洞）均成立"
cleanup
trap - EXIT
exit 0
