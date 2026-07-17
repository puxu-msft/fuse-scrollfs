#!/usr/bin/env bash
set -euo pipefail
# scrollz 迁移回滚脚本(逆序执行可还原)
systemctl --user enable 'zipfs@\x2dhome\x2dxp\x2dsrc\x2dneighbors.service'   # 如需还原托管(旧模板已删,需先恢复单元文件)
mv -n '/home/xp/.config/systemd/user/zipfs@.service.premigrate-bak' '/home/xp/.config/systemd/user/zipfs@.service'
mv -n '/home/xp/.config/systemd/user/zipfs-neighbors.service.premigrate-bak' '/home/xp/.config/systemd/user/zipfs-neighbors.service'
mv -n '/home/xp/.claude/projects/-home-xp-src-ghc2api-go.scrollz-orig' '/home/xp/.claude/projects/-home-xp-src-ghc2api-go.zipfs-orig'
mv -n '/home/xp/.claude/projects/-home-xp-src-neighbors.scrollz-orig' '/home/xp/.claude/projects/-home-xp-src-neighbors.zipfs-orig'
mv -n '/home/xp/.claude-zip/back/-home-xp-src-ghc2api-go/.scrollz.meta' '/home/xp/.claude-zip/back/-home-xp-src-ghc2api-go/.zipfs.meta'
mv -n '/home/xp/.claude-zip/back/-home-xp-src-neighbors.scrollz.meta' '/home/xp/.claude-zip/back/-home-xp-src-neighbors.zipfs.meta'
# 已删陈旧锁 /home/xp/.claude-zip/back/-home-xp-src-neighbors.zipfs.lock(重挂自动重建,无需回滚)
mv -n '/home/xp/.local/claude-scrollz' '/home/xp/.claude-zip'
