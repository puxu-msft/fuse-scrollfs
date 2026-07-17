# reconcile-undo + memory-symlink 短路 —— 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development 逐任务实施。步骤用 `- [ ]`。

**Goal:** 给已合并的 session-reconcile 加两件事：(1) memory 短路——underlay/`memory` 整体是 symlink 时零数据操作、仅清冗余软链；(2) `enable reconcile-undo <name>`——回退最近一次 reconcile，供重选。

**Architecture:** 在 `fuse/src/reconcile/orchestrator.rs` + `enable/mod.rs` 增量加。undo 靠 reconcile 落盘的 per-generation **manifest**（rel→逆转类）驱动；全程与 reconcile 对称（reconcile 锁 + `reconciling` marker + 陈旧门 + 逐条目守卫）。复用既有 `reingest_one_file`/`set_reconciling`/`prune_empty_underlay_dirs`/`stash_orig_preimage`/`live_entry_unchanged`/原子写。

**Spec:** [docs/09-session-reconcile.md](../09-session-reconcile.md) §6（memory 短路）+ §10（undo）。

## Global Constraints

- **零丢失铁律**：undo 只还原/新增；删除只删（a）由前镜像原子还原的 orig（b）new 增出的 orig+backing（c）经**逐字节校验 == stash underlay 快照**的 quarantine 重复副本。所有删除 NotFound 容忍。
- **陈旧门 + 逐条目覆盖守卫**：reconcile 之后的任何新 append 绝不被旧快照覆盖（§10.2/§10.3）。
- **marker 对称 + 挂载互斥**：undo 半改写窗口全程 `set_reconciling(true)`，收尾清。**注意（评审 C-plan1）**：reconcile 锁不 gate 挂载，`reconciling` marker 当前也只挡 lifecycle 维护（`bail_if_reconciling` 5 处），**不挡 systemd 自启入口** `resolve_managed_spec`（只调 `ensure_underlay_empty`）。故必须让 systemd 自启入口**也查 marker**（Task 2），否则 undo 先改 backing、后还原 underlay 的顺序会把后端改写暴露在"underlay 已空"窗口 → 自启可挂到半还原 backing。此修同时收窄 reconcile 自身的收尾空窗。
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
- 在既有排空清理调用点（`prune_empty_underlay_dirs(mp)` 之侧，约 orchestrator.rs:1396）**并列加**一个带 `paths+name` 的清理步 `fn prune_redundant_symlinks(paths,name,mp) -> io::Result<Vec<String>>`（**不改** `prune_empty_underlay_dirs` 的 `mp`-only 签名，评审 M1）：遍历 mp 顶层，**删除与 backing 同名同目标的顶层冗余 symlink**（underlay/`<top>` 是 symlink 且 `paths.backing(name,Backend::Shadow).join(top)` 也是 symlink 且 `read_link` 相等 → 删 underlay 那个）。目标不一致 → 保留 + 报告。返回报告并入 `ReconcileReport`。

- [ ] **Step 1: 写失败测试** —— underlay 顶层放 `memory` symlink → backing 同名 symlink 同目标：清理后 `underlay_has_fallthrough(mp)==false`；目标不一致 → 保留 + 记报告；真实目录 memory（split-brain）不被此步误删（仍走 passthrough）。
- [ ] **Step 2: 确认失败**（`cd fuse && cargo test -p scrollz reconcile`）。
- [ ] **Step 3: 实现** —— 遍历 mp 顶层：symlink 且 `paths.backing(name,Shadow).join(top)` 同为 symlink 且 target 相等 → `remove_file`；不等 → push 报告。空目录 prune 逻辑保持。
- [ ] **Step 4: 通过 + fmt/clippy。**
- [ ] **Step 5: 提交** `feat(reconcile): memory 短路——清与 backing 同目标的冗余 underlay 软链（§6 symlink 路）`。

---

### Task 2: systemd 自启入口认 `reconciling` marker（C-plan1，挂载互斥根因）

**Files:** Modify `fuse/src/enable/systemd.rs`（`resolve_managed_spec`）；可能 `fuse/src/enable/lifecycle.rs`（`remount` 已经 `bail_if_reconciling`？核实）

**背景:** `resolve_managed_spec`（systemd.rs:139 附近）是 systemd 自启 `mount-managed` 的唯一挂载入口，当前只调 `ensure_underlay_empty`、**不查 `reconciling` marker**。reconcile/undo 的后端半改写窗口若恰逢 underlay 空（undo 尤甚），自启会挂到半改写 backing 上。

**Interfaces:**
- 在 `resolve_managed_spec` 建 spec 挂载**前**（或 `SystemdMounter::spawn`/`run_mount_managed` 挂载前的单点），加 `reconciling_marker(name).exists()` 检查 → 存在即 Err（"<name> 正在 reconcile/undo，拒绝自动挂载"）+ 落 NEEDS-RECONCILE sentinel（复用 Task12 sentinel 机制），使自启 fail-closed 且不 crash-loop（ExecCondition/退出码语义同 §5.4）。
- 复用 `crate::enable::discovery`/`Paths::reconciling_marker` 与 guard 现有下沉点，避免漂移。
- **两个 chokepoint（评审 Minor）**：`ensure_underlay_empty` 有两处调用——`systemd.rs:139`（`resolve_managed_spec`，**开机自启无人值守路径 = 真洞**，本 Task 必加 marker 检查）与 `daemon.rs:75`（`SystemdMounter::spawn`，`systemctl start` 编排路径，已由上游 `bail_if_reconciling` + reconcile 锁串行化 fence）。**求稳起见两点都加 marker 检查**（marker 路径由 name 派生、两处均有 spec/name 上下文），并注明 spawn 路径本已被上游 bail 覆盖。

- [ ] **Step 1: 写失败测试** —— 置 `reconciling` marker 后，`resolve_managed_spec`（或其挂载前置）对该项目返回 Err/拒挂；marker 清后放行。若纯逻辑难测，抽 `fn managed_mount_blocked(paths,name)->bool` 单测（marker 存在 || underlay 非空）。
- [ ] **Step 2: 确认失败。**
- [ ] **Step 3: 实现** —— marker 检查加在自启挂载单点前；与 `ensure_underlay_empty` 并列 fail-closed。
- [ ] **Step 4: 通过 + fmt/clippy + enable/systemd 套件不回归。**
- [ ] **Step 5: 提交** `fix(autostart): systemd 自启挂载入口认 reconciling marker，堵半改写窗口（C-plan1）`。

---

### Task 3: per-generation manifest（reconcile 落盘逆转类）

**Files:** Modify `fuse/src/reconcile/orchestrator.rs`（+ `enable/model.rs` 若加 `Paths::reconcile_manifest`）

**Interfaces:**
- `enum ReversalClass { RestoreOrig, RemoveOrig, RemoveQuarantine, ReportMemory, Noop }`（可 Serialize，或手写行 `rel\tclass`）。
- **接线（评审 I-plan1，比"构造点一并更新"更细）**：核心 EntryReport 由**共享构造器 `finish_delete`（orchestrator.rs:1211）**产出（union/new/identical/keep-separate/subagents 共用），它只收 `kind:&str`、拿不到逆转类。故给 `finish_delete` + `reconcile_subagents_dir` **加 `reversal: ReversalClass` 入参**，由 `apply_entry` 据 `stash_orig_preimage` 回传的 **has_preimage 布尔**传 `RestoreOrig`（有前镜像）/`RemoveOrig`（无）；passthrough 分支（1059）传 `ReportMemory`（实际 relocate）/`Noop`（deferred）；deferred（1140）传 `Noop`。`EntryReport` 加 `reversal` 字段，9 处构造点更新。
- **`stash_orig_preimage` 改回传 `io::Result<bool>`**（是否真拷了前镜像 = orig 预存）：函数已有 `!orig_file.exists()` 早返回分支，布尔即其取反。3 处调用点（672 subagents / 1070 Union / 1088 New）更新。
- `fn write_manifest(paths,name,ts,entries:&[EntryReport]) -> io::Result<()>`：写 `reconcile_stash(name,ts)/manifest`（首行 `ts`，其后每行真实 `rel\tclass`）+ fsync。**必须过滤合成条目**（`<prune>`/`<meta>`/`<rebuild>`/`unmatched-snapshot` 等非真实 rel，评审 I-plan1），只写真实条目。best-effort：**在条目循环后、`set_reconciling(false)` 前**写（评审 M3），失败 `log::warn`（该 run 不可 undo）。
- `fn read_manifest(paths,name,ts) -> io::Result<Option<Vec<(String,ReversalClass)>>>`。

- [ ] **Step 1: 写失败测试** —— 跑含 union（orig 预存）+ new + keep-separate 的 reconcile（FakeMounter、confirm 全 Accept），断言 `read_manifest` 逐条逆转类正确。
- [ ] **Step 2: 确认失败。**
- [ ] **Step 3: 实现** —— `ReversalClass` + `stash_orig_preimage` 回传"是否落前镜像"布尔 + `EntryReport.reversal` + write/read_manifest + `reconcile()` 末 best-effort 写。
- [ ] **Step 4: 通过 + fmt/clippy。**
- [ ] **Step 5: 提交** `feat(reconcile): per-generation manifest 记逆转类（undo 依赖，§10.1）`。

---

### Task 4: `reconcile_undo()` 核心

**Files:** Modify `fuse/src/reconcile/orchestrator.rs`

**Interfaces:**
- Consumes: `read_manifest`、`reingest_one_file`、`set_reconciling`、`atomic_write`、`live_entry_unchanged`、`prune_empty_underlay_dirs`、`detect_activity`、`acquire_exclusive`、`Paths::{reconcile_stash,quarantine,orig,backing,mountpoint,reconciling_marker}`。
- `struct UndoReport { ts: String, reversed: Vec<(String,String)>, skipped_live_changed: Vec<String>, memory_manual: Vec<String> }`
- `fn reconcile_undo(paths,name) -> io::Result<UndoReport>`（**去掉 mounter 参数**，评审 M4：undo 不重挂、未挂载判定走 `discovery::is_mounted`）：
  1. 前置：未挂载 + shadow + reconcile 锁 + 选 **ts-max 且 manifest 存在** 代次（无 manifest→拒且不清 marker；无代次→Err）；**陈旧门**（detect_activity 空闲 + 每条目 live 缺失或 == `stash/<ts>/underlay/<rel>` 快照 mtime/size/ino，任一已变→拒绝整个 undo + 报告）。
  2. `set_reconciling(true)`。
  3. 逐条目按 `ReversalClass` 逆转：
     - `RestoreOrig`=**先 fail-closed 校验 `stash/<ts>/orig/<rel>` 前镜像存在**（评审 I-plan2，缺则中止/报错，绝不静默半还原）→ 前镜像原子还原 `orig/<rel>`（`tmp→rename`）→ `reingest_one_file`。
     - `RemoveOrig`=删 orig+backing（NotFound 容忍）；空父目录 prune 是 **orig/backing 树**（非 underlay，评审 M2：`prune_empty_underlay_dirs` 只清 mp，此处另用小 helper 或省略——空目录无害）。
     - `RemoveQuarantine`=byte-check（== `stash/<ts>/underlay/<rel>` 快照）后删 quarantine 副本。
     - `ReportMemory`=收集 memory 待手动清单；`Noop`=不动。
  4. 统一还原 underlay：`stash/<ts>/underlay/**` 拷回 `mp/<rel>`，**仅 live 缺失或逐字节一致才覆盖**，否则保留 live+记 `skipped_live_changed`。
  5. 落 `.undone` 标记（幂等：已 `.undone`→no-op 提示）；`set_reconciling(false)`；`prune_empty_underlay_dirs(mp)`（清还原过程可能留的空目录，若有）。

> 注（C-plan1）：Task 2 已让 systemd 自启挂载入口认 marker，故 step2–5 全程 systemd 自启被 marker 挡住；step3 后端改写期间即便 underlay 尚空也不会被自启挂上。

- [ ] **Step 1..N（TDD 分单元）**：
  - union+new+keep-separate 的 reconcile 后 undo → orig 还原前镜像 / 新增被删 / quarantine 被删、underlay 从快照还原、结束态可再 reconcile。
  - **陈旧门**：undo 前对某条目 append → undo 拒绝、零改动、报告该条目。
  - **逐条目守卫**：restore 步遇 live 不同则不覆盖、保留 live。
  - **marker 对称**：中途注入失败 → `reconciling` 仍在、重跑幂等。
  - 无 manifest 代次→拒且不清 marker；`.undone` 二次 undo→no-op。
  - 分语义单元提交。
- [ ] **末步提交** `feat(reconcile): reconcile_undo 核心——逆转类逐条还原 + 陈旧门 + marker 对称 + 幂等（§10）`。

---

### Task 5: CLI `enable reconcile-undo`

**Files:** Modify `fuse/src/enable/mod.rs`

**Interfaces:**
- `EnableAction::ReconcileUndo { name: String }`（clap，中文 doc）→ `cmd_reconcile_undo(&paths, name)`。
- `cmd_reconcile_undo`：**未挂载前置**（`discovery::is_mounted` 为真即 Err，复用 `reconcile_not_mounted_guard`）→ 调 `reconcile_undo(paths, name)`（无 mounter 参）→ 打印 UndoReport（选中 ts、逐条逆转、`skipped_live_changed`、memory 待手动 git 回退清单）；拒绝时明确文案。

- [ ] **Step 1: 写失败测试** —— `cmd_reconcile_undo` 对已挂载项目 Err；clap variant 渲染含子命令；打印函数抽纯单测。
- [ ] **Step 2: 确认失败。**
- [ ] **Step 3: 实现**（照 `cmd_reconcile` 模式）。
- [ ] **Step 4: 通过 + fmt/clippy + 全 enable 套件不回归。**
- [ ] **Step 5: 提交** `feat(cli): enable reconcile-undo 子命令 + 未挂载前置（§10.5）`。

---

## Self-Review 覆盖核对

- §6 memory-symlink 短路 → Task 1。
- C-plan1 systemd 自启认 marker（挂载互斥根因）→ Task 2。
- §10.1 manifest 逆转类（finish_delete 接线 + write_manifest 过滤合成条目）→ Task 3。
- §10.2 前置/陈旧门 + §10.3 逐条逆转（RestoreOrig fail-closed）/marker 对称/underlay 守卫/幂等 → Task 4。
- §10.4 零丢失 → Task 4 贯穿。
- §10.5 CLI → Task 5。
