# T1 严重发现：reuse-tail-slot 提交窗口在 append + 大块下丢失整个会话日志

> 工具：`bench/scripts/crash-test.sh`（进程级 kill -9 崩溃 harness，本次新增）。
> 性质：**真 durability bug**——`fsync()` 向应用返回成功，崩溃后已确认数据全失。约 40% 概率复现。
> 日期：2026-06-28。状态：**已定位根因，未修**（修复点在 `archive.rs`，属并发中的邻居热点 + 崩溃一致性设计决策，见末「交接」）。

## 1. 现象（harness 实测）

逐行 append `{"seq":N}` 到 shadow 挂载、**每行 fsync**，记录 fsync 返回成功的最高 `seq`（acked）。守护被 kill -9 后重挂，量 backing archive 大小：

| run | acked（已 fsync 成功） | backing archive 大小 |
|---|---|---|
| 1 | 86 | 149 B（数据在） |
| **2** | **116** | **12 B（仅 header，全失）** |
| **3** | **135** | **12 B** |
| 4 | 147 | 225 B（数据在） |
| **10/11/12/14** | 136/108/156/129 | **12 B** |

约 40% 的崩溃留下 **12 字节 = 纯 archive header（magic+version），无 footer 无数据块** → 重挂不可读 / 数据全失，**尽管这些行的 `fsync()` 都返回了成功**。

> kill -9 杀进程**不丢内核 page cache**，故 12 字节证明数据从未经 write 落到 backing 文件，而是被提交路径**主动截断**了。

## 2. 根因（`fuse/src/archive.rs` `ArchiveUpdater::set_block`，第 652-655 行）

append 微行 + 大块（默认 1MiB）下，块 0 长期填不满，**每次 fsync 都重写块 0**，命中 `reuse_tail_slot` 原地覆盖路径：

```rust
if reuse_tail_slot {
    self.file.set_len(offset)?;   // offset = index[0].offset = HEADER_LEN = 12
    self.file.sync_data()?;       // ← 把「文件截到 12 字节」持久化
}
self.file.seek(SeekFrom::Start(offset))?;
self.file.write_all(stored_bytes)?;   // 覆写块 0
// commit(): 写 index → sync → 写 footer → sync
```

**崩溃窗口**：kill -9 / 掉电落在 `sync_data()`（截断已 durable）之后、footer 落盘之前 → 文件 durably 停在 **12 字节（纯 header）**。块 0 是**唯一**承载全部已 append 数据的块，被截掉即全失，且 EOF 无合法 footer → open 报损坏（fail-closed 成立），但**旧版本不可恢复**（设计 §10 已承认这条边界）。

## 3. 为何严重性被低估 + **1MiB 默认放大了它**

设计把该窗口定级为「提交中崩溃丢该次提交」。但对**目标负载（append-only 微行）**实情是：

- 块 0 在填满 `chunk_size` 前，**每次 fsync 都被 set_len(12)+覆写一遍** → 风险不是「丢最后一次写」，而是**「任一 fsync 崩溃丢掉自上次封块以来的全部行」**。
- **块大小直接决定暴露规模**：
  - 64KiB 块：块 0 ~5000 微行即填满封死，风险窗口 ≤64KiB。
  - **1MiB 块（现默认）：块 0 容 ~80000 微行、全程每次 fsync 暴露 → 暴露数据量与时长各放大 16x。**
- 即：**压缩优化（块 64KiB→1MiB，提交 18b2d25）以可靠性暴露为代价**。这是必须摆上台面的 trade-off。

## 4. 复现

```bash
( cd fuse && cargo build --release --bin zipfs )
# 跑多次（约 40% 触发 12 字节全失）：
for i in $(seq 1 10); do bash bench/scripts/crash-test.sh 80000 $(awk "BEGIN{print 0.6+0.15*$i}"); done
```

harness 正确地检出违规（durability + fail-closed 双查）。它「失败」=正确抓到了产品 bug。

## 5. 交接（不在本次修，避并发冲突 + 属设计决策）

修复点在 `archive.rs` 的提交/崩溃一致性，**正是邻居 docs/02（head 缓存 v2）正在动的热点**，且 §4.1 M1 已在推敲 `live_data_end` + 两段 barrier + fail-closed。候选方向（由该文件 owner 定夺）：

1. **提交不就地截断**：新块永远 append 到 `write_cursor`（fresh 位置），旧块留到新 footer 落盘后才成空洞——崩溃后旧 footer 仍在、可读旧一致版本。代价：物理文件随 fsync 增长，靠周期压实回收（碎片化 §A 已有机制）。**最契合 append-only**。
2. **双 footer 交替**：两个 footer 槽轮换写，崩溃总能扫回最近合法者（设计 §10 / ROADMAP T1「S 崩溃恢复」已列）。
3. **temp + 原子 rename 整包重写**：最简单崩溃安全，但每次 fsync 重写整个块 0，大块下慢。

**临时缓解（产品决策，非本报告擅自做）**：在崩溃安全提交落地前，可考虑活跃写场景用较小 `--chunk-size`（缩小暴露窗口），与冷封存大块（`zipfs seal`）解耦——活跃小块保命、冷数据大块保压缩比。
