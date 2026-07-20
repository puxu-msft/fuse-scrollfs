# reconcile 子系统专项审查 / Code Review — 2026-07-06 (No.1)（已归档 / ARCHIVED）

> 状态：**已核验闭环、归档**。CRITICAL + HIGH + 2 Important + 1 wedge 已同会话 TDD 修复（见文末「修复状态」，370 lib 测试绿）；余 W2（需可信重挂路径）+ W3/W4（Low）为已记录的后续项，本报告作历史闭环记录归档。
> 对象：session-reconcile 子系统（`fuse/src/reconcile/*` + `enable/{autostart,hang_free,force_umount,guard}.rs` 相关接线，~6400 行）。设计见 [09-session-reconcile.md](../09-session-reconcile.md) + 计划 [session-reconcile.md](../plan/session-reconcile.md) / [reconcile-undo.md](../plan/reconcile-undo.md)。
> 方法：4 路并行 subagent（删除门 / 合并核 / 崩溃安全 / 路径并发），判据轴统一为**零数据丢失铁律 + 绝不 wedge**（非 ROI）；主代理对每条结论**独立核对当前源码**（标注"已核实"）。
> 总评：**零数据丢失基本成立**（删除门对 incoming 侧强壮、路径/symlink 全 fail-closed、undo 双保险陈旧门），但有 **1 CRITICAL + 1 HIGH + 2 Important + 2 Medium(wedge)** 需收口。

## 发现清单（按严重度）

### CRITICAL

**R-C1 · 删除门单边：只证 `incoming ⊆ merged`，从不证 `base ⊆ merged`，而金源 orig 在门前已被覆盖**
`orchestrator.rs:1145`（Union `atomic_write(orig,merged)`）→ `:1147 finish_delete` → `:310-313 durable_superset_ok(LinesSuperset)` 只校验 `snap_entry.bytes(=incoming)` 每行 ∈ merged。**已核实**（读 apply_entry + durable_superset_ok）。设计 §5.3 步4 明文要求双向超集，实现只做 incoming 半边。
- 丢数据情景：`session_merge` 若丢一条 **base-only** 记录（见 R-I1 空 uuid 折叠即可触发），orig 被静默写成缺行版、backing 重灌、underlay 被删，删除门查不出（只查 incoming）。base 仅靠 `stash_orig_preimage` 前镜像 + `reconcile-undo` 可人工找回，**live archive 已静默失真**；崩溃续跑另起代次时前镜像可能不可 undo。
- 同源覆盖点：`reconcile_subagents_dir:745`。
- 修复：Union/subagents 落 orig 后、finish_delete 前，加 `durable_superset_ok(orig_merged, base_preimage_bytes, LinesSuperset)`（base 侧）；不过则中止、还原 orig 前镜像、保两份。补齐 §5.3 步4 的双向超集。

### HIGH

**R-lock · reconcile 改写 backing 全程不取 backing `.scrollz.lock`，与 compact/seal TOCTOU**
`acquire_backing` 在 orchestrator 出现 **0 次**（**已核实** grep）；只取独立 `reconcile_lock`（`model.rs:119` 注释自承与 backing 锁"是两把锁"）。`reingest_one_file:344`、undo 反做均无锁 rewrite `backing/<rel>`。
- 情景：STOPPED+NEEDS-RECONCILE（backing 锁空闲）下并发 `enable compact` + `enable reconcile`：compact 取 backing 锁重编码，reconcile 走另一把锁无锁 rename 覆盖同一 archive → 交错写损坏。marker 只挡"reconcile 先置标记"半边，反向 TOCTOU 无锁拦截。对活守护的同类缺口被"非空 underlay + mount 守卫"补偿，但 compact/seal 不查 underlay，无补偿。
- 修复：reconcile 的 backing 变更区（reingest_one_file / undo 反做 / finalize 前）取 `acquire_backing_retry`，与 compact/seal/守护共用一把 backing 锁硬互锁；marker 保留作 mount 门禁/UX。

### Important

**R-I1 · 合并核对空串/碰撞 uuid 折叠 distinct 记录**
`record.rs:16-17` `record_uuid` 对 `"uuid":""` 返回 `Some("")` → `classify_record` 映射 `Transcript{uuid:""}` → 所有空串 uuid 共键 `""` → by_uuid 只留最长者，其余静默丢（**已核实**）。真实 Claude 数据罕见空 uuid，但退化/fork 拷贝可触发；且与 R-C1 组合成静默损坏路径。修复：空串/缺失/非字符串 uuid 一律降级 `NoUuid`（整行去重全保 distinct）。

**R-I2 · 同 uuid 内容分叉时按字节长度取一，落败字节不进输出也不进报告**
`merge.rs:115-130` 判据纯 `len()`；`conflicts` 只 push uuid 字符串不带落败行。真实数据几乎不触发（§3 断言 uuid 记录不可变、分叉仅来自崩溃截断而截断行不解析→进不了 by_uuid），但严格零丢失应收口：`conflicts` 携落败完整行，或同 uuid 分叉走 keep-both。

### Medium（wedge，非数据丢失）

**W1 · 前向 reconcile「underlay 抽干→manifest 落盘」窗口崩溃 → marker 永久 wedge、无自恢复**
`check_preconditions:82` underlay 空即 `Err("无需 reconcile")`（**已核实**），不看 marker；此窗口崩溃后 marker 留存但 manifest 不存在 → 重跑 reconcile 早退、undo 因无 manifest 拒绝（`:1850`）→ `bail_if_reconciling` 拦死 remount/compact/seal/autostart，需人工 `rm .reconciling`。**直接证伪 skill 文档"重跑自恢复"**。修复：check_preconditions 见 underlay 空 + marker 在，视为"收尾被打断"，清 marker 收敛（orig/backing 已权威）。

**W2 · `--rebuild` 在全量 reingest 前先清 marker → 自启可挂到半重建 backing / 双挂载**
`:1710` 清 marker 先于 `:1714 lifecycle::reingest`（因 reingest 自带 `bail_if_reconciling` 死结）。reingest 期间 marker 已清 + underlay 已空 → systemd 自启三入口判据全放行。修复：rebuild 让 marker 贯穿 reingest（内部不 bail 的 reingest 变体，或保持 marker 到重挂完成）。

### Low / Info

- **W3**：崩溃续跑用已 merged 的 orig 当"前镜像"，undo 只能回到已合并态而非 reconcile 前（无丢失）。
- **W4**：`write_manifest` 失败仅 warn，`latest_generation` 选中无 manifest 代次 → undo 拒绝且文案称"未完成"（实已完成），并连带挡更早代次 undo（纯可用性）。
- **W5(Info)**：undo 陈旧门只守 underlay 不守 orig golden——orig 是内部金源非用户 git 仓，超威胁模型，备案。

## 逐条 PASS（已核实无问题）

路径穿越（`validate_name` + `is_safe_rel` manifest rel 纵深防御）、symlink 注入（passthrough_restore_memory 四闸 + `symlink_metadata` 不跟随）、memory 短路（仅删同目标冗余软链）、quarantine 跨卷（EXDEV 回落、特殊文件报错中止）、所有真挂载入口守卫（RealMounter/SystemdMounter/mount-managed/remount/resolve_managed_spec 全过 `ensure_mountable`/`underlay_guard`/marker）、`ensure_underlay_empty` 空判定 fail-closed、删除硬顺序（stash fsync → 换 orig → 换 backing → 超集+readback+live_entry_unchanged 双门删 underlay）、undo marker 对称 + 崩溃 wedge 防、systemd crash-loop 三层防护、合并核纯函数确定性 + 幂等。

## 修复优先级

1. **R-C1（CRITICAL）** base 侧超集校验 + 中止还原
2. **R-lock（HIGH）** reconcile 取 backing 锁
3. **W1（Medium wedge）** check_preconditions 清陈旧 marker
4. **R-I1（Important）** 空 uuid 降级 NoUuid
5. **W2 / R-I2 / W3 / W4** 后续

## 修复状态（2026-07-06 同会话落地）

均 TDD RED→GREEN、精确 pathspec 提交、370 lib 测试全绿：

- ✅ **R-I1**（`fix(reconcile): 空串/非字符串 uuid 降级 NoUuid`）：`record.rs` `classify_record` 仅非空字符串 uuid 算 transcript 键，空串/缺失/非字符串走整行去重全保 distinct。
- ✅ **R-C1**（`fix(reconcile): apply_entry/subagents 补 base 侧超集铁律门`）：新 `merge::base_covered_by_merged`（**uuid 级** transcript + 行级 no-uuid，故不误判 §4.1 同 uuid 取更长者），在 Union/subagents 落 orig 前 fail-fast，不覆盖则中止、不删 underlay、保两份。
- ✅ **R-lock**（`fix(reconcile): reingest_one_file/undo 删 backing 取 backing 锁`）：`reingest_one_file` + `undo_remove_orig` 取 `acquire_backing_retry`，与 compact/seal/守护同一把 `.scrollz.lock` 硬互锁；`reingest_one_file_blocked_while_backing_locked` 回归。锁短持、不跨 rebuild 的 remount（无自死锁）。
- ✅ **W1**（`fix(reconcile): check_preconditions 见 underlay 空+陈旧 marker 清标记收敛`）：解崩溃 wedge，兑现「重跑自恢复」。
- ✅ **R-I2**（`fix(reconcile): 同 uuid 分叉 conflicts 携落败整行`）：落败字节可从报告复原（merge 输出仍按 §4.1 取更长者）。
- ⚠️ **W2（未修，已记为已知窄窗口）**：rebuild 前必须清 marker——`lifecycle::reingest` 的自我重挂经 `mounter.spawn → ensure_mountable`，marker 在则拒挂。保持 marker 会**破坏 rebuild 自身重挂**（比 W2 race 更糟）。彻底修需「可信重挂」路径区分 reconcile 自身重挂 vs 外部自启，属较大改动。无数据丢失（orig 权威、单文件 reingest 原子）。
- ☐ **W3（Low）**：崩溃续跑用已 merged 的 orig 当前镜像，undo 只回到已合并态（无丢失）。
- ☐ **W4（Low）**：`write_manifest` 失败后 undo 文案称"未完成"（实已完成）且连带挡更早代次 undo（纯可用性）。

**结论**：CRITICAL + HIGH + Important + 一个 wedge 已收口，reconcile 的**双向超集删除门 + backing 硬互锁 + wedge 自恢复**补齐；余 W2（需可信重挂路径）+ W3/W4（Low）留后续。
