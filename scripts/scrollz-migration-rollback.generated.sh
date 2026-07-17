#!/usr/bin/env bash
set -euo pipefail
# scrollz 迁移回滚(可直接 bash 正序执行;home 复原最先,故后续 sidecar 路径有效)
# 修复合并态复审 major:原生成脚本为「正序落盘、需倒序跑」的陷阱,本版已按正确顺序重排。

# ① backing 家目录复原(最先,使下面 ~/.claude-zip 路径重新有效)
mv -n '/home/xp/.local/claude-scrollz' '/home/xp/.claude-zip'
# ② sidecar 提交点改回(此时已在 ~/.claude-zip 下)
mv -n '/home/xp/.claude-zip/back/-home-xp-src-neighbors.scrollz.meta' '/home/xp/.claude-zip/back/-home-xp-src-neighbors.zipfs.meta'
mv -n '/home/xp/.claude-zip/back/-home-xp-src-ghc2api-go/.scrollz.meta' '/home/xp/.claude-zip/back/-home-xp-src-ghc2api-go/.zipfs.meta'
# ③ 真实原始 transcript 备份改回(在 ~/.claude/projects,与 home mv 无关)
mv -n '/home/xp/.claude/projects/-home-xp-src-neighbors.scrollz-orig' '/home/xp/.claude/projects/-home-xp-src-neighbors.zipfs-orig'
mv -n '/home/xp/.claude/projects/-home-xp-src-ghc2api-go.scrollz-orig' '/home/xp/.claude/projects/-home-xp-src-ghc2api-go.zipfs-orig'
# ④ 旧 systemd 单元从 .premigrate-bak 复原
mv -n '/home/xp/.config/systemd/user/zipfs@.service.premigrate-bak' '/home/xp/.config/systemd/user/zipfs@.service'
mv -n '/home/xp/.config/systemd/user/zipfs-neighbors.service.premigrate-bak' '/home/xp/.config/systemd/user/zipfs-neighbors.service'
systemctl --user daemon-reload || true
# ⑤ 还原旧托管 enable(旧模板已复原)
systemctl --user enable 'zipfs@\x2dhome\x2dxp\x2dsrc\x2dneighbors.service' || true

# 注:陈旧锁 .zipfs.lock 已删,重挂自动重建,无需回滚。
# 注:若已跑过 `scrollz enable autostart install`,回滚后退掉新托管:
#   systemctl --user disable --now 'scrollz@\x2dhome\x2dxp\x2dsrc\x2dneighbors.service'
#   rm -f '/home/xp/.config/systemd/user/scrollz@.service'; systemctl --user daemon-reload
echo "[rollback] 完成:已还原到迁移前状态(zipfs 后缀 / ~/.claude-zip / 旧 systemd 单元)。"
