# zipfs 五条件一致性大对照（CONSOLIDATED）

> TAG `20260628-1212` · 修复后二进制（BS reader 缓存 + BV compact 已落地）· 3 轮取中位数 · 64KiB 块 · zstd:3
> 原始数据：本目录 `r{1,2,3}/summary.csv`、`compression-ratio.txt`。前序：`FIRST-RUN.md`（未修复）、`FIXES-ADDENDUM.md`（修复点）。

## 0. 读数前必看（口径与局限）

- **热缓存，无 drop_caches**（无免密 sudo）。读数偏乐观，跨条件相对排名仍可比；绝对值非冷缓存。
- **A(btrfs) 压缩比未测到**：`compsize` 需 root，本轮 `SEARCH_V2: Operation not permitted` 失败。**待补**：`sudo compsize /mnt/zipfs-btrfs/<写入数据后>`。
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
| **A (btrfs zstd:3)** | **未测** | **未测** | compsize 需 root（待补） |
| **BV (容器/redb)** | 177 MiB | **3.84x** | `du -sh` 实际磁盘块（干净写，compact 无关） |
| **BS (影子树)** | 125 MiB | **5.42x** | `du -sb` archive 树 |
| zstd-3 单流上限 | 37.6 MiB | 18.0x | 跨文件去冗参照 |

## 4. 核心发现

### 4.1 BS reader 缓存修复 = 本项目最大反转
FIRST-RUN 里 BS 随机读病态（1.4 MiB/s）。修复后 **BS rand-read-4k 52.9 MiB/s（13.5k IOPS），与内核 btrfs(A) 53.7 基本持平**；rand-read-64k BS 971 甚至 **超过 A 的 516**。结论从「BV 随机 IO 压 BS 30x」**彻底反转为「BS 读侧与内核态同档、且压缩比最高」**。教训：FIRST-RUN 的布局结论是被一个实现 bug 污染的——基准必须先修正确性再下结论。

### 4.2 谁强在哪（修复后真实画像）
- **压缩比**：BS 5.42x > BV 3.84x（A 待补，预期落在两者与 18x 之间）。
- **随机读**：B0(透传) > BS ≈ A > BV。BS 读侧已是第一梯队。
- **顺序写**：B0 > A > BS > BV。内核 A 在压缩条件里最快。
- **随机写吞吐**：C0 > B0 > A > BV > BS（压缩 + RMW 拖累用户态）。
- **写尾延迟（决定性差异）**：**A/C0 亚毫秒（107/128µs）**，而 **FUSE 三条全是毫秒级**（B0 6.3ms、BS 13ms、BV 抖到 28ms）。内核态在写延迟稳定性上压倒性领先——FUSE 上下文切换 + 用户态 RMW 的尾部代价真实可见。

### 4.3 BV 的膨胀是「覆盖写」现象，非「写入」本身
干净写 709MB → BV 177 MiB（3.84x），**无需 compact**。8GB 膨胀只在 fio 反复随机覆盖（MVCC 旧页堆积）时出现。**目标负载 `~/.claude/projects` 是追加为主、极少中间覆盖** → BV 在真实负载下的空间表现应接近 3.84x 的健康值，膨胀风险被高估。但活跃覆盖场景仍需 compact/GC 兜底。

## 5. §4.6 场景适配表（数据支撑）

| 场景 | 主导判据 | 推荐 | 依据（本轮数据） |
|---|---|---|---|
| 冷归档 / 写少读多 / 压缩比优先 | 压缩比 + 读 | **BS** | 5.42x 最高 + 读已与内核同档（rr4k 52.9, rr64k 971） |
| 活跃随机读写 / 写延迟敏感 | 写尾延迟 | **A (btrfs)** | 写 p99 107µs vs FUSE 三条 ms 级；BV/BS 用户态写尾是结构短板 |
| 顺序大吞吐 | 吞吐 | C0/B0；压缩则 **A** | A seq-write 245 > BS 192 > BV 142 |
| **追加为主（目标负载 ~/.claude/projects）** | 综合 | **BS 或 A**（BV 亦可） | BS 追加友好 archive + 最高压缩比；BV 干净写 3.84x 无需 compact（§4.3）。**fio 未覆盖纯 append，需专测** |

## 6. 待办（下一步）

1. **补 A 压缩比**：`sudo compsize`，补齐三判据里 A 的最后一格。
2. **追加写专测**：fio 不测 append；写一个「持续 append 到大文件」微基准，量 BS/BV/A 在目标负载真实写模式下的吞吐与放大——这才是 `~/.claude/projects` 的主路径。
3. **冷缓存复跑**：需免密 sudo 或一次性 root，drop_caches 后复跑，得真实磁盘态（当前热缓存偏乐观）。
4. **BV 写尾抖动**（rw64k p99 抖到 28ms）定位：疑似 redb commit/MVCC 抖动，值得 profile。
5. 用户态写尾延迟（FUSE 三条 ms 级）是 BV/BS 对 A 的最大劣势，是否可优化（批量/异步 commit）值得探究。
