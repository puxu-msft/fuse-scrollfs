# scrollz 并发正确性整改计划（最全面 / 长远正确）

## Context（为什么做）

scrollz 是自研多线程 FUSE 透明压缩文件系统（fuser 0.17，`n_threads = available_parallelism`，`clone_fd`）。一次聚焦多线程正确性的审查（4 路并发审查 agent + 逐条源码复核）确认：**前端 per-inode 标记锁本身正确**（`lock_for`/`evict_lock` 同持 `locks` 表锁串行化，互斥成立——reviewer 的「两把不同 RwLock」C1 为误报，已用源码否定），但**两个 Store 后端存在真实的数据丢失 / torn-read / 孤儿块缺陷**，且整套锁纪律「靠注释约定、未被类型系统强制」，长期迭代脆弱。

用户定调：**最全面、最正确、不惜代价追求长远正确**；并发修复全量重构（含前端锁 dashmap 化），引入 `parking_lot` + `loom` + `dashmap`。执行 subagent 驱动，按阶段过 code review 与全测试绿门控；我负责编排。

目标产出：消灭已确认的数据损坏缺陷；把「靠约定」的加锁纪律升级为「类型系统强制 + loom 机器证明」；不退化既有 188 测试与压缩/性能收益。

## 已确认缺陷（源码已复核，括号为证据）

| # | 级别 | 位置 | 缺陷 |
|---|---|---|---|
| C-1 | CRITICAL | [container.rs:628](fuse/src/store/container.rs#L628) vs [:558](fuse/src/store/container.rs#L558)/[:585](fuse/src/store/container.rs#L585)/[:264](fuse/src/store/container.rs#L264) | `commit_pending` 先 `mem::take` 再 commit redb → torn read（get_block 读 `Ok(None)`、size 回退）；commit 任一 `?` 早返回丢整批 pending（含 truncations 删块意图），后续 fsync 因 pending 空返回 `Ok` 掩盖 |
| C-2 | HIGH | [container.rs:401](fuse/src/store/container.rs#L401) vs [:657](fuse/src/store/container.rs#L657) | unlink 删 redb inode 后才清 pending；并发 flush 把该 ino 脏块写回 redb（blocks 循环无存在性检查，sizes 循环有——不对称）→ 孤儿块 |
| C-3 | CRITICAL | [shadow.rs:402-493](fuse/src/store/shadow.rs#L402-L493)、[archive.rs:625](fuse/src/archive.rs#L625) | create/unlink/rename 对三把 Mutex 多次独立加锁、syscall 夹中间、无外层命名空间锁 → ino↔path 失配、孤儿 session/reader、ino 不稳定；`ArchiveWriter::create`=`File::create`（O_TRUNC 无 O_EXCL）→ 并发同名 create 互相截断 |
| C-4 | MEDIUM | [rwfs.rs:184-187](fuse/src/rwfs.rs#L184-L187) | `forget_inode_flush`：seal 成功但 `store.flush` 失败仍 `forget` 缓冲 → 未提交尾字节静默丢失 |
| C-5 | MEDIUM | [blockcache.rs:225](fuse/src/core/blockcache.rs#L225),[:277](fuse/src/core/blockcache.rs#L277) | 锁内裸 `cur_bytes -= len` 下溢 panic → 毒化 Mutex → 全局缓存 DoS |
| C-6 | MEDIUM | 全项目 `.lock().unwrap()` | 任一持锁 panic 毒化 std 锁 → 整挂载级联拒服务 |
| C-7 | LOW/MED | [archive.rs:722](fuse/src/archive.rs#L722) | `ArchiveWriter::finish` 只 `flush` 不 `sync_all`（create 路径已补 sync_all+fsync_dir，fixture/其它调用方未必） |
| C-8 | LOW | [container.rs:481-516](fuse/src/store/container.rs#L481) | readdir 对 dirents 与每个 child inode 分开 `begin_read`，跨事务类型可错报 |

**复核为正确、不动**：archive 双 superblock + append-only + barrier1→barrier2 崩溃一致性（测试 archive.rs:1541/1558 已钉）；blockcache 单锁守护全部可变态的线程安全；store/lock.rs 跨进程 flock。

## 方案（推荐，TDD + subagent 驱动）

实施顺序按「数据损坏严重度 × 复现确定性 × 改动隔离度」：**D2 → D1 → D3 → D5 → D4**，loom（D6）嵌入 D1 与 D4。每阶段：先写复现 bug 的失败测试（RED）→ 最小实现（GREEN）→ `ecc-fix:rust-reviewer` 过审 → `cargo test` + `cargo clippy -D warnings` 绿 → 提交（conventional commits，无 co-author）。每阶段一个 subagent 主刀，我编排与门控。

### 阶段 0 — 依赖与锁 wrapper 基建
- `Cargo.toml` 加 `dashmap`、`parking_lot`；`[dev-dependencies]` 加 `loom`；`[target.'cfg(loom)']` 处理。
- 新增 `src/sync.rs`：薄 wrapper newtype `Mutex<T>`/`RwLock<T>`，`#[cfg(loom)]` 转 `loom::sync`（`lock()` 内部 `.unwrap()`）、`#[cfg(not(loom))]` 转 `parking_lot`（`lock()` 直返 guard）。抹平两者 API 差异（loom 返回 `LockResult`、parking_lot 不返回），全 crate 统一从 `crate::sync` 取锁类型。**不可用裸 type alias**。

### 阶段 D2 — container 孤儿块（最先，最确定）
- 文件：[container.rs](fuse/src/store/container.rs)。
- RED：put_block(ino,idx)→ 手动 commit_pending（模拟并发 flush）→ unlink → 断言 `blocks.range((ino,0)..=(ino,u64::MAX))` 为空。
- GREEN：`commit_pending` 的 blocks 写入循环（657-662）加 inode 存在性检查（与 sizes 循环 665-670 对称），按 ino 缓存存在性避免每块查表；unlink 在删 redb inode 的**同协调**下先清 pending 该 ino 三表。

### 阶段 D1 — container 双缓冲 + 快照读（核心数据丢失）
- 文件：[container.rs](fuse/src/store/container.rs)、[store/mod.rs](fuse/src/store/mod.rs)、[rwfs.rs](fuse/src/rwfs.rs) read 路径。
- 结构改为单 `Mutex<Inner{ active: Pending, flushing: Pending }>` + 独立 `commit_lock: Mutex<()>`。
  - commit：持 `commit_lock` → 持 inner `mem::swap(active↔flushing)`（active 清空，新写继续进 active）→ **释放 inner** → 用 flushing commit redb（IO 不持数据锁）→ 成功持 inner 清空 flushing；失败持 inner 把 flushing 合并回 active（active 已有键优先，不覆盖更新写）。
  - 读：持单 inner 锁，锁内查 active 再查 flushing（每层各自 blocks+truncations 判定），释放，再查 redb。条目恒在 active∪flushing∪redb → 无 torn read；失败不丢 → 无 lost-update。
- 收口 rwfs torn 窗口：Store trait 新增组合快照方法（如 `geometry_and_reader(ino) -> Snapshot`），container 在**一次 inner 锁**内同时取几何 + 能回答任意 idx 的快照（active/flushing 克隆 + 一个 redb read txn），让 `read_range` 整区间用同一代视图；默认实现回退现有两调用，仅 container/shadow 覆写。
- 为 D6 铺 `BlockBackend` trait（container 对 redb 的最小依赖面：commit/get_block/get_size），生产用 `RedbBackend`，loom 用内存 map 替身（可注入 commit 失败）。
- RED：(a) 非 loom——注入「第 N 块失败」的 BlockBackend → commit 返 Err → 断言所有块仍可读（当前会丢）。(b) loom（见 D6）。

### 阶段 D3 — shadow 命名空间原子化 + O_EXCL
- 文件：[shadow.rs](fuse/src/store/shadow.rs)、[archive.rs:625](fuse/src/archive.rs#L625)。
- 新增 `ns: Mutex<()>`，**仅** create/mkdir/unlink/rmdir/rename/symlink 全程持有，覆盖「查存在→syscall→改三表」原子化。**数据路径（get_block/put_block/lookup-intern）不碰 ns**：仍只短暂持 inodes 锁解析 path（syscall 锁外，沿用现有 `cached_reader` 模式）+ readers 锁。粗 ns 锁优于合并单锁——后者会把慢 syscall 关进数据锁、阻塞高频 get_block。锁序文档化：`ns < inodes < sessions < readers`。
- `ArchiveWriter::create` 的 `File::create` → `OpenOptions::new().write(true).create_new(true).open()`（O_EXCL），失败 `AlreadyExists`→EEXIST。纵深防御（防跨进程 / ns 锁外意外路径）。
- RED：多线程并发 `create(parent,"同名")×N` → 断言仅一个成功且 by_ino/by_path 双向一致；并发 create+unlink 同名 → 表无悬挂孤儿、ino 稳定。

### 阶段 D4-a — 小修（随 D5 前顺带）
- C-4：`forget_inode_flush` 中 `store.flush` 失败视同 seal 失败——保留缓冲与锁、不 forget、下次重试（与现有 seal 失败分支一致）。
- C-5：blockcache `cur_bytes -= len` → `saturating_sub` + `debug_assert`。
- C-7：`ArchiveWriter::finish`（`ArchiveWriter<File>`）补 `sync_all`；审计 fixture/mkfixture 调用方父目录 fsync。
- C-8：readdir 在单个 `begin_read` 内同读 dirents+inodes（同快照），消类型错报。

### 阶段 D5 — parking_lot 全替换
- 全 crate `std::sync::{Mutex,RwLock}` → `crate::sync`（阶段 0 wrapper，底层 parking_lot），去掉所有锁 `.unwrap()`（无毒化）。消灭 C-6 级联 DoS。无新测试，跑通现有全套 + clippy 即验证；为 D6 别名铺路。

### 阶段 D4-b — 前端 dashmap 化（最大重构，长远加固）
- 文件：[rwfs.rs](fuse/src/rwfs.rs)、[core/wsession.rs](fuse/src/core/wsession.rs)。
- `locks: Mutex<HashMap<u64, Arc<RwLock<()>>>>` → `DashMap<u64, Arc<RwLock<InodeState>>>`，`InodeState{ tail: Option<Tail> }` 把 wsession 的 `Tail` 搬进 per-inode RwLock；`TailSessions` 内部 `Mutex<HashMap>` 消解，方法重构为对 `&InodeState`/`&mut InodeState` 操作 → **编译器强制「改 Tail 必须持 inode 写锁」**，注释约定升为类型约定。
- 取锁顺序硬约束：`lock_for = map.entry(ino).or_default().clone()`（Ref 立即 drop 释放 shard 锁）→ 再 `.read()/.write()` 内层；evict=`remove_if(ino, |v| Arc::strong_count(v)==1)`（shard 锁内与 entry 串行，TOCTOU 安全，等价现状）。
- **语义变更需显式处理**：getattr/lookup 的无锁 `geometry`/`read_tail_block` 折叠进 inode 读锁（原走 TailSessions 独立表锁）→ getattr 多一次读锁。1s TTL 下 getattr 低频，正确性收益 > 代价。
- block_cache **留 InodeState 外**（跨 inode 全局 LRU + 全局字节预算，进 InodeState 会破坏全局比较）；失效一致性仍靠「持 InodeState 写锁时 `block_cache.invalidate(ino)`」。
- RED：并发 write+forget 同 ino → 断言无 panic、无锁表泄漏、Tail 不丢。

### 阶段 D6 — loom 模型验证（嵌入 D1 与 D4）
- `#[cfg(loom)]` 模型测试，限 2-3 线程、每线程 2-3 op：
  - D1 pending 状态机（用 `BlockBackend` 内存替身，可注入 commit 失败）：穷举 put/get/commit-ok/commit-fail/unlink 交错，不变量——已 put 未被 truncate/unlink 的块 get 必返回它或更新版本（绝不 None/旧值）；commit 失败该块仍可见；unlink 后无残留可见块。
  - D4 `lock_for`/`evict` TOCTOU：同 ino 恒一把活锁、互斥成立。
- 运行：`RUSTFLAGS="--cfg loom" cargo test --test loom_*`（或 cfg gate 模块）。

## 关键文件
- [container.rs](fuse/src/store/container.rs) — D1 双缓冲/快照、D2 存在性检查、BlockBackend
- [shadow.rs](fuse/src/store/shadow.rs) — D3 ns 锁、数据路径保细锁
- [archive.rs](fuse/src/archive.rs#L625) — O_EXCL、finish sync_all
- [rwfs.rs](fuse/src/rwfs.rs) — D4 DashMap<InodeState>、read_range 共享快照、C-4
- [core/wsession.rs](fuse/src/core/wsession.rs) — D4 Tail 进 InodeState、消解内部 Mutex
- [store/mod.rs](fuse/src/store/mod.rs) — Store trait 快照方法 + BlockBackend
- [core/blockcache.rs](fuse/src/core/blockcache.rs) — C-5 saturating_sub
- 新增 `src/sync.rs`（loom/parking_lot wrapper）

## 验证
- 每阶段：`cargo test`（含新增并发回归测试）+ `cargo clippy -- -D warnings` 全绿；`ecc-fix:rust-reviewer` 过审无 CRITICAL/HIGH。
- 并发回归：container「多线程 put+fsync vs get_block 断言永不 `Ok(None)`」、「注入 commit 失败断言不丢」；shadow「并发同名 create/unlink/rename 断言表不变量 + ino 稳定」。
- loom：`RUSTFLAGS="--cfg loom" cargo test` 穷举 D1/D4 状态机。
- 全局回归：既有 188 测试不退化；`cargo test --features fault-injection`（Tier1 集成）；append-bench/ratio-bench 确认压缩与吞吐收益不退化。
- 收尾：`cargo +nightly test -Z sanitizer=thread`（可选，TSan 兜底数据竞争）。
