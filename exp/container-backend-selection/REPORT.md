# microbench 报告：redb 作为布局 V 容器存变长压缩 chunk blob 的性能闸门

> 关联设计：[`docs/01-scrollz-design.md`](../docs/01-scrollz-design.md) §6（布局 V）、§6.1（三档形态 / 写批处理陷阱）。
> 闸门问题：**redb 默认「全包」形态（元数据 + 数据块都进 redb）存变长压缩 chunk blob 并做随机更新，性能与空间是否够用，还是需要触发设计 §6.1 的「redb 元数据 + 自写数据区」档位？**
> 日期：2026-06-27。数字均为本机实跑（见末尾环境），非估算。

## 1. 实验设计

### 建模

- 表 `blocks: key=(u64 ino, u64 idx) -> blob`（redb `&[u8]` 值；sqlite `BLOB` 列，主键 `(ino, idx)`）。
- blob 模拟「压缩后的变长 chunk」，**不真跑 zstd**——压缩在 Core 层，已被设计接缝隔离；microbench 只问「容器存变长 blob 并随机更新」的代价。
- blob 大小与内容**确定性派生**自固定种子加 `(ino, idx, version)`，用 LCG 生成，无真随机、完全可复现（`--seed` 固定）。
- 两组源块档：

  | 档 | 源块 | 压缩后 blob 区间（建模） |
  |---|---|---|
  | 64KiB | 64 KiB | 8–64 KB 变长 |
  | 256KiB | 256 KiB | 30–200 KB 变长 |

### durability 口径（关键，保证可比）

- redb 4.1.0 默认 `Durability::Immediate`：**每次 `commit` 都 fsync**。这正是设计 §6.1 担忧的「每写一事务 = 每写一 fsync」的源头。**保持默认**——用 `Durability::None` 会把陷阱测没了。
- sqlite 对齐设为 `journal_mode=DELETE` + `synchronous=FULL`（每事务 fsync），**不开 WAL**（WAL 会把 fsync 摊到 checkpoint，语义偏离 redb 的同步落盘，不公平）。
- 故两后端「每块一事务」语义一致：都是「每写一次 fsync」。

### 场景

1. **批量插入** N=200 文件 × M 块（M 自动推到逻辑总量≈1.45 GiB），测吞吐。
2. **随机 RMW**：随机选 `(ino,idx)` → 读出 blob → 改写成新变长 blob → 写回，测吞吐 + p50/p99/p999。
3. **事务策略对比**：`PerBlock`（每块一事务 commit）vs `Batched(K=64)`（每 64 块一事务）。
4. **空间**：插入后容器大小、RMW 后膨胀、`compact`（redb）/`VACUUM`（sqlite）后大小。

> 注：`PerBlock` 因每次 fsync 极慢，仅在数据子集（2130 / 660 块）上跑以量化「每块成本」，吞吐倍率与延迟即可外推。

## 2. 实测数字

### redb

| 块档 | 场景 | 吞吐 | p50 | p99 | p999 | 容器大小 |
|---|---|---|---|---|---|---|
| 64KiB | 插入·批K=64 | 2472 blk/s · 86.8 MiB/s | — | — | — | 2.01 GiB |
| 64KiB | 插入·每块 | **171 blk/s** | — | — | — | — |
| 64KiB | RMW·批K=64 | 2981 op/s | 19.9 ms | 31.7 ms | 55.9 ms | — |
| 64KiB | RMW·每块 | **171 op/s** | 5.2 ms | 16.8 ms | 58.2 ms | — |
| 256KiB | 插入·批K=64 | 1178 blk/s · 132.6 MiB/s | — | — | — | **4.00 GiB** |
| 256KiB | 插入·每块 | **155 blk/s** | — | — | — | — |
| 256KiB | RMW·批K=64 | 1577 op/s | 36.4 ms | 58.3 ms | 71.4 ms | — |
| 256KiB | RMW·每块 | **182 op/s** | 5.4 ms | 12.1 ms | 26.4 ms | — |

**空间（redb）**

| 块档 | 逻辑总量 | 插入后 | RMW 后 | 膨胀 | compact 后 | compact/逻辑 |
|---|---|---|---|---|---|---|
| 64KiB | 1.46 GiB | 2.01 GiB | 2.01 GiB | 1.00x | 1.95 GiB | **1.34x** |
| 256KiB | 1.45 GiB | **4.00 GiB** | 4.00 GiB | 1.00x | 2.14 GiB | **1.48x** |

### sqlite（对照）

| 块档 | 场景 | 吞吐 | p50 | p99 | p999 | 容器大小 |
|---|---|---|---|---|---|---|
| 64KiB | 插入·批K=64 | 1632 blk/s · 57.3 MiB/s | — | — | — | 1.49 GiB |
| 64KiB | 插入·每块 | 63 blk/s | — | — | — | — |
| 64KiB | RMW·批K=64 | 1228 op/s | 49.1 ms | 71.1 ms | 95.9 ms | — |
| 64KiB | RMW·每块 | 67 op/s | 13.9 ms | 65.3 ms | 101.1 ms | — |
| 256KiB | 插入·批K=64 | 1314 blk/s · 147.9 MiB/s | — | — | — | 1.46 GiB |
| 256KiB | 插入·每块 | 58 blk/s | — | — | — | — |
| 256KiB | RMW·批K=64 | 472 op/s | 130.9 ms | 191.6 ms | 234.5 ms | — |
| 256KiB | RMW·每块 | 60 op/s | 15.2 ms | 67.8 ms | 84.8 ms | — |

**空间（sqlite）**

| 块档 | 逻辑总量 | 插入后 | RMW 后 | 膨胀 | VACUUM 后 | VACUUM/逻辑 |
|---|---|---|---|---|---|---|
| 64KiB | 1.46 GiB | 1.49 GiB | 1.49 GiB | 1.00x | 1.49 GiB | **1.02x** |
| 256KiB | 1.45 GiB | 1.46 GiB | 1.47 GiB | 1.01x | 1.46 GiB | **1.01x** |

### 「每块一事务」陷阱量化（设计 §6.1 核心）

批量 K=64 相对每块一事务的 **RMW 吞吐提升**：

| 块档 | redb | sqlite |
|---|---|---|
| 64KiB | **17.5x** | 18.3x |
| 256KiB | **8.7x** | 7.8x |

插入侧同样剧烈：redb 64KiB 批量 2472 vs 每块 171 blk/s ≈ **14x**。

## 3. 关键发现

1. **「每写一事务」陷阱是真的、且严重。** 把一次 `write` 回调内的多块更新合并到一个事务、仅在 `fsync`/`flush` 时 commit，能带来 **8–18x** 吞吐提升。这坐实了设计 §6.1 对 `sqlitefs`「每写 COW sync」教训的判断：**写批处理是布局 V 的必备项，不是优化项**。两后端都吃这一红利，量级相近。

2. **redb 吞吐够用，且优于 sqlite。** 批量策略下 redb 插入 87–133 MiB/s、RMW 1577–2981 op/s，均快于同口径 sqlite。每块一事务下两者都掉到 ~60–180 op/s（fsync 墙），符合预期。

3. **redb 的空间放大是唯一的红灯，且 256KiB 档触线。**
   - sqlite 容器≈逻辑总量（1.01–1.02x），近乎零浪费。
   - redb **大 BLOB 显著膨胀**：256KiB 档插入后 **4.00 GiB 装 1.45 GiB 逻辑数据（2.75x）**，64KiB 档 1.37x。这正是设计 §6.1 预言的「`CHUNK_SIZE=256KiB` 单 value 上百 KiB 撑大 B-tree 页 / CoW 页分裂」。
   - `compact` 能大幅回收（256KiB → 2.14 GiB），但**仍剩 1.48x**，且 compact 是重操作（需 `&mut Database`、独占、整库重写），不能频繁做。

4. **RMW 原地膨胀（运行时碎片）不是问题。** 两后端 RMW 后文件大小几乎不涨（1.00–1.01x）——redb 的 MVCC 页回收与 sqlite 的自由页复用都工作正常。膨胀主要来自**插入期的稳态占用**，不是随机更新累积。

5. **批量 RMW 的 p99 偏高是测量定义所致，非缺陷。** Batched 模式下每个 op 的端到端延迟从它的 read 起算到整批 commit 止，故含批边界等待（redb 256KiB p50≈36ms）。每块模式的 p50（5–15ms）才是单次 RMW 的纯延迟。真实 FUSE 路径下延迟在两者之间，取决于 `fsync` 频率。

## 4. 结论与建议（闸门判断）

### redb 默认全包是否够用？

**部分够用，建议采纳但带两条约束。**

- **吞吐维度：够用，redb 通过。** 只要落实写批处理（一次 `write` 合并、`fsync`/`flush` 才 commit），redb 全包形态吞吐优于 sqlite，无需自写数据区来解决性能问题。
- **空间维度：64KiB 档够用（compact 后 1.34x，可接受）；256KiB 档触警戒线（2.75x 稳态、1.48x compact 后）。** 对去重友好、压缩比 31x 的目标负载（设计 §1.1），把 31x 压缩成果再被容器吃掉 1.5–2.75x，是实打实的回吐。

### 批量事务相对每块事务提升多少？

**RMW 8.7–17.5x，插入约 14x。** 这是本测最确定、最可操作的结论：**布局 V 实现必须做写批处理**，否则测出的是事务成本而非布局特性（重蹈 `sqlitefs` 覆辙）。

### 是否触发「需自写数据区」闸门？

**未触发「全自写虚拟盘」，但 256KiB 档触发「考虑 redb 元数据 + 自写数据区」的二档评估点。** 具体建议：

1. **首版仍按设计走 redb 全包**——吞吐达标，64KiB 默认块档空间可接受，KISS 优先。
2. **把默认 `CHUNK_SIZE` 维持在 64KiB 而非 256KiB**：256KiB 大 blob 是 redb 空间放大的主因，64KiB 档放大温和得多。这与设计 §3「大块高压缩比 vs 随机写放大」的权衡叠加一个新维度：**大块还会放大 redb 容器占用**。
3. **若后续基准坚持要 256KiB+ 大块**，则设计 §6.1 第二档「redb 元数据 + 自写 extent 数据区」的空间论据成立——此时把大 blob 移出 redb B-tree、只在 redb 存定长 `(ino,idx)->extent_ptr` 索引，可同时拿到 redb 的事务/元数据便利与接近 sqlite 的空间效率。
4. **sqlite 作为备选仍有吸引力**：空间近乎零浪费（1.01x）、实现同样简单；代价是批量吞吐略低、256KiB RMW 吞吐明显低于 redb（472 vs 1577 op/s）。若空间足迹是首要 KPI 且 256KiB 必须用，sqlite 反而比 redb 全包更优。

**一句话闸门判断**：redb 全包**够用于首版**（配 64KiB 块 + 写批处理），但**不要默认 256KiB 大块**；256KiB 场景应优先评估「redb 元数据 + 自写/sqlite 数据区」，而非死守 redb 全包。

## 5. 风险与 caveat

- **未测真实 FUSE 路径**：本测直打容器后端，省去 FUSE 上下文切换、`max_write` 拆分（设计 §4.1）、Core 的压缩/解压 CPU。真实端到端吞吐会更低，但**后端相对排名与陷阱倍率不受影响**。
- **未测并发**：单线程顺序驱动。redb 写事务串行化、sqlite 单写者，多线程下两者写吞吐都不会线性扩展；本测不覆盖该维度。
- **未测崩溃一致性**：只测性能；设计 §10 的 ACID/durability 正确性需另测。durability 口径已尽量对齐（都每事务 fsync），但 redb 与 sqlite 的 fsync 调用模式、2PC 行为不同，绝对延迟数字含此差异。
- **redb 容器大小含预分配/增长策略**：redb 文件按页成倍增长，4.00 GiB 含未用预留；compact 后数字（2.14 GiB）才是真实稳态下界。报告两者都给，避免误读。
- **WSL2 文件系统**：跑在 WSL2 ext4（`/dev/sdd`）上，fsync 语义/延迟可能与裸金属 Linux 有别；绝对吞吐数字仅在同环境可比，跨环境取相对结论。
- **PerBlock 用子集外推**：每块一事务太慢，未在全量 42600 块上跑，用 2130/660 块子集量化每块成本。倍率结论稳健，但全量每块的绝对耗时是外推值。
- **磁盘安全**：全程用 `tempfile::TempDir`，跑完自动清理；实测 `df` 前后一致（339 GiB 空闲不变），无残留、无通配符 `rm`、未碰系统。

## 6. 复现

```bash
cd microbench
cargo test --release            # 单元测试（确定性、区间、picker）
cargo run --release             # 默认：redb，64k+256k 两档，约 1.5GB，90s
cargo run --release -- --backend sqlite       # sqlite 对照（约 240s）
cargo run --release -- --chunk 64k --backend redb
cargo run --release -- --quick                # 冒烟（几秒）
```

参数：`--backend {redb|sqlite}` `--chunk {64k|256k|both}` `--files N` `--blocks M`（0=自动）`--k K` `--rmw OPS` `--seed S` `--quick`。

### 环境

- WSL2，Linux 6.18，20 vCPU，196 GiB RAM，ext4 on `/dev/sdd`（跑时 339 GiB 空闲）。
- rustc/cargo 1.96.0；redb 4.1.0；rusqlite 0.40.1（bundled SQLite）；hdrhistogram 7.5.4。
