#!/usr/bin/env bash
# scrollz 改名一次性磁盘迁移(zipfs → scrollz)。
# 只在无挂载安全窗口执行。全部同盘原子 mv -n(不覆盖);删除仅限可重建的陈旧锁与旧 systemd 单元(移 .premigrate-bak)。
# 回滚:反向命令**倒序收集、正序落盘**到 $ROLLBACK,故 $ROLLBACK 可直接 `bash` 正序执行即还原(修复合并态复审 major:回滚方向陷阱)。
# 设计见 docs/scrollz-rename-plan.md §4。
set -euo pipefail

PROJECTS="$HOME/.claude/projects"
OLD_HOME="$HOME/.claude-zip"
NEW_HOME="$HOME/.local/claude-scrollz"
SYSD="$HOME/.config/systemd/user"
ROLLBACK="$HOME/scrollz-migration-rollback.sh"

log() { printf '[migrate] %s\n' "$*"; }
RB=()                              # 反向回滚命令,按迁移正序 append;落盘时**反转**,使回滚脚本正序可跑
rb()  { RB+=("$1"); }

# ===== 前置断言 =====
if mount | grep -i fuse | grep -iv fusectl | grep -qi -e scrollz -e zipfs; then
  echo "ABORT: 检测到 live FUSE 挂载,先卸载再迁移" >&2; exit 1
fi
[ -e "$NEW_HOME" ] && { echo "ABORT: 目的地 $NEW_HOME 已存在" >&2; exit 1; }
[ -d "$OLD_HOME" ] || { echo "ABORT: 源 $OLD_HOME 不存在" >&2; exit 1; }
log "前置断言通过(无挂载 / 目的地空 / 源在)"

# ===== ① systemd:disable 实例 + 删旧单元/链接 =====
log "① systemd 迁移"
INST='zipfs@\x2dhome\x2dxp\x2dsrc\x2dneighbors.service'
if systemctl --user is-enabled "$INST" >/dev/null 2>&1; then
  systemctl --user disable --now "$INST" || true
  rb "systemctl --user enable '$INST'   # 还原旧托管(需旧模板已复原)"
  log "  disabled 实例 $INST"
fi
for u in "$SYSD/zipfs@.service" "$SYSD/$INST" "$SYSD/default.target.wants/$INST" "$SYSD/zipfs-neighbors.service"; do
  if [ -e "$u" ] || [ -L "$u" ]; then
    bak="$u.premigrate-bak"; mv -n "$u" "$bak"; rb "mv -n '$bak' '$u'"; log "  旧单元→备份: $u"
  fi
done
systemctl --user daemon-reload || true

# ===== ② sidecar / 备份 后缀改名(先于 home mv;仍在 OLD_HOME / projects)=====
log "② sidecar / 备份 改后缀"
while IFS= read -r -d '' f; do
  new="${f%.zipfs-orig}.scrollz-orig"; mv -n "$f" "$new"; rb "mv -n '$new' '$f'"; log "  备份改名: $f"
done < <(find "$PROJECTS" -maxdepth 1 -name '*.zipfs-orig' -print0 2>/dev/null)
while IFS= read -r -d '' f; do
  new="${f%.zipfs.meta}.scrollz.meta"; mv -n "$f" "$new"; rb "mv -n '$new' '$f'"; log "  sidecar 改名: $f"
done < <(find "$OLD_HOME" -name '*.zipfs.meta' -print0 2>/dev/null)
while IFS= read -r -d '' f; do
  rm -f "$f"; log "  删陈旧锁(重挂重建,无需回滚): $f"
done < <(find "$OLD_HOME" -name '*.zipfs.lock' -print0 2>/dev/null)

# ===== ③ backing 家目录整树 mv =====
log "③ backing 家目录整树 mv"
mkdir -p "$(dirname "$NEW_HOME")"; mv -n "$OLD_HOME" "$NEW_HOME"; rb "mv -n '$NEW_HOME' '$OLD_HOME'"
log "  $OLD_HOME → $NEW_HOME"

# ===== 落盘回滚脚本(反转 RB:home 复原最先,故正序 bash 可跑)=====
{
  echo '#!/usr/bin/env bash'; echo 'set -euo pipefail'
  echo '# scrollz 迁移回滚(可直接 bash 正序执行;还原后如需退 scrollz autostart 见末尾注释)'
  for ((i=${#RB[@]}-1; i>=0; i--)); do echo "${RB[$i]}"; done
  echo '# 若已跑过 `scrollz enable autostart install`,回滚后可:'
  echo "#   systemctl --user disable --now 'scrollz@\\x2dhome\\x2dxp\\x2dsrc\\x2dneighbors.service'"
  echo "#   rm -f '$SYSD/scrollz@.service'; systemctl --user daemon-reload"
} > "$ROLLBACK"
chmod +x "$ROLLBACK"

# ===== 迁移后校验 =====
log "校验"
leftover=$(find "$HOME/.claude" "$NEW_HOME" -name '*.zipfs*' 2>/dev/null | grep -v '.premigrate-bak' || true)
[ -z "$leftover" ] && log "  ✓ 无残留 *.zipfs*" || { echo "  ✗ 残留: $leftover" >&2; exit 1; }
log "迁移完成。回滚脚本(正序可跑): $ROLLBACK"
log "提示:确认无需回滚后,可删旧单元备份 rm $SYSD/*.premigrate-bak"
