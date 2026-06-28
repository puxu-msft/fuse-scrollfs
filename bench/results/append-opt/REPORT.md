# append 优化（开放尾块缓冲）—— before/after 微基准报告

> 日期：2026-06-28。优化：未压缩**开放尾块缓冲**（open-tail buffer），落在 Core 写会话（per-inode）。
> 对应设计 [docs/01-zipfs-design.md](../../../docs/01-zipfs-design.md) §1.1 追加写硬约束、§3 分块内核、§5 Store 接缝。

## 问题

原 `core/rmw.rs` 写到未满尾块走 `get_block → decompress → patch → compress → put_block`，即**每次小 append 都把尾块整块重压一遍**。64KiB 块 + 1KB 行 → 一个尾块封块前被重压约 64 次。

## 优化

- **尾块缓冲放在 Core 的「写会话」（per-inode，`core/wsession.rs::TailSessions`），不放 Store**。Store 仍只存已封的压缩块，压缩仍全在 Core，保持 §5 接缝干净，两布局（BS/BV）同时受益。
- **append / 写尾块** → 直接写进未压缩缓冲，**不压缩**。
- **封块（seal）时机**：尾块填满 chunk_size、flush/fsync/release、或一个需要旧尾块的非尾块写。
- **读协调**：读路径（`rwfs::read_range`）先查写会话；命中缓冲尾块则从未压缩缓冲返回，不走 `get_block`。
- **开关**：默认**开启**；`--no-tail-buffer` 走旧路径（每次 append 重压尾块），供基准对照。

## 测量方法

`fuse/src/bin/append-bench.rs`：直接驱动 Core（`TailSessions` + `rmw`）+ 真实 Store（ShadowStore / ContainerStore），不经 FUSE 挂载（优化恰落在这层，免挂载噪声，且能读 `rmw::block_compress_count` 埋点）。负载：逐行 append 小记录（半可压缩 JSON 行）到一个增长文件 + 每 100 行 fsync 一次。**默认单次短跑**（用户要求减少测试量）。

复现：

```bash
cd fuse && cargo build --release --bin append-bench
./target/release/append-bench                                   # 默认 64KiB 块 / 1KB 行 / 20000 行
./target/release/append-bench --chunk-size 4096 --line-size 512 # 小块场景
```

## 结果：默认配置（chunk=64KiB，行=1KB，20000 行，fsync/100）

| 后端 | 配置 | 墙钟 | 吞吐(行/s) | 吞吐(MiB/s) | **重压次数** | 压缩比 |
|---|---|---|---|---|---|---|
| BS 影子树 | OFF (before) | 2.50s | 8004 | 7.82 | **20000** | 27.86x |
| BS 影子树 | ON (after)  | 1.79s | 11192 | 10.93 | **500** | 27.86x |
| BV 容器 | OFF (before) | 1.29s | 15553 | 15.19 | **20000** | 65.79x |
| BV 容器 | ON (after)  | 0.51s | 38890 | 37.98 | **500** | 65.79x |

**对比（after vs before）**：

- **重压次数：20000 → 500，降 40x**（≈ 满块数；旧路径每次 append 重压尾块）。
- **吞吐：BS +40%（8004→11192 行/s），BV +2.5x（15553→38890 行/s）**。
- **压缩比不变**（27.86x / 65.79x）：优化只改「何时压」，不改「压什么」，最终落盘块完全一致。

## 结果：小块配置（chunk=4KiB，行=512B，20000 行，fsync/100）

| 后端 | 配置 | 墙钟 | 吞吐(行/s) | 吞吐(MiB/s) | 重压次数 | 压缩比 |
|---|---|---|---|---|---|---|
| BS 影子树 | OFF (before) | 2.45s | 8175 | 3.99 | 20000 | 1.94x |
| BS 影子树 | ON (after)  | 2.05s | 9748 | 4.76 | 2600 | 1.94x |
| BV 容器 | OFF (before) | 1.35s | 14797 | 7.23 | 20000 | 12.38x |
| BV 容器 | ON (after)  | 0.82s | 24310 | 11.87 | 2600 | 12.38x |

小块下满块更频繁（每块仅约 8 行），重压降幅 20000→2600（≈7.7x），吞吐 BV +1.6x、BS +19%。块越大、行越小，优化收益越大（趋近设计 §1.1 的「64x 重压」上限）。

## 结论

开放尾块缓冲把 append 路径的尾块重压从「每次 append 一次」降到「每满块一次」，在目标负载（逐行 append 大 jsonl + 周期 fsync）上：

- 重压次数降一个数量级以上（默认配置 40x）。
- 吞吐显著提升（BV 最高 2.5x）。
- 压缩比与正确性零回退（落盘块与旧路径逐字节一致，由 model_based 两后端差分测试守住）。

## §A fsync 碎片化消除（fsync 频率扫描）

### 问题：fsync 把尾块切碎

周期 fsync 必须把当前**未满**尾块 durably 落盘（POSIX fsync 契约）。朴素做法是每次 fsync 都把当前部分尾块封块、随后 append 时**另起新块**——于是一个本应是「满块」的逻辑块被切成若干个「部分块版本」：

- **BS（archive）**：每个旧部分尾块版本在包内成永久空洞（旧 index/footer/块字节遗留），物理文件随 fsync 次数线性膨胀，压缩比被一堆「半满小块」拖垮。
- **BV（redb）**：覆盖同 key 不留逻辑空洞，但块边界变碎仍降低每块压缩率。

### 改法：续写同一逻辑尾块，复用 slot

fsync 时仍 durably 提交当前部分尾块（满足契约），但**不另起新块**：

- Core 写会话（`wsession`）在 fsync 的 `seal` 后移除缓冲；下次 append 经 `ensure_tail_loaded` 从 Store **解压一次**把那个未满尾块装回尾缓冲，**续写同一 idx**——封满时覆盖同 idx 的旧部分块（BV/redb 覆盖同 key；BS 见下）。
- BS archive 层（`archive::ArchiveUpdater`）：`open` 时写游标置于 **live 数据区末尾**（`live_data_end` = 所有 index 项 `offset+clen` 最大值），回收上次提交遗留在尾部的「已死旧尾块版本 + 旧 index/footer」空洞；`set_block` 重写**当前最末 live 块**时复用其自身 slot（`reuse_tail_slot`），原地覆盖而非追加，使 archive 在 append 主负载下保持紧凑。
- **代价**：每个 fsync 区间多 **1 次重压**（把封掉的部分尾块解压回缓冲继续追加）。换来块保持满尺寸、压缩比/物理体积不随 fsync 频率劣化。

### 数据：块数 / 压缩比 / 物理体积不随 fsync 频率劣化

`append-bench` 新增 `--fsync-sweep K1,K2,...`：同一负载下扫描多个 fsync 频率，对照「块数 / 压缩比 / 重压 / 吞吐」。复现：

```bash
cd fuse && cargo build --release --bin append-bench
./target/release/append-bench --backend both --lines 5000 --line-size 1024 \
    --chunk-size 65536 --fsync-sweep 100000,100,10,5
```

**BS 影子树（5000 行 × 1KB，64KiB 块）** —— 块数 / 压缩比 / 物理体积**完全恒定**，只有重压随 fsync 变密：

| fsync 频率 | 吞吐(行/s) | 重压次数 | **块数** | **压缩比** | **物理(B)** |
|---|---|---|---|---|---|
| /100000（≈无 fsync） | 329198 |   79 |   79 | 283.70x | 18047 |
| /100 | 7566 |  125 |   79 | 283.70x | 18047 |
| /10  |   736 |  563 |   79 | 283.70x | 18047 |
| /5   |   388 | 1063 |   79 | 283.70x | 18047 |

**BS 影子树（5000 行 × 512B，4KiB 块）** —— 同样恒定：

| fsync 频率 | 吞吐(行/s) | 重压次数 | **块数** | **压缩比** | **物理(B)** |
|---|---|---|---|---|---|
| /100000 | 206803 |  625 |  625 | 34.88x | 73387 |
| /100 | 12665 |  650 |  625 | 34.88x | 73387 |
| /10  |  1237 | 1000 |  625 | 34.88x | 73387 |
| /5   |   644 | 1500 |  625 | 34.88x | 73387 |

**结论（BS）**：块数、压缩比、物理体积在 fsync 频率从「几乎不 fsync」到「每 5 行 fsync」之间**逐字节不变**，碎片化彻底消除。唯一随频率上升的是重压次数（每 fsync 区间 +1 次重压），且仍远低于 append 次数。修复前（朴素另起新块）同负载下 fsync/5 会把 archive 膨胀约 15x、压缩比从 ~76x 崩到 ~5x（见 `shadow_频繁_fsync_不碎片化_*` 测试注释的历史对照）。

> **崩溃 fail-closed（rust-review CRITICAL 修复）**：BS 的 reuse_tail_slot 原地覆盖最末 live 块时，若新压缩长度 <= 旧长度，覆盖只触及该块前缀、不碰其后遗留的旧 index/footer——崩溃后 open 会据旧 footer 读出「新前缀 + 旧残尾」的 Frankenstein 块（archive 无 per-block 校验拦不住），静默损坏已 fsync 过的旧版本。修复：原地覆盖**前**先 `set_len` 截掉该 slot 之后的旧 index/footer 再 `sync_data`，使崩溃窗口内 EOF 不再是合法 footer → open 检测损坏报错，恢复 append-only 的 fail-closed 语义。代价是每次 reuse 多一次 fsync barrier——这正是上表 BS 在高 fsync 频率下吞吐相对 append-only 偏低的来源（fsync/5 ≈388 行/s），是「最大化 durability」请求下的正当成本，碎片化指标不受影响。

**BV 容器** —— 逻辑**块数同样恒定**（redb 覆盖同 key，无碎片），块边界不随 fsync 变碎；其物理体积/压缩比的波动来自 **redb MVCC 未引用页**（与本碎片化无关，由离线 `compact` 回收，见 FIRST-RUN §4），不是尾块切碎：

| fsync 频率 | 吞吐(行/s) | 重压次数 | **块数** |
|---|---|---|---|
| /100000 | 652000 |   79 |   79 |
| /100 | 33232 |  125 |   79 |
| /10  |  4770 |  563 |   79 |
| /5   |  2360 | 1063 |   79 |

重压次数与 BS 完全一致（每 fsync 区间 +1），证明「续写同一逻辑尾块」逻辑落在 Core 层、两后端共享。

### 正确性（§A 新增测试）

- `shadow_频繁_fsync_不碎片化_物理体积与块数对齐稀疏_fsync`：fsync/5 与 fsync/100 跑同一负载，断言最终块数相等、物理体积比 < 1.05、压缩比不被拖垮。
- `shadow_频繁_fsync_后内容_durable_且续写逐字节一致`：高频 fsync → 重开 store（模拟重挂）经 archive 读回已 fsync 内容逐字节一致（durable）→ 继续 append → 读回整文件逐字节与期望一致（续写同一逻辑尾块无错位）。
- `archive::updater_reuse_尾块原地覆盖中崩溃_不静默错读旧版本`（rust-review CRITICAL 守护）：reuse 原地覆盖（新压缩长度 < 旧长度）后不 commit 即模拟崩溃，断言 open 必须 fail-closed（报损坏或读回一致旧版本），绝不返回 Frankenstein 块。

## 正确性守护

- 新增 `core::wsession` 单测（8 个）：大量小 append 内容正确 + 重压远少于行数、fsync 后尾块已封可读、read-while-appending 读缓冲尾块、append 与随机写/截断混合、越 EOF 空洞、关闭优化退化旧路径、forget 不封块。
- 新增 `tests/append_tail_buffer.rs`（6 个，两后端）：端到端重压次数 < 行数/2 + 内容/size 正确 + container 重开持久 + `--no-tail-buffer` 退化正确 + 并发读/seal 无 torn read + §A 抗碎片化（物理/块数对齐稀疏 fsync）+ §A durable 续写逐字节一致。
- 既有 `model_based`（两后端差分）、`mount_rw`（真实挂载，含 read-while-appending / RMW / truncate / rename）、`append_growth`（增量增长）全绿。

> 全套：`cargo test --release` → unit 63 + append_tail_buffer 6 + model_based 3 + mount_rw 2 + append_growth 3 + passthrough 1，全绿；`cargo clippy --release --all-targets -D warnings` 干净。
