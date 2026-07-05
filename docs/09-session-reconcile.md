# 09 — 会话感知的回落写重合并（Session-aware Reconcile）

> 设计文档。目标：当影子挂载**处于停用/卸载态**时，Claude Code 会把会话数据直接写进裸挂载点（回落写 fall-through），archive 完全不知情。本特性提供**会话感知、无损**的重合并，把回落写安全并回 archive，并在**真正执行挂载的入口**加"失败即拒"守卫，杜绝"挂上就静默盖住回落写"的丢数据陷阱。
>
> 意图见 [00-overview.md](./00-overview.md) / [03-target-data-scope.md](./03-target-data-scope.md)，可逆生命周期见 `enable/lifecycle.rs`，卸载引擎见 [07-hangfree-umount.md](./07-hangfree-umount.md)。
>
> **本版据两轮 subagent 评审（架构 + 对抗性数据安全，2026-07-04）重构**：纠正了对 Claude jsonl 记录语义的两个错误假设（见 §3），把有损的"单例元数据折叠"换成**统一无损并集**，并补齐活跃门禁、真挂载入口守卫、原子替换、超集不变量、并发锁等"零丢失"前提。

## 1. 背景与触发

### 1.1 真实事故（2026-07-04 实测）

`-home-xp-src-neighbors` 于 2026-06-30 被 `enable apply`（shadow，1MiB/level-3，11.49x，`bytes_src=1.68GB`）。随后经历一轮 bug fix，其守护被卸载、pid 陈旧、挂载停用。停用期（06-30 → 07-04）Claude Code 继续写该项目——挂载点此刻只是**空的裸目录**，写落进 underlay，与 archive 分歧：

| underlay 条目 | LIVE | archive/golden(orig) | 性质 |
|---|---|---|---|
| `925fc3a1….jsonl` | 25.5MB（07-03） | 25.67MB（06-28） | 同 sessionId，**transcript uuid 全disjoint、时间窗不相交**（LIVE 06-30；ORIG 06-24~25）；无 `isCompactSummary` 桥 → 疑 **session-id 重用**，两段不同对话 |
| `925fc3a1…/subagents/` | 06-30 | 06-25 | 会话 sidecar（**也是 jsonl transcript**） |
| `373e2835….jsonl` | 213B（`ai-title`+`mode`） | 1.90MB（404 recs） | LIVE 只有少量日志记录、**无 transcript**；正文全在 base |
| `de756008….jsonl` | 425B（`last-prompt`+`mode`） | 2.37MB（261 recs） | 同上 |
| `memory/` | 4 个 .md（07-03~04） | golden 里是 **symlink → `/home/xp/src/neighbors/docs/memory`** | 透传软链在停用期被当空目录、写成真实目录 → **split-brain** |

### 1.2 现存缺口

- **真挂载入口无 underlay 守卫**：开机/登录/崩溃自愈的挂载走 systemd 模板单元 `ExecStart=zipfs mount-managed --name %i` → `resolve_managed_spec`（`enable/systemd.rs`），**不经** `lifecycle::remount`。守卫只加在 remount 会被自启完全绕过 → 回落写仍被静默盖住。
- **无重合并原语**：分歧后无手段把二者并回；`restore` 因挂载点非空会失败（安全但不解决）。

## 2. 目标 / 非目标

**目标**

- **零静默丢失（铁律）**：合并**永远是双方内容的超集**；任何拿不准一律 keep-both；删除 underlay 前必须由**运行时超集不变量**证明未丢，否则中止保两份。
- 会话感知的**无损并集**合并核：正确切分 Claude jsonl 记录（§3），带 uuid 记录按 uuid 并集、无 uuid 日志记录整行去重保全 distinct，**绝不折叠**。
- **智能决策推荐**：对每个分歧给证据 + 置信度 + 理由 + 推荐动作；但**从不自动执行**，逐条确认才落盘（执行策略 B）。因合并本身无损，推荐只在"并入同一文件 vs 保留为两个文件"之间选，永不触发有损。
- 在**所有真挂载入口**加失败即拒的 underlay 守卫（单点 `ensure_underlay_empty` 复用），永久堵住静默盖住。
- 增量落盘：只动分歧少数条目，以 `.zipfs-orig` 为真源、按文件**原子替换 + 重灌** backing；全程可逆、可续跑。

**非目标**

- **不**做 `MEMORY.md` 语义合并——memory 只做「透传恢复 + 冲突改名保留两份」（例外，§6）。
- **不**做 CDC/跨会话去重、跨文件合并。
- **不**改挂载/写入耐久协议（文档 04）与卸载引擎（文档 07）。
- **仅 shadow 后端**：per-file archive 替换与 symlink 透传在 container（redb 单文件、无 symlink、MVCC）上不成立，container 项目直接拒绝。

## 3. Claude jsonl 记录语义（评审实测校正）

设计的正确性依赖对记录类型的准确切分。**以下是对真实 `~/.claude/projects` 数据的实测结论，纠正初版两处错误假设：**

- **transcript 记录 = 任何带 `uuid` 字段的记录**，与 `type` 无关。包括 `user`/`assistant`/`attachment`/`system`/工具记录等；`attachment` 常是**量最大**的类型，`system` 带 `subtype`（如 `stop_hook_summary`）。**切分只看有无 `uuid`，绝不按 type 白名单**（否则漏掉海量 attachment/system）。
- **`last-prompt`/`ai-title`/`custom-title` 是 append-only 每回合日志，不是单例**：实测单文件 `last-prompt` 数千条 distinct、各带 `leafUuid` 指回 transcript 叶。**必须整行去重保全 distinct，绝不 newest-wins 折叠成一条**（折叠即丢几千条）。运行时"当前值"天然是最后一条，无需折叠。
- **仅 `mode` 实测真单例**（单文件数千条但仅 1 distinct）。即便如此也走去重并集（自然收敛为该 distinct 值），**不做特判折叠**——统一规则更安全、无损。
- **森林/悬空根是常态**：`parentUuid:null` 根一个文件可有多个；`isSidechain:true` 极常见，每个 sidechain 产生一个悬空子根。故"悬空根"**不能**作为 reuse 的判据。
- **compaction 续写标记 = `type:"user"` + `isCompactSummary:true`**（`parentUuid` 指向上一会话叶），**不是** `type:"summary"`（后者在这些项目里为 0）。reuse 检测必须认 `isCompactSummary`。

## 4. 无损并集合并核 `reconcile::session_merge`

纯函数（无 IO），输入同名文件的 base（archive/golden 明文）+ incoming（underlay 明文）原始行 + 解析 JSON，输出 `(Decision, Evidence, Vec<RawLine>)`。**合并恒为超集，无任何折叠：**

1. **带 uuid 记录**：按 uuid 并集。同一 uuid 两侧内容不同（仅可能由崩溃截断产生）→ 取**可完整解析且更长**者，并记入报告（不静默择一）。
2. **无 uuid 记录**（last-prompt/ai-title/custom-title/mode/queue-operation 等）：整行去重并集，**保全部 distinct**。
3. **不可解析行**：verbatim 保留；但对**末行做截断探测**（半截 JSON），半截行单列待确认、不当正文并入，避免两侧各自残尾被双份 append。
4. **排序**：稳定全序 —— 有 `timestamp` 按 timestamp，其次原始行号；保持交织不失真。

**分类（只用于 advisor 推荐 union vs keep-separate，两者都无损）：**

| incoming transcript uuid 相对 base | 分类 | 推荐 |
|---|---|---|
| 有交集，或存在 `isCompactSummary` 桥指向 base | **incremental / compaction 续写** | 并入同一文件（union，无损） |
| 空（incoming 无 transcript，只有日志记录） | **log-only 更新** | 并入同一文件（base 正文 + incoming 日志，无损） |
| 与 base **全disjoint** 且双方非空、无 compaction 桥 | **疑 reuse** | 低置信 → 默认 keep-separate（base 不动，incoming 隔离，见 §5.3），或经确认改并入 |

**幂等契约**：`merge(merge(base,inc), inc) == merge(base,inc)`（uuid/整行去重天然可达），保证中断重跑不出错，写进测试。

## 5. 组件架构

**5.1 `reconcile::session_merge`** —— §4 的纯核，确定性、fixture 全覆盖（含 attachment/system/isCompactSummary/截断行）。内存上界：会话 jsonl 可达上百 MB，设 size cap，超限降级 keep-both 或拒绝并提示（呼应 `ingest` 的流式防 OOM）。

**5.2 `reconcile::advisor`** —— 纯函数。**复用 session_merge 一次遍历产出的 `Evidence`**（uuid 交集、时间窗、`isCompactSummary` 桥、大小、orig 明文 mtime），只叠 `Confidence` + 措辞，**不重新解析、不另采证**（避免与 merge 裁决漂移）。

**5.3 `reconcile::orchestrator`** —— IO，接入 enable。前置门禁（缺一即拒）：
- 项目 STOPPED 且 backing 已提交、orig 在、underlay 非空；
- **shadow 后端**（container 拒绝）；
- **活跃检测**：`discovery::detect_activity` 判空闲，活跃即拒（无 `--force`）——STOPPED≠空闲，裸挂载点可写，Claude 可能正 append；
- 取 **reconcile 锁**（`<name>.reconcile.lock` flock，与挂载入口共享语义）→ 与挂载、彼此互斥。

流程（真源 = `.zipfs-orig` 明文；backing 按变更文件派生）：
1. **快照 underlay 进 stash**，全程对快照运算；stash 落盘并 fsync **成功后**才允许动 orig/backing。
2. 逐条目分类 + 出建议单 → 逐条确认（策略 B）。
3. 对确认条目：
   - `*.jsonl` 两侧都在 → session_merge → `Merged`：**先**把合并明文原子写入 orig（`<file>.tmp`→fsync→rename→fsync_dir），**再**原子重灌进 backing（`<file>.reconcile-tmp`→fsync→rename 覆盖→fsync_dir）。
   - `疑 reuse` 且用户选 keep-separate → base 不动；incoming 副本**保留原 UUID 文件名**隔离到 `~/.claude-zip/reconcile-quarantine/<name>/<ts>/<uuid>.jsonl`（**移出 projects 树**，Claude 不会在活动视图撞见、语义完整、用户可手动恢复）。
   - `*.jsonl` underlay 独有 → new → 并入 orig + 灌入。
   - sidecar 目录 `<uuid>/`（subagents，**也是 jsonl transcript**）→ **走同一 session_merge**，同名一律并集/keep-both，**绝不按 mtime 删较旧者**。
   - 遮蔽 backing **symlink** 的条目（memory）→ **透传恢复**（§6）。
   - 其他非-jsonl → 内容相同即丢；不同即 keep-both 改名，**mtime 仅作提示、平局/反向一律 keep-both**。
4. **删 underlay 条目前**（通用删除许可，覆盖**所有**接收方路径 —— merged / quarantine 隔离 / new / memory 目标）：校验**运行时超集不变量** —— 该 underlay 条目的接收方已 **durable（fsync + readback）** 且逐字节 **⊇ 或 ==** 该条目内容（merged：base transcript uuid 集 ⊆ merged、base 无uuid 行多重集 ⊆ merged、已接受 incoming 项 ⊆ merged；quarantine/new/memory：接收文件字节 == 源条目，含跨卷 copy 后的 readback）+ 复核 underlay 快照未变（mtime/size/inode）；任一不满足 → 中止、保两份、报告。
5. 全部处置完、underlay 清空 → 更新 committed meta 字节数（呼应 `reingest` 自写 meta）→ 清陈旧 pid/lock。

**reconciling 中间标记**：改 backing 期间置 `committed=0`（或落 `reconciling` sidecar），使中途崩溃被 `classify` 判为 **Broken/需人工**而非"可自动重挂"，杜绝半 reconcile 的 backing 被当权威挂出。完成才复位 `committed=1`。

**5.4 挂载前 underlay 守卫（失败即拒，单点复用）** —— 抽成 `ensure_underlay_empty(paths,name)`，下沉到**真正挂载前的最后一道**：`resolve_managed_spec` / `Mounter::spawn` 前置，被 `remount` / `mount-managed`(systemd 自启) / `apply` 重挂路径**全部**复用。underlay 非空即拒、指向 `enable reconcile`。
- **systemd crash-loop 处理**：模板单元 `Restart=on-failure`+`WatchdogSec` 下，守卫直接非零退出会反复重启至 start-limit。改用 `ExecStartPre` 显式检查：非空 underlay → 落 `NEEDS-RECONCILE` sentinel + 明确日志 + 不进入重启循环（oneshot 失败或自我 `disable`），给可操作指引而非噪声。
- 守卫谓词精确：忽略已知无害隐藏项白名单（`.fuse_hidden*`/`.DS_Store`/编辑器锁），只认 fall-through 语义条目，避免残渣永久阻塞挂载。
- `apply` 的 mount 分支此刻 mp 定义上为空（rename 后 create_dir），**不纳入**守卫（避免误拒正常 apply）。

**5.5 CLI / TUI** —— `zipfs enable reconcile <name> [--dry-run] [--force] [--rebuild]`（`--force` 越过活跃门禁由人确认；`--rebuild` = 全量重灌兜底，backing 可疑时用）。`enable list`：STOPPED 且 underlay 非空 → 标 `NEEDS-RECONCILE`；reconcile 进行中因 `committed=0` 会短暂显示 Broken —— 属正常（有 reconcile 锁保护、崩溃本就应判 Broken），list/TUI 对持锁项标注"reconciling"以免误判。TUI 同标记 + 逐条建议复核。

## 6. memory 透传恢复（例外规则）

memory 本应是**单份透传软链**：ingest 在 backing 按软链重建、shadow store 透明服务 `readlink` → 挂载时读写只落唯一 canonical。停用期软链缺失才写出第二副本。重合并须**恢复单份透传**：

- **先判 underlay 形态（短路，用户 2026-07-05 定）**：若 underlay 的 `memory` 条目**整体就是 symlink**（且目标与 backing 同名软链一致）→ 说明写入当时软链在、已透传到 canonical，**无 split-brain、无任何数据要合并**——数据层面零操作；仅把这个**冗余 symlink 从 underlay 移除**（backing 上有同样软链，挂载时照常透传），好让 `underlay_has_fallthrough` 放行、remount 不被拒（否则 `walk_snapshot` 跳过 symlink → 该软链既不进快照被处理、又被顶层守卫判非空 → 卡挂，即终审 M3 的 symlink 具化）。若目标与 backing 软链**不一致**（异常）→ 保守不删、报告待人工。
- 仅当 underlay 的 `memory` 是**真实目录**（split-brain）才走下述 relocate：
  - 写目标前：`canonicalize` 目标，校验其**存在且可写**、解析后仍在允许根内（**拒 `../` 穿越**）；目标不可写/悬空/被前次半截 reconcile 物化成真实目录 → **不删** underlay memory、报告待人工（防向外部 live git 仓注入 / 递归 split-brain）。
  - underlay 里目标**不存在**的新文件 → 复制进目标（找回停用期新增记忆）。
  - underlay 里与目标**同名异内容**者（如 `MEMORY.md`）→ **不合并**；underlay 版本改名保留两份，**用内容哈希做后缀**（`MEMORY.md.underlay-<hash8>`）而非时间戳，保证重复中断重跑**幂等**、不产生多副本。
  - 目标加锁/活跃检测防与别的 worktree/Claude 竞态。
  - 处置完删 underlay 的 `memory/` → 软链重新生效。canonical 原版始终不动。

## 7. 错误处理与数据安全

- **落盘硬顺序**：stash（含①改写的 orig 明文 ②替换的 backing archive ③删除的 underlay 原始字节）落盘 + fsync + readback 校验**成功后**，才动 orig；orig 原子 rename 替换后才重灌 backing；backing 原子替换 + 超集校验后才删 underlay 条目。**绝不先删后写、绝不就地截断金源 orig**。
- **超集不变量**是删除的唯一许可（§5.3 步 4），且是**通用门**——凡删 underlay 条目，其接收方（merged / quarantine / new / memory 目标，含跨卷 copy）必须先 durable 且逐字节 ⊇/== 该条目。`verify_file` 只证 `backing==orig` 编码保真，**不证内容超集**，故不能仅凭它删 underlay。
- **崩溃恢复**：reconciling 标记 → 判 Broken；残留 stash 有发现/GC/**续跑**规则；合并幂等保证重跑不放大损坏；backing 疑损时 playbook 指引 `--rebuild`。
- 冲突一律 keep-both；截断行单列待确认；坏行 verbatim；reuse 保守（有交集/有 compaction 桥即并入，不拆散）。

## 8. 测试

- **单元 · session_merge**：真实 fixture（373e2835/de756008 log-only、925fc3a1 疑 reuse）+ 合成 —— attachment/system 带 uuid 参与并集、`last-prompt`×N 全保留（**反 newest-wins 折叠**）、`isCompactSummary` 桥判 incremental、同 uuid 截断冲突取完整者、末行截断单列、幂等不动点、size cap 降级。
- **单元 · advisor**：证据/置信度断言（925fc3a1 无 compaction 桥 + disjoint → 低置信 keep-separate；有 `isCompactSummary` → incremental）。
- **集成（tempdir）**：orchestrator 全条目类型 —— log-only 并集、reuse 隔离（原 UUID 名、移出树）、new 并入、subagents 走 merge、memory 透传恢复（`../` 拒绝、悬空目标不删、内容哈希改名幂等）、活跃门禁拒绝、并发锁互斥、stash 可回滚。
- **超集不变量**：注入丢行的 buggy merge → orchestrator 必须中止且不删 underlay。
- **守卫**：`mount-managed`(自启入口) 对非空 underlay 失败即拒（**证明不被绕过**）；无害隐藏项白名单放行；reconcile 后重挂成功。
- **崩溃/续跑**：改 backing 中途崩 → 判 Broken；重跑幂等收敛。
- **字节校验**：重合并后 golden 各行 ⊆ 结果，坏行仍在。

## 9. 落地本次事故（neighbors）

特性合入后：`enable reconcile -home-xp-src-neighbors --dry-run` → 复核建议单（预期 373e2835/de756008 = log-only 并入；925fc3a1 = 疑 reuse 低置信、默认隔离保原 UUID；memory 走透传恢复）→ 逐条确认实跑 → 超集 + 字节校验 → `remount` → 确认 ACTIVE 且内容一致。ghc2api-go 本轮不动（数据安全存于其 `.zipfs-orig`）。

## 10. reconcile-undo（回退最近一次重合并，供重选）

> 增补（2026-07-05，据 subagent 评审重构）：`reconcile` 是 stash-backed、非破坏的，故一次 run 可被完整回退，让用户"重选"每条目的处置。`enable reconcile-undo <name>` 把项目还原到该 run **之前**的状态（underlay + orig + backing），随后可换选项重跑 `reconcile`。undo 本身也须满足零丢失铁律 + 与 reconcile 对称的挂载互斥。

### 10.1 依赖：per-generation manifest（记**逆转类**，非展示串）

undo 要按各条目当初的处置分别逆转，故 `reconcile` 落盘时须在该代次 stash 里持久化 **manifest**：`reconcile_stash(name,ts)/manifest`，记录 `ts` 与每条目 `rel → 逆转类`。**逆转类不是展示用的 action 字符串，而是"undo 该怎么反做"的精确分类**（评审 C3：`reconcile_subagents_dir` 对 new 与 merge 都报同一 `subagents-union`，展示串抹平了 new-vs-merge，会致孤儿残留）——由 `apply_entry` 在处置时按其**实际走的分支**判定，`restore-orig` vs `remove-orig` 的判别子是**是否落了 orig 前镜像**（union/subagents 伞下也据此精确区分 merge 与 new），其余三类由分支上下文直接确定：

| 逆转类 | 何时记 | undo 逆转动作 |
|---|---|---|
| `restore-orig` | union / subagents-merge：orig 预先存在、落了前镜像 | 从 `stash/<ts>/orig/<rel>` 原子还原 `orig/<rel>` → `reingest_one_file(rel)` 重建 `backing/<rel>` |
| `remove-orig` | new / subagents-new：orig 原不存在、新增 | 删 `orig/<rel>` + 删 `backing/<rel>`（NotFound 容忍）→ prune 空父目录 |
| `remove-quarantine` | keep-separate | orig/backing **不碰**；删本代次 `quarantine/<ts>/<rel>`（删前逐字节校验其内容 == `stash/<ts>/underlay/<rel>` 快照，非 live） |
| `report-memory` | passthrough**且实际 relocate 了**（真实目录、路径安全闸通过、往外部目标写过文件） | orig/backing 不碰；**不触碰外部 memory 目标**，仅报告本代次往目标写过的文件（新文件 + `.underlay-<crc>` 变体）供用户 git 回退 |
| `noop` | identical / skipped / keep-both / memory-symlink 短路（§6）/ **memory-deferred（路径安全闸拦截、外部未写、underlay 未 relocate）** | orig/backing 不碰；无外部写可报 |

manifest 随 run 结束写入（best-effort：写失败仅告警，但该 run **不可 undo**，需手动从 stash 恢复）。

### 10.2 前置门禁（缺一即拒）

- 项目 **未挂载** + **shadow** 后端。
- 取 **reconcile 锁**（与 reconcile / 其他 undo 互斥）。
- **目标代次 = ts 最大且 manifest 存在** 的一代（评审 I2）。若最新代次**无 manifest**（崩溃未完成的 run）→ **拒绝**并指明"该 run 未完成、不可 undo，请查 stash 手动恢复"；**绝不清除属于崩溃 run 的 `.reconciling` marker**。无任何代次 → Err（"无可回退的 reconcile 记录"）。
- **陈旧门（评审 C1）**：`detect_activity` 判空闲；且 underlay 里**没有**晚于 manifest ts 的新 append（逐条目 `live_entry_unchanged` 式对比：live 条目要么缺失、要么与 `stash/<ts>/underlay/<rel>` 快照 mtime/size/ino 一致）。任一 live 条目**已变**（reconcile 后 Claude 又写了）→ **拒绝整个 undo**并报告哪些条目已有新写，指引用户先 `reconcile` 收编新写、或手动处理——绝不用旧快照覆盖新数据。

### 10.3 逆转机制（置 marker → 逐条目 → 还原 underlay → 清 marker）

1. **`set_reconciling(true)`**（评审 C2）：undo 改 orig/backing 是与 reconcile 等价的半改写窗口；reconcile 锁按 `model.rs` **不 gate 挂载**，挂载/维护让路唯一靠 `.reconciling` marker（`bail_if_reconciling` 守 remount/reingest/restore/compact/seal + 挂载守卫）。故 undo 全程必须持 marker，收尾才清。
2. **逐条目按 manifest 逆转类**处置（表见 §10.1）。orig 还原走原子 `tmp→rename`；backing 走 `reingest_one_file` 原子替换 / 删除，与 reconcile 同原语。删除均 **NotFound 容忍**（保证幂等重跑，评审 I3）。
3. **统一还原 underlay**：把 `stash/<ts>/underlay/**` 逐文件拷回 `mp/<rel>`（重建目录结构）。**逐条目守卫（承 C1）**：仅当 `mp/<rel>` **缺失**或与快照**逐字节一致**才覆盖还原；若 live 已存在且不同（reconcile 后新写）→ **不覆盖、保留 live、报告**。还原后 underlay 非空 → `list` 重标 `NEEDS-RECONCILE`（正确：又回待处理态）。
4. **`set_reconciling(false)`**。目标代次 stash 落 `.undone` 标记（评审 I3：防二次误触；再敲 undo 认出已消费）。

**幂等 & 层级**：undo **只回退最近一代**（latest run），不逐级回溯多代。声明幂等——所有删除 NotFound 容忍、还原覆盖对一致内容无害；已 `.undone` 的代次再 undo → no-op 提示。

### 10.4 零丢失与安全

- undo **只还原/新增**数据（拷回 underlay、还原 orig 前镜像）；它删除的只有：被合并覆盖的 orig（由前镜像原子还原）、new 增出的 orig/backing、quarantine 重复副本（删前**逐字节校验 == stash underlay 快照**，与 live 还原解耦，评审 I1）。undo 自身零丢失。
- **陈旧门 + 逐条目覆盖守卫**是 C1 的双保险：reconcile 之后的任何新 append 绝不被旧快照覆盖。
- **marker 对称**（C2）保证 undo 半改写窗口内 systemd 自启 / lifecycle 维护全部让路。
- undo **绝不写外部 memory 目标**（用户 2026-07-05 定）：仅报告本代次往目标写过的文件，指引用户用其 git（目标本就是 git 仓）或冷备份回退；再跑 reconcile 的 memory 幂等由 `place_memory_files` 内容哈希去重保证，不会重复注入。
- **非 bit-exact 但更安全的偏差**（评审 M1）：reconcile 的 `Identical` 分支在"orig 有、backing 缺"时会补建 backing；undo 的 `noop` 不动 → backing 保持存在（reconcile 前是缺的）。无数据丢失、反而修了潜在不一致，记录备案。

### 10.5 CLI

`zipfs enable reconcile-undo <name>`（前导 `-` 项目名须 `--` 分隔）。打印：**实际选中的代次 ts**、逐条目逆转报告、被守卫跳过（reconcile 后新写）的条目清单、memory 外部目标待手动回退清单。拒绝时（已挂载 / 无 manifest / 陈旧门未过）给明确文案与下一步指引。之后可重跑 `enable reconcile` 换选项。
