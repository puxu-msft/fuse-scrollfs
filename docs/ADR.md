# scrollz 架构决策记录 / ADR（Decision Log 形态）

> **本文回答「为什么」**——决定了什么、为什么、现在还算不算数。把散落在 [00-overview.md](./00-overview.md) §9、[01-scrollz-design.md](./01-scrollz-design.md) §13、[03-target-data-scope.md](./03-target-data-scope.md) §4、[ROADMAP.md](./ROADMAP.md) 决策门里的「已定/裁决」收敛成一处运行台账。
>
> **形态说明**：采用 flat ledger（一行一决策）而非 adr/NNNN 一决策一文件——因本项目决策多为**工程实测裁定**（如去重 31x、字典 16x 均被实测推翻），台账形态更利于高频增补与推翻标注。语义上等同 ADR，故文件名对齐为 `ADR.md`。
>
> **改动约束**：源于**用户决策**的条目（如 G1–G3 布局取向、范围分层）改动需用户明确同意；源于**实测裁定**的条目可由工程复测更新，但须在 §3 留存旧结论与推翻原因。
>
> **职责边界（避免再造「单一信息源」冲突）**：
> - 本文（ADR）= **为什么** / 决策的当前状态（flat ledger，一行一决策）。
> - [ARCH.md](./ARCH.md) = **是什么 / 在哪里**（当前架构骨架视图）。
> - [DESIGN.md](./DESIGN.md) = **怎么做**（跨模块内部设计导航）。
> - [ROADMAP.md](./ROADMAP.md) = **下一步做什么**（T0–T4 优先级 + 开放决策门）。
> - [TRACKING.md](./TRACKING.md) = **正在做什么**（跨会话 WIP 指针，不复制状态）。
> - [BACKLOG.md](./BACKLOG.md) = **推迟做什么**（Stretch / 研究 / 低优先搁置）。
> - [CHANGELOG.md](./CHANGELOG.md) = **建成了什么、何时**（实现/实测进展日志）。
> - 编号 docs（00/01/…）= 各自领域的 intent / 设计 / spec 快照（多为冻结历史，不回改）。
>
> 决策一旦被实测推翻或被新决策取代，**不删除**——挪到 §3 存档并标注取代者，保留「为什么当时那样想」。

## 1. 生效决策

| # | 日期 | 决策 | 为什么（一句） | 出处 / 详情 |
|---|---|---|---|---|
| D1 | 2026-06-27 | 路线 B = **Rust 自研** FUSE，以 `fuser`+`zstd`/`lz4_flex`(+可选 `redb`/`rusqlite`) 为积木；**布局 V + 布局 S 都做** | 无成熟读写透明压缩 FUSE 成品 | overview §6/§9、design §1 |
| D2 | 2026-06-27 | 负载**读写并重** → **排除** SquashFS/DwarFS 等只读方案（仅作参考） | 目标是活跃读写目录 | overview §9 |
| D3 | 2026-06-27 | 判据 = **压缩比/吞吐/随机写延迟三者并重**；结论形态 = **场景适配表**，非单一冠军 | 不预设路线高低 | overview §4.6/§9 |
| D4 | 2026-06-27 | 压缩算法 = **zstd 多等级 + lz4(`lz4_flex`)对照**，`--algo` 切换 | 比值/速度两端对照 | design §13（注：lz4 codec 仍 unimplemented，见 ROADMAP T3） |
| D5 | 2026-06-27 | **仅 Linux/WSL 原生目录**，不覆盖 Windows `/mnt/c` | /mnt/c 性能/语义另类，另开 | overview §9、design §13 |
| D6 | 2026-06-27 | 容器后端 = **redb 全包 + 默认块 + 写批处理**；sqlite 作空间敏感备选；自写数据区仅大块档触发评估（见 G2） | microbench 实测 redb 吞吐够用（纯 Rust/ACID，选它非因胜过 sqlite——两者量级相近；写批处理 vs 每写一事务才是 8–18x 那档收益） | design §13/§6.1、[exp/container-backend-selection/REPORT.md](../exp/container-backend-selection/REPORT.md) |
| D7 | 2026-06-28 | **目标范围分层**：Tier 1 首要 `projects/*.jsonl`+append 日志(8GB) / Tier 2 后续 `file-history`(524MB) / **排除** plugins+已压缩媒体 | 聚焦 append-only jsonl 放大 zipfs 差异化 | [03-target-data-scope.md](./03-target-data-scope.md) §4 |
| D8 | 2026-06-28 | 默认块大小 **64KiB → 1MiB** | 64KiB 砍掉长程冗余（Shadow 5.43x→13.7x） | ROADMAP T3、提交 18b2d25 |
| D9 | 2026(定调) | **hardlink 正式不支持**（保持 `ENOTSUP`，布局 S 一文件=一 archive，无 inode-id 命名层） | 命名层成本 vs 收益不划算 | ROADMAP T1 |
| D10 | 2026-07(实测后) | **LDM（zstd 长程匹配）保守 opt-in**：`DEFAULT_SEAL_CHUNK=8MiB` 不变，LDM 仅在 `seal --seal-chunk >8MiB` 时开 | 64MiB 档 +5~16% 兑现，但默认档收益≈0 且冷读 RMW 放大 | ROADMAP T3、提交 e9643b6/993ed72、[bench/results/ldm-ratio/REPORT.md](../bench/results/ldm-ratio/REPORT.md) |
| D11 | 2026-06-28 | **共享字典默认关**（opt-in `--dict`/`train-dict`） | 真实路径收益次于纯大块 | ROADMAP T3、提交 96e69a9/df47794 |
| D12 | 2026-07-11 | **Cargo workspace + crates/zipfs + zipfs-bench + exp/**；`archive.rs`/`orchestrator.rs` 拆子模块；rwfs/shadow/container/lifecycle 不拆 | PoC 转正遗留骨架现代化 | [plan/workspace-restructure.md](./plan/workspace-restructure.md)、[CHANGELOG.md](./CHANGELOG.md)（2026-07-11） |
| D13 | 2026-07-18 | 项目由原名 `zipfs` 改名为 `scrollz`；构建/FUSE/指标/磁盘后缀/env/systemd/docs/bench 全量跟随；仓库根仍为 `/home/xp/src/zipfs`；`CLAUDE_PROJECTS` 保留；`ZIPFS_HOME` 仅兼容回落；backing 默认迁至 `~/.local/claude-scrollz` | 避免名称与通用 ZIP/文件系统概念混淆，同时以一次性一致改名消除长期双品牌；六项分叉均由用户定稿 | [scrollz-rename-plan.md](./scrollz-rename-plan.md) §0.2/§2 |
| D14 | 2026-07-18 | 归档 `MAGIC=b"ZIPFSAR\x01"` 与 `SB_MAGIC=b"ZSB2"` 永久兼容冻结，品牌改名不得改字节 | 改魔数会使全部存量归档拒读，等价于数据不可访问 | [scrollz-rename-plan.md](./scrollz-rename-plan.md) §1.2/§2.G |

## 2. 开放决策门（详情与状态在 ROADMAP）

| 门 | 待决 | 触发/条件 |
|---|---|---|
| **G1 布局取向** | V / S / 两者并存（按场景） | T0 收尾评估补齐后，在 CONSOLIDATED 落定 |
| **G2 自写数据区** | 是否放弃 redb 全包、自写 extent 数据区 | 仅当 redb 在真实规模空间/性能不达标（microbench 已给 256KiB 红线）；**默认不做** |
| **G3 去重投入** | V 全局去重（内容寻址）是否进主线 | G1 选了含 V 后再定；**先做编码侧 `--long`**，dedup 价值由 CDC 命中率实测裁定 |

## 3. 被取代 / 被实测推翻（存档，勿再当既得）

- **~~容器后端未定（redb vs rusqlite）~~** → 被 **D6** 取代（microbench 裁定 redb）。
- **~~默认 64KiB 块~~** → 被 **D8** 取代（1MiB）。256KiB 档触发 redb 膨胀 2.75x 红线，不默认。
- **~~跨会话去重「远超 31x」~~** → **实测推翻**：定长块 0% 命中、同目录拼接增益仅 1.0x，冗余主在**文件内**；去重价值降级为「待 CDC 实测的潜力」（G3）。且 31x 是单 838MB jsonl **单流**数，FS 级实测仅 5.4/13.7x。
- **~~共享字典 CLI「16x」~~** → **实测推翻**：单文件过拟合；真实路径 64K+字典 10.24x 仍输纯 256K 11.2x（见 D11）。
- **~~`fuse/` 为主 crate 目录名~~** → 被 **D12** 取代（`crates/zipfs/`，"fuse" 是 PoC 路线名残留）。
