# reconcile-undo + memory-symlink 短路 —— 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development 逐任务实施。步骤用 `- [ ]`。

**Goal:** 给已合并的 session-reconcile 加两件事：(1) memory 短路——underlay/`memory` 整体是 symlink 时零数据操作、仅清冗余软链；(2) `enable reconcile-undo <name>`——回退最近一次 reconcile，供重选。

**Architecture:** 在 `fuse/src/reconcile/orchestrator.rs` + `enable/mod.rs` 增量加。undo 靠 reconcile 落盘的 per-generation **manifest**（rel→逆转类）驱动；全程与 reconcile 对称（reconcile 锁 + `reconciling` marker + 陈旧门 + 逐条目守卫）。复用既有 `reingest_one_file`/`set_reconciling`/`prune_empty_underlay_dirs`/`stash_orig_preimage`/`live_entry_unchanged`/原子写。

**Spec:** [docs/09-session-reconcile.md](../09-session-reconcile.md) §6（memory 短路）+ §10（undo）。

## Global Constraints

- **零丢失铁律**：undo 只还原/新增；删除只删（a）由前镜像原子还原的 orig（b）new 增出的 orig+backing（c）经**逐字节校验 == stash underlay 快照**的 quarantine 重复副本。所有删除 NotFound 容忍。
- **陈旧门 + 逐条目覆盖守卫**：reconcile 之后的任何新 append 绝不被旧快照覆盖（§10.2/§10.3）。
- **marker 对称**：undo 半改写窗口全程 `set_reconciling(true)`，收尾清。reconcile 锁不 gate 挂载，marker 才是挂载互斥。
- **不触碰外部 memory 目标**：undo 只报告，用户 git 回退。
- 原子替换（orig `tmp→rename`、backing `reingest_one_file`）；未挂载 + shadow 前置；tempdir 子目录测试；`cargo fmt` + `clippy --all-targets -D warnings` 零警告；无 prod `unwrap`/`unsafe`；避免 `.map_or(false,..)`；conventional commits。

## File Structure

- `fuse/src/reconcile/orchestrator.rs`：加 `ReversalClass`、manifest 读写、`reconcile_undo()`、扩 underlay 清理。
- `fuse/src/enable/mod.rs`：`EnableAction::ReconcileUndo` + `cmd_reconcile_undo`。
- `fuse/src/enable/model.rs`：按需 `Paths::reconcile_manifest(name,ts)`。

---

### Task 1: memory-symlink 短路 + 冗余软链清理（§6）

**Files:** Modify `fuse/src/reconcile/orchestrator.rs`

**Interfaces:**
- 扩既有排空后清理（`prune_empty_underlay_dirs` 调用点）：除删空目录外，**删除与 backing 同名同目标的顶层冗余 symlink**（underlay/`<top>` 是 symlink 且 `backing(name,Shadow)/<top>` 也是 symlink 且 `read_link` 相等 → 删 underlay 那个）。目标不一致 → 保留 + 报告。返回的报告并入 `ReconcileReport`。

- [ ] **Step 1: 写失败测试** —— underlay 顶层放 `memory` symlink → backing 同名 symlink 同目标：清理后 `underlay_has_fallthrough(mp)==false`；目标不一致 → 保留 + 记报告；真实目录 memory（split-brain）不被此步误删（仍走 passthrough）。
- [ ] **Step 2: 确认失败**（`cd fuse && cargo test -p zipfs reconcile`）。
- [ ] **Step 3: 实现** —— 遍历 mp 顶层：symlink 且 `paths.backing(name,Shadow).join(top)` 同为 symlink 且 target 相等 → `remove_file`；不等 → push 报告。空目录 prune 逻辑保持。
- [ ] **Step 4: 通过 + fmt/clippy。**
- [ ] **Step 5: 提交** `feat(reconcile): memory 短路——清与 backing 同目标的冗余 underlay 软链（§6 symlink 路）`。

---

### Task 2: per-generation manifest（reconcile 落盘逆转类）

**Files:** Modify `fuse/src/reconcile/orchestrator.rs`（+ `enable/model.rs` 若加 `Paths::reconcile_manifest`）

**Interfaces:**
- `enum ReversalClass { RestoreOrig, RemoveOrig, RemoveQuarantine, ReportMemory, Noop }`（可 Serialize，或手写行 `rel\tclass`）。
- `apply_entry` 旁记每条目 `ReversalClass`：**判别子 = 是否落了 orig 前镜像**（`stash_orig_preimage` 是否真拷了文件——orig 预存=RestoreOrig，不存在=RemoveOrig）；keep-separate=RemoveQuarantine；passthrough 实际 relocate=ReportMemory；deferred/identical/skip/memory-deferred/memory-symlink=Noop。`EntryReport` 加 `reversal: ReversalClass`（构造点一并更新）。
- `fn write_manifest(paths,name,ts,entries:&[EntryReport]) -> io::Result<()>`：写 `reconcile_stash(name,ts)/manifest`（`ts` + 每行 `rel\tclass`）+ fsync。best-effort：`reconcile()` 末调用，失败 `log::warn`。
- `fn read_manifest(paths,name,ts) -> io::Result<Option<Vec<(String,ReversalClass)>>>`。

- [ ] **Step 1: 写失败测试** —— 跑含 union（orig 预存）+ new + keep-separate 的 reconcile（FakeMounter、confirm 全 Accept），断言 `read_manifest` 逐条逆转类正确。
- [ ] **Step 2: 确认失败。**
- [ ] **Step 3: 实现** —— `ReversalClass` + `stash_orig_preimage` 回传"是否落前镜像"布尔 + `EntryReport.reversal` + write/read_manifest + `reconcile()` 末 best-effort 写。
- [ ] **Step 4: 通过 + fmt/clippy。**
- [ ] **Step 5: 提交** `feat(reconcile): per-generation manifest 记逆转类（undo 依赖，§10.1）`。

---

### Task 3: `reconcile_undo()` 核心

**Files:** Modify `fuse/src/reconcile/orchestrator.rs`

**Interfaces:**
- Consumes: `read_manifest`、`reingest_one_file`、`set_reconciling`、`atomic_write`、`live_entry_unchanged`、排空清理、`detect_activity`、`acquire_exclusive`、`Paths::{reconcile_stash,quarantine,orig,backing,mountpoint,reconciling_marker}`。
- `struct UndoReport { ts, reversed: Vec<(String,String)>, skipped_live_changed: Vec<String>, memory_manual: Vec<String> }`
- `fn reconcile_undo(paths,name,mounter) -> io::Result<UndoReport>`：
  1. 前置：未挂载 + shadow + reconcile 锁 + 选 **ts-max 且 manifest 存在** 代次（无 manifest→拒且不清 marker；无代次→Err）；**陈旧门**（detect_activity 空闲 + 每条目 live 缺失或 == `stash/<ts>/underlay/<rel>` 快照 mtime/size/ino，任一已变→拒绝整个 undo + 报告）。
  2. `set_reconciling(true)`。
  3. 逐条目按 `ReversalClass` 逆转：RestoreOrig=前镜像原子还原+`reingest_one_file`；RemoveOrig=删 orig+backing（NotFound 容忍）；RemoveQuarantine=byte-check==快照 后删 quarantine 副本；ReportMemory=收集 memory 待手动清单；Noop=不动。
  4. 统一还原 underlay：`stash/<ts>/underlay/**` 拷回 `mp/<rel>`，**仅 live 缺失或逐字节一致才覆盖**，否则保留 live+记 `skipped_live_changed`。
  5. 落 `.undone` 标记（幂等：已 `.undone`→no-op 提示）；`set_reconciling(false)`；排空清理。

- [ ] **Step 1..N（TDD 分单元）**：
  - union+new+keep-separate 的 reconcile 后 undo → orig 还原前镜像 / 新增被删 / quarantine 被删、underlay 从快照还原、结束态可再 reconcile。
  - **陈旧门**：undo 前对某条目 append → undo 拒绝、零改动、报告该条目。
  - **逐条目守卫**：restore 步遇 live 不同则不覆盖、保留 live。
  - **marker 对称**：中途注入失败 → `reconciling` 仍在、重跑幂等。
  - 无 manifest 代次→拒且不清 marker；`.undone` 二次 undo→no-op。
  - 分语义单元提交。
- [ ] **末步提交** `feat(reconcile): reconcile_undo 核心——逆转类逐条还原 + 陈旧门 + marker 对称 + 幂等（§10）`。

---

### Task 4: CLI `enable reconcile-undo`

**Files:** Modify `fuse/src/enable/mod.rs`

**Interfaces:**
- `EnableAction::ReconcileUndo { name: String }`（clap，中文 doc）→ `cmd_reconcile_undo(&paths, name)`。
- `cmd_reconcile_undo`：**未挂载前置**（`discovery::is_mounted` 为真即 Err，复用 `reconcile_not_mounted_guard`）→ 调 `reconcile_undo(paths,name,select_mounter().as_ref())` → 打印 UndoReport（选中 ts、逐条逆转、`skipped_live_changed`、memory 待手动 git 回退清单）；拒绝时明确文案。

- [ ] **Step 1: 写失败测试** —— `cmd_reconcile_undo` 对已挂载项目 Err；clap variant 渲染含子命令；打印函数抽纯单测。
- [ ] **Step 2: 确认失败。**
- [ ] **Step 3: 实现**（照 `cmd_reconcile` 模式）。
- [ ] **Step 4: 通过 + fmt/clippy + 全 enable 套件不回归。**
- [ ] **Step 5: 提交** `feat(cli): enable reconcile-undo 子命令 + 未挂载前置（§10.5）`。

---

## Self-Review 覆盖核对

- §6 memory-symlink 短路 → Task 1。
- §10.1 manifest 逆转类 → Task 2。
- §10.2 前置/陈旧门 + §10.3 逐条逆转/marker 对称/underlay 守卫/幂等 → Task 3。
- §10.4 零丢失 → Task 3 贯穿。
- §10.5 CLI → Task 4。
