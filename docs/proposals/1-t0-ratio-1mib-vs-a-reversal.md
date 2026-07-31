# 提案 #1：在现默认 1MiB 块 + CONSOLIDATED 同口径下重测 scrollz 压缩比，判定 G1 排名是否再反转

> 由 scrollz harness 自动生成。lane=perf
> HARNESS-OP:81064221927644dab0e4728dcfc21f9b

### 意图
现默认块大小已从 64KiB 切到 1MiB（T3 已落地），但 CONSOLIDATED 五条件大对照给出的「A(btrfs force) 6.74x > BS 5.42x」这一压缩比排名结论，测的仍是旧 64KiB 默认。ROADMAP T0 表把这条列为「关键：可能再次反转 G1 依据」且工作量「极小」，因为脚本已就绪（`ratio-matrix.sh`），只差用同一语料重跑一次并把数字填回表格。这不是新功能开发，是补齐一个已知的、被标记为决策门 G1（布局取向：V/S/两者并存）关键依据的实测缺口。

### 证据
- `docs/ROADMAP.md` T0 表第 16 行原文：「上面 6.74x 反转是基于 scrollz 旧 64KiB（5.42x）；现默认已退役 64KiB→1MiB，ratio-bench 真实路径 Shadow 13.7x **应再反转回 scrollz 领先**。需在同 CONSOLIDATED 口径复测确认」。
- `bench/results/20260628-1212/CONSOLIDATED.md` §3/§4.2：同一份 709MiB/408 文件 `~/.claude/projects` 快照上，64KiB 块测得 BS=5.42x，A(compress-force)=6.74x，A 领先。
- `bench/results/dict-chunk-ratio/REPORT.md`：**不同**语料子集（单项目目录 128MiB）上，Shadow 1MiB/level3=13.70x、1MiB/level19=15.92x，远超 6.74x——方向强烈提示会反转，但语料不同，不能直接替代 CONSOLIDATED 口径下的结论。

### 验收判据
用 CONSOLIDATED 那份 709MiB/408 文件语料（而非 dict-chunk-ratio 用的 128MiB 子集）跑 `bench/scripts/ratio-matrix.sh`，chunk=1MiB（当前默认）、level=3；得到的 Shadow 压缩比 R 与 A 的 6.74x 比较：R > 6.74x 记「反转成立」，否则记「反转不成立」。两种结果都需要把具体数字写回 `docs/ROADMAP.md` T0 表（状态 ☐→☑）与 `CONSOLIDATED.md` §3/§4.2（若反转需改写「压缩比不是 zipfs 的优势」这句结论）。判据本身不预设哪个方向为「对」，只要求可复算、可回填。

### 触碰文件面
`bench/scripts/ratio-matrix.sh`（复用，不改逻辑）、`bench/results/<new-run>/`（新增报告）、`docs/ROADMAP.md`（表格状态更新）、`bench/results/20260628-1212/CONSOLIDATED.md`（若结论反转需追加勘误段，不覆盖原始数据）。

### 风险
- 语料必须与 CONSOLIDATED 完全一致（同一份 709MiB/408 文件快照），否则数字不可比，需先确认该语料快照是否仍在（或需要重新固化一份，避免直接对生产 `~/.claude/projects` 做任何挂载/整理操作，只读快照）。
- 若反转成立，这条结果会牵动 G1（布局取向 V/S/并存）决策，但本候选本身只负责把数字测出来、写回文档，不代为做 G1 决策——G1 仍需人工/architect 拍板，故不触碰红线，`needs_decision` 记 false。
