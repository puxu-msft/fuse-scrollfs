# zipfs 首轮真实基准对照（FIRST-RUN）

> ⚠️ **已被取代（SUPERSEDED）**：本轮 BV/BS 数据是 **BS reader 缓存修复前** 的，其中 BS 随机读（1.4 MiB/s）是实现 bug 所致、非布局特性。权威对照见 [../20260628-1212/CONSOLIDATED.md](../20260628-1212/CONSOLIDATED.md)（修复后），修复点见 [FIXES-ADDENDUM.md](./FIXES-ADDENDUM.md)。**本文仅作历史记录，勿据其 BV/BS 数字下结论。**
>
> 本轮覆盖四个**无需 sudo** 的条件：**C0**（裸 ext4 目录）、**B0**（FUSE 透传）、**BV**（布局 V 容器 / redb）、**BS**（布局 S 影子树）。
> 条件 **A**（btrfs + zstd，需 sudo + `modprobe btrfs`）本轮**跳过**，由用户另行测量。
> 对齐 `docs/00-overview.md` §4（条件/指标/数据集）与 `docs/01-zipfs-design.md` §6.1（默认 64KiB 块）。

## 0. 本轮局限（先声明，避免误读）

- **热缓存，未 drop_caches**：本机非 root 且无免密 sudo，`run-suite.sh` 显式降级为热缓存模式。所有速度数字是 **page cache 命中态**，跨条件对比偏乐观，**不是冷缓存**。
- **无 btrfs（条件 A 缺位）**：本轮没有内核态压缩参照，「FUSE 税 vs 内核压缩税」的对比待 A 补齐。
- **单轮，未取中位数**：总纲 §4.5 要求每点重复 ≥3 取中位数；本轮每条件每 job 仅 1 次，存在抖动。
- **合成 fio 数据可压缩性 = 50%**（`buffer_compress_percentage=50`），非真实数据；压缩比另用真实子集单独量（见 §4）。
- **随机 job 有 30s 运行上限**（`FIO_EXTRA="--runtime=30"`），但 layout 阶段（建 1G 文件）不计入该上限，故 BS 随机 job 总耗时可达十余分钟（见 §5 解读）。

## 1. 环境与版本

| 项 | 值 |
|---|---|
| 内核 | `6.18.33.1-microsoft-standard-WSL2`（WSL2） |
| CPU | Intel Core i9-10900X @ 3.70GHz，20 线程 |
| 后端磁盘 | ext4 on `/dev/sdd`（约 323G 空闲） |
| fio | 3.42（`/home/linuxbrew/.linuxbrew/bin/fio`） |
| zstd CLI | v1.5.7（理论上限基准用） |
| Rust / cargo | 1.96.0 |
| zipfs commit | `877e18c` |
| **逻辑块大小** | **64 KiB**（`--chunk-size 65536`，§6.1 裁决，BV/BS 均用此值） |
| zipfs 压缩算法 | zstd level 3（`ZipfsRw::new(store, Algo::Zstd, 3, chunk)`） |
| 挂载方式 | 普通用户 FUSE（`/dev/fuse` + `fusermount3`），全程无 sudo |

## 2. 对照条件（怎么挂的、backing 在哪）

| 代号 | 条件 | 挂载/目标 | backing | 备注 |
|---|---|---|---|---|
| **C0** | 裸 ext4 目录（吞吐地板） | `bench/.mnt/c0`（普通 ext4 子目录） | 自身 | 不压缩，压缩比 = 1.0 |
| **B0** | FUSE 透传（passthrough） | `bench/.mnt/b0` | `bench/.b0-backing`（ext4 目录） | 隔离纯 FUSE 税，不压缩，压缩比 = 1.0 |
| **BV** | 布局 V 容器（container / redb 全包） | `bench/.mnt/bv` | `bench/.bv-backing/zipfs.redb`（单文件 redb 容器） | zstd:3，64KiB 块，读写 |
| **BS** | 布局 S 影子树（shadow / 每文件压缩包） | `bench/.mnt/bs` | `bench/.bs-backing`（ext4 archive 树目录） | zstd:3，64KiB 块，读写 |

挂载命令（新增脚本）：
- BV：`zipfs --backend container --backing bench/.bv-backing/zipfs.redb --mountpoint bench/.mnt/bv --chunk-size 65536`（`bench/scripts/mount-bv.sh`，注意 container backing 是 **redb 文件**，不存在则由 zipfs 创建）。
- BS：`zipfs --backend shadow --backing bench/.bs-backing --mountpoint bench/.mnt/bs --chunk-size 65536`（`bench/scripts/mount-bs.sh`，backing 是**目录**）。
- 卸载：`umount-bv.sh` / `umount-bs.sh`（`fusermount3 -u` + SIGTERM 收尾守护）。

## 3. 速度表（fio，合成数据 50% 可压缩，**热缓存**）

数据源：`bench/results/20260627-1641/summary.csv`（`collect.py` 汇总）。bw 单位 MiB/s，延迟单位 µs。

### 3.1 顺序写（seq-write, bs=1M, 1G）

| 条件 | bw (MiB/s) | IOPS | lat p50 (µs) | lat p99 (µs) | usr/sys CPU% |
|---|---|---|---|---|---|
| C0 | 202.6 | 202.6 | 505.9 | 839.7 | 2.9 / 10.5 |
| B0 | **321.0** | 321.0 | 823.3 | 1269.8 | 4.8 / 2.4 |
| BV | 115.5 | 115.5 | 3948.5 | 6717.4 | 2.4 / 0.7 |
| BS | 118.1 | 118.1 | 3653.6 | 6389.8 | 1.9 / 1.2 |

> 注：B0 顺序写 bw 高于 C0，是热缓存 + 透传回写合并的假象，不代表 FUSE「更快」；冷缓存下会反转。

### 3.2 随机读（rand-read, 4k / 64k, 各 1G）

| 条件 | 块 | bw (MiB/s) | IOPS | lat p50 (µs) | lat p99 (µs) |
|---|---|---|---|---|---|
| C0 | 4k  | 13.0  | 3332.8  | 197.6  | 423.9 |
| C0 | 64k | 157.0 | 2512.1  | 391.2  | 544.8 |
| B0 | 4k  | 67.7  | 17328.4 | 53.0   | 97.8 |
| B0 | 64k | 1175.7| 18810.6 | 46.3   | 110.1 |
| BV | 4k  | 43.1  | 11046.5 | 86.5   | 169.0 |
| BV | 64k | 492.5 | 7880.7  | 118.3  | 240.6 |
| BS | 4k  | **1.4**   | 359.3   | 2605.1 | 5210.1 |
| BS | 64k | **12.2**  | 194.9   | 5079.0 | 5865.5 |

### 3.3 随机写（rand-write, 4k / 64k, 各 1G, 30s 上限）

| 条件 | 块 | bw (MiB/s) | IOPS | lat p50 (µs) | lat p99 (µs) |
|---|---|---|---|---|---|
| C0 | 4k  | 26.7  | 6835.0 | 4.7    | 58.1 |
| C0 | 64k | 116.0 | 1856.5 | 47.9   | 113.2 |
| B0 | 4k  | 14.4  | 3675.5 | 91.6   | 201.7 |
| B0 | 64k | 72.6  | 1162.3 | 130.6  | 7897.1 |
| BV | 4k  | 6.5   | 1656.5 | 284.7  | 593.9 |
| BV | 64k | 47.1  | 754.3  | 346.1  | 823.3 |
| BS | 4k  | 1.8   | 472.8  | 1548.3 | 4292.6 |
| BS | 64k | 24.0  | 384.5  | 1613.8 | 16056.3 |

## 4. 压缩比表（真实数据子集）

**数据集**：`~/.claude/projects/-home-xp-src-ghc2api-go` 的**只读副本**（`cp -a` 到 `bench/datasets/claude-projects/`，源**绝对未改**）。676.3 MiB 逻辑 / 408 文件，含一个 ~96 MiB 大 jsonl + 大量较小文件（双峰特征）。1 个 symlink（`memory`）未能在 BV/BS 创建（`Operation not permitted`，见 §5），其余 408 文件全部写入。

| 条件 | 逻辑 | 物理占用 | 压缩比（逻辑/物理） | 口径 |
|---|---|---|---|---|
| C0 / B0 | 676.3 MiB | 676.3 MiB | **1.00x** | 不压缩（对照） |
| **BS**（影子树 archive） | 676.3 MiB | **123.2 MiB** | **5.49x** | `du -sb bench/.bs-backing`（含 ext4 4KiB 块取整） |
| **BV**（redb 容器，**未 compact**） | 676.3 MiB | **1260.4 MiB** | **0.54x** ⚠️ | redb 容器文件大小（含 MVCC 未回收页 + 预分配） |
| zstd-3（理论上限参照） | 676.3 MiB | 37.5 MiB | **18.02x** | 整目录 tar 单流 `zstd -3`（跨文件去冗） |

要点：
- **BV 未 compact**：zipfs 当前**未暴露 compact 子命令**（CLI 仅 `--backend/--backing/--mountpoint/--chunk-size`），redb 4.1 库层有 `compact()` 但程序未调用，故只能记**稳态（未 compact）**值。设计 §6.1 microbench 结论：**64KiB 块 compact 后约 1.34x 膨胀**——按此推算 compact 后 BV 物理约 `123MiB × 1.34 ≈ 165 MiB`、压缩比约 **4.1x**（**待 zipfs 暴露 compact 后实测验证**，本轮未实测）。当前稳态 0.54x（物理比逻辑还大）即设计文档预警的 redb 大 BLOB 膨胀。
- **BS** 用 `du`（含块取整），5.49x；与 zstd-3 单流 18x 的差距主要来自：分块（64KiB 独立压缩，无跨块/跨文件去冗）+ 每块/每文件 archive 元数据 + ext4 4KiB 最小块向上取整。
- README 标称 31x 是针对**完整 8.7GB**（跨会话冗余更高）；本子集 676MB 的 zstd-3 单流仅 18x，量纲不同，不矛盾。

## 5. 简短解读：BV vs BS 初步差异与成因

**速度（热缓存下）**：
- **FUSE 税（B0 − C0）**：随机读 B0 反而比 C0 快（热缓存 + 透传 readahead 假象）；真实税要等冷缓存。顺序/随机写 B0 ≈ C0 量级，FUSE 透传本身税不大。
- **压缩前端税（BV/BS − B0）**：两者顺序写都掉到 ~115–118 MiB/s（zstd:3 + 分块 + 回写），约为 B0 的 1/3。
- **BV 明显优于 BS 的随机读/写**：BV 随机读 4k 43 MiB/s（11k IOPS）vs BS 1.4 MiB/s（359 IOPS），**约 30x**；64k 同样 BV 492 vs BS 12 MiB/s。随机写 BV 也全面领先（4k 6.5 vs 1.8、64k 47 vs 24 MiB/s）。
  - **成因推测**：BS 每文件独立 archive，随机 4k 访问触发「定位块→解压整 64KiB 块→（写）改尾块/footer 原子更新」，每 op 走文件系统多次 open/seek/小 IO，**读放大 ~16x + 每 op archive 元数据开销**；BV 把所有块塞进单个 redb B-tree，块定位是内存态 B-tree 查找 + 单文件偏移读，省掉了 per-file 文件系统往返。BS 随机 job 实测单 job 耗时十余分钟（layout 1G 经压缩前端 + 30s 读阶段），佐证其 per-op 重。
- **延迟**：BS p99 普遍最差（随机读 5–6 ms、随机写 64k 16 ms），BV p99 维持在亚毫秒到 ~0.8 ms，量级优势明显。

**压缩比**：
- **BS 5.49x 实测可用**；BV 稳态 0.54x 是**未 compact 的 redb 膨胀**，不是真实压缩失败——这正是设计 §6.1 把 64KiB 定为默认（而非 256KiB）的动机，但即便 64KiB，未 compact 的稳态仍严重膨胀，**compact 是 BV 路线的必需步骤**（当前 CLI 缺该入口，是首要补强项）。

**对齐设计 §4.6「场景适配」雏形（待 A/冷缓存/多轮补全）**：
- 活跃随机读写 → **BV 明显优于 BS**（B-tree 块定位省掉 per-file FS 往返）；BS 的每文件 archive 在随机小 IO 下是结构性短板。
- 冷归档 / 写少读多 / 压缩比优先 → **BS 当前唯一给出真实压缩比（5.49x）**；BV 需先解决 compact，否则空间反而是负收益。
- 顺序大吞吐 → 两者都掉到 B0 的 ~1/3，FUSE+压缩前端税明显，预期是未来条件 A（内核态）的优势区。

## 6. 成功/失败与残留

- **成功**：C0 / B0 / BV / BS 四条速度全跑通（fix 了 `seq-write.fio` 的 `buffer_pattern=` 空值导致 fio 3.42 解析失败的 bug）；BV/BS 真实数据压缩比、zstd-3 理论上限均测得。
- **已知缺陷（如实记录，非失败跳过）**：
  1. **BV 无 compact 入口** → 压缩比只能记未 compact 稳态（0.54x），compact 后值为设计文档推算，未实测。
  2. **symlink 创建被拒**（BV/BS 均 `Operation not permitted`）→ 数据集 1 个 symlink 未复制；408 个常规文件全部成功。
  3. **BS 随机 IO 极慢**（单 job 十余分钟）→ 是真实性能特征，非脚本错误。
- **残留**：已全部卸载，`/proc/mounts` 无 zipfs，`pgrep zipfs` 无进程。fio-work 临时数据已随卸载清理；BV/BS backing 与数据集副本保留供复算（均在 `bench/` 下，源 `~/.claude/projects` 只读未改）。

## 7. 复现命令

```bash
# 构建
( cd fuse && cargo build --release )
# 挂载（无 sudo）
mkdir -p bench/.mnt/c0
bash bench/scripts/mount-b0.sh
bash bench/scripts/mount-bv.sh
bash bench/scripts/mount-bs.sh
# 速度（30s 随机上限，热缓存）
CONDITIONS="C0=$PWD/bench/.mnt/c0 B0=$PWD/bench/.mnt/b0 BV=$PWD/bench/.mnt/bv BS=$PWD/bench/.mnt/bs" \
  FIO_EXTRA="--runtime=30" bash bench/scripts/run-suite.sh
python3 bench/scripts/collect.py bench/results/<tag>
# 压缩比：cp -a 真实子集进 BV/BS，sync，量 backing 物理占用
# 卸载
bash bench/scripts/umount-bs.sh; bash bench/scripts/umount-bv.sh; bash bench/scripts/umount-b0.sh
```
