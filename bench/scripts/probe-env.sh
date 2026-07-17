#!/usr/bin/env bash
# probe-env.sh — 重新探测 scrollz 基准所需的运行环境。
# 幂等、只读：不加载模块、不改系统、不写文件。只打印一份简洁报告。
# 用法: bash bench/scripts/probe-env.sh
#
# 退出码恒为 0（探测脚本不应因「缺某依赖」而失败，缺失项会在报告中标 MISSING）。

set -uo pipefail

# 注意：这里刻意不使用 set -e。探测会大量运行「可能失败」的检查命令（如 which/grep），
# 其非零退出是正常信号而非脚本错误，不应中断整份报告。

# ANSI 颜色（仅在 stdout 是终端时启用，便于重定向到文件时保持纯文本）。
if [ -t 1 ]; then
  C_OK=$'\033[32m'; C_WARN=$'\033[33m'; C_BAD=$'\033[31m'; C_DIM=$'\033[2m'; C_RST=$'\033[0m'
else
  C_OK=''; C_WARN=''; C_BAD=''; C_DIM=''; C_RST=''
fi

ok()   { printf '  %s[ OK ]%s %s\n' "$C_OK"   "$C_RST" "$*"; }
warn() { printf '  %s[WARN]%s %s\n' "$C_WARN" "$C_RST" "$*"; }
bad()  { printf '  %s[MISS]%s %s\n' "$C_BAD"  "$C_RST" "$*"; }
hdr()  { printf '\n%s== %s ==%s\n' "$C_DIM" "$*" "$C_RST"; }

has() { command -v "$1" >/dev/null 2>&1; }

printf '%s\n' "scrollz 环境探测报告 / probe-env"
printf '采集时间: %s\n' "$(date -Iseconds 2>/dev/null || date)"
printf 'hostname: %s\n' "$(hostname 2>/dev/null || echo '?')"

# ── 主机与内核 ──────────────────────────────────────────────────
hdr "主机与内核 / host & kernel"
KREL="$(uname -r 2>/dev/null || echo '?')"
printf '  kernel: %s\n' "$KREL"
printf '  os:     %s\n' "$(uname -s -m 2>/dev/null)"
if [ -r /etc/os-release ]; then
  # shellcheck disable=SC1091
  printf '  distro: %s\n' "$(. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-?}")"
fi
if grep -qi microsoft /proc/version 2>/dev/null; then
  ok "检测到 WSL（注意: btrfs 模块每次启动需 sudo modprobe；drop_caches 需 root）"
fi

# ── CPU / 内存 ─────────────────────────────────────────────────
hdr "CPU / 内存 / cpu & mem"
NCPU="$(nproc 2>/dev/null || echo '?')"
printf '  逻辑核 nproc: %s\n' "$NCPU"
if [ -r /proc/meminfo ]; then
  MEMKB="$(awk '/^MemTotal:/{print $2}' /proc/meminfo 2>/dev/null)"
  if [ -n "${MEMKB:-}" ]; then
    printf '  MemTotal:    %s kB (~%s GiB)\n' "$MEMKB" "$(awk "BEGIN{printf \"%.1f\", $MEMKB/1024/1024}")"
  fi
fi

# ── 根文件系统 / 后端块设备 ─────────────────────────────────────
hdr "后端存储 / backing store"
ROOT_LINE="$(df -hT / 2>/dev/null | awk 'NR==2{print "  dev="$1"  fstype="$2"  size="$3"  avail="$5"  on="$7}')"
[ -n "$ROOT_LINE" ] && printf '%s\n' "$ROOT_LINE"
printf '  %s基准应放在 Linux 原生 ext4 后端，勿用 /mnt/c（9p/drvfs 慢且不代表原生）。%s\n' "$C_DIM" "$C_RST"

# ── btrfs（路线 A）─────────────────────────────────────────────
hdr "btrfs / 路线 A (kernel zstd)"
# btrfs.ko 是否在内核模块树里
KO=""
if [ -n "$KREL" ] && [ "$KREL" != "?" ]; then
  KO="$(find "/lib/modules/$KREL/kernel/fs/btrfs" -maxdepth 1 -name 'btrfs.ko*' 2>/dev/null | head -n1)"
fi
if [ -n "$KO" ]; then
  ok "btrfs.ko 存在: $KO"
elif has modinfo && modinfo btrfs >/dev/null 2>&1; then
  ok "btrfs 模块可被 modinfo 解析（已随内核可用）"
else
  bad "未找到 btrfs.ko（modinfo 也无）——内核可能未带 btrfs"
fi
# btrfs 是否已加载到 /proc/filesystems
if grep -qw btrfs /proc/filesystems 2>/dev/null; then
  ok "btrfs 已加载（/proc/filesystems 含 btrfs）"
else
  warn "btrfs 未加载——挂载前需手动: sudo modprobe btrfs（本脚本不擅自加载）"
fi
if has mkfs.btrfs; then
  ok "mkfs.btrfs: $(command -v mkfs.btrfs)  ($(mkfs.btrfs --version 2>/dev/null | head -n1))"
else
  bad "mkfs.btrfs 缺失——需 sudo apt install btrfs-progs"
fi
if has compsize; then
  ok "compsize: $(command -v compsize)（用于测 btrfs 压缩比）"
else
  bad "compsize 缺失——需 sudo apt install btrfs-compsize（测压缩比用）"
fi

# ── FUSE（路线 B）──────────────────────────────────────────────
hdr "FUSE / 路线 B (B0 透传, B2 fuse-zstd)"
if [ -c /dev/fuse ]; then
  ok "/dev/fuse 存在: $(ls -l /dev/fuse 2>/dev/null | awk '{print $1}')（普通用户可挂 FUSE）"
else
  bad "/dev/fuse 不存在——FUSE 路线不可用"
fi
for fm in fusermount3 fusermount; do
  if has "$fm"; then ok "$fm: $(command -v "$fm")"; fi
done
has fusermount3 || has fusermount || bad "fusermount/fusermount3 均缺失"
if has pkg-config && pkg-config --exists libzstd 2>/dev/null; then
  ok "libzstd dev: $(pkg-config --modversion libzstd 2>/dev/null)"
else
  warn "libzstd pkg-config 未就绪（Rust zstd crate 自带 vendored 源，通常不阻塞）"
fi
# fuse-zstd（B2）当前是否已构建/可用——纯探测，不强求
if has fuse-zstd; then
  ok "fuse-zstd 二进制在 PATH: $(command -v fuse-zstd)"
else
  warn "fuse-zstd 未在 PATH（B2 条件待构建/安装；见 bench/README.md）"
fi

# ── fio（基准主力）─────────────────────────────────────────────
hdr "fio / 基准工具"
if has fio; then
  ok "fio: $(command -v fio)  ($(fio --version 2>/dev/null))"
else
  bad "fio 缺失——需 sudo apt install fio（基准跑不起来）"
fi

# ── 构建工具链（路线 B 自研）───────────────────────────────────
hdr "构建工具链 / toolchain"
for t in cargo rustc go gcc python3 zstd; do
  if has "$t"; then
    ok "$t: $(command -v "$t")"
  else
    warn "$t 不在 PATH"
  fi
done

# ── drop_caches 权限（冷缓存测试用）────────────────────────────
hdr "冷缓存能力 / drop_caches"
if [ "$(id -u)" -eq 0 ]; then
  ok "以 root 运行——可 drop_caches 做冷缓存测试"
elif has sudo && sudo -n true 2>/dev/null; then
  ok "sudo 免密可用——run-suite.sh 冷缓存项可工作"
else
  warn "非 root 且 sudo 需密码——冷缓存项将降级为热缓存（run-suite.sh 会显式告警）"
fi

printf '\n%s探测结束。[MISS] 为基准前置硬缺口，[WARN] 多为可降级/待实现项。%s\n' "$C_DIM" "$C_RST"
exit 0
