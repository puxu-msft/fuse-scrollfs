# 05 · 故障注入测试 / Fault Injection Testing — 设计 Spec

> 类型：测试 spec · 状态：**已实现**（下方正文为当时的设计 spec，已落地；BlockIo/FaultIo + dm-* 脚本，见 ROADMAP T1）。文档索引见 [README.md](./README.md)。

> 状态：设计已定（经 architect + rust-reviewer + 代码事实核查三方评审）。日期：2026-06-29。
> 上游动机见 [04-crash-safe-commit.md](./04-crash-safe-commit.md)（§8.3 双 superblock + 两 barrier 提交、§8.4 尾日志）。本文是「如何系统性地把这套崩溃安全协议变成可回归的故障注入测试」的规格。

## §0 背景与动机

崩溃安全提交协议（双 superblock + append-only + 尾日志）已落地，现有崩溃测试有两类：archive.rs 单元测试（「未 commit 即 drop」+ 手工翻转 SB/index 字节）与 [crash-test.sh](../bench/scripts/crash-test.sh)（守护进程级 `kill -9` 于写中途）。两者都覆盖不到一类真实风险：**fsync 已确认、却被内核/硬件在掉电时丢弃或重排的写**，以及**写中途返回 `EIO` 时错误是否被静默吞掉**。`kill -9` 只杀进程（RAM 不丢），无法模拟掉电；在 tmpfs 上 fsync 更是 no-op，连进程级测试都被削弱。

补这个缺口需要在「真实进程/内核杀不到」的层面注入故障。最自然的注入点是 archive 持久化所依赖的 IO 原语——今天它直接打在具体 `std::fs::File` 上，没有抽象接缝。本 spec 设计这个接缝（`BlockIo` 中间层）与其上的两层故障注入。

## §1 目标 / 非目标

**目标**
- G1：在 archive 写/提交/打开路径插入唯一 IO 差异面 `BlockIo`，生产零行为变化、零性能退化。
- G2（Tier 1，主层）：进程内确定性崩溃模拟器，按写操作序号注入 `EIO`/撕裂/掉电/重排，**穷举每个崩溃点**断言「重开要么是合法已提交前缀、要么 fail-closed 报损，且 durability 不退」。无 root、毫秒级、进 CI 日常门。
- G3（Tier 2，加固层）：基于 device-mapper 的真实块层门，验证真实内核/fs 兑现 barrier 排序与目录项 durability。root 门控、发布前跑。
- G4：现有 [crash-test.sh](../bench/scripts/crash-test.sh) 保留，三层互补。

**非目标**
- 不重测 redb 引擎本身的崩溃恢复（ContainerStore 的 durability 委托 redb，见 §9）。仅保留一个最小 smoke 验证「委托正确」。
- 不做 xfstests 全量；Tier 2 收窄到 2–3 个招牌场景。
- 不改 `ArchiveWriter`（顺序 `Write+Seek` 流，单测依赖 `Cursor<Vec<u8>>`，改之净损）。
- 不在本 spec 内修 §9 列出的 archive 之外的 durable 写点 bug（如 seal.rs 缺父目录 fsync）——单独立项。

## §2 三层覆盖边界（关键：先划死各层管什么，避免高估 Tier 1）

durable 写并非全集中在 archive.rs——它散落在 shadow.rs（独立 `f.sync_all`、`create` 落盘、目录项 `fs::rename`）、compact.rs（temp+rename + 父目录 fsync）、seal.rs、ingest.rs。因此**单层故障注入无法覆盖全部**，必须分层并写明边界：

| 层 | 注入手段 | 覆盖 | **结构上不覆盖** |
|---|---|---|---|
| 单元（既有） | drop-未提交 / 翻字节 | archive 格式解析 fail-closed | 端到端时序 |
| **Tier 1**（本 spec 主层） | 进程内 `FaultIo` | **单个 archive 字节流**的格式层不变量：双 SB 选活、`index_crc`、尾日志 `rec_crc` 重放、错误传播不静默 | WriteSession 脏块层、temp+rename 原子性、reader 缓存失效世代、**目录项 durability**、跨 archive |
| crash-test.sh（既有） | 守护 `kill -9` | FUSE→Core 尾块缓冲→Store→updater 端到端进程级 | 掉电语义（RAM 不丢）、barrier 排序 |
| **Tier 2**（本 spec 加固层） | dm-log-writes / dm-flakey | 真实内核 fs：barrier 排序（barrier 1 真的先于 SB 写生效吗）、目录项/rename durability、跨 archive | —（最高保真，代价 root/慢/Linux-only） |

**Tier 1 是格式层不变量的穷举证明，不是端到端崩溃恢复的等价替身。** 文档与测试注释须显式声明这一点，防止后人误判覆盖面。

## §3 `BlockIo` 中间层（接缝）

在 archive.rs 的 `ArchiveUpdater`（写 + 打开/恢复读）路径引入定位 IO trait：

```rust
pub trait BlockIo: Send + Sync {
    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<()>;
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;          // 唯一 durability barrier
    fn len(&self) -> io::Result<u64>;
    fn set_len(&self, len: u64) -> io::Result<()>;  // 可选：当前 append-only 写路径不用，留待压实
}
```

设计要点（评审定稿）：
- **方法取 `&self`**（含 `write_at`/`sync`）：贴合 `std::os::unix::fs::FileExt::{write_all_at, read_exact_at, sync_all}`（均 `&self`、≥Rust 1.33 稳定、pwrite/pread 不移动游标），且免改现有 `ArchiveUpdater::sync(&self)` 签名。生产实现 `FileIo(File)` 直接转调 `FileExt`。内部可变（`FaultIo` 的计数器/计划）用 `Mutex`/原子封装，故 `&self` 不碍注入。
- **`Send + Sync`**：当前 reader 不动时 `Send` 已够，但 `Sync` 零成本（`File` 已 `Sync`），为日后 `ArchiveReader` 泛型化（`Arc<ArchiveReader<W>>` 跨线程并发读）留路。
- **覆盖 `ArchiveUpdater::open` 的整条读链**（评审 CRITICAL ①）：`open` 不是纯写——它调 `read_exact_at`(header)、`load_active`（读双 SB + index + 每块 bounds）、`read_head_cache_bytes`。这些 `fn(file: &File)` 自由函数必须改成接受 `&impl BlockIo`，否则「打开/恢复阶段读失败」无法注入——而那正是双 SB 崩溃安全最该测的路径（历史 reuse-tail-slot durability 洞即在此）。
- **`impl BlockIo for File`（评审定稿，取代「包 FileIo」）**：让 `std::fs::File` 直接实现 `BlockIo`（转调 `FileExt`）。如此 `ArchiveReader` 的 `file: File` 字段**原地**满足 `&impl BlockIo`，那 4 处 `read_exact_at(&self.file, …)` 与共享自由函数零所有权改动即可复用——避免「`from_file` 拿 File 所有权要存字段、中途包 `FileIo` 会 move 走」的陷阱。`FileIo(File)` newtype 可保留作 updater `from_io` 注入对称用，或直接弃用、生产走 `ArchiveUpdater<File>`。
- **`write_superblock_slot<W: Write+Seek>` 不动**（评审 MED）：它被 `ArchiveWriter::finish` 共用，泛型化后 updater 无 `Write+Seek` 可传。updater 侧改用 `self.io.write_at(off, &serialize_superblock(sb))`（与 append-only 绝对偏移写等价），把该自由函数留给 `ArchiveWriter` 专用。
- **泛型 `ArchiveUpdater<W: BlockIo>` + 双构造**：`open(path) -> ArchiveUpdater<FileIo>`（生产，内部 `OpenOptions` 打开 File）与 `from_io(io: W, …)`（注入）。泛型传染被 `open` 的具体返回类型截断在 archive.rs 内；`ShadowStore` 不持 updater 字段（均为方法内局部变量、用完即 drop、不跨线程），故 `W` 不灌进 `Store`/`ShadowStore`。写路径是热点（每 fsync 一次 `commit`），泛型零虚调。
- **`seek+write_all` → `write_at(off)` 严格等价**：现有写全是 `seek(SeekFrom::Start(绝对偏移)) + write_all`，写后手动维护 `write_cursor`，从不读回游标，故 pwrite 替换等价且省一次 seek syscall。
- **不动 `ArchiveReader` 与 `ArchiveWriter`**：reader 按计划保持吃 `File`（崩溃校验靠「镜像落临时文件 → `ArchiveReader::from_file`」复用，见 §4）；Writer 维持 `Write+Seek`（`Cursor` 单测依赖）。

## §4 Tier 1 — `FaultIo` 确定性崩溃模拟器（主层，CI 日常门）

`FaultIo` 实现 `BlockIo`，内部建页缓存模型：
- `durable`：已 sync、能扛崩溃的盘面字节。
- `dirty`：自上次 sync 以来的写覆盖层（模拟内核页缓存）。
- `sync()`：把 `dirty` 合入 `durable`（唯一 durability barrier）。
- `crash()`：产出一份「崩溃后磁盘镜像」。

**崩溃模型必须支持脏页的任意重排子集持久化（评审 CRITICAL ②）。** 真实 fs 会在 fsync 前自行回写脏页且可重排——危险场景正是「superblock 槽先于它依赖的 index/数据落盘」。若模型只「crash 全丢 dirty」，则永远不会让 SB 落盘而其数据不落盘，§8.3 双 superblock 防的那类 bug 它根本碰不到。故 `crash()` 的语义是：对 `dirty` 页集合，按 seed 持久化**任意子集 × 任意顺序**（只尊重 sync 屏障：barrier 之前的写在 barrier 完成后必已 durable）。「重排」因此从 stretch 升为**核心**，语义是「脏页回写子集」而非「写序」。

故障调度（按写操作序号 N 编程）：
- **EIO/ENOSPC**：第 N 次 `write_at`/`sync` 返错。断言错误上传不被静默吞、archive 仍开为上一版（非活跃槽未污染）。
- **撕裂/部分写**：第 N 次写只把前 K 字节落入 `durable`（K **量化到 512B 扇区**，免造真实块设备不产生的字节边界）。断言 CRC/双 SB 检损回退、`rec_crc` 拒半条尾日志。
- **掉电**：第 N 次写后 `crash()` 穷举。
- **重排（核心）**：见上，崩溃持久化乱序子集。

**镜像 → 复用 reader 的 oracle 必须独立于被测代码（评审 CRITICAL）**：`crash()` 产出的 `durable` 镜像字节落到真实临时文件，用**现有** `ArchiveReader::from_file` 打开。但「恢复版本 ≥ 最后 fsync-acked」与「合法已提交前缀」两条断言，**不得**用 `commit` 的返回值或 reader 的自洽当基准（否则被测代码给自己打分、自证）。须仿 [crash-test.sh](../bench/scripts/crash-test.sh) 的带外 `PROGRESS` 台账：
- `FaultIo` 在每次 `sync()` **成功返回的瞬间**，由测试侧把当时 `durable` 镜像独立快照、`parse_superblock` 出活跃 seq 记入带外台账；`acked 版本 = 台账末项的活跃 seq`（测试侧解析镜像字节得出，不经 `commit` 返回值）。
- 测试侧同时维护逻辑内容的 `expected` 历史快照（仿 `append_tail_buffer.rs` 的 `expected: Vec<u8>`）。崩溃后断言 reader 读出内容 ∈ {某历史 `expected` 前缀}，且其 seq ≤ 镜像活跃 seq。只断言「reader 开成功且自洽」远不够——自洽的坏数据照样自洽。

**注入按语义 offset 区间调度，非裸写序号（评审 防脆弱）**：`FaultIo` 据 offset 落在 SB_A/SB_B 槽区间、index 区间、journal 区间来决定注入点（offset 区间是格式契约、稳定；写序号是实现细节、易变）。断言只钉「是 `Err` 且非静默吞」+「镜像开出上一版」，**不钉**中间错误的具体 `ErrorKind`。

**招牌测试——穷举崩溃点**：
```
for crash_after in 0..total_writes {
    跑固定 append+fsync 工作负载，第 crash_after 次写后 crash()（含乱序子集枚举，按写序窗口剪枝防组合爆炸）；
    用 durable 镜像开 ArchiveReader：
      断言 ① 要么开成功且为合法连续前缀，要么 fail-closed 报损（绝不静默错读）；
      断言 ② 恢复版本 ≥ 最后一次 fsync-acked 版本（durability 不退）。
}
```

**乱序子集枚举必须与 barrier-sync 失败注入交叉（评审 关键盲区）**：单独的「所有 barrier 成功 + 子集枚举」在 sync 屏障语义下**恰好排除了** §8.3 双 SB 防的唯一危险 case——「SB 槽落盘、而它依赖的 index 没落盘」要求横跨 barrier 1，但 barrier 1 一旦标记成功就强制 index durable，该组合自动出局。要真测到它，必须注入「barrier 1 的 sync 失败/被乱序但 commit 仍推进」，构造出「barrier 2 的 SB 写进 durable、barrier 1 本应保护的 index 写未进 durable」的镜像，断言 reader 必须 fail-closed 或回落。即任务 2.5（乱序）须与 2.2（barrier sync 注入）交叉，而非各自独立。

**剪枝窗口具体起点**：子集枚举窗口 = 从上一次 barrier 2 到本次 barrier 2 之间的全部 `write_at`；枚举上限 N≤12，超过则固定 seed 随机采样。模型层须有测试钉死「`crash()` 能产出『含 dirty 子集 A 但不含 B』的镜像」（任务 2.1），否则剪枝退化成「全丢 dirty」无人发觉。

## §5 Tier 2 — device-mapper 真实块层门（加固层，root 门控，发布前）

证明真实内核/fs 真的兑现屏障语义（Tier 1 只验自家逻辑）：

- **首选 dm-log-writes**（xfstests 同级）：loop + dm-log-writes 记录每个 write/flush/fua → ext4 → zipfs `--backing` 指此 → 跑工作负载 → `replay-log` 回放到**每个 flush 边界**，逐个 mount + 跑 zipfs 恢复校验。收窄到 **2–3 个招牌场景**：(a) append+fsync 序列回放、(b) rename 覆盖、(c) create 后崩溃（验目录项 durability）。**不做 xfstests 全量**。
- **退路 dm-flakey smoke**：跑中途切 `drop_writes` 表 + kill，重挂校验。粗粒度，证「丢写不致命」，证不了 barrier 排序。
- **门控**比照现有 `/dev/fuse` 与 `drop_caches`：非 root / 无 device-mapper 即 `SKIP`，不进默认 `cargo test`。
- 新增脚本 `bench/scripts/crash-test-dm.sh`。

## §6 模块布局与 feature 门控

- 新增 `crates/zipfs/src/blockio.rs`：`BlockIo` trait + `FileIo`（生产，精简）。
- `FaultIo`：门控 `#[cfg(any(test, feature = "fault-injection"))]`。**关键（评审 HIGH）**：`#[cfg(test)]` 对 `tests/` 独立 crate 不可见，集成测试唯一途径是 feature。
  - `crates/zipfs/Cargo.toml` 加 `[features] fault-injection = []`。
  - 运行：`cargo test --features fault-injection`（CI 与本地统一带 flag）。
  - 依赖 `FaultIo` 的集成测试文件加文件级 `#[cfg(feature = "fault-injection")]`，不带 feature 时整文件跳过，免裸跑编译失败。
- 改 `archive.rs`：`ArchiveUpdater` 泛型化到 `W: BlockIo`，open 读链改吃 `&impl BlockIo`；`ArchiveReader`/`ArchiveWriter` 不动。
- 改 `shadow.rs`：构造 updater 处传 `FileIo(File)`，行为不变；**保全 `invalidate_reader` 失效协议**（见 §9）。
- 新增 `bench/scripts/crash-test-dm.sh`（Tier 2）。

## §7 验证矩阵（每模型 ↔ 一条不变量）

| 故障模型 | 注入点 | 层 | 断言的 zipfs 不变量 |
|---|---|---|---|
| EIO/ENOSPC | 第 N 写/sync | T1 | 错误上传不静默吞；archive 仍开为上一版（§8.3 非活跃槽未污染） |
| 撕裂/部分写（512B 对齐） | index/SB/journal 写一半 | T1 | CRC/双 SB 检损回退；`rec_crc` 拒半条尾日志（§8.4） |
| 掉电（fsync 后丢，穷举崩溃点） | 每写点 crash | T1 | 恢复到最后已提交版、durability 不退 |
| 重排（脏页乱序子集） | crash 持久化乱序子集 | T1 | 两 barrier 协议真有序（SB 不得先于其数据生效） |
| barrier 排序真实裁判 | dm-log-writes 回放 flush 边界 | T2 | 真实内核兑现 barrier 1→SB→barrier 2 顺序 |
| 目录项 / rename durability | dm-log-writes create/rename 场景 | T2 | 崩溃后看到新文件而非旧/丢失 |
| 委托正确性 | container fsync + kill smoke | crash-test | ContainerStore 正确把 durability 委托 redb |

## §8 Tier 1 明确不覆盖什么（防高估，评审 C1/H1/H3）

- **WriteSession 脏块层**：fsync 前会话全在进程内存，未到任何盘。Tier 1 起点是「已到 archive 字节流的写」，看不到这层。
- **temp+rename 原子性**：归 Tier 2（dm-log-writes 的 rename/create 场景）。
- **reader 缓存失效世代**：shadow.rs `commit_session` 有意把 `invalidate_reader` 放在 `up.sync()` 前以应对 sync 失败。这条**可由 FaultIo 在 ShadowStore 层注入 `up.sync()` 失败覆盖**（评审 HIGH-1，历史 reuse-tail-slot durability 洞高发区）——故新增一条专项测试（任务 2.6），不留在「不覆盖」里。
- **目录项 durability**：Tier 1 无目录概念，结构上无法覆盖。归 Tier 2。
- 以上归 crash-test.sh（端到端进程级）+ Tier 2（barrier 排序 + 目录项）。

## §9 重构边界与既有协议保全

- **差异面收口在 archive.rs 的 `ArchiveUpdater`（写 + open 读链）+ 其共享只读自由函数**（`load_active`/`read_sb_slot`/`read_head_cache_bytes`/`read_exact_at`）。这些自由函数被 reader 与 updater 共享：改成接受 `&impl BlockIo`，并 `impl BlockIo for File`（见 §3），使 `ArchiveReader` 的 `file: File` 字段原地满足、零所有权改动地复用——避免被迫改 reader 或产生 File/BlockIo 双实现。
- **保全 reader 缓存失效协议**：shadow.rs 依赖「写路径每次改盘后调 `invalidate_reader`」（`commit_session`/`append_tail`/`seal_tail_block`）。重构写路径必须保持这些失效点，否则 per-inode `Arc<ArchiveReader>` 热读缓存读到陈旧 footer/index。
- **ContainerStore 不涉**：其 `fsync`/`sync_all` 只把内存挂起合并成一个 redb 写事务 commit，durability 100% 委托 redb（`Durability::Immediate`）。仅保留一个最小「container fsync 后 kill、redb 重开数据在」smoke。
- **out of scope（单独立项，不混入本实施）**：§2 列出的 archive 之外 durable 写点的既有 bug，尤其 **seal.rs 做 temp+rename 但缺父目录 fsync**（compact.rs 有，不一致；崩溃后 seal 的 rename 可能丢失）。记入 ROADMAP 待办。

## §10 决策记录

**已定**：两层架构；`BlockIo` `&self` + `Send+Sync`；**`impl BlockIo for File`（reader 字段原地满足，弃「包 FileIo」）**；泛型 `ArchiveUpdater<W>` + 双构造；覆盖 open 读链；**superblock 写改 `self.io.write_at`，`write_superblock_slot` 留 ArchiveWriter**；崩溃模型支持乱序子集持久化（重排升为核心）；**乱序枚举须与 barrier-sync 失败注入交叉**；**断言用带外 oracle（FaultIo 每次 sync 快照活跃 seq + 测试侧 expected 历史），不靠 commit/reader 自证**；**注入按语义 offset 区间调度，不钉具体 ErrorKind**；剪枝窗口 = barrier2↔barrier2 间 `write_at`，N≤12 否则定 seed 采样；撕裂 512B 对齐；feature 门控（`pub mod blockio` + pub 类型 + 测试文件 inner `#![cfg]`）；不动 reader/writer；container 仅 smoke；dm-log-writes 2–3 场景；reader 缓存失效世代由 FaultIo 注入 `up.sync()` 失败专项覆盖（任务 2.6）。

**实施已定（as-built，2026-06-29）**：
- **`FaultIo` 接口**（`crates/zipfs/src/blockio.rs`，门控 `#[cfg(any(test, feature = "fault-injection"))]`、`pub use`）：`from_bytes(initial)` 播种 durable；注入武装 `fail_write_in(lo,hi)`（区间相交即 EIO，fire-once）/ `fail_sync_in(nth_from_now)`（第 N 次 sync EIO）/ `tear_write_in(lo,hi,prefix)`（只落前 prefix 字节，静默成功）/ `soften_syncs(count)`（接下来 count 次 sync 返 Ok 但不合并 dirty，构造乱序窗口）；崩溃镜像 `crash_with_mask(mask)`（durable + 按 bit 选中的 dirty 子集）；穷举阶梯 `history()`（初始 durable + 每次成功 sync 后的 durable 快照）；自检 `dirty_count()`。EIO 用 `io::Error::from_raw_os_error(5)`，断言只钉「is_err / 不静默错读」，**放宽为 `InvalidData | UnexpectedEof`**（spec §10「不钉具体 ErrorKind」，评审 H3）。
- **带外 oracle 接口**（`crates/zipfs/tests/fault_injection.rs`）：`active_seq_of(&[u8])` 经 pub `parse_superblock` 取两槽较大有效 seq（不经 commit/reader）；`expected: Vec<Vec<u8>>` 历史前缀台账（仿 `append_tail_buffer.rs`）；崩溃镜像经临时文件 + **现有** `ArchiveReader::open` 校验（reader 独立于被测写路径）。
- **dm-log-writes 脚本形态**（`bench/scripts/crash-test-dm-logwrites.sh`）：loop(data)+loop(log)+dm-log-writes → ext4 → zipfs；在 3 招牌边界 `dmsetup message <dm> 0 mark <label>`（append-a / rename-b / create-c）；撤 dm 层后逐 mark `replay-log --end-mark <label>` 回放到 data 盘 → 直接挂 ext4 + zipfs → per-scenario python 校验（连续前缀 / rename 新内容 / create 目录项 durable）。全部 root 门控、SKIP exit 0。
- **补强（评审纠偏）**：增「双 SB 非互污 + sb_crc 拒损坏活跃槽 → 回落上一已提交版」用例（钉死真正的单缓冲降级 bug，突变实证有牙）。

