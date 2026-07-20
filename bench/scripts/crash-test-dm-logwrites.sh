#!/usr/bin/env bash
# crash-test-dm-logwrites.sh — Tier 2 真实块层 barrier 排序门（dm-log-writes 回放，root 门控，docs/05 §5）。
#
# dm-log-writes 把每个 write/flush/fua 记到独立 log 设备；`replay-log` 可把数据盘**回放到任一 flush
# 边界（mark）**，逐个 mount + 跑 scrollz 恢复校验，从而证明**真实内核 fs 真的兑现 barrier 排序**
# （barrier 1 → SB 写 → barrier 2），以及目录项 / rename durability——这是 Tier 1（只验自家逻辑）和
# dm-flakey（粗粒度丢写）都覆盖不到的最高保真层。收窄到 3 个招牌场景（docs/05 §5），**不做 xfstests 全量**：
#   (a) append+fsync 序列：回放到每个 fsync 边界，恢复内容必是连续前缀；
#   (b) rename 覆盖：回放到 rename 后，必看到新内容而非旧/丢失；
#   (c) create 后崩溃：回放到 create+fsync 后，新文件目录项 durable。
#
# 安全（用户规则 no-unconscious）：只用自建 mktemp 工作区 + 自建唯一命名 loop/dm；cleanup 只卸自己的
# 挂载、remove 自己的 dm、losetup -d 自己的 loop、rm -rf 仅 mktemp 目录（case 守卫）。绝不通配 rm、绝不动系统挂载。
#
# 用法：sudo bash bench/scripts/crash-test-dm-logwrites.sh
# 退出码 0=PASS 或 SKIP，非 0=FAIL。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BIN="${BIN:-$REPO_DIR/target/release/scrollz}"
CHUNK_SIZE="${CHUNK_SIZE:-65536}"

log()  { printf '[dm-logw] %s\n' "$*"; }
skip() { printf '[dm-logw] SKIP：%s\n' "$*"; exit 0; }

# ---- 门控 ----
[ "$(id -u)" -eq 0 ] || skip "需 root"
[ -c /dev/fuse ] || skip "/dev/fuse 不存在"
for t in dmsetup losetup mkfs.ext4 mountpoint fusermount3 replay-log; do
  command -v "$t" >/dev/null 2>&1 || skip "缺工具：$t（replay-log 属 xfstests src/log-writes）"
done
dmsetup targets 2>/dev/null | grep -qiw log-writes || skip "内核无 dm-log-writes target"
[ -x "$BIN" ] || skip "未找到 scrollz 二进制：$BIN"

UNIQ="scrollzlogw$$"
DM_NAME="$UNIQ"
DM_DEV="/dev/mapper/$DM_NAME"
WORK="$(mktemp -d -t scrollz-logw-XXXXXX)"
DATA_IMG="$WORK/data.img"; LOG_IMG="$WORK/log.img"
FSMNT="$WORK/fs"; MNT="$WORK/mnt"
DATA_LOOP=""; LOG_LOOP=""; DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill -9 "$DAEMON_PID" 2>/dev/null
  fusermount3 -u "$MNT" 2>/dev/null || true
  mountpoint -q "$FSMNT" 2>/dev/null && umount "$FSMNT" 2>/dev/null
  dmsetup remove "$DM_NAME" 2>/dev/null
  [ -n "$DATA_LOOP" ] && losetup -d "$DATA_LOOP" 2>/dev/null
  [ -n "$LOG_LOOP" ] && losetup -d "$LOG_LOOP" 2>/dev/null
  case "$WORK" in
    /tmp/scrollz-logw-*|"${TMPDIR:-/tmp/}"scrollz-logw-*) rm -rf "$WORK" 2>/dev/null ;;
  esac
}
trap cleanup EXIT
fail() { printf '[dm-logw] FAIL：%s\n' "$*" >&2; exit 1; }

mkdir -p "$FSMNT" "$MNT"
truncate -s 512M "$DATA_IMG" || fail "建 data 镜像失败"
truncate -s 512M "$LOG_IMG"  || fail "建 log 镜像失败"
DATA_LOOP="$(losetup --find --show "$DATA_IMG")" || fail "losetup data 失败"
LOG_LOOP="$(losetup --find --show "$LOG_IMG")"   || fail "losetup log 失败"
SECTORS="$(blockdev --getsz "$DATA_LOOP")" || fail "读扇区数失败"

dmsetup create "$DM_NAME" --table "0 $SECTORS log-writes $DATA_LOOP $LOG_LOOP" || fail "dmsetup create 失败"
mkfs.ext4 -q -F "$DM_DEV" || fail "mkfs.ext4 失败"
mount "$DM_DEV" "$FSMNT" || fail "挂 ext4 失败"
BACKING="$FSMNT/backing"; mkdir -p "$BACKING"

mark() { dmsetup message "$DM_NAME" 0 mark "$1"; }

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
umount_scrollz() { [ -n "$DAEMON_PID" ] && { fusermount3 -u "$MNT" 2>/dev/null; wait "$DAEMON_PID" 2>/dev/null; DAEMON_PID=""; }; }

# ===== 跑工作负载（写经 dm-log-writes 全程记录），在 3 个招牌边界打 mark =====
log "跑工作负载，打 mark（append-a / rename-b / create-c）…"
mount_scrollz "$MNT" || fail "挂 scrollz 失败"

# (a) append+fsync 10 行；末行 fsync 后 mark。
python3 - "$MNT/log.jsonl" 10 <<'PY' || fail "append 工作负载失败"
import os, sys
path, n = sys.argv[1], int(sys.argv[2])
fd = os.open(path, os.O_WRONLY|os.O_CREAT|os.O_APPEND, 0o644)
for i in range(n):
    os.write(fd, ('{"seq":%d}\n' % i).encode()); os.fsync(fd)
PY
mark append-a

# (b) rename 覆盖：建 old（有内容）+ dst（旧内容），fsync，rename old→dst，fsync 目录，mark。
python3 - "$MNT" <<'PY' || fail "rename 工作负载失败"
import os, sys
root = sys.argv[1]
for name, body in (("old.txt", b"NEWCONTENT"), ("dst.txt", b"OLDCONTENT")):
    fd = os.open(os.path.join(root, name), os.O_WRONLY|os.O_CREAT|os.O_TRUNC, 0o644)
    os.write(fd, body); os.fsync(fd); os.close(fd)
os.rename(os.path.join(root, "old.txt"), os.path.join(root, "dst.txt"))
dfd = os.open(root, os.O_RDONLY|os.O_DIRECTORY); os.fsync(dfd); os.close(dfd)
PY
mark rename-b

# (c) create 后崩溃：新建 created.txt + 写 + fsync 文件 + fsync 父目录，mark。
python3 - "$MNT" <<'PY' || fail "create 工作负载失败"
import os, sys
root = sys.argv[1]
fd = os.open(os.path.join(root, "created.txt"), os.O_WRONLY|os.O_CREAT|os.O_TRUNC, 0o644)
os.write(fd, b"CREATED"); os.fsync(fd); os.close(fd)
dfd = os.open(root, os.O_RDONLY|os.O_DIRECTORY); os.fsync(dfd); os.close(dfd)
PY
mark create-c

umount_scrollz
umount "$FSMNT" 2>/dev/null
dmsetup remove "$DM_NAME" 2>/dev/null   # 撤掉 log-writes 层，后续直接对 DATA_LOOP 回放/挂载

# ===== 逐 mark 回放 → 直接挂 DATA_LOOP 的 ext4 → 跑 scrollz 校验 =====
replay_and_check() {
  local target_mark="$1" checker="$2"
  log "回放至 mark=$target_mark …"
  replay-log --log "$LOG_LOOP" --replay "$DATA_LOOP" --end-mark "$target_mark" \
    >"$WORK/replay-$target_mark.log" 2>&1 || fail "replay-log 至 $target_mark 失败（见 $WORK/replay-$target_mark.log）"
  mount "$DATA_LOOP" "$FSMNT" || fail "回放后挂 ext4 失败（fs 不一致？）"
  mount_scrollz "$MNT" || { umount "$FSMNT" 2>/dev/null; fail "回放后挂 scrollz 失败（违反 fail-closed）"; }
  "$checker"; local rc=$?
  umount_scrollz
  umount "$FSMNT" 2>/dev/null
  [ "$rc" -eq 0 ] || fail "mark=$target_mark 校验失败"
}

check_a() {
  python3 - "$MNT/log.jsonl" <<'PY'
import sys, json
data = open(sys.argv[1], "rb").read()
lines = data.split(b"\n")
if lines and lines[-1] == b"": lines = lines[:-1]
else: print("撕裂行：末行无换行", file=sys.stderr); sys.exit(1)
for i, raw in enumerate(lines):
    if raw != ('{"seq":%d}' % i).encode():
        print("第 %d 行不匹配：%r" % (i, raw), file=sys.stderr); sys.exit(1)
    json.loads(raw)
print("[dm-logw] (a) 回放至 append-a：%d 行连续前缀、字节完好" % len(lines))
PY
}
check_b() {
  python3 - "$MNT/dst.txt" <<'PY'
import sys
body = open(sys.argv[1], "rb").read()
# rename 覆盖后必看到新内容（old 的 NEWCONTENT），绝不旧 OLDCONTENT / 丢失。
if body != b"NEWCONTENT":
    print("rename durability 违反：dst.txt=%r（应为 NEWCONTENT）" % body, file=sys.stderr); sys.exit(1)
print("[dm-logw] (b) 回放至 rename-b：dst.txt 为 rename 后新内容")
PY
}
check_c() {
  python3 - "$MNT/created.txt" <<'PY'
import sys, os
p = sys.argv[1]
if not os.path.exists(p):
    print("目录项 durability 违反：created.txt 崩溃后丢失", file=sys.stderr); sys.exit(1)
if open(p, "rb").read() != b"CREATED":
    print("created.txt 内容损坏", file=sys.stderr); sys.exit(1)
print("[dm-logw] (c) 回放至 create-c：新文件目录项 + 内容 durable")
PY
}

replay_and_check append-a check_a
replay_and_check rename-b check_b
replay_and_check create-c check_c

log "PASS：dm-log-writes 回放 3 招牌场景（append+fsync / rename 覆盖 / create durability）均 fail-closed + durable"
exit 0
