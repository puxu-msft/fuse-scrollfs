#!/usr/bin/env bash
# teardown.sh — 安全卸载 btrfs loop 挂载，并可选删除镜像文件。
#
# 用法:
#   MNT=/mnt/scrollz-btrfs                     bash bench/scripts/teardown.sh        # 仅卸载
#   MNT=/mnt/scrollz-btrfs IMG=/path/btrfs.img DELETE_IMG=1 bash .../teardown.sh     # 卸载并删 image
#
# 参数:
#   MNT          挂载点（必填）
#   IMG          镜像路径（仅当 DELETE_IMG=1 时需要）
#   DELETE_IMG   置 1 才删除镜像；默认 0（保守，绝不默认删数据）
#   RMDIR_MNT    置 1 才删除空挂载点目录；默认 0
#
# 安全原则（遵循「无意识数据丢失不可接受」）:
#   - 所有 umount/rm 前都校验目标变量非空、路径形态符合预期。
#   - 绝不使用通配符 rm；只 rm 明确的单个文件。
#   - 删除镜像前确认它已不再被任何挂载占用，且确是普通文件。
#   - umount 需要 root。

set -euo pipefail

MNT="${MNT:-}"
IMG="${IMG:-}"
DELETE_IMG="${DELETE_IMG:-0}"
RMDIR_MNT="${RMDIR_MNT:-0}"

log() { printf '[teardown] %s\n' "$*"; }
die() { printf '[teardown] ERROR: %s\n' "$*" >&2; exit 1; }

[ -n "$MNT" ] || die "MNT 为空——拒绝继续（不知道卸载哪里）"

# ── 卸载 ───────────────────────────────────────────────────────
if mountpoint -q "$MNT" 2>/dev/null; then
  if [ "$(id -u)" -ne 0 ]; then
    die "卸载 $MNT 需要 root。请用: sudo -E bash $0"
  fi
  log "卸载: $MNT"
  # -d 让内核在卸载 loop 挂载时自动释放对应的 loop 设备。
  umount -d "$MNT" || die "umount 失败: $MNT（可能仍被占用，lsof/fuser 排查）"
  log "已卸载: $MNT"
else
  log "$MNT 当前未挂载，跳过 umount。"
fi

# ── 可选删除挂载点空目录 ───────────────────────────────────────
if [ "$RMDIR_MNT" = "1" ]; then
  if [ -d "$MNT" ] && [ -z "$(ls -A "$MNT" 2>/dev/null)" ]; then
    log "删除空挂载点目录: $MNT"
    rmdir "$MNT"   # rmdir 只能删空目录，天然安全
  else
    log "挂载点非空或不存在，保留: $MNT"
  fi
fi

# ── 可选删除镜像（重重设防）────────────────────────────────────
if [ "$DELETE_IMG" = "1" ]; then
  [ -n "$IMG" ] || die "DELETE_IMG=1 但 IMG 为空——拒绝删除（防误删）"

  # 形态校验：必须以 .img 结尾，且是已存在的普通文件，且不是目录/链接/根。
  case "$IMG" in
    *.img) : ;;
    *) die "IMG 不以 .img 结尾: '$IMG'——安全起见拒绝删除" ;;
  esac
  [ -f "$IMG" ] || die "IMG 不是普通文件（或不存在）: '$IMG'——拒绝删除"
  [ -L "$IMG" ] && die "IMG 是符号链接: '$IMG'——拒绝删除（防止穿越到别处）"

  # 确认该镜像未被任何挂载占用（防止删掉仍在用的 image）。
  if findmnt -rno SOURCE 2>/dev/null | grep -Fxq "$IMG"; then
    die "镜像仍被挂载占用: $IMG——请先卸载再删"
  fi
  if losetup -j "$IMG" 2>/dev/null | grep -q .; then
    die "镜像仍绑定到 loop 设备: $IMG（losetup -j 有结果）——请先释放再删"
  fi

  log "删除镜像（单文件，无通配符）: $IMG"
  rm -f -- "$IMG"
  log "已删除: $IMG"
else
  [ -n "$IMG" ] && log "保留镜像（DELETE_IMG!=1）: $IMG"
fi

log "完成。"
