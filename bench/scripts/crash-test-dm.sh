#!/usr/bin/env bash
# crash-test-dm.sh — Tier 2 真实块层崩溃门（dm-flakey smoke，root 门控，docs/05 §5）。
#
# 目标：在**真实块设备**（loop + device-mapper，非 tmpfs——tmpfs 上 fsync 是 no-op，会废掉保真）
# 上证明「丢写不致命」：写中途把 dm-flakey 切到 drop_writes（静默丢弃后续写）+ kill -9 守护，重挂后
#   1. 重挂本身不报损坏（fail-closed：archive 仍开为合法连续前缀）；
#   2. drop_writes 生效**之前**已 fsync 确认的行全部存活（durable）。
# 粗粒度 smoke：证「丢写不致命」，**证不了** barrier 排序（那归 dm-log-writes，见 crash-test-dm-logwrites.sh）。
#
# 安全（用户规则 no-unconscious）：只用自建 mktemp 工作区 + 自建唯一命名的 loop/dm 设备；cleanup 只
# 卸自己的挂载、dmsetup remove 自己命名的设备、losetup -d 自己的 loop、rm -rf 仅 mktemp 目录（case 守卫）。
# **绝不**通配 rm、绝不动系统挂载/设备。
#
# 用法：sudo bash bench/scripts/crash-test-dm.sh [行数]
# 退出码 0=PASS 或 SKIP（非 root / 缺工具），非 0=FAIL。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BIN="${BIN:-$REPO_DIR/target/release/scrollz}"
LINES="${1:-20000}"
CHUNK_SIZE="${CHUNK_SIZE:-1048576}"

log()  { printf '[crash-dm] %s\n' "$*"; }
skip() { printf '[crash-dm] SKIP：%s\n' "$*"; exit 0; }

# ---- 门控（比照 crash-test.sh 的 /dev/fuse 门控，docs/05 §5）----
[ "$(id -u)" -eq 0 ] || skip "需 root（dmsetup/losetup/mount 需特权）"
[ -c /dev/fuse ] || skip "/dev/fuse 不存在，FUSE 不可用"
for t in dmsetup losetup mkfs.ext4 mountpoint fusermount3; do
  command -v "$t" >/dev/null 2>&1 || skip "缺工具：$t"
done
dmsetup targets 2>/dev/null | grep -qiw flakey || skip "内核无 dm-flakey target"
[ -x "$BIN" ] || skip "未找到 scrollz 二进制：$BIN（先 cargo build --release -p scrollz）"

# 唯一命名，避免与系统现有 dm/loop 冲突；cleanup 只认这些名字。
UNIQ="scrollzdm$$"
DM_NAME="$UNIQ"
DM_DEV="/dev/mapper/$DM_NAME"
WORK="$(mktemp -d -t scrollz-crashdm-XXXXXX)"
IMG="$WORK/backing.img"
FSMNT="$WORK/fs"          # ext4 挂载点（dm 设备上）
MNT="$WORK/mnt"           # scrollz 首次挂载点
MNT2="$WORK/mnt2"         # scrollz 重挂点
PROGRESS="$WORK/acked.log"
LOOP=""
DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill -9 "$DAEMON_PID" 2>/dev/null
  for m in "$MNT" "$MNT2"; do fusermount3 -u "$m" 2>/dev/null || true; done
  mountpoint -q "$FSMNT" 2>/dev/null && umount "$FSMNT" 2>/dev/null
  dmsetup remove "$DM_NAME" 2>/dev/null
  [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null
  # 只删 mktemp 建的唯一目录（case 守卫，绝不通配）。
  case "$WORK" in
    /tmp/scrollz-crashdm-*|"${TMPDIR:-/tmp/}"scrollz-crashdm-*) rm -rf "$WORK" 2>/dev/null ;;
  esac
}
trap cleanup EXIT
fail() { printf '[crash-dm] FAIL：%s\n' "$*" >&2; exit 1; }

mkdir -p "$FSMNT" "$MNT" "$MNT2"

# 256MiB 真实块设备：稀疏镜像 + loop。
truncate -s 256M "$IMG" || fail "创建镜像失败"
LOOP="$(losetup --find --show "$IMG")" || fail "losetup 失败"
SECTORS="$(blockdev --getsz "$LOOP")" || fail "读设备扇区数失败"

# dm-flakey 表：起步「永远 up」（up=60 down=0，正常透传），写阶段在此之上跑。
flakey_up()   { dmsetup load "$DM_NAME" --table "0 $SECTORS flakey $LOOP 0 60 0" && dmsetup resume "$DM_NAME"; }
# 切「永远 down + drop_writes」：静默丢弃后续写（fsync 撒谎），模拟掉电丢写。
flakey_drop() { dmsetup load "$DM_NAME" --table "0 $SECTORS flakey $LOOP 0 0 60 1 drop_writes" && dmsetup resume "$DM_NAME"; }

dmsetup create "$DM_NAME" --table "0 $SECTORS flakey $LOOP 0 60 0" || fail "dmsetup create 失败"
mkfs.ext4 -q -F "$DM_DEV" || fail "mkfs.ext4 失败"
mount "$DM_DEV" "$FSMNT" || fail "挂载 ext4 失败"
BACKING="$FSMNT/backing"; mkdir -p "$BACKING"

mount_scrollz() {
  "$BIN" --backend shadow --backing "$BACKING" --mountpoint "$1" --chunk-size "$CHUNK_SIZE" \
    >"$WORK/daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 50); do
    mountpoint -q "$1" 2>/dev/null && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || { cat "$WORK/daemon.log" >&2; return 1; }
    sleep 0.1
  done
  return 1
}

log "真实块设备就绪：loop=$LOOP dm=$DM_DEV（chunk=$CHUNK_SIZE）"
mount_scrollz "$MNT" || fail "首次挂载 scrollz 失败"

# 写者：逐行 append+fsync，每行 fsync 成功记 acked。drop_writes 切换前的 acked 行为真 durable。
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

sleep 1.0
# 记录「drop_writes 生效前」已确认的 acked 行——这些是真 durable 的下界。
sync
ACKED_BEFORE=-1
[ -s "$PROGRESS" ] && ACKED_BEFORE="$(tail -n1 "$PROGRESS")"
[ "$ACKED_BEFORE" -ge 0 ] || fail "drop_writes 前无已确认行（写太慢），无法验证 durability"
log "drop_writes 切换前已确认到 seq=$ACKED_BEFORE，切 dm-flakey drop_writes 并 kill 守护"

flakey_drop || fail "切 drop_writes 失败"
sleep 0.5
kill -9 "$DAEMON_PID" 2>/dev/null
wait "$WRITER_PID" 2>/dev/null
DAEMON_PID=""
fusermount3 -u "$MNT" 2>/dev/null || true

# 重挂前把设备切回 up（停止丢写），重挂 ext4 + scrollz 验证。
flakey_up || fail "切回 flakey up 失败"
umount "$FSMNT" 2>/dev/null
mount "$DM_DEV" "$FSMNT" || fail "重挂 ext4 失败"

mount_scrollz "$MNT2" || fail "重挂 scrollz 失败——崩溃后 archive 不可打开（违反 fail-closed）"

# 校验：恢复内容是 0..S-1 连续前缀、每行字节完好，且 S-1 >= ACKED_BEFORE（drop 前 acked 行存活）。
python3 - "$MNT2/session.jsonl" "$ACKED_BEFORE" <<'PY' || fail "恢复校验失败（见上）"
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
    sys.stderr.write("重挂后会话文件始终不可读（违反 durability）\n"); sys.exit(1)
lines = data.split(b"\n")
if lines and lines[-1] == b"":
    lines = lines[:-1]
else:
    sys.stderr.write("撕裂行：恢复文件末行无换行结尾\n"); sys.exit(1)
for i, raw in enumerate(lines):
    expect = ('{"seq":%d,"payload":"%s"}' % (i, payload)).encode()
    if raw != expect:
        sys.stderr.write("第 %d 行字节不匹配（损坏/错位）：%r\n" % (i, raw[:80])); sys.exit(1)
    json.loads(raw)
survived = len(lines)
if survived - 1 < acked:
    sys.stderr.write("durability 违反：恢复 %d 行，但 drop_writes 前已确认 seq=%d\n" % (survived, acked)); sys.exit(1)
print("[crash-dm] 恢复 %d 行，全部字节完好且连续；覆盖 drop_writes 前已确认 seq 0..%d" % (survived, acked))
PY

log "PASS：真实块层 dm-flakey drop_writes + kill 后，已确认行存活 + fail-closed"
exit 0
