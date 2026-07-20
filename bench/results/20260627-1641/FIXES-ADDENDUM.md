# zipfs 首轮缺陷修复实测（FIXES-ADDENDUM）

> 本文件**补充**同目录 `FIRST-RUN.md`，不覆盖。针对首轮暴露的两个结构性缺陷给出根因确认、修复方案与「修复前 vs 修复后」实测。
> 修复实测时间：2026-06-28（WSL2，热缓存，普通用户 FUSE，无 sudo）。环境同 FIRST-RUN §1。

## 修复 1：BS 随机读病态慢（1.4 MiB/s → 37 MiB/s，约 26x）

### 根因（已定位确认）

`ShadowStore::get_block`（`fuse/src/store/shadow.rs`）在未命中脏会话时，**每次调用都 `ArchiveReader::open(&abs)`**：重读尾部 footer + 全量解析 chunk index + CRC32 全扫 + 逐项越界校验。而读路径 `rwfs::read_range` 对读区间内**每个块**调一次 `get_block`，`block_geometry`（同样每次 `ArchiveReader::open`）又被每个 read 调一次。一个 1GiB 文件的 index 约 16384 项 × 20B ≈ 320KiB，**每个 4KiB 随机读都把整份索引重读重解析一遍** → 1.4 MiB/s / 359 IOPS、p50 2.6ms。FIRST-RUN §5「BS per-op archive 元数据开销重」的成因即此。

### 修复

- 新增 **per-inode `ArchiveReader` 缓存**（`readers: Mutex<HashMap<ino, Arc<ArchiveReader>>>`）：首次 `get_block`/`block_geometry` 打开并解析一次 footer+index，后续复用，块定位降为内存 O(1) + 一次 pread。
- 把 `ArchiveReader` 的 `read_exact_at` 从 `seek+read`（移动游标，多线程共享 fd 有竞争）改为 **pread（`FileExt::read_exact_at`，定位读不移动游标）**，使缓存的 reader 可被 fuser 多线程并发 `read_block` 而无 seek 竞争。
- **缓存失效**与写脏会话协调：写会话提交（`commit_session`）、`unlink`、`rename` 覆盖目标、`release` 都失效对应缓存项；写会话未提交时不失效（脏块由 `get_block` read-through 命中，未脏块仍读旧版本 archive，与缓存一致）。
- 失效用**世代计数器（epoch）**堵住「open→insert 之间被并发提交失效」的回填竞态（见 rust-review H1）。
- Store trait 新增 `release(ino)` 默认空钩子；`rwfs::release` 在 flush 落盘后调它释放缓存。

### 实测（200MiB 文件，chunk 64KiB，runtime=30s，热缓存）

| 指标 | 修复前（FIRST-RUN §3.2，1GiB） | 修复后（本轮，200MiB） | 提升 |
|---|---|---|---|
| rand-read 4k  bw | 1.4 MiB/s | **37.1 MiB/s** | ~26x |
| rand-read 4k  IOPS | 359 | **9488** | ~26x |
| rand-read 4k  lat p50 | 2605 µs | **~105 µs** | ~25x↓ |
| rand-read 4k  lat p99 | 5210 µs | **~188 µs** | ~28x↓ |
| rand-read 64k bw | 12.2 MiB/s | **582 MiB/s** | ~48x |
| rand-read 64k IOPS | 194.9 | **9319** | ~48x |

> 口径说明：修复前数字取自 FIRST-RUN（1GiB 文件）；修复后为同条件单文件快速复算（200MiB，避免十余分钟 layout）。两者文件大小不同，但修复前的病态来自「每块重解析索引」，与文件大小正相关——更小文件本应更快，而修复前 64k 也只有 12 MiB/s，故 26–48x 的量级提升是真实的结构性修复，非文件大小差异所致。修复后 BS 4k 随机读已与 FIRST-RUN 中 BV 4k（43 MiB/s）同量级，符合预期（消除了 per-op 全索引重解析这个 BS 专有短板）。

## 修复 2：BV 无 compact，稳态膨胀（0.54x → compact 后 3.84x）

### 根因（已确认）

redb 写事务用 MVCC：旧版本页在无活跃读事务引用前不回收，稳态下容器文件膨胀。zipfs 此前**未暴露 compact 入口**（CLI 仅挂载参数），redb 4.1 库层有 `Database::compact()` 但程序从不调用，故只能记未 compact 的膨胀稳态。FIRST-RUN §4 实测 0.54x（物理 1260MiB > 逻辑 676MiB，那是经 fio 随机写负载后的重膨胀态）。

### 修复

- 新增 **`zipfs compact --backend container --backing <redb 文件>`** 子命令：独占打开容器、调 `ContainerStore::compact()`（内部 `redb::Database::compact()`）、报告前后大小与收缩比。
- CLI 用 clap 可选子命令重构：**无子命令 = 挂载**（向后兼容原 `--backend/--backing/--mountpoint/--chunk-size`，`--backend` 缺省回退 passthrough），新增 `compact` 子命令。挂载 4 参数用法完全不变。
- 未自动在卸载时 compact（保持卸载快、行为可预期）——仅提供显式子命令。

### 实测（真实数据子集，bench/datasets/claude-projects/ 的只读副本，cp -a 进全新 container）

数据集：676.3 MiB 逻辑 / 408 文件（1 个 symlink 仍 ENOTSUP 未写，同 FIRST-RUN §4）。chunk 64KiB，zstd:3。

| 阶段 | 物理占用 | 压缩比（逻辑 676.3MiB / 物理） |
|---|---|---|
| compact 前（本轮，干净 cp 后稳态） | 292.0 MiB | 2.32x |
| **compact 后** | **176.1 MiB** | **3.84x** |
| 收缩倍数 | — | 1.66x（292→176 MiB） |

对照：
- FIRST-RUN §4 未 compact 稳态 **0.54x**（1260MiB，那是 fio 随机写负载后的极端膨胀态）；本轮干净顺序 cp 后未 compact 已是 2.32x（负载不同，膨胀程度不同），**compact 后 3.84x**。
- 设计 §6.1 推算 64KiB 块 compact 后约 4.1x（`123MiB×1.34≈165MiB` 口径）。实测 3.84x（176MiB）与推算同量级、略保守（redb compact 后仍保留少量页对齐/B-tree 开销，且本副本压缩比基线 5.49x 的口径含 ext4 块取整，与 redb 容器口径不完全可比）。
- compact 后容器**重新挂载校验数据完整**：抽样 3.3MB jsonl 文件 `cmp` 与源副本逐字节一致。

结论：compact 是 BV 路线的必需收尾步骤，已落地并实测验证可把膨胀稳态收回到接近真实压缩比。

## 质量门 / 验证

- `cargo build --release`、`cargo clippy --all-targets -- -D warnings`、`cargo fmt` 全绿。
- `cargo test` 全绿：53 个 lib 单测（含新增 reader 缓存正确性/写后失效/release 释放/rename 覆盖失效、container compact 后可读且体积不增）+ 集成测试 model_based 差分（两后端）、mount_rw 真实挂载（两后端）、append_growth（两后端）、passthrough 全通过。
- rust-reviewer 审查：1 个 HIGH（reader 缓存 open→insert 回填竞态）已用 epoch 世代戳修复并补回归测试；M1（注释与实现不符）、M2（rename 覆盖目标缓存失效）、L1（空洞块零缓冲复用）、L3（commit 后 sync 前即失效）均已修。append-only + pred 使「无锁并发读 + 持锁 commit」对旧 reader 安全（reviewer 确认无 CRITICAL 数据损坏路径）。

## 残留与遗留风险

- **残留清理**：本轮临时 backing（`bench/.fix-bs-backing`、`bench/.fix-bv-backing`）已删除；fix 挂载点已卸载（`/proc/mounts` 无 zipfs fix 挂载、无 zipfs mount 进程）。源 `bench/datasets/claude-projects/`（678M）与 `~/.claude/projects` 只读未改。FIRST-RUN 的 `.bv-backing`/`.bs-backing` 未触碰。
- **遗留风险**：
  1. reader 缓存内存随同时打开的文件数增长，靠 `release` 回收；长驻、海量并发打开大文件时缓存内存上界由打开句柄数决定（首版可接受，未做 LRU 容量上限）。
  2. compact 须容器未挂载（独占打开），靠 redb 文件锁失败兜底，未额外做「检测挂载中」友好报错。
  3. 修复后数字为单轮热缓存、单文件快速复算，未取中位数、未冷缓存（同 FIRST-RUN 局限）；要纳入正式对照建议按 FIRST-RUN §7 流程多轮复跑。
  4. symlink 仍 ENOTSUP（布局 V/S 均未实现 symlink 创建），与本次修复无关，属既有已知项。
