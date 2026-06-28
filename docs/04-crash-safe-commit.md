# 崩溃安全提交协议：双 superblock + 不可变块 + in-archive 尾日志（设计 spec）

> 文档性质：**实现设计（how）**，根治 `bench/results/crash-durability/REPORT.md` 实测到的 durability bug。
> 承接 [01-zipfs-design.md](./01-zipfs-design.md) §7（archive 格式）、§10（一致性），并**吸收并取代** [02-layered-chunking.md](./02-layered-chunking.md) 的 footer v2 head-cache 字段（合入本协议的 superblock，避免双格式分叉）。
> 日期：2026-06-28。状态：草案，**格式改动前的 gate**；实现以崩溃 harness（`bench/scripts/crash-test.sh`）40%→0% 为验收门。

## 0. 为什么不是缓解而是根治

实测 bug 的根（REPORT §2）是两个被违反的原则，必须各自从结构上消除，而非缩小窗口：

1. **提交非原子**：`ArchiveUpdater::set_block` 的 reuse-tail-slot 先 `set_len(offset)+sync_data()` 销毁唯一 durable 旧版本，再写新块+footer。崩溃落在中间 → 数据 durably 蒸发。
2. **durability 粒度 ≠ 压缩粒度**：压缩块「全有或全无」不可追加，但 fsync 要按行持久化 → 每次 fsync 整块重压重写块 0。reuse-in-place 正是为省这份重写代价而生的 hack，它顺手破坏了原则 1。

> **为何不引库**（已核实，见会话）：`fjall`/`sled`/`redb`/`okaywal` 全是「容器/KV/自管 WAL 文件」模型 = 布局 V。布局 S 的立身不变量是「每逻辑文件 = 后端一个真实 archive 文件」，无法外包给容器库而不沦为布局 V。崩溃安全在布局 S 必须自实现；唯一对口的库级复用是 **`crc32fast`**（替换手搓逐位 crc32）。本 bug 同时是 **G1 的新证据**：崩溃安全在布局 V（redb）免费，给 V 加分——但 S 压缩比/随机读领先，故 S 仍做根治。

## 1. 三件套（互相耦合，缺一不可）

| 件 | 治什么 | 机制 |
|---|---|---|
| **双 superblock 固定槽** | 提交原子性（原则 1） | 两个定长 commit 记录在**文件头部固定偏移**，带单调 `seq`+CRC，交替写。open 取「**级联校验通过**且 seq 最大」者（§4 M4：sb_crc→index_crc→journal 可重放，三者全过才算可用）。半截写总留另一槽完好 → 永远能恢复到上一原子提交点。 |
| **不可变封块 + append-only** | 不销毁 live 数据（原则 1） | 新块/新 index/新 journal 记录/新 head 缓存**一律追加到 `write_cursor`**；封块即不可变；**绝不 `set_len` 截 live 数据、绝不原地覆盖**。被取代旧区成空洞，**只能由压实（temp+rename 整文件重写）回收**。 |
| **in-archive 原始尾日志** | durability/压缩解耦（原则 2） | 未封尾块以**原始字节**追加进 archive 内的日志区，fsync 只**增量追加自上次以来的新字节**（O(delta)，不重压不重写整块）。封块（块满）时一次性压成不可变块、重置日志。 |

> **核心不变量（架构审查 C2，CRITICAL——不写死这条则「忠实实现也丢数据」）**：`write_cursor` 必须**始终 ≥ 两个 superblock 中任一槽所指向区间的最大 end**（含 data / index / journal / head 缓存）。新写一律落到该 end 之后；**任何被某个仍可能被选为活跃的 superblock 可达的字节，在下一次 barrier 2 成功前绝不被覆写**——否则「append 覆写活跃槽仍依赖的旧 index/journal」就是原 `set_len` 销毁 live 数据的同构再现（凶器从 set_len 换成 append 覆写）。旧空洞**仅压实（§6，整文件 temp+rename）可回收，绝不在线就地覆写**。本不变量覆盖**全部元数据区**，不只数据块。

## 2. 格式（单一格式，不背向后兼容）

> **简化（据 archive.rs 既有约定「项目无历史 archive，单一格式」）**：fixture/bench 后端均可重生成，故**无需 v1 EOF-footer 兼容读路径**。直接把 commit 点从 EOF footer 搬到头部双 superblock，`VERSION` 升 2 仅作健康校验（遇旧版报损坏）。去掉 spec 初稿的双格式分支，实现更干净。

```text
偏移 0    [magic(8) | version=2(4)]                         ← header 12B，magic 不变
偏移 12   [superblock A : SB_LEN]                           ← commit 槽 A（固定偏移）
偏移 12+SB_LEN [superblock B : SB_LEN]                      ← commit 槽 B（固定偏移）
...       [数据区：不可变压缩块，append-only]                 ← 旧版本成空洞，压实回收
          [尾日志区：原始字节记录 append-only]
          [index 区：每次提交 append 一份新 index]
          [head 缓存区：压缩首 64KB（吸收自 docs/02）]
EOF       （无尾部 footer——commit 点搬到头部双 superblock）
```

### 2.1 superblock（定长，二选一活跃）

| 字段 | 类型 | 说明 |
|---|---|---|
| `sb_magic` | u32 | superblock 标识，区分未初始化槽 |
| `seq` | u64 | 单调提交序号；open 取最大且 CRC 合法者 |
| `chunk_size` | u32 | |
| `uncompressed_size` | u64 | 逻辑文件大小（含尾日志未封部分） |
| `chunk_count` | u64 | 已封块数 |
| `index_offset` / `index_len` | u64×2 | 本次提交的 index 区位置 |
| `index_crc` | u32 | index 区 CRC（crc32fast） |
| `tail_journal_offset` / `tail_journal_len` | u64×2 | 尾日志区位置与长度（0=无未封尾） |
| `head_cache_offset` / `head_cache_clen` / `head_cache_rawlen` | u64×3 | 吸收自 docs/02；0=无 |
| `sb_crc` | u32 | 覆盖以上全部字段的 CRC（crc32fast）；半截写靠它检出 |

SB_LEN 取 128B（对齐、留扩展位）。两个 superblock 各 128B，固定在 header 之后。

### 2.2 尾日志记录

```text
[rec_len(4) | rec_crc(4) | raw_bytes(rec_len)]   × N 条
```
- 每次 fsync 把「自上次 fsync 以来新追加的原始字节」作为**一条记录**追加到尾日志区末尾，更新活跃 superblock 的 `tail_journal_len += 记录总长`，写 superblock，fsync。
- `rec_crc` 让重放时检出半截写的最后一条（截断到最近完整记录，fail-closed）。

## 3. 提交协议（原子性的关键：写序 + barrier）

一次 `commit`（fsync/flush 触发）：

1. **追加新数据**：若有新封块 → 写到 `write_cursor`（数据区末尾，不碰 live）。若尾块有新增原始字节 → 追加一条尾日志记录。**barrier 1**：因 journal/数据是**扩文件尾**（长度增长属元数据），用 `sync_all()`（= `fsync`，非 `sync_data`/`fdatasync`——后者不保证 i_size 元数据落盘）。
2. **追加新 index**（若块集变化）+ 新 head 缓存（若块 0 变）到末尾。`sync_all()`。
3. **写非活跃 superblock 槽**（A/B 交替，固定偏移，**原地覆写但不是 live 数据区**）：填 `seq = 旧 seq + 1`、新 index/journal/head 指针、`sb_crc`。`sync_all()`（**barrier 2：原子提交点**）。

**barrier 契约（架构审查 C1，CRITICAL）**：每个 `sync_all()` **必须检查返回值**；任一 barrier 失败（EIO/ENOSPC）则**立即中止本次 commit、不写/不推进 superblock**，活跃槽保持旧 seq（上一致版本不受损）。superblock 落盘前其指向的全部区间必经 barrier 1 落盘——这是「活跃 superblock 不指向未落盘数据」的唯一防线，故 barrier 顺序 + §1 核心不变量缺一不可；任何弱化 barrier 的 FS 配置（如 ext4 `data=writeback`）都会破坏 durability，须文档化为部署约束。

崩溃落点分析（皆 fail-safe）：
- barrier 1/2 之前崩 → 活跃 superblock 仍指旧 index，旧数据完整可读（新追加字节是末尾空洞，被忽略；§1 不变量保证它们没覆写任何旧活跃可达区间）。
- 写 superblock 半截崩 → 该槽 `sb_crc` 不符 → open 取**另一槽**（上一 seq）→ 恢复到上一致提交点。
- **任何时刻盘上都有 ≥1 个级联校验通过的 superblock 指向一份完整数据**——set_len 销毁数据的整类窗口结构性消失。

## 4. 打开与恢复

1. 读 header → version。`version != 2` → 报损坏（无历史 archive，不背兼容）。
2. 读两个 superblock。**「槽可用」= 级联校验全过（架构审查 M4）**：`sb_magic` 合法 → `sb_crc` 自洽 → 其 `index_offset/len` 指向的 index 区 `index_crc` 通过 → 其 `tail_journal_*` 可完整重放（见 4）。任一环节失败则该槽**不可用**，尝试另一槽。
3. open 取**「可用且 seq 最大」**者为活跃；**两槽都不可用才报损坏**（避免「sb_crc 过但 index 坏」时白丢另一个本可用的旧槽）。
4. 重放尾日志（`tail_journal_len>0` 时）：**先对每条 `rec_len` 与剩余 `tail_journal_len` 做 bounds 校验**（防越界/OOM，复用 archive.rs 既有约束）→ 逐条校 `rec_crc`，遇坏即停 = 最近完整前缀。「遇坏即停」仅在「§1 不变量 + §3 barrier 保证损坏只可能在尾部」前提下正确（中间记录损坏意味着不变量已被破坏，属实现 bug，非正常路径）。
5. 逻辑文件 = 已封块解压 ⧺ 尾日志重放字节；`uncompressed_size` = Σ封块 rawlen + 重放字节数（二者在单一 superblock 视图下**互斥覆盖**，不重叠）。

## 5. 封块（seal）

尾块原始字节累计达 `chunk_size`（或 `zipfs seal` 显式触发）：压成不可变块 append 到数据区 → 新 index 记该块 → **新 superblock 置 `tail_journal_len=0`（逻辑上忽略旧日志区，旧字节原位不动、由压实回收——绝不物理清零/截断旧日志区，否则旧 superblock 回落时会重放被清空的区间，H2）** → 走 §3 提交。封块提交后该段逻辑字节从 journal 迁到封块，二者在新视图下不同时计入 `uncompressed_size`（由 `sb_crc` 覆盖 `chunk_count`+`tail_journal_len` 保证自洽快照）。

## 6. 空间回收（压实）

append-only + 不可变块 → 被取代的旧块/旧 index/旧 journal/旧 head 缓存成空洞，文件单调增长。**压实**（卸载时或后台）：顺序重写为紧凑布局（活跃 superblock 指向的 live 块 + 当前 journal）到 temp，原子 rename 覆盖。要点（架构审查 H3）：
- **并发互斥**：压实全程持该 inode 写锁（或仅在 `commit_session` 内联触发），杜绝并发 journal 追加被快照漏掉而在 rename 后丢失。
- **rename 后 `fsync` 父目录**：保证崩溃后看到的是新文件而非旧文件（rename 持久化需目录项落盘）。压实中崩溃 → temp 被丢弃、原文件完好（唯一可接受结局）。
- **seq 单调不重置**（架构审查 C3）：压实后两槽 seq **继续递增**（u64 永不耗尽），避免「两槽 seq 相等」歧义。
- 尾日志让「每 fsync 不再 append 整块」，物理增长稳态 O(实际新数据)；但 **journal 记录条数 = 两次封块间的 fsync 次数**（M3：1MiB 块 / 32B 行 ≈ 数万条），open 重放成本与记录头开销随 fsync 频率线性增长——这是已知 trade-off（重放仅 CRC 校验，ms 级；非 O(数据)）。

## 7. 与既有代码的接口变化

- `archive.rs`：`Footer`（EOF）→ `SuperBlock`（双固定槽）；`ArchiveWriter::finish` 写双 SB；`ArchiveReader::open` 读双 SB 选活跃 + 重放尾日志；`ArchiveUpdater`：删除 reuse-tail-slot 整段（`set_len`+原地覆盖），改为「append 新块/append 尾日志记录/append index → 写非活跃 SB」。手搓 `crc32` → `crc32fast`。
- `shadow.rs`：`commit_session` 改调新 updater 的 append 语义；尾块缓冲（wsession）封块时把原始字节交给尾日志而非每次整块重压。
- `core/wsession.rs`：seal 时机不变；新增「fsync 时把尾块原始增量交尾日志」的路径（这正是写放大根治）。

## 8. 测试门（TDD 顺序）

0. **崩溃 harness 现状 = 40% 丢数据**（已实测，回归基线）。
1. superblock 编解码 + 双槽选活跃（纯函数单测）：精确字节布局 round-trip、`sb_crc` 检出半截、级联校验（M4）、seq 选择与 tie-break、`version!=2`/坏 magic 报损坏（**负向**，非「兼容读 v1」）。
2. 尾日志记录编解码 + 重放（截到最近完整记录）。
3. `ArchiveUpdater` append-only 提交 + open 恢复（round-trip + 构造性崩溃单测：barrier 1/2 各点崩 → 恢复上一致版本，**绝无 set_len 丢数据**）。
4. `crc32fast` 替换后全量回归。
5. **端到端崩溃 harness 转绿**：`crash-test.sh` 多次跑 **0% 丢数据**——校验收紧为 **`survived-1 == acked`（精确）且断言无中段缺失**（架构审查：原 `>=` + 仅查「连续前缀」会漏放 H1 类中间丢失）。这是本设计的最终验收门。

## 9. 不在本协议（记录边界）

- 多文件事务 / 跨文件原子 rename（布局 S 的 rename 仍走底层 FS）。
- 布局 V（container/redb）无此问题（ACID 自带），本协议仅治布局 S。
- head 缓存的**读快路径**（docs/02 §4.2 的 `read_head_cache` + read_range 快路径）正交，其**存储字段已并入 superblock**，读路径实现可在本协议落地后接续。

## 10. 架构审查纪要与修订（2026-06-28，ecc-fix:architect）

审查结论：主结构方向正确（append-only 根治 set_len 整类窗口、双 SB 提供原子提交点），但初稿留白会让「忠实实现也丢数据」。已逐条修订入正文：

- **C2（CRITICAL，已修入 §1 核心不变量）**：append-only 不变量初稿只声明「数据块」，未覆盖 index/journal/head 缓存元数据区 → 新 journal 可覆写活跃 SB 仍指向的旧 index → 同构 bug 复活。修：不变量覆盖**全部可达区间**，旧空洞仅压实回收、绝不在线覆写。
- **C1（CRITICAL，已修入 §3 barrier 契约）**：barrier 须检查返回值、失败不推进 seq；journal 扩文件尾是元数据增长，改用 `sync_all()`(=fsync) 而非 `sync_data()`；明确「barrier 顺序 + 不变量」是 superblock 不指向未落盘数据的唯一防线，弱 barrier FS 配置为部署约束。
- **M4（已修入 §4）**：「槽可用」从「仅 sb_crc」改为**级联校验**（sb_crc→index_crc→journal 可重放）；两槽都不可用才报损坏，避免白丢可恢复旧槽。
- **C3（已修入 §6）**：seq 单调不重置（含压实后），杜绝两槽相等歧义；create 时两槽写 seq=0 合法空 SB（任何 ack 都对应一次成功 barrier 2）。
- **H1（已修入 §4.4）**：重放先做 `rec_len` bounds 校验；明确「遇坏即停」仅在不变量保证「损坏必在尾部」前提下成立。
- **H2（已修入 §5）**：seal「清空 journal」= 逻辑置 `tail_journal_len=0`，**绝不物理清零旧日志区**（否则旧 SB 回落重放被清空区间）；给出 `uncompressed_size` 唯一计算式与「封块/journal 互斥覆盖」。
- **H3（已修入 §6）**：压实持写锁互斥并发追加；rename 后 fsync 父目录。
- **M2（已修入 §11）**：head 缓存为**可丢弃派生数据**——解压失败静默回退逐块路径，**绝不**因缓存损坏 fail-closed 整个文件。
- **M3（已记入 §6）**：journal 记录数 = fsync 数，open 重放/头开销随 fsync 频率线性增长，作已知 trade-off。
- **遗漏 6（架构决策）**：评估「单 SB + 全量 journal（log-structured）」替代「双 SB + 尾日志」。**裁决：保留双 SB + 尾日志**。理由：单 SB+全量 journal 把封块也写进 journal，open 须重放「自上次 checkpoint 以来全部变更」，重放成本与 journal 体积更大（M3 恶化），且仍需一个原子 checkpoint 指针（等于把双 SB 问题搬进 checkpoint）。双 SB 给 O(1) 开包、把重放界定在「两次封块间的尾增量」，更契合 append 负载。但二者功能确有重叠——若未来 open 重放成为瓶颈，再评估收敛为单一 journal。
- **harness 收紧（已记入 §8.5 验收门）**：`crash-test.sh` 校验从 `survived-1 >= acked` 收紧到 **`== acked` 且断言无中段缺失**，以抓 H1 类「中间丢失但仍是连续前缀」的漏网。

## 11. head 缓存的崩溃语义（M2）

head 缓存（发现读快路径，字段并入 superblock）是**可丢弃的派生数据**，非权威状态：
- 其压缩字节**无独立 CRC**（仅指针受 `sb_crc` 保护）。读时若解压失败 / 越界 → **静默回退逐块路径**（`read_range` 既有路径），**绝不** fail-closed 整个文件。
- 块 0 改写时随 commit 重建（docs/02 §4.3）。一个可重建的读优化缓存损坏，绝不能拖垮文件可打开性。
- 可选增强：给 head 缓存也加一个 crc 字段进 superblock，命中校验失败即回退——首版可不做。
