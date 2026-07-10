# zipfs 变更日志 / CHANGELOG

> **本文回答「建成了什么、何时」**——实现与实测的进展日志。设计意图见 [01-zipfs-design.md](./01-zipfs-design.md)（§1–§13 为 2026-06-27 冻结设计快照）；决策状态见 [decisions.md](./decisions.md)；下一步见 [ROADMAP.md](./ROADMAP.md)。
>
> 原为 `01-zipfs-design.md` §14，2026-07-11 抽出独立，避免设计文档一身三任。以时间倒序追加。

## 2026-07-11 · 工程骨架整改

> PoC 转正遗留的顶层骨架现代化。记录当前实际布局，供后续读者对齐「是什么/在哪里」。

- **Cargo workspace**：仓库根新增 `Cargo.toml`（`[workspace]`），统一 `Cargo.lock` 与 `target/`；`[profile.release]` 上提到根（member profile 被 workspace 忽略）。`default-members` 只含产品 + 基准 crate，日常 `cargo build/test` 跳过归档 PoC。
- **目录迁移（git mv 保历史）**：`fuse/` → `crates/zipfs/`（产品 crate，"fuse" 是 PoC 时代路线名）；4 个基准 bin（append/ratio/ldm-ratio/discovery）→ 新 crate `crates/zipfs-bench/`（依赖 zipfs 公有 lib）；`mkfixture` 留产品 crate；`microbench/` → `exp/container-backend-selection/`（归档 redb-vs-sqlite 选型 PoC）。
- **巨型文件拆分（保历史 + 测试就近 + `pub(crate)` 提升，测试数不变）**：
  - `archive.rs`（2018 行）→ `archive/{mod,format,superblock,journal,reader,writer,updater}.rs`。
  - `reconcile/orchestrator.rs`（4832 行）→ `orchestrator/{mod,preconditions,io,delete_gate,reingest,plan,quarantine,apply,manifest,prune,driver,undo,routes/{subagents,memory_passthrough}}.rs`。两 pub 入口 `reconcile`（driver）/`reconcile_undo`（undo）+ 类型集中在 `mod.rs`。
  - `rwfs.rs`/`store/{shadow,container}.rs`/`enable/lifecycle.rs` **不拆**：33–46% 为尾部测试，各自内聚单一 trait-impl/命令集，拆分只会散落共享 helper。
- 详见计划文档 [plans/workspace-restructure.md](./plans/workspace-restructure.md)。

## 2026-06-28 · 首批实现与实测

> 「实际建成 + 实测」的进展。上文之后的设计快照见 01-zipfs-design.md §1–§13（不回改）。

### 实际模块布局（与设计 §11 计划略有出入，且早于骨架整改）

```text
crates/zipfs/src/
├── main.rs            # 挂载 + `compact` 子命令；--backend {passthrough|shadow|container} --chunk-size
├── passthrough.rs     # P0 透传（B0）
├── rwfs.rs            # 读写 FUSE 层（对应 §11 计划的 fuse_fs.rs），持 TailSessions
├── archive.rs         # 布局 S 每文件分块压缩包（footer 索引 + 尾块 slot 复用）
├── core/{mod,rmw,codec,chunk,inode,wsession}.rs
└── store/{mod,shadow,container,tests_support}.rs
```

> 注：此为 2026-06-28 布局，`archive.rs`/`orchestrator.rs` 后于 2026-07-11 拆为子模块目录（见上）。

### 计划外/超出计划的关键实现

- **open-tail buffer（`core/wsession.rs::TailSessions`）**：append 路径的「未压缩开放尾块缓冲」，落在 Core 写会话（per-inode），两布局共享。把尾块重压从「每次 append」降到「每满块/每 fsync」一次。
- **fsync 抗碎片（archive 尾块 slot 原地复用）**：fsync 封部分尾块后续写**同一逻辑块**，不另起新块——块数/压缩比不随 fsync 频率劣化。配套**崩溃 fail-closed**：复用覆盖前先 `set_len+sync_data` 铲除旧 footer，杜绝崩溃后读出「新前缀+旧残尾」的 Frankenstein 块（当时 archive 无 per-block 校验，故构造性 fail-closed；per-block CRC 后于 T1 补上）。
- **BS reader 缓存（`store/shadow.rs`）**：per-inode `ArchiveReader` 缓存 + epoch 失效，修复「每次 read 重开 reader 重解析全量 footer 索引」导致的随机读病态（1.4→37 MiB/s）。
- **BV `compact` 子命令**：`zipfs compact --backend container --backing <redb>`，调 redb compact 回收 MVCC 未引用页。

### 实测结论（指针，勿在此重复数字）

- **选型**：[../exp/container-backend-selection/REPORT.md](../exp/container-backend-selection/REPORT.md) —— redb 全包 + **64KiB 块** + 批事务；256KiB 触发膨胀红线。
- **五条件大对照**：[../bench/results/20260628-1212/CONSOLIDATED.md](../bench/results/20260628-1212/CONSOLIDATED.md) —— BS 读修复后**与内核 btrfs 同档**、压缩比最高（5.42x）；BV 干净写 3.84x（compact 仅对「随机覆盖写的 MVCC 膨胀」有意义，对追加/干净写无关）；**写尾延迟是 FUSE 对内核的结构性劣势**（FUSE 三条 ms 级 vs btrfs 亚毫秒）。
- **append 优化**：[../bench/results/append-opt/REPORT.md](../bench/results/append-opt/REPORT.md) —— 尾块缓冲重压 40x↓、吞吐 BV +2.5x；fsync 抗碎片后块数/压缩比/物理体积**与 fsync 频率无关**。
- **早期对照与修复**：[../bench/results/20260627-1641/](../bench/results/20260627-1641/) 的 `FIRST-RUN.md` / `FIXES-ADDENDUM.md`。

> 后续的块大小/seal/字典/LDM/head 缓存等**空间优化的实测与决策**收敛在 [ROADMAP.md](./ROADMAP.md) T3 与 [decisions.md](./decisions.md)，不在此重复。
