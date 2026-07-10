# zipfs 工程骨架现代化（PoC 转正遗留整改）

## Context

zipfs 起源是一次「btrfs+zstd vs FUSE 透明压缩」的横向评测实验（见 [docs/00-overview.md](../00-overview.md)）。FUSE 路线转正为整个产品后，顶层工程骨架仍带着 PoC 时代的形态，与内部已相当规整的模块化（`core/`、`store/`、`enable/`、`reconcile/`）不匹配。全面扫描发现四类骨架债务：

1. **无 Cargo workspace** —— `fuse/` 与 `microbench/` 是两个独立 crate，各有 `Cargo.lock` 和 `target/`（16G + 239M，互不共享），共享依赖解析两遍且无版本对齐保证，仓库根无法一键 build/test。
2. **`fuse/` 目录名是 PoC 化石** —— 它是当年「方案四 FUSE 路线」的目录，如今装的是整个产品（crate 名 `zipfs`），"fuse" 只是实现细节却占着顶层要名。
3. **基准散落三处并混入生产 crate** —— `bench/`（脚本+结果）、`microbench/`（独立 crate）、以及 `fuse/src/bin/*-bench.rs`（4 个基准二进制住在生产 crate 的 bin/ 里）。
4. **`microbench/` 是一次性设计闸门 PoC**（2026-06-27 redb-vs-sqlite 选型，结论早落地），却与生产代码平起平坐。

外加一项代码级债务：`reconcile/orchestrator.rs`（2232 行生产代码，真正过大）与 `archive.rs`（1082 行生产代码，banner 分明）值得拆分。其余大文件（`rwfs.rs`/`shadow.rs`/`container.rs`/`lifecycle.rs`）经核实 33–46% 是尾部测试模块、各自内聚于单一 type/trait-impl，**不拆**。

**目标结果**：仓库根一键 `cargo build` / `cargo test`（走 `default-members`，含产品 crate + 基准 crate）；统一 `Cargo.lock` 与 `target/`；产品 crate 落在语义准确的 `crates/zipfs/`；基准与 PoC 各归其位；两个巨型文件拆成内聚子模块。全程 `git mv` 保历史，分两阶段提交与审查。

> 归档 PoC（exp/）仍列为 workspace member，故 `cargo build/test --workspace`（显式全量）会额外编译它（含 `rusqlite { bundled }` 的 C 构建，首次较慢、之后缓存）。这是刻意选择：让 PoC 随 workspace 长期保持可编译（`keep-poc-in-project`），而日常一键用 `default-members` 跳过它。若将来 PoC 腐化拖累 `--workspace`，再降级为 `[workspace] exclude`（代价：根目录 `-p zipfs-microbench` 失效）。

**范围决策（已与用户确认）**：拆分范围 = orchestrator + archive；节奏 = 两阶段；历史 = git mv 保留。

## 目标目录结构

```
zipfs/
  Cargo.toml                        # 新增：[workspace] 根，统一 profile / lock / target
  Cargo.lock                        # 新增：唯一 lockfile（删除子 crate 的旧 lock）
  crates/
    zipfs/                          # ← 原 fuse/：产品 crate（lib + zipfs bin + mkfixture bin）
    zipfs-bench/                    # ← 新建：4 个基准 bin，依赖 zipfs lib
  exp/
    container-backend-selection/    # ← 原 microbench/：归档 PoC（default-members 排除）
  bench/                            # 不动（脚本/结果/数据集），仅修内部路径引用
  docs/                            # 不动，仅修 fuse/ → crates/zipfs/ 路径引用
```

## 阶段一：workspace + 目录迁移 + 路径联动（纯机械，低风险）

### 1.1 建立 workspace 根

新建 `Cargo.toml`（仓库根）：
```toml
[workspace]
resolver = "2"
members = ["crates/zipfs", "crates/zipfs-bench", "exp/container-backend-selection"]
default-members = ["crates/zipfs", "crates/zipfs-bench"]  # 默认 build 跳过归档 PoC

[profile.release]                   # 从两个子 crate 上提（workspace 只认根 profile）
lto = "thin"
codegen-units = 1
```
- `crates/zipfs/Cargo.toml`：删除其 `[profile.release]`（原 `fuse/Cargo.toml:106-108`）。
- `exp/container-backend-selection/Cargo.toml`：删除其 `[profile.release]`（opt-level=3 已是 release 默认；lto 交给根）。

### 1.2 迁移目录（git mv 保历史）

- `git mv fuse crates/zipfs`
- `git mv microbench exp/container-backend-selection`
- 新建 `crates/zipfs-bench/`（crate 名 `zipfs-bench`），把 4 个基准 bin 迁入：
  `git mv crates/zipfs/src/bin/{append-bench,ratio-bench,ldm-ratio,discovery-bench}.rs crates/zipfs-bench/src/bin/`
  —— **`mkfixture.rs` 留在产品 crate**（它是离线 fixture 生成工具，非基准）。
  - `crates/zipfs-bench/Cargo.toml` 依赖必须**完整声明 bin 直接 `use` 的 crate**（否则编译失败）：
    `zipfs = { path = "../zipfs" }`、`clap = { version = "4", features = ["derive"] }`（四个 bin 均 `use clap::Parser/ValueEnum`）、`libc = "0.2"`（`discovery-bench.rs:91-95` 直接用 `libc::posix_fadvise`）。传递依赖不可直接 `use`。
- 从 `crates/zipfs/Cargo.toml` 删除 4 个 `[[bin]]` 基准块（原 `fuse/Cargo.toml:27-51`），保留 `zipfs` 与 `mkfixture`；在 `crates/zipfs-bench/Cargo.toml` 声明对应 `[[bin]]`。
  - 源码零改动前提：4 个基准 bin 位于 `src/bin/`，只触及 `zipfs::` 公有 API，故 **bin 源文件无需改**；改动仅在上面的 Cargo.toml 依赖声明（下方验证兜底）。
- 删冗余：`git rm crates/zipfs/Cargo.lock exp/container-backend-selection/Cargo.lock`（workspace 生成唯一根 lock）；删两子 crate 的 `.gitignore`（内容仅 `/target`，根 `.gitignore:4-5` 的 `target/`+`**/target/` 已覆盖）。

### 1.3 路径联动修复（来源：两份 Explore 勘察报告，穷举清单以其为准）

**关键简化**：workspace 把 `fuse/target/` 收敛为根 `target/`，故所有 `.../fuse/target/release/X` 统一改为 `$REPO_DIR/target/release/X`（更简单、更统一）。

- **bench 脚本（~15 处，最高风险）**：`bench/scripts/{zipfs-mount,zipfs-cutover,crash-test,crash-test-dm,crash-test-dm-logwrites,crash-test-container,mount-b0,mount-bs,mount-bv,ratio-matrix,ldm-ratio-matrix}.sh`。三种派生模式（`$REPO_DIR/fuse/...`、`FUSE_DIR=../../fuse`）统一改为指向根 `target/`；`cd fuse && cargo build` 提示改为 `cargo build --release -p zipfs`（基准 bin 改 `-p zipfs-bench --bin ratio-bench` 等）。
- **静态 systemd 单元** `bench/scripts/zipfs.service:15`：`ExecStart` 路径 `.../fuse/target/release/zipfs` → `.../target/release/zipfs`。
  （注：代码生成的 systemd 单元用 `current_exe()`，不受影响 —— 已核实。）
- **README**：根 `README.md`（目录树 + `cd fuse` + 11 行命令示例）、`crates/zipfs/README.md`（`../docs/` 相对链接因多嵌一层 → `../../docs/`）、`bench/README.md`（构建提示与产物路径）。
- **docs（仅活文档）**：`docs/02-layered-chunking.md:22` 的真实 MD 链接 `../fuse/src/...` → `../crates/zipfs/src/...`；`docs/{00-overview,01-zipfs-design,05-fault-injection-testing,07-hangfree-umount}.md` 的 `fuse/src/...` 散文引用与 `microbench/REPORT.md` 引用批量改写。`.claude/skills/zipfs-reconcile-ops/SKILL.md:27` 的 `cd .../fuse`。
  - **不改冻结历史**：`docs/archive/*.md`（已归档评审快照）与**已完成的** `docs/plans/*kickoff*.md`、`bench/results/**/REPORT.md` 里 `cd fuse && ...`、`git add fuse/src/...` 是当时真实执行过的命令，按项目文档规则属冻结快照，重写会篡改历史且对命令块做无差别替换有损坏风险。保持原样；如需可加一行「目录已迁移至 crates/zipfs/」批注，不动命令。
- **微移引用**：`README.md:21`、`docs/01-zipfs-design.md:144,286`（`microbench/REPORT.md` → `exp/container-backend-selection/REPORT.md`）；`exp/.../REPORT.md` 内 `../docs/` 链接因多嵌一层 → `../../docs/`。
- **本地配置** `.claude/settings.local.json:11-17`：按旧路径的许可 glob（`Read(.../fuse/**)`、`Bash(ls -1 fuse/)`、`Bash(./fuse/target/debug/zipfs *)`）随迁移改为 `crates/zipfs`（否则仅多几次授权提示，无功能损坏）。
- `.gitignore:1` 注释「子目录另有更细规则：fuse/、microbench/」更新为新路径。

> **合并 lockfile 提示（M1）**：删两个子 lock、生成单一根 lock 会对共享依赖重解析（`redb "4"`↔`4.1.0`、`tempfile "3"`↔`3.27.0`、`clap` 等，均 semver 兼容，构建验证兜底）。留意 fuse 原锁定的传递版本可能小幅漂移。

### 阶段一验证

1. `cargo build --workspace` 成功（三 crate 全绿；确认根 `target/` 生成、单一 `Cargo.lock`）。
2. `cargo test -p zipfs`（原 fuse 全部单测+集成测试）通过，数量与迁移前一致。
3. `cargo build -p zipfs-bench` 成功（4 基准 bin 编译）；`cargo build -p zipfs-microbench` 成功。
4. 抽查一个 bench 脚本 dry-run（如 `BIN=target/release/zipfs bench/scripts/mount-bs.sh` 的路径解析段）不再引用 `fuse/`。
5. 活文档路径清零（I2，pattern 须覆盖 `fuse/src` 而非仅 `fuse/target`）：
   `grep -rnE 'fuse/(src|target|README)|cd fuse|\.\./fuse|\bmicrobench\b' README.md docs bench .claude --include='*.md' --include='*.sh' --include='*.service'`
   —— 结果应仅剩 `docs/archive/` 与已完成 kickoff 里刻意保留的历史命令（逐条确认是冻结快照，非漏改活文档）。
6. `git log --follow crates/zipfs/src/main.rs` 显示历史连续（rename 保留）。

**阶段一提交**（语义单元分开）：`chore: 引入 Cargo workspace + 上提 profile`；`refactor: fuse/ → crates/zipfs/（git mv 保历史）`；`refactor: 基准 bin 拆出 crates/zipfs-bench`；`refactor: microbench → exp/container-backend-selection`；`docs: 路径引用随目录迁移更新`。

## 阶段二：拆分 orchestrator.rs + archive.rs（纯代码，逐步验证）

> **核心手法（C2 —— 拆分不是纯 move）**：单文件内成片的**私有** `fn`/`const` 被跨簇引用，一旦分到不同子模块就编译不过。故每拆一簇，必须把被跨模块引用的私有项提升为 `pub(crate)`（或在 `mod.rs` 集中 `pub(crate) use`）。「外部调用点零改动」只对已 `pub` 的入口成立，**不代表 crate 内部零改动**。待提升清单（勘察实证）：
> - orchestrator：`fsync_path`/`fsync_dir`/`fsync_dir_chain`（`:234/239/247`）、`is_safe_rel`（`:1386`）、`reversal_for_preimage`（`:487`）、`is_synthetic_rel`（`:497`）、`oversize_rec`..`identical_rec`（`:584-629`）、`reingest_one_file`（apply+undo 两处用）、以及各簇间互调的私有助手。（`atomic_write :268` 已 pub。）
> - archive：`put_u32/put_u64/get_u32/get_u64`（`:255-271`）、`read_exact_at`（`:334`）、`read_sb_slot`/`load_active`/`write_superblock_slot`/`footer_from_sb`/`corrupt`，及私有常量 `HEADER_LEN`/`INDEX_ENTRY_LEN`/`SB_CRC_OFFSET`。
> 逐簇拆、逐簇 `cargo test -p zipfs` 绿，让编译器暴露遗漏的可见性。

### 2.1 `reconcile/orchestrator.rs` → `reconcile/orchestrator/`（2232 生产行 + 2598 测试行）

按 Explore 勘察的函数簇边界（行号见勘察报告）拆为：
```
orchestrator/
  mod.rs                # 类型定义 + pub 重导出（保 reconcile/mod.rs 的 `pub mod orchestrator;` 不变）
  preconditions.rs      # check_preconditions / snapshot_underlay / walk_snapshot
  io.rs                 # fsync_path/dir/dir_chain, atomic_write（pub 复用，先拆最稳）
  delete_gate.rs        # SupersetMode / durable_superset_ok / delete_permitted（零丢删除许可）
  reingest.rs           # reingest_one_file / set_reconciling
  plan.rs               # EntryPlan / ReversalClass / plan_entries / *_rec 建议构造
  quarantine.rs         # quarantine_reuse
  routes/subagents.rs   # is_subagents_entry / reconcile_subagents_dir
  routes/memory_passthrough.rs  # passthrough_restore_memory / place_memory_files / ...
  apply.rs              # apply_entry / stash_orig_preimage / finish_delete（每 entry 提交核心）
  manifest.rs           # write_manifest / read_manifest
  prune.rs              # prune_empty_underlay_dirs / prune_redundant_symlinks
  driver.rs             # pub fn reconcile（主驱动，编排各阶段）+ ReconcileOptions/Confirm
  undo.rs               # pub fn reconcile_undo + 快照还原簇
```
- 两个 pub 入口 `reconcile`（driver.rs）、`reconcile_undo`（undo.rs）与 `delete_permitted` 保持公有并从 `mod.rs` 重导出，外部调用点零改动。
- 测试（2598 行单一 `#[cfg(test)] mod tests`）：**优先按被测簇就近拆入各子模块的 `#[cfg(test)]`**（此为首选，非兜底）。原因（M4）：测试 `use super::*` 直接戳私有内部（`is_safe_rel` 等），若整体落 `orchestrator/tests.rs`，其 `use super::*` 只见 `mod.rs` 作用域、**看不到**已迁入兄弟子模块的私有项——除非配合上面的 `pub(crate)` 提升。就近放测试可避开此问题。**测试总数不得变**。
- 先拆 pub 复用簇（`io.rs`/`delete_gate.rs`），每拆一簇即 `cargo test -p zipfs` 绿再继续。

### 2.2 `archive.rs` → `archive/`（1082 生产行 + 934 测试行）

按已有 banner 边界拆：
```
archive/
  mod.rs         # ChunkEntry/HeadCache/Footer 类型 + pub 重导出（lib.rs 的 `pub mod archive;` 不变）
  format.rs      # crc32 + put/get_u32/u64 整数编解码
  superblock.rs  # SuperBlock / serialize/parse_superblock / pick_active
  journal.rs     # serialize/replay_journal_record
  reader.rs      # ArchiveReader + load/validate/parse_index 助手
  writer.rs      # ArchiveWriter
  updater.rs     # ArchiveUpdater（flip_active/commit_journal/set_block/commit）
```
- 测试同 2.1 策略（就近拆入各子模块的 `#[cfg(test)]`，避开 `use super::*` 看不到兄弟模块私有项的问题）。`ArchiveReader/Writer/Updater` 及格式常量保持公有、`mod.rs` 重导出，`store/{shadow,container}.rs` 等调用点零改动。

### 阶段二验证

1. 每拆一个子模块后 `cargo test -p zipfs` 全绿，测试数量与阶段一末尾**逐字一致**（`cargo test -p zipfs 2>&1 | grep 'test result'` 比对）。
2. `cargo clippy --workspace --all-targets -- -D warnings` 无新增告警。
3. 历史保留如实核对（I1）：`git mv orchestrator.rs orchestrator/mod.rs` 只让 **mod.rs** 继承 `--follow`；从 mod.rs 切出的新文件（`driver.rs`/`apply.rs`…）是新对象，`--follow` 会在拆分那次提交断掉。故：对 `mod.rs` 断言 `git log --follow` 连续；碎出文件靠 `git blame -C -C -C`（跨文件追同一行来源）归因，不强求 `--follow`。
4. 公有 API 未变：`cargo public-api`（若可用）或 grep 确认 `reconcile`/`reconcile_undo`/`ArchiveReader` 等导出签名不变。

**阶段二提交**：`refactor: 拆分 orchestrator.rs 为 orchestrator/ 子模块`；`refactor: 拆分 archive.rs 为 archive/ 子模块`。保历史：先 `git mv orchestrator.rs orchestrator/mod.rs` 单独提交（mod.rs 得 `--follow`），再从 mod.rs 切出各子模块提交（碎出文件不继承 `--follow`，靠 `git blame -C -C -C` 归因，如 I1 所述——不追求 mod.rs 极薄与 `--follow` 兼得的矛盾目标）。

## 不做 / 已记录

- `rwfs.rs`/`store/shadow.rs`/`store/container.rs`/`enable/lifecycle.rs` **不拆**：33–46% 为尾部测试、各自内聚单一 backend/前端/命令集，拆分只会散落共享 helper。已在勘察中留证，未来若某文件生产行显著增长再议。
- docs 三分（ARCH/DESIGN/ADR）与当前扁平编号的偏离：**本次不动**，属独立文档整理议题，记入待办。

## 关键复用点（勿重造）

- systemd 单元生成 `crates/zipfs/src/enable/autostart.rs` 用 `std::env::current_exe()` —— 迁移后自解析，无需改。
- 根 `.cargo/config.toml` 的 `[env] TMPDIR` 对 workspace 全体生效 —— 无需改。
- 根 `.gitignore:4-5` 的 `target/`+`**/target/` 已覆盖 workspace 单一 target —— 子 crate ignore 可删。

## 对抗性评审采纳记录

一轮 subagent 对抗审查，结论已并入上文：
- **已采纳并修订**：C1（zipfs-bench 补 clap/libc 依赖）、C2（拆分需 `pub(crate)` 提升，附待提升清单）、I1（历史 `--follow` 只对 mod.rs 成立，验证如实改写）、I2（活文档清零 grep 覆盖 `fuse/src`）、I3（不改冻结 archive/ 与已完成 kickoff 历史命令）、I4（exp/ 作 member 的 trade-off 显式化，一键用 default-members）、M1（合并 lock 版本重解析提示）、M2（settings.local.json 旧路径许可）、M4（就近放测试规避私有可见性）。
- **未采纳，记录理由**：
  - M3（rwfs 992 生产行逼近 archive、体量上属边界）—— 仍不拆。承重论据是内聚性（单一 `ZipfsRw` 的 `Filesystem` trait impl，勘察确认非多关注点混装），非体量；已留「生产行显著增长再议」触发器，符合项目价值观。
  - M5（`zipfs-microbench` 包名不再暗示 `exp/` 位置）—— 纯外观，保持包名不变以减少 churn，验证一致用 `-p zipfs-microbench`。
