# enable 探测层加固：熔断缓存 + 三态健康 + 探测编排反转

## Context（为什么做这个）

围绕 enable 工具"hung FUSE 挂载不卡死"的加固已大部分落地在 main@ddb20c4：

- `hang-free 分档卸载`（`force_umount.rs` 的 clean/lazy/abort/auto + `hang_free::with_timeout`）已合并（`63fff00`）。
- 探测层 `endpoint_ok` / `canonicalized_target` / 活跃扫描的超时硬化已集成（`3065d96`，等价并入了主工作区那份未提交的 discovery.rs 改动 —— 工作树现已干净）。
- **C-残留卸载接线已完成**（`ccb05b0`）：`Mounter::unmount` 带 `UmountLevel` 参数、走 `force_umount::umount`，lifecycle/systemd 全程 hang-free；维护操作传 `Clean`、清理/还原传 `Auto`。

在此基线上，架构评审识别出的三项增量**尚未做**，本计划完成它们。按价值/风险排序为 **B → A → D**；B 与 A 相互独立可单独提交，**D 依赖 B 的最终 memo 接线**（须在 B 之后做，审查 M2）。可在任意阶段叫停。

> 本计划已经一轮 subagent 审查（判 needs-rework），下列内容已据审查 C1/C2/I1–I4/M1 修订。

> 原范围里的 "C-残留" 已被 `ccb05b0` 更完整地实现，不在本计划内。

## 现状 ground truth（ddb20c4）

- `endpoint_ok(path) -> bool`（`discovery.rs:182`）：超时/ENOTCONN 都塌缩为同一个 `false`。
- `hang_free.rs`：仅 `with_timeout` + `PROBE_TIMEOUT`，**无任何 hung 状态缓存**（已确认 grep 熔断/circuit/HashMap = 空）。
- `probe`（`discovery.rs:169-173`）：先 `endpoint_ok(&mp)`（无条件起线程）再 `is_mounted`，顺序使普通目录也付线程税。
- `endpoint_ok` 的 7 处生产消费点：`daemon.rs:106`、`systemd.rs:144`（readiness poll）、`discovery.rs:169-173`（喂 classify）、`force_umount.rs:177`（abort 守卫）、`lifecycle.rs:245/248`（remount 幂等 + stale-clear）、`model.rs:156-172`（classify）。

---

## 阶段 B —— 进程级 hung 挂载熔断缓存（最高杠杆、低风险、独立）

**问题**：每个探测入口独立起线程、超时后线程泄漏且不可回收。TUI 定时刷新对同一 hung 挂载**每次刷新泄漏一个线程**。

**做法**：在 `hang_free.rs`（B 的自然归属）新增进程级缓存 + 包装器，**仅作用于 `endpoint_ok` 这一个高频泄漏点**（审查 C2/I1/I3 收敛结论：`canonicalized_target` 与 `recent_log_write` 一律不进 memo，见下）：

- `static HUNG: OnceLock<Mutex<HashMap<PathBuf, Instant>>>`（惰性初始化；低频，`std::sync::Mutex` 足够，不引 parking_lot）。
- `HUNG_TTL = Duration::from_secs(1)`（对齐 `PROBE_TIMEOUT`=800ms 量级；**不用**卸载的 3s `STEP_TIMEOUT`——两者无关，3s 会放大"同路径 remount 一个健康 daemon"的误判窗口，审查 I2）。
- `pub(crate) fn with_timeout_memo<T>(key: &Path, dur, f) -> Option<T>`：命中且未过 TTL → 直接返回 `None`（跳过起线程，杜绝重复泄漏）；否则跑 `with_timeout(dur, f)`：`Some` → 从缓存移除 key（恢复）并返回；`None` → 记入 `key → Instant::now()` 返回 `None`。

**接线（收窄到一处）**：只有 `endpoint_ok`（`discovery.rs:184` 的 `with_timeout`）改走 `with_timeout_memo(mp, ...)`，键用**原始挂载路径 `mp`**（与消费点 `discovery.rs:169` 一致，避免 canonical/raw 双键别名，审查 I1）。以下两处**明确保持裸 `with_timeout`，不进 memo**：

- `canonicalized_target`（`discovery.rs:215`）：其 `with_timeout` 包的是 `fs::canonicalize(parent)`，超时/未命中会回退**未规范化原路径**；若被 memo 的 `None` 触发回退，`is_mounted` 会与 mountinfo 内核规范路径失配、误判未挂载、classify 把 Active/Stopped 翻成 **Broken**——正确性回归（审查 C2，Critical）。绝不 memo。
- `recent_log_write`（`discovery.rs:362`，`WALK_TIMEOUT`=2s）：子树遍历超时与 endpoint stat 超时信号强度/节奏不同，共享 TTL 会互相压制（审查 I3）；它已 `.flatten()` 安全处理 `None`。

**泄漏语义（如实，审查 I4）**：memo **不消除**泄漏——首次 hung 探测仍泄漏一个线程（`hang_free.rs:21-24` 设计使然）；memo 把泄漏频率**界定为 ≤1 次/TTL/挂载**。同理 `scan`（`discovery.rs:159`）首次仍串行付 N×800ms，memo 只救**后续刷新**（审查 Missed-1）。手动验证时线程数不会持平，而是增长变慢。

**关键文件**：`fuse/src/enable/hang_free.rs`（缓存 + `with_timeout_memo` + 单测），`fuse/src/enable/discovery.rs`（仅 `endpoint_ok` 一处改走 memo）。

**测试**（TDD，先红）：
- 同一 key 第二次调用在 TTL 内不执行闭包（`AtomicUsize` 计数闭包调用次数，断言只跑一次）。
- 超时后 key 入缓存；TTL 过期后再次执行闭包。
- 闭包成功时 key 从缓存清除（恢复路径）。

---

## 阶段 A —— `endpoint_ok` 由 bool 升为三态 `EndpointHealth`

**问题**：`stale(ENOTCONN, daemon 死)` 与 `hung(超时, daemon 无响应)` 塌缩成同一 `false`，`classify` 只能把两者都标 `Broken`，用户无法区分"僵尸挂载"与"daemon 卡住"。

**做法**（契约改动，收益集中在诊断展示，成本跨 7 处，故排在 B 之后）：

- `discovery.rs` 定义 `pub enum EndpointHealth { Healthy, Stale, Hung }`；`endpoint_ok` 改名/新增 `endpoint_health(path) -> EndpointHealth`：`Some(Ok)→Healthy`、`Some(Err ENOTCONN)→Stale`、其他 `Err`→`Healthy`（原语义：非 ENOTCONN 不算坏）、`None`→`Hung`。
- 保留一个 `endpoint_ok(path) -> bool`（`matches!(health, Healthy)`）薄封装，让只关心"健康与否"的消费点（`daemon.rs:106`、`systemd.rs:144` readiness poll、`lifecycle.rs:245/248` remount、`force_umount.rs:177` abort 守卫）**零改动**——避免无谓爆炸半径。
- 仅在真正获益处消费三态：
  - `model.rs::classify`：签名把 `endpoint_ok: bool` 换为 `health: EndpointHealth`；`(true,true)` 分支 `Hung → ProjectStatus::Hung`（新增枚举值）、`Stale → Broken`、`Healthy → Active`。新增 `ProjectStatus::Hung` 的展示文案（`mod.rs` status / `tui.rs` 行）。
  - `discovery.rs::probe`：传 health 给 classify。

**风险/边界**：`force_umount.rs:177` 的 abort 守卫当前用 `endpoint_ok`（`!ok` = stale 或 hung 都算"确证卡死可 abort"）。**保持用 bool 封装，不改其语义**——三态若排除 Stale 会破坏 abort，明确不动。

**`ProjectStatus::Hung` 的穷尽/gate 落点（审查 C1，漏列会编译失败或逻辑错，须全改）**：
- 编译强制（穷尽 `match`）：`model.rs:137-142`（`label()`）、`tui.rs:427-432`（颜色 match）。
- 逻辑 gate（`matches!`/`if`，漏改会让 Hung 错误落入 Plain 分支）：`tui.rs:202,314`（`Active|Stopped|Broken` 批量 gate——Hung 应**并入**此组）、`tui.rs:331`（Stopped-only gate——Hung **不属于**）、`mod.rs:234-239`（ratio/note，Hung 落 `(_, Some(m))` 臂，仅需确认文案）。

**关键文件**：`discovery.rs`（enum + `endpoint_health` + probe），`model.rs`（classify 签名换 `health: EndpointHealth` + `label()` + `ProjectStatus::Hung`），`mod.rs` / `tui.rs`（上述 6 处 + 状态展示）。readiness/remount/abort 五处消费点靠 `endpoint_ok -> bool` 封装保持零改动（审查 M1 已核实）。

**半径含测试（审查 M1）**：classify 签名改动波及其测试调用点 `model.rs:284/292-293/299/304/310/312` 与 lifecycle 测试——机械改动，但须计入 A 的工作量，不止"7 个消费点"。

**测试**：`endpoint_health` 三分支单测；`classify` 对 Hung/Stale 分别产出 Hung/Broken 的单测（复用 `model.rs:291` 既有 classify 测试模式）。

---

## 阶段 D —— probe 探测编排反转（边际性能优化，最低优先）

**问题**：`probe` 先 `endpoint_ok`（无条件起线程）再 `is_mounted`。普通非挂载目录（占多数）也付一次线程创建成本。

**做法**：`is_mounted` 读 `/proc/self/mountinfo`（永不阻塞）先行；仅当确为 fuse 挂载点才对 endpoint 做超时探测，非挂载目录走同步 `symlink_metadata`。保持 `mounted = healthy && is_mounted` 的短路语义。

**风险**：触及 probe 短路逻辑（`mounted = healthy && is_mounted`，AND 对 bool 结果可交换、语义可保），需现有 16 个 discovery 测试 + 新增边界测试兜底。收益在卸载已 hang-free 后偏边际，故排最后、可评估后砍。**非独立于 B（审查 M2）**：须在 B 之后做；若 D 引入新的 endpoint 探测调用，须一并走 B 的 `with_timeout_memo`（同键 raw mp）。

**关键文件**：`discovery.rs::probe`（约 `169-173`）。

---

## 端到端验证

- `cargo test --manifest-path fuse/Cargo.toml --lib enable::`（discovery/model 单元）+ `cargo test --manifest-path fuse/Cargo.toml`（全量，基线 ~279 绿，不得回归）。
- `cargo clippy --manifest-path fuse/Cargo.toml -- -D warnings`。
- 每阶段独立提交（conventional commits：`feat(enable):` / `refactor(enable):`），阶段间全量测试绿方进入下一阶段。
- 阶段 B 手动验证（可选）：对一个真实 wedge 挂载连续两次 `enable list`，观察第二次不再新增泄漏线程（`ls /proc/self/task | wc -l` 前后对比）。
