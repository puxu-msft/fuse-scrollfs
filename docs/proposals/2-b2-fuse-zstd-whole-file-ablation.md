# 提案 #2：跑通 B2（fuse-zstd 整文件）消融，补齐「分块价值」外部实证

> 由 scrollz harness 自动生成。lane=perf
> HARNESS-OP:aef9944988fb4a209e35045bcaa90ded

### 意图
设计文档三处（00-overview.md、01-scrollz-design.md、ROADMAP.md）都把 B2（`fuse-zstd` 整文件单 zstd 流）列为验证「分块是否有价值」的消融对照项，但从未实际跑过。run-suite.sh 的默认条件映射里已经预留了 `B2=/mnt/scrollz-b2` 这个挂载点变量，说明这不是要新设计实验，只是把已规划好的第四条腿接上去跑一次。

### 证据
- ROADMAP.md T0 表：「B2（fuse-zstd 整文件）消融 | §9 矩阵的分块 vs 整文件参照项未跑，缺分块价值的外部实证 | 小（装并挂 fuse-zstd 跑一遍） | ☐」。
- 00-overview.md §6.2：`Big-Dig-Data/fuse-zstd`「整文件单 zstd 流…随机写弱…B2 反例对照，非候选解」，且 §9 矩阵表把 B2 列为对照条件。
- 01-scrollz-design.md §7/208：「CHUNK_SIZE 与 zstd 等级扫描同时施加于 BV/BS…BS vs B2 验证分块的价值」——是设计阶段就定好的对照实验，非临时想法。
- run-suite.sh:43 DEFAULT_CONDITIONS 已含 `B2=/mnt/scrollz-b2`，脚本侧无需改动即可跑，只差把 fuse-zstd 装好并挂到这个路径。

### 验收判据
用 `CONDITIONS="...B2=/mnt/scrollz-b2"` 跑一次 `run-suite.sh`，`results/<tag>/B2/{seq-write,rand-read,rand-write}.json` 三份结果非 SKIP、非空。把 B2 的 rand-write 吞吐/延迟与同轮 BS 对比：B2 明显更差（数量级或方向一致的劣化）记「分块价值证实」；若接近或更好则记「未证实，需重估分块设计的必要性」。两种结果都要写回 ROADMAP.md 该行状态。

### 触碰文件面
新增 `bench/scripts/mount-b2.sh`（安装+挂载 fuse-zstd，参照现有 `mount-b0.sh`/`mount-bs.sh` 风格）；产出 `bench/results/<new-run>/`；更新 `docs/ROADMAP.md` 该行状态。不改任何核心代码路径。

### 风险
fuse-zstd 是第三方实验项目（~13★，非生产级），需确认能在当前内核/fuser 版本下编译挂载；若装不上或挂载失败，应如实记录为「环境阻塞」而非跳过不提，避免这个已规划三年的对照项继续被静默搁置。
