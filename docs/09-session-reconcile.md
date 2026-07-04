# 09 — 会话感知的回落写重合并（Session-aware Reconcile）

> 设计文档。目标：当影子挂载**处于停用/卸载态**时，Claude Code 会把会话数据直接写进裸挂载点（回落写 fall-through），archive 完全不知情。本特性提供**会话感知、时间知情**的重合并，把回落写安全并回 archive，并在挂载路径上加**失败即拒**的守卫，杜绝"挂上就静默盖住回落写"的丢数据陷阱。
>
> 意图与范围见 [00-overview.md](./00-overview.md) / [03-target-data-scope.md](./03-target-data-scope.md)，可逆生命周期见 `enable/lifecycle.rs`，卸载引擎见 [07-hangfree-umount.md](./07-hangfree-umount.md)。

## 1. 背景与触发

### 1.1 真实事故（2026-07-04 实测）

`-home-xp-src-neighbors` 在 2026-06-30 被 `enable apply`（shadow，1MiB/level-3，11.49x，`bytes_src=1.68GB`）。随后经历一轮 bug fix，其守护被卸载、pid 陈旧、挂载停用。停用期间（06-30 → 07-04）Claude Code 继续写该项目会话——由于挂载点此刻只是**空的裸目录**，写落进了 underlay，形成与 archive 分歧的第二副本：

| underlay 条目 | LIVE | archive/golden(orig) | 性质 |
|---|---|---|---|
| `925fc3a1….jsonl` | 25.5MB（07-03） | 25.67MB（06-28） | 同 sessionId，**uuid 全disjoint、时间窗不相交**（LIVE 06-30 一段；ORIG 06-24~25 另一段）→ **session-id 重用**，两段不同对话 |
| `925fc3a1…/subagents/` | 06-30 | 06-25 | 会话 sidecar 目录 |
| `373e2835….jsonl` | 213B | 1.90MB（404 recs） | LIVE 只含 `ai-title`/`mode` 元数据、**零正文** → 纯元数据更新 |
| `de756008….jsonl` | 425B | 2.37MB（261 recs） | LIVE 只含 `last-prompt`/`mode` → 纯元数据更新 |
| `memory/` | 4 个 .md（07-03~04） | golden 里是 **symlink → `/home/xp/src/neighbors/docs/memory`** | 透传软链在停用期被当空目录、写成真实目录 → **split-brain** |

925fc3a1 的重用判定有四条独立证据齐备：uuid 交集为 0；双方时间窗不相交；两文件均**无** `summary`/`isCompactSummary` 等 compaction 标记；LIVE 根记录的 `parentUuid` 落在**两文件之外**（其真正母会话在别的文件）。故它是重用，不是 compaction 续写。

### 1.2 根因与现存缺口

- **挂载路径无 underlay 守卫**：`remount`/`apply`/autostart 的 mount 分支不检查挂载点 underlay 是否非空，直接把 archive 挂上去 → 回落写被**静默盖住**（物理仍在，但不可见、且后续易被覆盖）。
- **无重合并原语**：回落写与 archive 分歧后，工具没有任何把二者并回来的手段；`restore` 因挂载点非空会失败（这是安全特性，但不解决问题）。

## 2. 目标 / 非目标

**目标**

- **零静默丢失**：任何冲突/暧昧一律**保留两份**（把错误时期的版本重命名），绝不静默合并或丢弃。
- 提供**会话感知、时间知情**的合并核：能区分「纯元数据更新 / 增量续写 / session-id 重用」并分别处置。
- 提供**智能决策推荐**：对每个分歧给出证据 + 置信度 + 理由 + 推荐动作；但**从不自动执行**，每一条都需用户逐条确认后才落盘（执行策略 B）。
- 在挂载路径加**失败即拒**的 underlay 守卫，永久堵住静默盖住的陷阱。
- 增量落盘：只动分歧的少数条目，以 `.zipfs-orig` 为真源、按文件重灌 backing；不为一次重合并重灌整树。

**非目标**

- **不**做项目记忆 markdown 索引（`MEMORY.md`）的语义合并——memory 只做「透传恢复 + 冲突改名保留两份」，是明确例外。
- **不**引入 CDC/跨会话去重、跨文件合并。
- **不**改挂载/写入耐久协议（文档 04）与卸载引擎（文档 07）。

## 3. 分歧分类与判据

对同名 `*.jsonl`，以 base=archive(golden) 记录、incoming=underlay 记录，按下表分类（判据即 §1.1 在真实数据上用过的那套）：

| incoming 含有 | 分类 | 处置 |
|---|---|---|
| 仅单例元数据（无 transcript uuid） | **metadata-only** | 保留 base 正文，叠加 incoming 更新的元数据（新旧以源文件 mtime 判） |
| transcript uuid 与 base **有交集** | **incremental** | 按 uuid 并集（base 顺序稳定，incoming 独有者按 DAG/timestamp 追加），元数据 newest-wins |
| transcript uuid 与 base **全disjoint** 且双方非空 | **reuse** | **不合并**：base 原样保留，incoming 落成改名兄弟文件 `<uuid>__underlay-<ts>.jsonl` |
| base 无此文件 | **new** | underlay 独有的新会话，直接并入 |

**记录切分**：transcript 记录 = 带 `uuid`（含 `parentUuid`/`timestamp`）；单例元数据 = `ai-title`/`custom-title`/`mode`/`last-prompt`（无 uuid、每会话一份当前值，newest-wins）；其余无 uuid 记录（如 `queue-operation`、无 uuid 的 `file-history-snapshot`）按整行内容去重后并入、保持原相对序（既不当 transcript 参与 uuid 分类，也不当单例覆盖）。**不可解析行原样保留**（verbatim），永不丢弃。分类判据只看 transcript uuid 集，故 metadata-only 判定 = incoming 的 transcript uuid 集为空。

**保守取向**：只要 uuid 有任何交集就走 incremental（合并），不判 reuse；disjoint 且双方非空才 keep-separate。降低误把续写判成重用而拆散的风险。

## 4. 智能决策推荐层 `reconcile::advisor`

对每个分歧条目产出一条 `Recommendation`，喂给「建议单」报告与逐条确认流程：

| 字段 | 内容 |
|---|---|
| **分类** | metadata-only / incremental / reuse / conflict / new |
| **证据** | uuid 交集数、双方时间窗、DAG 根与悬挂父链、compaction/summary 标记有无、大小、mtime |
| **推荐动作** | merge / keep-separate(rename) / newest-wins / passthrough-restore / keep-both |
| **置信度** | High / Medium / Low |
| **理由** | 一句人话说明判据 |

**置信度启发式：**

- metadata-only（正文全在 base）→ **High**：正文零风险，只叠新标题/模式。
- incremental（incoming 是 base 超集，DAG 无冲突）→ **High**：uuid 并集无丢失。
- reuse（uuid 全disjoint + 时间窗不相交 + 无 summary 桥 + 根父链悬空，四证齐备）→ **High** 判重用 → 推荐 keep-separate。
- 暧昧（部分交集 / 检测到 DAG 桥 / 一侧疑似截断 / 时间窗交叠）→ **Low** → 推荐 keep-both，交用户定。

**执行策略 = B（从不自动执行）：**

- `reconcile <name>` 交互式：先打印建议单，再**逐条**征询确认（接受推荐 / 改选 keep-both / 跳过），仅落盘用户确认的条目。
- `--dry-run`：只出建议单，不提示、不动手。
- 非交互且无确认来源 → **拒绝改动**，只输出建议单（B 禁止任何自动落盘）。
- 全程 stash 预镜像、可回滚；置信度只影响推荐排序与措辞，**不**改变"必须确认"这一铁律。

## 5. 组件架构

**5.1 `reconcile::session_merge` —— 纯合并核（无 IO，全单测）**

输入 base+incoming 的原始行 + 解析 JSON；输出 `Merged(Vec<RawLine>)` | `KeepSeparate` | `Unchanged`。按 §3 分类并产出合并后的行序（metadata-only/incremental）或"保持分离"裁决（reuse）。确定性、可用 fixture 完整覆盖。

**5.2 `reconcile::advisor` —— 决策推荐（无 IO）**

在 session_merge 分类之上附加证据采集与置信度评估（§4），产出 `Recommendation`。纯函数，可单测。

**5.3 `reconcile::orchestrator` —— IO，接入 enable 流程**

前置：项目 STOPPED（未挂载）、backing 已提交、orig 在、underlay 非空。真源 = `.zipfs-orig`（plain）；backing 按变更文件经 `ingest` 派生。所有改动**先 stash 预镜像**到 `~/.claude-zip/reconcile-stash/<name>/<ts>/`（可逆）。逐 underlay 条目对照 backing/orig：

- `*.jsonl` 两侧都在 → session_merge：`Merged`→写 orig + 重灌该文件；`KeepSeparate`→base 保留、incoming 落改名兄弟 + 灌入；`Unchanged`→丢弃 underlay 副本。
- `*.jsonl` underlay 独有 → new → 并入 orig + 灌入。
- sidecar 目录 `<uuid>/`（subagents）→ 文件级 newest-wins（mtime），冲突改名保留两份。
- 遮蔽了 backing **symlink** 的条目（memory）→ **透传恢复**（§6，例外规则）。
- 其他非-jsonl → newest-wins（mtime），冲突改名保留两份。

产出**重合并报告**：每条的分类 + 动作 + 每个改名/stash 项，确保无一静默。所有确认条目处置完毕后 underlay 清空 → 可安全挂载。清理陈旧 pid/lock。

**5.4 挂载前 underlay 守卫（失败即拒）**

`remount`/autostart/`apply` 的 mount 分支：underlay 非空即拒绝挂载，报错指向 `enable reconcile <name>`。永久堵住静默盖住缺口。

**5.5 CLI / TUI**

- `zipfs enable reconcile <name> [--dry-run] [--rebuild]`：`--rebuild` = 全量重灌兜底（把 golden ⊕ 已确认合并树重建 orig+backing，用于 backing 可疑时）。
- `enable list`：STOPPED 且 underlay 非空 → 标 `NEEDS-RECONCILE`。
- TUI：同标记 + 逐条建议复核动作。

## 6. memory 透传恢复（例外规则）

memory 本应是**单份透传软链**：ingest 在 backing 里按软链重建（`ingest.rs`），shadow store 透明服务 `readlink`（`shadow.rs`）→ 挂载时 `projects/…/memory` 即软链，Claude 跟随它，读写只落唯一的 canonical `docs/memory`。停用期软链缺失才写出了真实目录的第二副本。重合并须**恢复单份透传**，而非留两份：

- underlay 里 canonical 目标**不存在**的新文件 → 复制进目标（找回停用期新增的记忆条目）。
- underlay 里与目标**同名但内容不同**的文件（如 `MEMORY.md`）→ **不合并**；把 underlay 版本改名 `MEMORY.md.underlay-<ts>` 放到目标旁**保留两份**，canonical 原版不动。
- 处置完删除 underlay 的 `memory/` 目录 → 软链重新生效。

## 7. 错误处理与数据安全

- **Stash 先于改动**；每次写 fsync；失败留 stash + 部分 backing 可恢复，报告如实说明进度。
- **落盘顺序**：underlay 条目的重合并内容**先durably进 orig+backing 并校验**，才删该 underlay 条目——绝不先删后写。
- 不可解析行 verbatim 保留；冲突一律 keep-both（改名），永不静默。
- reuse 检测保守（有交集即合并，不拆散）。
- 执行策略 B：无用户逐条确认，绝不落盘。

## 8. 测试

- **单元 · session_merge**：分类真值表 + 三个**真实 fixture**（373e2835 metadata-only、de756008 metadata-only、925fc3a1 reuse）+ 合成 incremental（incoming 为 base 超集）用例；verbatim 保留坏行；newest-wins 元数据。
- **单元 · advisor**：各分类的证据/置信度断言（尤其 925fc3a1 四证齐备 → High reuse）。
- **集成（tempdir）**：orchestrator 覆盖全部条目类型——metadata-only 合并、reuse 改名、new 并入、sidecar 目录冲突改名、memory 透传恢复（含 `MEMORY.md.underlay-<ts>` 保留）、stash 可回滚。
- **守卫**：remount 对非空 underlay 失败即拒；reconcile 后 remount 成功。
- **字节校验**：重合并后 golden 各行 ⊆ 结果（正文无丢失），坏行仍在。

## 9. 落地本次事故（neighbors）

特性合入后：`enable reconcile -home-xp-src-neighbors --dry-run` → 复核建议单（预期三个 jsonl 均 High、memory 走透传恢复）→ 逐条确认实跑 → 字节校验 → `remount` → 确认 ACTIVE 且内容一致。ghc2api-go 本轮不动（数据安全存于其 `.zipfs-orig`）。
