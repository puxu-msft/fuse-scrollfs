# 计划：压力感知解压块缓存（#1）+ release profile 调优（#3）

## Context（为什么做）

读路径分析发现两处可兑现的性能优化：

1. **读放大**：[fuse/src/rwfs.rs:281-298](fuse/src/rwfs.rs#L281-L298) 的 `read_range` 对每个覆盖块都 `get_block → decompress_block`，**无任何解压结果缓存**（已确认无 `lru`/`moka` 依赖，且只协商 `max_write` 未设 `max_readahead`）。内核按 ~128KiB 粒度下发 read，而块默认 1MiB → resume 的整文件顺序前向扫描把**同一个 1MiB 块解压约 8 次**。resume 正是主力读负载。`ArchiveReader` 缓存（[shadow.rs:114](fuse/src/store/shadow.rs#L114)）只解决索引重解析，没解决重复解压。

2. **release profile 未调优**：`fuse/Cargo.toml` 无 `[profile.release]`（且无 workspace 根），默认 `lto=off` / `codegen-units=16`。compress/decompress 是纯 CPU 热点。

目标产出：默认开启、可配置、感知内存压力并自动缩减自身占用的解压块缓存，把顺序读的解压 CPU 降约 8×；外加零行为风险的编译期优化。用户决策：缓存**默认开、128MiB 上限**。

## 关键正确性杠杆（subagent 评审后强化）

### 杠杆 A：结构性规则——只缓存 `idx < tail_idx` 的严格内部块（承重）

`tail_idx = uncompressed_size == 0 ? 无 : (uncompressed_size - 1) / chunk_size`（由 `read_range` 已有的 `geometry` 即可算）。**只在 `idx < tail_idx` 时存入缓存**。代码实证两条事实使此规则充分：
- `get_block` 对 `idx == chunk_count` 返回**可变的尾日志重放 verbatim 块**（[shadow.rs:589-595](fuse/src/store/shadow.rs#L589-L595)）——缓存它会在下次 append 后读到陈旧字节（评审 MEDIUM-3，已证实为真）。`idx < tail_idx` 排除它。
- append/seal/`materialize` **只写尾块索引**、从不改写内部已封存块（[wsession.rs:185-196](fuse/src/core/wsession.rs#L185-L196)，已证实）。故 `idx < tail_idx` 的块在 append 主负载下**恒不可变**。
- 推论：fsync/flush/release/forget 只 seal 尾块，**结构上不可能影响任何被缓存的块** → 它们无需失效。能改写内部块的只有 RMW write 与 truncate。
- 边界取保守：块对齐文件的最后一满块虽不可变，`idx < tail_idx` 也不缓存它（少缓存一块，安全优先；顺序读它本就只读一遍，损失微小）。

### 杠杆 B：per-inode RwLock 串行化（无需 epoch）

rwfs 的 per-inode `RwLock`（[rwfs.rs:42](fuse/src/rwfs.rs#L42)）已串行化「同 inode 读(共享锁) vs 变更(排他锁)」。`get`/`insert` 只在 `read_range`（持读锁）发生，`invalidate` 只在变更路径（持写锁）发生 → 同一 inode 二者互斥，**无「失效后又插入陈旧块」竞态**，无需 per-ino epoch，仅自身 `Mutex` 守护跨 inode 并发。此不变量写入模块文档。

不缓存：① head-cache 快路径（已是廉价小解压）；② 开放尾块 / 尾日志重放块（杠杆 A 的 `idx < tail_idx` 已排除）；③ 脏会话 read-through 块由 invalidate-on-write 兜底（同字节 commit 安全，后续 write 失效）。

> **不采纳评审 HIGH-2「缓存下沉 Store 层」**：Store 层只搬运不透明压缩字节、解压所需 `CodecParams`(algo/level/dict) 在 rwfs 层，下沉违反分层且需给每个后端注入回调。杠杆 A 的结构性规则用更少代码达到同等（更强）正确性。

## 变更一：release profile（先做，trivial）

`fuse/Cargo.toml` 追加：
```toml
[profile.release]
lto = "thin"
codegen-units = 1
```
**不加** `panic = "abort"`：守护进程中单个 FUSE worker panic 不应 abort 整个挂载，保留 unwind。

## 变更二：新模块 `fuse/src/core/blockcache.rs`

- `trait AvailableMemory: Send + Sync { fn available_bytes(&self) -> Option<u64>; }`
  - `struct ProcMeminfo;`：读 `/proc/meminfo` 取 `MemAvailable:` kB（不可读返回 `None`）。
- `struct BlockCache { inner: Mutex<Inner>, configured_cap: usize, probe: Box<dyn AvailableMemory> }`
  - `configured_cap == 0` → 全 no-op（`get` 恒 `None`，`insert` 不做事）。
  - `Inner { map: HashMap<(u64,u64), Node>, order: BTreeMap<u64,(u64,u64)>, cur_bytes, eff_cap, seq, last_probe: Option<Instant> }`，`Node { bytes: Arc<[u8]>, seq }`。
- 方法：
  - `get(ino, idx) -> Option<Arc<[u8]>>`：命中则更新 LRU 次序、返回 `Arc` clone（不拷字节）。
  - `insert(ino, idx, Arc<[u8]>)`：先 `refresh_effective_cap()`（节流），eff_cap 为 0 或单块 > eff_cap 则跳过；按字节预算 LRU 逐出（替换已存在键时先减旧字节）；插入。**LRU 记账（评审 MEDIUM-1）**：`Node` 存 `seq`；替换同键时**先用旧 seq 从 `order` 删除**再插新 seq；逐出时弹出 `order` 最小 seq 后**校验 `map[key].seq` 与之一致**才删（不一致＝过期 order 条目，跳过），杜绝悬空 seq 致记账漂移、缓存膨胀超预算。并发双读对同键各 miss 各 insert 在 `Mutex<Inner>` 内做「查旧→减→插→加」原子序列，安全。
  - `invalidate(ino)`：急切移除该 ino 全部条目、回收字节。**文档注明须在持 inode 写锁下调用**。
  - `refresh_effective_cap()`：距上次探测 ≥ `PRESSURE_PROBE_INTERVAL` 才探测；`eff = min(configured_cap, budget)`，**`budget = (MemAvailable + cur_bytes).saturating_sub(RESERVE_BYTES) / 2`**（评审 MEDIUM-2：把本缓存已占字节加回 available，消除「缓存占用压低 available→压低 cap→逐出」的自激震荡；available 为 `None` 时退化为 configured_cap）。eff 缩小后由 `insert` 的逐出收敛占用。
- 常量：`PRESSURE_PROBE_INTERVAL = 1s`、`RESERVE_BYTES = 256 MiB`、`DEFAULT_CACHE_BYTES = 128 MiB`。
- 在 `core/mod.rs` 加 `pub mod blockcache;`。

## 变更三：接入 `fuse/src/rwfs.rs`

- `ZipfsRw` 加字段 `block_cache: BlockCache`。
- 构造：现有 `new`/`with_tail_buffer` 默认 cap=0（保持既有单测确定性、零行为变更）；新增 builder `with_block_cache(mut self, bytes: usize) -> Self`（复用 [with_max_write](fuse/src/rwfs.rs#L91) 的链式风格）。main 经 flag 注入 128MiB。
- `read_range` 块循环（committed 分支，[rwfs.rs:281-314](fuse/src/rwfs.rs#L281-L314)）改为：先 `block_cache.get(ino, idx)`，命中则切片返回；未命中走 `get_block`+`decompress_block`，**仅当 `idx < tail_idx`（杠杆 A）** 才把明文 `Arc::from(...)` 存入缓存，再切片。统一成「拿到 `Arc<[u8]>` 后切片」一条路径。head-cache 快路径与 tail-buffer 分支不变。
- 失效：在变更路径调用 `block_cache.invalidate(ino)`。**正确性所需**（能改内部块的路径）：`write`（无条件、写锁内、写之前）、`setattr`-size(truncate)、`unlink`、`rename`（新旧目标）。**防御纵深**（结构上不改缓存块、但廉价且不伤 resume 读路径，兜底我方推理失误）：`fsync`、`flush`、`release`、`forget_inode_flush`、`forget_inode_locked`。均在已持写锁处插入。注释说明哪些是「必需」哪些是「纵深」。

## 变更四：CLI 与 managed 挂载

- `main.rs` `MountArgs` 加 `--block-cache-bytes`，`default_value_t = DEFAULT_CACHE_BYTES`（0 关闭），doc 串说明压力感知。`run_mount` 链式 `.with_block_cache(args.block_cache_bytes)`。
- `enable/daemon.rs` `MountSpec`（[daemon.rs:15](fuse/src/enable/daemon.rs#L15)）加 `block_cache_bytes: usize` 字段，`#[serde(default = "默认函数")]` 使旧持久化 spec 仍解析为 128MiB；`main.rs` `mount_args_from_spec`（[main.rs:258](fuse/src/main.rs#L258)）回填该字段。

## 测试（TDD，先写后实现）

1. `blockcache.rs` 单测：① 命中返回同字节、不重复解压（用计数 fake 探测/或断言 get 后无 store 触达）；② 字节预算 LRU 逐出（超 cap 逐出最旧）；③ `invalidate(ino)` 只清该 ino、回收字节、他 ino 保留；④ 压力：fake `AvailableMemory` 低值 → eff_cap 缩小 → 大块被拒/逐出；高值 → 用 configured cap；节流：区间内改 fake 值不重新探测（用原子计数 fake 断言探测次数）；⑤ cap=0 全 no-op；⑥ 单块 > eff_cap 不缓存；⑦ **并发/重复 insert 同键记账不漂移**（同键 insert 两次后 `cur_bytes` 不双计、`order` 无悬空 seq、逐出数正确）。
2. `rwfs.rs` 集成回归（头条）：用**计数 Store 装饰器**包裹内存 store，同一内部块（`idx < tail_idx`）内多次小读 → `get_block` 只被调用一次（其余命中缓存），字节逐字节正确；写入该 inode 后缓存失效、下次读重新 `get_block`。复用 [store/tests_support.rs](fuse/src/store/tests_support.rs) 内存 store 加原子计数包装。**显式 `with_block_cache(非0)` 构造**（否则 cap=0 全 no-op，测不到逻辑）。
3. **尾块不缓存回归（杠杆 A，评审 MEDIUM-3）**：构造一个文件使某 `idx` 先是开放/尾块、读它、再 append 越过该块边界使其内容变化，重读必须见**新字节**而非缓存旧字节；并断言尾块（`idx == tail_idx`）从不进缓存（计数 Store：尾块重复读不因缓存而漏掉 get_block / 内容随 append 变化）。

## 验证

- `cargo test`（现有 188+ 测试全绿 + 新增）。
- `cargo clippy -- -D warnings`、`cargo fmt --check`。
- `cargo build --release`（确认 profile 生效、无回归）。
- 手动 sanity（可选）：用现有 `discovery-bench`/`append-bench` 对比挂载前后顺序读，观察解压调用下降。

## 提交（conventional commits，按梯队）

1. `perf(build): release profile lto=thin + codegen-units=1`
2. `perf(read): 压力感知解压块缓存，默认 128MiB，降顺序读解压放大 ~8x`（含模块、接入、CLI、测试）
