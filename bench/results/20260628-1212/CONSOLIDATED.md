# zipfs 五条件一致性大对照（CONSOLIDATED）

> TAG `20260628-1212` · 修复后二进制（BS reader 缓存 + BV compact 已落地）· 3 轮取中位数 · 64KiB 块 · zstd:3
> 原始数据：本目录 `r{1,2,3}/summary.csv`、`compression-ratio.txt`。前序：`FIRST-RUN.md`（未修复）、`FIXES-ADDENDUM.md`（修复点）。

## 0. 读数前必看（口径与局限）

- **热缓存，无 drop_caches**（无免密 sudo）。读数偏乐观，跨条件相对排名仍可比；绝对值非冷缓存。
- **A 配置已改为 `compress-force`（本场景最佳）；压缩比已用 force 重测 = 6.74x**：早先 2.44x 是 btrfs 默认启发式（漏压 212M）的误配置。force 重测后只剩 41K 未压、676M→100M = **6.74x，压缩比第一**（见 §3/§4.2，结论已据此反转）。**注意**：本轮 A 的**速度列仍是启发式配置下测的**（force 多压 212M、写 CPU 略增、读数据更小），属次要偏差，未重测——见 roadmap T0。
- **BV 空间口径**：redb 是稀疏文件，`du -sb`(apparent 306M) 误导；**实际磁盘块 `du -sh`=177 MiB 才是真占用** → 3.84x。本轮 `compact` 对**干净写入**几乎不缩（apparent 反而涨），因为 **BV 膨胀是「随机覆盖写产生 MVCC 旧页」现象，不是干净/追加写**——对追加为主的目标负载是利好（见 §4）。
- 单机单盘；fio 合成数据 50% 可压缩、size 512m、随机 job 30s 上限；btrfs 128KiB extent vs zipfs 64KiB chunk 的粒度差异。
- 3 轮方差小（除 BV rand-write-64k 的 p99 有抖动，r2 飙到 28ms）。

## 1. 环境

| 项 | 值 |
|---|---|
| 内核 / 平台 | `6.18.33.1-microsoft-standard-WSL2` |
| CPU / RAM | i9-10900X 20 线程 / 196 GiB |
| 后端盘 | ext4 `/dev/sdd`；A 为 btrfs loop(zstd:3) 镜像于同盘 |
| 块大小 / 算法 | 64 KiB / zstd level 3（A 与 BV/BS 对齐） |

## 2. 速度（3 轮中位数，热缓存，bw=MiB/s）

| job | C0 (ext4) | A (btrfs) | B0 (透传) | BV (容器) | BS (影子) |
|---|---|---|---|---|---|
| seq-write | 216 | **245** | 355 | 142 | 192 |
| rand-read 4k | 19.5 | **53.7** | 71.4 | 42.7 | **52.9** |
| rand-read 64k | 158 | 516 | 1295 | 554 | **971** |
| rand-write 4k | 28.2 | **11.6** | 20.0 | 7.6 | 5.8 |
| rand-write 64k | 97 | 93 | 112 | 67 | 58 |

延迟（rand-read-4k p50/p99 µs；rand-write-64k p99 µs）：

| | C0 | A | B0 | BV | BS |
|---|---|---|---|---|---|
| rr4k p50/p99 | 189/354 | 69/114 | 53/82 | 86/138 | 74/106 |
| **rw64k p99（写尾延迟）** | 128 | **107** | 6259 | 2376（抖到 27918） | 13435 |

## 3. 压缩比（真实数据：`~/.claude/projects` 子集，709 MB / 408 文件）

| 条件 | 物理占用 | 压缩比 | 口径 |
|---|---|---|---|
| C0 / B0 | 678 MiB | 1.00x | 不压缩 |
| **A (btrfs `compress-force` zstd:3)** | 100 MiB | **6.74x** | compsize（force 重测 2026-06-28，本场景最佳配置）；几乎全压（`none` 仅 41K）。**压缩比第一** |
| **BS (影子树)** | 125 MiB | **5.42x** | `du -sh` 磁盘块；`du -sb` apparent 则 5.49x（FIRST-RUN 口径） |
| **BV (容器/redb)** | 177 MiB | **3.84x** | `du -sh` 实际磁盘块（干净写，compact 无关） |
| ~~A (btrfs 默认启发式)~~ | 277 MiB | 2.44x | 旧配置：采样跳过 212M 不压，**非本场景最佳，已弃用**（setup 默认改 force） |
| zstd-3 单流上限 | 37.6 MiB | 18.0x | 跨文件去冗 + 全窗口参照（大窗口/去重的天花板） |

## 4. 核心发现

### 4.1 BS reader 缓存修复 = 本项目最大反转
FIRST-RUN 里 BS 随机读病态（1.4 MiB/s）。修复后 **BS rand-read-4k 52.9 MiB/s（13.5k IOPS），与内核 btrfs(A) 53.7 基本持平**；rand-read-64k BS 971 甚至 **超过 A 的 516**。结论从「BV 随机 IO 压 BS 30x」**彻底反转为「BS 读侧与内核态同档」**（压缩比 BS 5.42x，仅次于 btrfs-force 的 6.74x）。教训：FIRST-RUN 的布局结论是被一个实现 bug 污染的——基准必须先修正确性再下结论。

### 4.2 谁强在哪（修复后真实画像）
- **压缩比**：**A(btrfs force) 6.74x > BS 5.42x > BV 3.84x**。**结论已修正（force 重测）**：早先「zipfs 压缩比反超内核」是 btrfs 误用默认启发式（漏压 212M）的假象；换成 `compress-force`（本场景最佳配置）后**内核 btrfs 压缩比第一**——kernel zstd + 128KiB extent 窗口 > BS 64KiB chunk，且无每文件 archive 开销。**压缩比不是 zipfs 的优势**；zipfs 的价值在逐文件透明 / 无 root 无内核模块 / 策略可控（去重·字典·append 尾块缓冲），不在压缩比本身。
- **随机读**：B0(透传) > BS ≈ A > BV。BS 读侧已是第一梯队。
- **顺序写**：B0 > A > BS > BV。内核 A 在压缩条件里最快。
- **随机写吞吐**：C0 > B0 > A > BV > BS（压缩 + RMW 拖累用户态）。
- **写尾延迟（决定性差异）**：**A/C0 亚毫秒（107/128µs）**，而 **FUSE 三条全是毫秒级**（B0 6.3ms、BS 13ms、BV 抖到 28ms）。内核态在写延迟稳定性上压倒性领先——FUSE 上下文切换 + 用户态 RMW 的尾部代价真实可见。

### 4.3 BV 的膨胀是「覆盖写」现象，非「写入」本身
干净写 709MB → BV 177 MiB（3.84x），**无需 compact**。8GB 膨胀只在 fio 反复随机覆盖（MVCC 旧页堆积）时出现。**目标负载 `~/.claude/projects` 是追加为主、极少中间覆盖** → BV 在真实负载下的空间表现应接近 3.84x 的健康值，膨胀风险被高估。但活跃覆盖场景仍需 compact/GC 兜底。

## 5. §4.6 场景适配表（数据支撑）

| 场景 | 主导判据 | 推荐 | 依据（本轮数据） |
|---|---|---|---|
| 冷归档 / 写少读多 / 压缩比优先 | 压缩比 | **A (btrfs force)** | 6.74x 最高；BS 5.42x 次之，但有逐文件透明 / 无 root 之利 |
| 活跃随机读写 / 写延迟敏感 | 写尾延迟 | **A (btrfs)** | 写 p99 107µs vs FUSE 三条 ms 级；BV/BS 用户态写尾是结构短板 |
| 顺序大吞吐 | 吞吐 | C0/B0；压缩则 **A** | A seq-write 245 > BS 192 > BV 142 |
| **追加为主（目标负载 ~/.claude/projects）** | 综合 | **A（最优）；BS（要透明/无 root 时）** | 配 `compress-force` 的 btrfs 在压缩比(6.74x)/速度/写尾上全面领先；选 zipfs-S 是为逐文件透明/无 root/可控，非性能。append 专测见 append-opt/REPORT |

## 6. 待办（下一步）

> **完整路线图见 [docs/ROADMAP.md](../../../docs/ROADMAP.md)（单一信息源）**。下列为本轮直接相关的近期项摘要。

1. **补 A 压缩比**：`bash bench/scripts/measure-a-ratio.sh`（需 sudo compsize），补齐三判据里 A 的最后一格。
2. ~~追加写专测~~ **已完成**：见 [append-opt/REPORT.md](../append-opt/REPORT.md)（尾块缓冲重压 40x↓ + fsync 抗碎片）。
3. **B2（`fuse-zstd` 整文件）消融**：§9 矩阵此项从未跑，缺「分块 vs 整文件」外部实证。
4. **冷缓存复跑**：需免密 sudo / 一次性 root，drop_caches 后复跑（当前全热缓存偏乐观）。
5. **FUSE 写尾延迟**（FUSE 三条 ms 级 vs btrfs 亚毫秒）是 BV/BS 对 A 最大劣势，批量/异步 commit 待探（roadmap T2）。
