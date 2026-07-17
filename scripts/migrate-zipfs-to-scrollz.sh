#!/usr/bin/env bash
# scrollz 改名一次性磁盘迁移(zipfs → scrollz)。
# 只在无挂载安全窗口执行。全部同盘原子 mv -n(不覆盖);删除仅限可重建的陈旧锁与旧 systemd 单元。
# 反向回滚命令写入 $ROLLBACK。设计见 docs/scrollz-rename-plan.md §4。
set -euo pipefail

PROJECTS="$HOME/.claude/projects"
OLD_HOME="$HOME/.claude-zip"
NEW_HOME="$HOME/.local/claude-scrollz"
SYSD="$HOME/.config/systemd/user"
ROLLBACK="$HOME/scrollz-migration-rollback.sh"

log() { printf '[migrate] %s\n' "$*"; }
rb()  { printf '%s\n' "$*" >> "$ROLLBACK"; }   # 记一条反向回滚命令

echo '#!/usr/bin/env bash' > "$ROLLBACK"; echo 'set -euo pipefail' >> "$ROLLBACK"
rb "# scrollz 迁移回滚脚本(逆序执行可还原)"

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
  rb "systemctl --user enable '$INST'   # 如需还原托管(旧模板已删,需先恢复单元文件)"
  log "  disabled 实例 $INST"
fi
for u in "$SYSD/zipfs@.service" \
         "$SYSD/$INST" \
         "$SYSD/default.target.wants/$INST" \
         "$SYSD/zipfs-neighbors.service"; do
  if [ -e "$u" ] || [ -L "$u" ]; then
    bak="$u.premigrate-bak"
    mv -n "$u" "$bak"
    rb "mv -n '$bak' '$u'"
    log "  旧单元移到备份: $u → $bak"
  fi
done
systemctl --user daemon-reload || true

# ===== ② sidecar / 备份 后缀改名(先于 home mv;此时仍在 OLD_HOME / projects)=====
log "② sidecar / 备份 改后缀"
# 2a. 真实原始 transcript 备份(.zipfs-orig → .scrollz-orig;只改名不改内容)
while IFS= read -r -d '' f; do
  new="${f%.zipfs-orig}.scrollz-orig"
  mv -n "$f" "$new"; rb "mv -n '$new' '$f'"; log "  备份改名: $f → $new"
done < <(find "$PROJECTS" -maxdepth 1 -name '*.zipfs-orig' -print0 2>/dev/null)
# 2b. 提交点 sidecar(.zipfs.meta → .scrollz.meta)
while IFS= read -r -d '' f; do
  new="${f%.zipfs.meta}.scrollz.meta"
  mv -n "$f" "$new"; rb "mv -n '$new' '$f'"; log "  sidecar 改名: $f → $new"
done < <(find "$OLD_HOME" -name '*.zipfs.meta' -print0 2>/dev/null)
# 2c. 陈旧锁(可重建 → 删)
while IFS= read -r -d '' f; do
  rm -f "$f"; rb "# 已删陈旧锁 $f(重挂自动重建,无需回滚)"; log "  删陈旧锁: $f"
done < <(find "$OLD_HOME" -name '*.zipfs.lock' -print0 2>/dev/null)

# ===== ③ backing 家目录整树 mv =====
log "③ backing 家目录整树 mv"
mkdir -p "$(dirname "$NEW_HOME")"
mv -n "$OLD_HOME" "$NEW_HOME"
rb "mv -n '$NEW_HOME' '$OLD_HOME'"
log "  $OLD_HOME → $NEW_HOME"

# ===== 迁移后校验 =====
log "校验"
leftover=$(find "$HOME/.claude" "$NEW_HOME" -name '*.zipfs*' 2>/dev/null | grep -v '.premigrate-bak' || true)
[ -z "$leftover" ] && log "  ✓ 无残留 *.zipfs*" || { echo "  ✗ 残留: $leftover" >&2; exit 1; }
sysleft=$(ls "$SYSD" 2>/dev/null | grep -i zipfs | grep -v premigrate-bak || true)
[ -z "$sysleft" ] && log "  ✓ systemd 无 zipfs 单元" || echo "  ! systemd 残留(备份态): $sysleft"
log "迁移完成。回滚脚本: $ROLLBACK"
