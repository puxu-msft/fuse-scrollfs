# Kick-off Prompt — zipfs 故障注入测试（两层）

> 复制下面「===PROMPT===」之间的内容到新会话即可启动。**自包含**：新会话无需本会话上下文，照此执行。
> 完整设计见 `docs/05-fault-injection-testing.md`（三方评审定稿）；本文是其可执行计划。

===PROMPT===

你在 `/home/xp/src/zipfs`（Rust FUSE 透明压缩文件系统，布局 S = 每文件一个 archive）。任务：实现**故障注入测试两层架构**，把已落地的崩溃安全协议（双 superblock + append-only + 尾日志）变成可回归的故障注入测试。**你是实施者，按 TDD 一步步来，每个绿阶段单独提交。**

## 背景（已完成，勿重做）

- 崩溃安全提交协议已落地：`docs/04-crash-safe-commit.md` §8.3（双 superblock + 两 barrier）、§8.4（尾日志）。
- 现有崩溃测试：archive.rs 单元测试（drop-未提交 / 翻字节）+ `bench/scripts/crash-test.sh`（守护 `kill -9`）。
- 缺口：`kill -9`（RAM 不丢）+ tmpfs（fsync no-op）测不到「fsync 后丢失 / EIO / 撕裂 / 重排」。本任务补这个缺口。

## 必读（建立准确认知，勿凭记忆）

1. `docs/05-fault-injection-testing.md`——**完整设计，先通读**。尤其 §2（三层覆盖边界）、§3（BlockIo seam）、§4（FaultIo 崩溃模型）、§8（Tier 1 不覆盖什么）、§9（重构边界）。
2. `fuse/src/archive.rs`：`ArchiveUpdater`（`open`/`commit`/`commit_journal`/`set_block`/`append_journal`/`sync`）对 `self.file: File` 的全部用法；自由函数 `load_active`/`read_sb_slot`/`read_head_cache_bytes`/`read_exact_at`/`write_superblock_slot`；`ArchiveReader::from_file`（**保持不动**）。
3. `fuse/src/store/shadow.rs`：`commit_session`/`append_tail`/`seal_tail_block`（构造 `ArchiveUpdater::open`），`cached_reader`/`invalidate_reader`（**写后失效协议，必须保全**）。
4. `fuse/src/store/container.rs`：`fsync`/`sync_all` 委托 redb（不涉，仅 smoke）。

## 核心约束（全有或全无，别只改一半）

- **`ArchiveUpdater::open` 是读重头戏**，不是纯写：它调 `read_exact_at`(header) + `load_active`（读双 SB + index + bounds）+ `read_head_cache_bytes`。这些读必须经 `BlockIo`，否则「打开/恢复阶段读失败」无法注入——而那正是双 SB 崩溃安全最该测的路径。
- **生产零行为变化**：生产经 `impl BlockIo for File`（`ArchiveUpdater<File>`）的路径必须与改造前逐字节等价；现有 122 测试 + crash-test.sh 必须全程绿，并由任务 1.2c 的 spy 字节比对正向守网。
- **保全 `invalidate_reader`**：重构写路径不得丢失 shadow.rs 的写后缓存失效点，否则热读 `Arc<ArchiveReader>` 读陈旧 footer。
- **崩溃模型必须支持脏页乱序子集持久化**（非「全丢 dirty」），否则测不到「SB 先于其数据落盘」——§8.3 双 SB 防的正是这个。

## 设计（统一签名，照此实现）

`fuse/src/blockio.rs`（新）：
```rust
pub trait BlockIo: Send + Sync {
    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<()>;
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn set_len(&self, len: u64) -> io::Result<()>;   // append-only 写路径暂不用，留压实
}

pub struct FileIo(pub std::fs::File);   // 用 FileExt::{write_all_at, read_exact_at, sync_all}
```
- 方法取 `&self`（贴合 `FileExt`，免改 `ArchiveUpdater::sync(&self)`）；`FaultIo` 内部用 `Mutex`/原子做可变。
- **`impl BlockIo for File`（关键，取代「包 FileIo」）**：让 `File` 直接实现 `BlockIo`（转调 `FileExt`）。如此 `ArchiveReader` 的 `file: File` 字段**原地**满足 `&impl BlockIo`，4 处 `read_exact_at(&self.file,…)` 零所有权改动复用——避免 `from_file` 拿 File 所有权要存字段、中途包 `FileIo` 被 move 走的陷阱。`FileIo(File)` newtype 可留作 `from_io` 注入对称用或弃用、生产走 `ArchiveUpdater<File>`。
- `ArchiveUpdater<W: BlockIo>` 泛型；`open(path) -> ArchiveUpdater<File>`（生产）+ `from_io(io, …)`（注入）。`load_active`/`read_sb_slot`/`read_head_cache_bytes`/`read_exact_at` 自由函数改吃 `&impl BlockIo`。
- **`write_superblock_slot<W: Write+Seek>` 不动**（它被 `ArchiveWriter::finish` 共用）：updater 侧改用 `self.io.write_at(off, &serialize_superblock(sb))`（与 append-only 绝对偏移写等价），把该自由函数留给 `ArchiveWriter`。`ArchiveWriter` 整体不动（`Write+Seek` + `Cursor` 单测）。
- `seek(Start(abs))+write_all` → `write_at(abs, buf)` 严格等价（写后手动维护 `write_cursor`，从不读回游标）；`&self` 写与 updater 的 `&mut self` 方法相容（`&mut self` 蕴含可取 `&self.io`，分句写无冲突）。

## 阶段与任务（TDD：先写失败测试 → 跑挂 → 最小实现 → 跑过 → 提交）

### 阶段 1 — BlockIo seam（纯重构，生产零行为变化）

**任务 1.1：`BlockIo` trait + `impl BlockIo for File`。**
1. 写失败测试 `fuse/src/blockio.rs` `#[cfg(test)]`：对 tempfile 的 `File` 做 `write_at(0,b"abc")` → `sync` → `read_at` 回读 `b"abc"`，`len()==3`，`set_len(1)` 后 `len()==1`。
2. `cargo test --release blockio::` → FAIL。
3. 实现 trait + `impl BlockIo for File`（转调 `FileExt::{write_all_at, read_exact_at, sync_all}`、`set_len`、`metadata().len()`）。`lib.rs` 加 `pub mod blockio;`，类型全 `pub`。
4. PASS。提交：`feat: BlockIo trait + impl for File（定位 IO 接缝）`。

**任务 1.2a：open 读链 + 共享自由函数泛型化（updater 暂仍写 File，但写路径先不动）。**
- `load_active`/`read_sb_slot`/`read_head_cache_bytes`/`read_exact_at`/`validate_and_load_index` 签名 `fn(&File)` → `fn(&impl BlockIo)`。`ArchiveReader`（`from_file`/`read_block`/`read_tail`/`read_head_cache` 共 4 处 `read_exact_at(&self.file,…)`）因 `impl BlockIo for File` 原地复用，**reader 一行所有权代码不动**。
1. 不写新测试；跑现有 reader/open 测试：`cargo test --release --lib archive::` 与 `--test append_tail_buffer` → 全 PASS（守读链等价）。
2. 提交：`refactor: archive 读链泛型化到 BlockIo（reader 经 impl for File 原地复用）`。

**任务 1.2b：`ArchiveUpdater<W: BlockIo>` 写路径泛型化。**
- 字段 `file: File` → `io: W`；`commit`/`commit_journal`/`set_block`/`append_journal`/`sync` 的 `seek+write_all`/`sync_all` → `self.io.write_at`/`sync`；**superblock 写改 `self.io.write_at(off, &serialize_superblock(sb))`**（不再调 `write_superblock_slot`，后者留 `ArchiveWriter`）；`open(path)->ArchiveUpdater<File>` 包装、新增 `from_io`。**以下必须同一提交内改完，缺一编译不过**：字段 + 5 写方法 + superblock 写 + `from_io`。
1. 跑现有 updater 单元测试：`cargo test --release --lib archive::`（含 `updater_未提交即崩溃_*`、`updater_活跃sb损坏_回落另一槽恢复`、`journal_未提交即崩溃_*`）→ 全 PASS。
2. `cargo clippy --release --all-targets` → 0 warning。
3. 提交：`refactor: ArchiveUpdater 写路径泛型化到 BlockIo`。

**任务 1.2c：字节等价正向回归网（现有测试 CRC 自洽、对布局不敏感，必须补这条）。**
- 现有 updater 测试只经 reader 间接验最终字节正确，offset 错位可能因 CRC 自洽蒙混。加一个 `#[cfg(test)]` spy `RecordingIo`（记录所有 `write_at(off,buf)`，内部转调一个 `Vec<u8>` 或真实 File）。
1. 失败测试：同一工作负载（建档 + 两次 `set_block` + `commit`）分别经「改造前已知正确字节」与「经 spy 的 updater 产出镜像」逐字节比对相等。golden 可取本任务前 `git show` 出的旧实现产物，或与 `ArchiveWriter` 等价产物对比。
2. PASS。提交：`test: ArchiveUpdater 字节等价回归网（spy IO）`。

**任务 1.3：shadow.rs 经 `File`/`open` 构造，保全失效协议。**
1. shadow.rs 的 `ArchiveUpdater::open(&abs)` 返回 `ArchiveUpdater<File>`（类型推断），**调用处通常一字不改**；确认 `commit_session`/`append_tail`/`seal_tail_block` 的 `invalidate_reader` 调用点（shadow.rs 约 304/552/572）一字未动。
2. **显式声明接缝外 durable 写点**：shadow.rs 约 273-274 `commit_session` 无脏数据分支 `fs::File::open + f.sync_all()` 是一条独立 fsync，**有意留在 BlockIo 之外**（归 crash-test.sh / Tier 2），勿误以为 shadow 所有 durable 写已收口 FileIo。
3. 全量绿网（见下「测试网」全部命令）。
4. 提交：`refactor: shadow 写路径经 File（行为不变）`。

### 阶段 2 — Tier 1 `FaultIo` 确定性崩溃模拟器

先在 `fuse/Cargo.toml` 加：
```toml
[features]
fault-injection = []
```
`FaultIo` 门控 `#[cfg(any(test, feature = "fault-injection"))]` 且 **`pub`、经 `lib.rs` `pub mod blockio;` 导出**（否则 `tests/` 用不到）。**引用 `FaultIo` 的集成测试文件首行加 inner attribute `#![cfg(feature = "fault-injection")]`**——使不带 feature 的那趟 `cargo test`（测试网第 3 行）整文件消失、不致 `use zipfs::blockio::FaultIo` 找不到符号而红。运行带 `--features fault-injection`（`cfg(test)` 对 `tests/` 独立 crate 不可见）。

**任务 2.1：`FaultIo` 页模型（durable + dirty，sync 合并，crash 可产乱序子集）。**
1. 失败测试：① `write_at` 后未 `sync` → `crash()` 镜像不含该写；`sync` 后含。② **乱序能力存在性**：两次未 sync 的 `write_at`（A、B）后，`crash()` 能产出「含 A 不含 B」的镜像（钉死模型未退化成「全丢 dirty」——否则 2.5 剪枝可能悄悄废掉整条链）。
2. 实现 `FaultIo`（`Mutex<{durable: Vec<u8>, dirty: BTreeMap<u64,Vec<u8>>, seed}>`，`sync` 合并 dirty→durable，`crash()` 按 seed 持久化 dirty 子集 → `Vec<u8>`）。
3. PASS。提交：`test: FaultIo 页缓存崩溃模型（含乱序子集能力）`。

**任务 2.2：注入按语义 offset 区间调度 + EIO 全 commit 点覆盖。**
- **不按裸写序号**（实现细节、易变），而据 offset 落区间（SB_A 槽 / SB_B 槽 / index 区 / journal 区）调度（格式契约、稳定）。EIO 注入点**参数化覆盖 commit 的全部 sync/write 点**，尤其 **barrier 2 的 SB 槽写一半** 与 **barrier 2 的 sync** ——而非只钉 barrier 1（barrier 1 失败时非活跃槽天然没污染，断言太弱、易假绿）。
1. 失败测试：对每个注入点（含写 SB 槽中途 EIO、barrier 2 sync EIO）→ 断言 ① `commit` 返 `Err`（**只钉「是 Err 且非静默吞」，不钉具体 `ErrorKind`**）② `crash()` 镜像经 `ArchiveReader::from_file` 开成上一已提交版（半截 SB 被 `sb_crc` 拒、活跃槽回落）。
2. 实现 offset-区间调度 + 镜像落盘 helper。
3. PASS。提交：`test: FaultIo EIO 全 commit 点注入，错误传播 + 回落上一版`。

**任务 2.3：撕裂（512B 对齐部分写）。**
1. 失败测试：撕裂 index/SB 写（只落前 512B）→ 镜像开 reader 经 `index_crc`/双 SB 检损、回落上一版或 fail-closed 报损（绝不静默错读）。
2. 实现撕裂调度（K 量化 512B）。
3. PASS。提交：`test: FaultIo 撕裂写（512B 对齐）→ fail-closed`。

**任务 2.4：穷举崩溃点（掉电模型）+ 带外 oracle。**
- **断言不得自证**（评审 CRITICAL）：「fsync-acked 版本」须由带外台账独立确定，仿 crash-test.sh 的 `PROGRESS`。`FaultIo` 每次 `sync()` 成功瞬间，由测试侧把当时 `durable` 镜像 `parse_superblock` 出的活跃 seq 记入台账；`acked = 台账末项`（测试侧解析镜像得出，**不经 `commit` 返回值**）。测试侧另维护逻辑内容 `expected` 历史快照（仿 `append_tail_buffer.rs`）。
1. 失败测试：固定 append+fsync 工作负载（如 20 行各 fsync），`for crash_after in 0..total_writes` 每点 `crash()` → 断言 ① reader 读出内容 ∈ {某历史 `expected` 前缀}（**不只是「reader 自洽」**——自洽的坏数据照样自洽）② 镜像活跃 seq ≥ 带外 `acked`。
2. 实现穷举驱动 + 带外台账 + `parse_superblock` 测试 helper。
3. PASS。提交：`test: 穷举崩溃点 durability + fail-closed（带外 oracle）`。

**任务 2.5：脏页乱序子集 × barrier-sync 失败交叉（核心，非 stretch）。**
- **必须与 barrier 失败交叉**（评审 关键盲区）：单独「所有 barrier 成功 + 子集枚举」在屏障语义下**恰好排除**唯一危险 case（SB 落盘而其依赖 index 没落）。须注入「barrier 1 的 sync 失败/被乱序但 commit 仍推进」，构造「barrier 2 的 SB 写进 durable、barrier 1 本应保护的 index 写未进 durable」的镜像。剪枝窗口 = barrier2↔barrier2 间全部 `write_at`，枚举上限 N≤12，超过则定 seed 采样。
1. 失败测试：对交叉枚举出的每个镜像，断言「reader 读出 ∈ {历史 `expected` 前缀} 或 fail-closed 报损」——**SB 不得先于其依赖数据生效**；专构一例「SB 进 durable、index 未进」断言 reader 必 fail-closed 或回落（直接钉死 §8.3 核心价值）。
2. 实现乱序子集枚举 + 与 2.2 的 barrier-sync 注入交叉。
3. PASS。提交：`test: 脏页乱序子集 × barrier 失败（验双 barrier 真有序）`。

**任务 2.6：shadow `invalidate_reader` 失效世代（FaultIo 驱动）。**
- 历史 reuse-tail-slot durability 洞高发区，三层中只有 FaultIo 能在进程内覆盖。
1. 失败测试：用注入 `up.sync()` 返 EIO 的 `BlockIo` 驱动 `ShadowStore.commit_session` → 断言即便 sync 失败提前返回，`readers` 缓存已不含该 ino 旧 reader（shadow.rs 约 302-305 不变量），后续读不命中陈旧 footer。
2. 需让 ShadowStore 可注入 updater 的 IO（构造期注入点或 `#[cfg(feature)]` 钩子）。
3. PASS。提交：`test: shadow sync 失败后 reader 缓存失效（不读陈旧）`。

### 阶段 3 — Tier 2 device-mapper 真实块层门（root 门控）

**任务 3.1：`bench/scripts/crash-test-dm.sh` dm-flakey smoke。**
- 非 root / 无 `dmsetup` / 无 `/dev/fuse` → `SKIP exit 0`（比照 crash-test.sh 门控）。loop+dm-flakey，跑中途切 `drop_writes` + kill 守护，重挂校验 fail-closed + 已 ack 行存活。只用自建临时设备/目录，**绝不通配 rm，绝不动系统挂载**。
- 提交：`test: dm-flakey 真实块层 smoke（root 门控）`。

**任务 3.2：dm-log-writes 回放（2–3 招牌场景）。**
- (a) append+fsync 序列回放每个 flush 边界 mount+校验；(b) rename 覆盖；(c) create 后崩溃验目录项 durability。root 门控同上。**不做 xfstests 全量。**
- 提交：`test: dm-log-writes 回放 flush 边界（appendfsync/rename/create）`。

**任务 3.3：container fsync+kill smoke。**
- 在 crash-test.sh 加 `--backend container` 变体（或新增最小脚本）：fsync 后 kill、redb 重开数据在。只验委托正确，不重测 redb。
- 提交：`test: container fsync 后崩溃 smoke（验委托 redb）`。

## 测试网（必跑，安全网）

```bash
cd fuse
cargo fmt
cargo clippy --release --all-targets            # 0 warning
cargo test --release                            # 现有 122 必须全绿
cargo test --release --features fault-injection # Tier 1 新测
cd .. && for i in $(seq 1 10); do CHUNK_SIZE=4096 bash bench/scripts/crash-test.sh 40000 $(awk "BEGIN{print 0.6+0.12*$i}"); done   # 仍 10/10 0%
# Tier 2（若 root + dmsetup 可用）：
sudo bash bench/scripts/crash-test-dm.sh
```
注：测试临时目录已走 `/dev/shm` tmpfs（`.cargo/config.toml`），但 **Tier 2 的 dm/loop 必须用真实块设备**，不要放 tmpfs（fsync no-op 会废掉保真）。

## 验收

1. `cargo test --release` 与 `--features fault-injection` 全绿、clippy 0、fmt 干净。
2. 阶段 2 招牌穷举测试通过，**含乱序子集**（任务 2.5）——否则没测到双 barrier 的核心价值。
3. crash-test.sh 10/10 仍 0% 丢数据（重构未破坏端到端）。
4. Tier 2 在 root 环境跑通（无 root 则 SKIP，不算失败）。
5. 生产路径零行为变化（现有 122 测试逐条仍绿）。

## Git 工作纪律（关键——共享 worktree，有「邻居」并发）

- **只提交你自己改的文件**：`git add -- <具体文件>` → `git diff --cached --name-status` 核对只含你的 → `git commit -m ...`（**索引提交，不带 pathspec**，因 `git commit -- <file>` 会提交工作树该路径全部内容含邻居 hunk）。**绝不 `git add -A`**。
- 同文件与邻居 hunk 重叠时只提自己的 hunk（`git add -p`）。邻居未提交 WIP（尤其 `docs/ROADMAP.md`）不要碰、不要替他提交。
- conventional commits、不要 `Co-authored-by`、中文 commit/注释。

## 收尾

- 把 `docs/05-fault-injection-testing.md` §10「待实施期定」的三项按你的实现填实。
- ROADMAP「故障注入测试（两层）」改 ☐→☑（**若 ROADMAP 仍是邻居未提交 WIP，只提示用户、别替邻居提交**）。
- 提示用户单独修 `seal.rs 缺父目录 fsync`（本任务 out of scope，见 spec §9）。

===PROMPT===

## 给用户的备注（本文档外）

- 本会话为计划者，未写实现代码。设计 spec + ROADMAP 立项已提交（`10eb58d`）。
- 阶段 1 自带等价回归网（任务 1.2c 的 spy `RecordingIo` 是 `#[cfg(test)]`、不依赖阶段 2 的 `FaultIo`），故阶段 1 可独立合并、与阶段 2/3 互不阻塞。
- 块大小 durability（MEMORY 记的「1MiB 默认每 fsync 截块 0」）**不在本计划 scope**——本计划不改块大小，crash-test `CHUNK_SIZE=4096` 的 10/10 0% 不等于「所有块大小都安全」，那是 archive 配置另案。
- seal.rs 父目录 fsync bug 已记 ROADMAP T1，建议独立小修，勿混进本计划。
