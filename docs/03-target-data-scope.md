# zipfs 目标数据范围（Target Data Scope）

> 文档性质：**界定 zipfs 承载什么数据、分几期**。意图见 [00-overview.md](./00-overview.md)，实现见 [01-zipfs-design.md](./01-zipfs-design.md) 与 [02-layered-chunking.md](./02-layered-chunking.md)，路线见 [ROADMAP.md](./ROADMAP.md)。
> 实测日期：2026-06-28（`~/.claude` 全量扫描）。

## 1. 为什么要界定范围

`~/.claude` 总量约 **12 GB**，但「大」与「适合 zipfs」是两回事。zipfs 的研究价值在**灵活可定制**——per-file 透明、append 尾块缓冲、不可压缩 verbatim 跳过、冷文件 `seal` 重压、可选共享字典/去重。这些只在**单调 append-only、高冗余、可压缩**的数据上才兑现收益；对可重装代码、已压缩媒体则是纯开销。故必须**圈定范围**，而非「整个 `~/.claude` 一锅端」。

判据：**(可压缩性 × 体量 × append/冷归档特性 × 不可重得性)**。可重装的、已压缩的、非 append 的，即使大也排除或原样存。

## 2. `~/.claude` 全量实测（2026-06-28）

### 2.1 顶层目录按体量

| 顶层 | 体量 | 主要内容 |
|---|---|---|
| **projects** | **8.8 GB** | 会话 transcript，主 `.jsonl`（8.0 GB / 2202 文件）+ `.txt` + `.json` |
| plugins | 1.8 GB | 已安装插件代码：`.js`/`.ts`/`.map`/`.mjs`（~5 万+ 小文件）+ `.pack`(194 MB git 包) |
| file-history | 524 MB | 文件编辑快照历史 |
| my / session-env / scripts / … | 各 <10 MB | 杂项 |
| `cost-tracker.log` / `bash-commands.log` | 4.7 / 4.4 MB | **纯 append 文本日志** |

### 2.2 按扩展名聚合体量（top）

| 扩展 | 体量 | 文件数 | 性质 |
|---|---|---|---|
| `.jsonl` | **8093 MB** | 2202 | **append-only transcript，高可压（~31x）** |
| `.txt` | 799 MB | 4325 | 工具输出，可压 |
| (无扩展) | 564 MB | 14083 | 多为 plugins/git 小文件 |
| `.js` / `.ts` / `.mjs` / `.map` | ~610 MB | ~12 万 | 插件代码，碎文件，中等可压，**非 append** |
| `.pack` | 194 MB | 18 | git 包，**已压缩（不可压）** |
| `.png` / `.pdf` | ~168 MB | — | **已压缩媒体（不可压）** |
| `.md` / `.json` | ~130 MB | ~2.4 万 | 可压文本 |

### 2.3 极端样本（巨型 jsonl）

单个 openvmm 会话 transcript 高达 **838 MB**；openvmm 一个项目就 ~2.7 GB、dpdk-bench ~0.6 GB。**少数重项目占据大头**，且这些巨型 jsonl 多为**冷数据**（旧会话写完即不再追加，只偶尔读）——正是 `seal` 大块重压的甜点。

## 3. 按数据类型的 zipfs 适配度

| 适配 | 数据 | 理由 |
|---|---|---|
| ★★★ **理想** | `projects/*.jsonl` | append-only、高可压、跨会话高冗余、per-file 透明价值大 |
| ★★★ **理想（小）** | append 日志（`cost-tracker.log`/`bash-commands.log`） | 纯 append 文本，理念完全契合（体量小，象征意义 > 收益） |
| ★★ 中（后续） | `file-history` 快照 | 文本可压，但**非 append**、是派生数据（可丢可重建），优先级低 |
| ★ 差 | plugins `.js/.ts/.map` | 可重装代码、碎文件、非 append；压它价值低、拖累基准 |
| ❌ 排除 | `.pack` / `.png` / `.pdf` | 已压缩，zipfs 不可压启发式应 **verbatim 原样存**，压了纯亏 |

## 4. 分层目标（核心决策）

| 期 | 目标 | 内容 | 为什么 |
|---|---|---|---|
| **Tier 1 · 首要** | **`projects/*.jsonl` + append 日志** | 8.0 GB jsonl + 两个 `.log` | zipfs 设计正为之而生：append 尾块缓冲、巨型 jsonl 的 footer 索引增量追加、冷会话 `seal` 重压、跨会话去重潜力。**研究价值最大、收益最实**。 |
| **Tier 2 · 后续** | **`file-history`** | 524 MB 快照 | 文本可压，但非 append、是派生数据。待 Tier 1 跑通、迁移/可靠性成熟后再纳入；需先评估其写模式（快照 vs 追加）。 |
| **排除** | plugins、已压缩媒体 | `.js/.ts/.map/.pack/.png/.pdf` | 可重装 / 已压缩 / 非 append。纳入只增噪声与开销。即便误纳入，verbatim 启发式也应原样存不浪费 CPU。 |

> **「首要 = projects/jsonl + log，后续 = file-history」** 是本项目的正式范围决策（用户 2026-06-28 定）。plugins 与已压缩类明确排除。

## 5. 为什么这个范围放大 zipfs 的研究价值

把范围收到 **单调 append-only jsonl**，恰好让 zipfs 的「灵活可定制」相对内核 btrfs 形成**真实差异化**：

- **跨会话去重（价值未证实）**：openvmm 那 10 个会话概念上共享前缀（系统提示 / CLAUDE.md / 重复文件读取），但**实测定长块去重 0% 命中、同目录拼接整流增益仅 1.0x**——冗余其实主在文件内。内容寻址去重要兑现**必须上 CDC，且命中率尚未实测**；「远超 31x」是未经验证的推测，不应作为既定收益。又：31x 是单 838MB jsonl 单流数，非 8GB 数据集整体，FS 级实测仅 5.4/13.7x（见 ROADMAP T3/G3）。
- **冷会话 `seal`**：会话写完即冷，可用大块 + zstd-19 重压逼近整流上限（实测 shadow 8MiB/zstd-19 ~25–30x），而活跃文件保持 1MiB 随机访问甜点——**冷热分治**是 btrfs 单一 extent 策略做不到的。
- **per-file 透明**：每个 jsonl 是底层 FS 上独立对象，可单独备份/同步/检视/部分恢复；运行时被 Claude Code 实时追加的活跃会话尤其需要这种「不依赖守护存活、不挂载也能取」的特性。
- **无 root / 无内核模块**：WSL 里 btrfs 要 root + modprobe + loop 镜像；zipfs 普通用户 FUSE 即可。

→ **结论**：纯压缩比内核 btrfs（force）能打平甚至更高，但在**这个范围**下，zipfs 靠**冷热分治（seal）+ 透明性 + 无 root**提供 btrfs 给不了的组合价值（去重为**待验证潜力**，非既得收益——见 §5 与 ROADMAP T3）。范围越聚焦 append-only jsonl，zipfs 越站得住。

## 6. 与现有能力的对接

| 能力（已落地/在建） | 对 Tier 1 的作用 |
|---|---|
| 默认 1 MiB 块（提交 18b2d25） | jsonl 长程冗余受益，Shadow 真实 13.7x |
| `seal` 冷封存（bb04640） | 冷会话 jsonl → 大块/zstd-19 ~25–30x |
| 共享字典（96e69a9/df47794，默认关） | 补 boilerplate 长程冗余；实测次于大块，opt-in |
| head 缓存 / 分层分块（02-layered-chunking.md） | 发现读（头尾 64KB）避免解压整 1MiB，降读放大 |
| 去重（T3，G3 门控） | 跨会话前缀去重——本范围的最大潜在杀手锏，**须 CDC** |

## 7. 迁移分期（对接 ROADMAP T4）

1. **Tier 1a**：`projects/*.jsonl` 灌入、校验、切换工具（可逆、零丢失）；活跃会话的实时追加路径压测。
2. **Tier 1b**：append 日志接入（小，验证纯 append 长跑稳定性）。
3. **Tier 2**：`file-history` 评估写模式后纳入。
4. 全程：plugins 与已压缩媒体**不迁入**（留原位或单独 raw 存）。

## 8. 边界与开放问题

- **活跃会话**：当前正被 Claude Code 写入的 jsonl，迁移时机与一致性（不能丢正在写的会话）。
- **日志轮转**：`.log` 若有轮转/截断，与 append-only 假设的交互。
- **去重 CDC**：定长块去重 0% 命中已实测，跨会话去重必须上内容定义分块（CDC）才有效。
- **file-history 写模式**：是追加还是整体重写快照，决定它更适合 S 还是 V，待 Tier 2 评估。
- **plugins 若将来想压**：更适合布局 V 容器打包碎文件，但价值低，暂不投入。
