# 分层分块：按访问模式给 head/body/tail 分层（设计草案）

> 文档性质：**实现设计（how）**，承接 [01-zipfs-design.md](./01-zipfs-design.md) §3/§7 的均匀分块与 §1.1 追加硬需求。
> 日期：2026-06-28。状态：草案，**已过一轮 code-architect 审查（见 §8 审查纪要）**，待发现读 micro-bench 验证后再决定是否落格式改动。
> 依据：对 Claude Code 2.1.191 研究版源码的实证（项目记忆 `claude-code-session-io-access-pattern`），行号引自 `~/src/neighbors/refs/claude-code-2.1.191/app.pretty.js`。

## 1. 动机：源码实证的真实访问面

旗舰负载 `~/.claude/projects/*/*.jsonl` 的访问模式，由 harness 源码坐实（非推测）：

| 访问面 | harness 行为 | 频率 | 字节量 |
|---|---|---|---|
| 写 | 纯 append 到尾部（`appendFile`/`flags:"a"`，去重靠内存 uuid 集，不回读正文） | 高频 | 每次一行 |
| **会话发现** | 每个文件读**首 64KB + 末 64KB**（`tan` line 30799），`dgt`(251617) 仅在这两段抽标题/firstPrompt/cwd/gitBranch | 每次 `--resume` 选择器 × N 文件 | 2×64KB/文件 |
| resume | 目标文件**整体前向顺序**读（`iPf` 325507 / `aPf` 325515），内存按 parentUuid 重建树 | 罕见 | 全文件 |
| 元数据刷新 | 活跃会话每写 ~32KB 读末 64KB 再 append（`mBf` 337268） | 中频（仅活跃文件） | 末 64KB |

关键常量 `Rv = 65536`（line 31010）= harness 的**头/尾窗口真实粒度**。**中段无热随机读**。

## 2. 现状与缺口

现状（[mod.rs](../crates/zipfs/src/core/mod.rs) / [archive.rs](../crates/zipfs/src/archive/mod.rs) / [wsession.rs](../crates/zipfs/src/core/wsession.rs) / [rwfs.rs](../crates/zipfs/src/rwfs.rs)）：

- **正文块 = 均匀 1MiB**（`DEFAULT_CHUNK_SIZE`，已按 algo-compare 实测从 64KiB 退役），每块独立 zstd 压。
- **开放尾块缓冲**（`TailSessions`）：活跃尾块未压缓冲，append 成本与块大小解耦。
- **读路径在 Core**：`rwfs::read_range` 先 `block_range(offset,…)` 算块号，再**逐块** `Store::get_block(ino, idx)`（按块号、返回整块压缩字节）。Store **没有按字节区间读的入口**——这点决定了 §4.2 的设计形状（见 §8 C1）。
- archive footer 固定 32B、块索引 `(offset,clen,flags)`，崩溃安全（reuse-tail-slot fail-closed）。

**唯一未被覆盖的访问面 = 会话发现的首/尾 64KB 读**：

- 读 `[0, 64KB)` 命中均匀块 0（1MiB）→ **解压整个 1MiB 才取 64KB**（16x 字节放大）。
- 活跃文件的尾 64KB 已被开放尾缓冲廉价覆盖；**已封存（旧）文件**读末 64KB 仍要解压末块。
- 发现是「open→读首尾→close」逐文件冷扫，BS 的 per-inode reader 缓存 `release` 即失效、跨文件不复用 → 每文件每次发现各付一次 `ArchiveReader::open`（**全量解析 footer+index+CRC+逐项越界校验**）+ 一次块 0 解压。

## 3. 设计取舍（实测裁决，2026-06-28）

**收益已实测（`bench/results/20260628-discovery-read/REPORT.md`，96MB 文件 / 1MiB 块 / zstd-3，分离三段成本）**：

| 段(中位) | HOT | COLD（`fadvise(DONTNEED)` 驱逐） |
|---|---|---|
| open 解析（footer+index+CRC，**head 缓存救不了**） | 10us | 527us |
| 块 0 读+解压取 64KB（现状） | 342us | 842us |
| 独立 head 流读+解压取 64KB（缓存） | 51us | 265us |
| **单文件发现读：现状→带缓存** | **353→62us（砍 82%）** | **1369→792us（砍 42%）** |

选择器扫描外推：N=200 省 58ms(HOT)/115ms(COLD)，N=500 省 145ms/289ms。**通过门槛**——交互尺度可感知。

**诚实边界（审查 H2 实测坐实）**：`open 解析`是不可省地板，HOT 可忽略(10us)、**COLD 达 527us 成主导固定项**，把冷态收益封在 42%。即 head 缓存在页缓存温热（反复开选择器）时最有效，冷首扫减半。block0 首 1MiB 仅压到 67KiB（高度可压样板，印证 §1.1），head 缓存 20KiB；决策量 block0(342)−head(51) 远大于 open(10)，方向正确。

**两条路线，风险差一个量级：**

| 路线 | 机制 | 触及面 | 风险 |
|---|---|---|---|
| A 变长逻辑块 | 块 0 设小，块 1.. 大 | 推倒「均匀 `chunk_size`」不变量——`chunk.rs::block_range`、`rmw.rs` 全部边界算术、`live_data_end`、reuse-tail-slot 崩溃语义全要重证 | **高** |
| **B 加性 head 缓存（选用）** | 把首 `HEAD_CACHE_BYTES` 再单独压一份存档内，发现读经 Core 快路径走它 | `archive.rs`（格式 v2，向后兼容）+ **Core `read_range` 快路径 + `Store` 新增 head-cache 探测接口**（审查 C1 修正：读判定在 Core 不在 store）+ shadow 写接线 | **中偏高** |

**选 B**，但**诚实修正触及面**：B 不碰块数学/RMW（路线 A 才碰，风险判断成立），但**必然要碰 Core 读路径与 `Store` trait**——因「`off+len <= head_cache_rawlen` 走缓存」的字节区间判定只能在 `read_range` 做，Store 的 `get_block(idx)` 看不到 `off/len`。故风险定级从「中」上调「中偏高」。

理由仍成立：①不触碰已充分测试的均匀块不变量与 RMW；②与文件大小**双峰分布**契合，head 缓存**只在大文件**（`size > HEAD_CACHE_BYTES` 即值得，见 §3 注）建；③缓存**压缩存储**（首 64KB zstd ≈ 10KB），仅少数大文件 ×10KB，对压缩比影响可忽略（未压缩冗余 64KB×8000 文件 = 512MB 会毁比值，故必须压缩 + 仅大文件）。

> **建缓存阈值（L1 澄清）**：以 `size > HEAD_CACHE_BYTES(64KiB)` 为界，而非 `body_chunk_size(1MiB)`。64KB–1MiB 的单块文件块 0 = 整文件，发现读 `[0,64KB)` 仍有最高 16x **块内**放大；它们是否建缓存由 micro-bench 的绝对耗时定（数量虽多但单文件块 ≤1MiB）。

**被否的替代（取舍审计闭环）**：
- **块 0 verbatim 不压**（复用现成 `FLAG_VERBATIM`）：否。块 0 是高可压的系统提示/CLAUDE.md 样板（§1.1 冗余源），verbatim 会实打实伤总压缩比（与「压缩是主线」冲突）。head 缓存以 ~10KB/文件小冗余换取**保留块 0 压缩**，更优（L2）。
- **xattr sidecar 存缓存**：否。ext4 单 inode xattr 容量上限 ~4KB，放不下 ~10KB 压缩缓存（L3）。故必须放数据区。

## 4. 增量 1 设计：archive 格式 v2 + head 缓存（**仅当 §6 micro-bench 达标才落地**）

### 4.1 格式（单一最优格式 + 崩溃安全）

项目 v0.0.0 未发布、无任何历史 archive，故**不背向后兼容**：footer 首版即含 head cache 字段，无版本分支。

```text
[magic|version=1]
[compressed chunk 0]...[compressed chunk N]
[compressed head-cache]                         ← 首 HEAD_CACHE_BYTES 的独立 zstd 流（仅大文件）
[chunk_index: (offset, clen, flags) × count]
[footer: <既有字段> | head_cache_offset(8) | head_cache_clen(8) | head_cache_rawlen(8) | head_cache_flags(4)]
```

- **覆盖语义恒为 `head_cache_rawlen = min(HEAD_CACHE_BYTES, uncompressed_size)`，不做行对齐**（H1）：行对齐是 harness `dgt` 自己的事，archive 对齐只会制造「请求 64KB 但缓存只有 65500B」的部分命中黑洞——那会把发现读劈成「缓存+块 0」两段、块 0 照样解压、收益归零。恒满 64KB 让 `read(0,65536)` 完整命中。
- `head_cache_* = 0` 表示无缓存（小文件 / 未触发建缓存），读路径透明回退逐块。`head_cache_flags` 复用 `FLAG_VERBATIM` 语义（不可压缩的 head 原样存）。
- **head 缓存随 index/footer 一起在每次 commit 的「元数据尾区」重写**（在 `live_data_end` 之后、index 之前），与 index 同生命周期——故**不需要纳入 `live_data_end`，靠 commit 的两段 barrier + footer 在 EOF 的 fail-closed 兜底**（消解 M1：head 缓存不是永久 fixture，无「下次 commit 原地覆盖旧缓存成 Frankenstein」之虞）。代价：每次 fsync 重写 ~20KB head 缓存（小，可接受；后续可优化为「块 0 不变则跳过重写」）。

### 4.2 读路径（Core，非 store——C1 修正）

在 `rwfs::read_range` **顶部**加快路径：若 `offset + len <= head_cache_rawlen` 且该 ino 有缓存 → 经 `Store` 新增的 `read_head_cache(ino, off, len) -> Option<Vec<u8>>` 直接返回（解压 64KB ≈ 0.03ms），跳过 `block_range`/块 0。否则透明回退既有逐块路径。`Store` trait 因此新增一个**只读探测方法**（ShadowStore 实现为读 archive 的 head 缓存；ContainerStore 首版返回 `None`）。

### 4.3 写路径与失效（M2：数据来源接线）

- **建缓存时机 = 块 0 由本次提交首次封存**：append-only 下块 0 在「第一次跨过 `body_chunk_size`」那次 commit 时其明文恰好在脏集（块 0 刚被 seal），**顺带**压 head 缓存，避免事后 `get_block(0)` 再解压回捞。规定「仅当 idx 0 在本次脏集且 `new_size > HEAD_CACHE_BYTES` 时建/更新缓存」。
- **失效**：append-only 下块 0 前缀写一次即不再变 → 缓存写一次永不失效（主路径）。RMW 命中 `[0, HEAD_CACHE_BYTES)`（罕见）→ 该块 0 进脏集，commit 时重建。靠 commit 重建保证缓存 == 块 0 前缀，无需额外校验。

### 4.4 常量

`HEAD_CACHE_BYTES = 65536`（= harness `Rv`）。`--no-head-cache` 可关（基准对照 / 排障）。

## 5. 暂不做（记录，避免范围蔓延）

- **尾缓存（已封存文件末 64KB）**：对称问题，但旧文件极少是 resume 目标（选择器按 mtime 排序、热的在顶部且尾在开放缓冲）。v2 先不做；发现 bench 若显示尾读也痛再加对称 footer 字段。
- **变长/行对齐正文块**（路线 A，提升压缩比）：正交的更大改动，归 01-design §3「块大小核心权衡」，单独立项 + 基准门控。

## 6. 验证：bench 先行（**改格式前的不可逆门，遵循 repo「分块决策必经基准门」文化**）

### 6.1 第 0 步（先做，零格式改动）：发现读 micro-bench

用**现有 v1 格式 + 离线对照**量化，达门槛才进 §7 的格式改动：

- **对照组无需任何新格式**：把真实 96MB jsonl 用现有 `ArchiveWriter`（1MiB/zstd-3）建成 v1 archive，测三个量并**分离**：
  1. `ArchiveReader::open` 解析成本（footer+index+CRC+越界校验，head 缓存救不了，H2）。
  2. `read_block(0)+decompress` 取首 64KB（= 现状）。
  3. 把同一首 64KB 单独 zstd-3 压成独立流、解压取 64KB（= head 缓存模拟）。
- 热缓存（重复跑）+ 冷缓存（`drop_caches`，若无免密 sudo 则记为待补）各一组；按 N=50/200/500 文件外推选择器单次扫描总耗时。
- **门槛**：若 (2)−(3) 的单文件节省 × 典型 N 在选择器交互尺度上不显著（且相对 (1) 的 open 解析成本占比小），则**只留本文档、不改格式**。

### 6.2 达标后（§7）：TDD 单测

- footer v2 编解码（纯函数）+ 向后兼容：v1 文件被 v2 读者读为「无缓存」且 index 不被 footer 增长污染；v2 round-trip。
- head 缓存切片读 == 块 0 前缀切片（一致性）。
- **崩溃 fail-closed**：写 head 缓存后 footer 未落盘即崩溃 → open 报损坏（补 `updater_未提交即崩溃_*` / `updater_reuse_*` 的 head-cache 变体，M1）。
- RMW 命中头区后缓存重建一致。
- 端到端：挂载后 `read(0,65536)` == 全量读前缀，且不触发块 0 解压（埋点计数验证）。

## 7. TDD 落地顺序（**bench 提到第 0 步，M3 修正**）

0. ~~**发现读 micro-bench（§6.1，零格式改动）**~~ **✅ 已跑（2026-06-28，`discovery-bench` bin + `bench/results/20260628-discovery-read/`）：HOT 砍 82% / COLD 砍 42%，通过门槛 → 进格式改动。**
1. footer 含 head cache 字段（单一格式，无 v1 兼容）的编解码（纯函数，`archive.rs`）—— 测试先行。
2. `ArchiveWriter`/`ArchiveUpdater` 写 head 缓存（随元数据尾区每次 commit 重写）+ `ArchiveReader::read_head_cache()` —— round-trip + 崩溃测试。
3. `Store::read_head_cache` 接口 + `rwfs::read_range` 快路径 + shadow 写接线（块 0 首封时建）—— 端到端 + 计数埋点。

## 8. code-architect 审查纪要（2026-06-28）

- **C1（已修正入 §3/§4.2）**：读快路径必碰 Core `read_range` + `Store` trait（字节区间判定不在 store）；触及面从「仅 archive+store」改为含 Core 读路径，风险「中」→「中偏高」。选 B 不选 A 的方向获认同。
- **M3（已修正入 §6/§7）**：bench 先行、格式改动门控在数据后，提到第 0 步。
- **H1（已修正入 §4.1）**：head 缓存恒 `min(64KiB,size)` 不行对齐，消除部分命中黑洞。
- **H2（已修正入 §3/§6.1）**：冷缓存是净赢非摊薄；`ArchiveReader::open` 解析成本须分离测量。
- **H3（已消解）**：原指「footer 长度随版本变需先读 header 定版本」。用户裁定项目 v0.0.0 无历史 archive、**不背向后兼容** → 单一最优格式、无版本分支，H3 不复存在。
- **M1（已消解，方式优于初稿）**：head 缓存随 index/footer 在每次 commit 的元数据尾区重写（非永久 fixture），与 index 同生命周期、靠相同 barrier + EOF footer fail-closed 兜底，无需纳入 `live_data_end`，也无 Frankenstein 之虞。
- **M2（已修正入 §4.3）**：建缓存仅在块 0 首次封存时取明文，不事后解压回捞。
- **L1/L2/L3（已修正入 §3）**：阈值以 `HEAD_CACHE_BYTES` 为界并澄清块内放大；verbatim-块 0 与 xattr-sidecar 被否的理由入审计。
