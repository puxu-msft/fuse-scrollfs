# zipfs 路线图 / Roadmap

> 单一信息源：把散落在 [CHANGELOG.md](./CHANGELOG.md)、各 `bench/results/*/` 报告里的待办收敛于此。
> 优先级 **T0→T4** 递减；状态 ☐ 未开始 / ◐ 进行中 / ☑ 完成。动机一律落到目标负载（`~/.claude/projects`：追加为主、高冗余、运行时活跃写）或已有实测依据。
> 日期：2026-06-28。
> 缺陷侧（两轮审查已修 / 未做 + 判断依据）见 [archive/06-defect-audit.md](./archive/06-defect-audit.md)——已核验闭环、归档。

## T0 · 收尾评估（先让对照结论完整、可信）

当前 CONSOLIDATED 的五条件对照有两个缺口 + 一个口径偏乐观，补齐才算定论。

| 方向 | 为什么 | 工作量/风险 | 状态 |
|---|---|---|---|
| **A(btrfs) 压缩比** | 三判据补全 | 极小 | ☑ **2.44x**（`compress=zstd:3` 启发式；btrfs 跳过 212M 不压） |
| **A 在 `compress-force` 下重测** | 目标负载是 append-only 可压缩 jsonl；btrfs 默认启发式漏压、非最佳。`setup-btrfs.sh` 已默认 `compress-force=zstd:3` | 小 | ☑ **压缩比 6.74x（force）= 第一**，已反转「zipfs 压缩比赢内核」结论；**A 速度列仍是启发式数据、待重测**（次要） |
| **zipfs 在 1MiB 默认下重测压缩比 vs A 6.74x** | 上面 6.74x 反转是基于 zipfs 旧 64KiB（5.42x）；现默认已退役 64KiB→1MiB，ratio-bench 真实路径 Shadow 13.7x **应再反转回 zipfs 领先**。需在同 CONSOLIDATED 口径复测确认 | 极小（ratio-bench 已就绪） | ☐ **关键：可能再次反转 G1 依据** |
| **B2（`fuse-zstd` 整文件）消融** | §9 矩阵的「分块 vs 整文件」参照项未跑，缺「分块价值」的外部实证 | 小（装并挂 fuse-zstd 跑一遍） | ☐ |
| **冷缓存复跑** | 现全热缓存（无免密 sudo drop_caches），读数偏乐观，磁盘真实态未知 | 小（需 root drop_caches） | ☐ |
| **多轮中位数** | 默认已降为 1 轮省测试量；权威定论需 `ROUNDS=3+` 复跑关键条件 | 小 | ☐ |

**决策门 G1**：T0 补齐后，在 CONSOLIDATED 落「最终布局取向」（V / S / 两者并存按场景），作为后续投入依据。

## T1 · 正确性与可靠性（活跃写负载的前提，丢会话日志不可接受）

| 方向 | 为什么 | 工作量/风险 | 状态 |
|---|---|---|---|
| **archive per-block CRC** | 现靠 `set_len+sync` 构造性 fail-closed；每块 CRC 是更稳的根治，杜绝静默错读 | 中 | ✅ block_crc（封块）；head 缓存/尾日志暂无 CRC（前者可丢弃、后者 rec_crc） |
| **S 崩溃恢复（双 footer / 扫回最近合法 footer）** | 设计 §10 已承认「完整恢复属后续」；mid-commit 崩溃现在 fail-closed 但不可恢复 | 中 | ✅ 双 superblock + append-only（§8.3） |
| **掉电/崩溃测试 harness** | `kill -9` 守护于写中途，自动验证 fail-closed/恢复，把一致性边界变成可回归测试 | 中 | ✅ crash-test.sh 10/10 0% |
| **fsync 写放大根治（in-archive 尾日志）** | append 微行 + fsync 每次重压整块 → §8.4 尾日志只追原始增量 + remount 重放重建 | 中 | ✅ §8.4 |
| **daemon 健壮 + WSL `[boot]` 自挂载** | 目标负载是 Claude Code 运行时写入，守护必须随 WSL 起、崩溃可重挂 | 中 | ✅ --pid-file + zipfs-mount.sh（幂等/stale 清理）+ systemd unit + wsl.conf 片段；fork 守护化未做 |
| **hardlink 决策** | 现 `ENOTSUP`（`cp -al`/git 会触发）；决定支持（需 inode-id 命名层）或正式不支持 | 中/低 | ✅ 定调：正式**不支持**（保持 ENOTSUP，布局 S 一文件=一 archive，无 inode-id 命名层） |
| **故障注入测试（两层）** | `kill -9` + tmpfs 测不到「fsync 后丢失 / EIO / 撕裂 / 重排」；进程内 `BlockIo` seam 穷举崩溃点 + dm-log-writes 真实块层门，把崩溃安全协议变可回归 | 中/大 | ☑ 已实施（[05-fault-injection-testing.md](./05-fault-injection-testing.md)）：`BlockIo` 接缝 + `FaultIo` 确定性崩溃模拟器（EIO/撕裂/掉电/乱序 × barrier 软化交叉 + 双 SB 非互污）+ shadow `up.sync()` 失败失效专项；Tier 2 dm-flakey / dm-log-writes / container 三脚本（root 门控） |
| **seal.rs 缺父目录 fsync** | seal temp+rename 后未 fsync 父目录（compact.rs 有，不一致）；崩溃后 seal 的 rename 可能丢失 | 小 | ✅ 已修：rename 后 fsync 父目录，与 compact.rs 一致 |

## T2 · 性能（FUSE 用户态对内核的差距）

| 方向 | 为什么 | 工作量/风险 | 状态 |
|---|---|---|---|
| **FUSE 写尾延迟优化** | CONSOLIDATED 指认：FUSE 三条写 p99 ms 级 vs btrfs 亚毫秒，是对内核**最大结构劣势**。方向：异步/批量 commit、writeback cache、FUSE passthrough/io_uring | 大/中 | ◐ 多线程派发（--threads，per-inode RwLock）；writeback/passthrough 待 fuser 升级 |
| **BV 写尾抖动定位** | rand-write-64k p99 抖到 28ms，疑 redb commit/MVCC；profile 定位（曾叫停，待需要时再做） | 中 | ☐（搁置） |
| **读写锁粒度** | append 修复让 `read_range` 持 per-inode 写锁、读写串行；高并发读需改 RwLock/更细粒度 | 中 | ✅ per-inode RwLock（多读并发、写排他堵 torn-read） |
| **mmap（至少只读）** | 与 `direct_io` 互斥，需定写模型后回填；overview 列为 B 核查项 | 中 | ✅ 只读 fd KEEP_CACHE 启 mmap、写 fd 仍 direct_io；跨 fd 并发写陈旧页未保证（待 notify_inval） |

## T3 · 空间与压缩（本负载高冗余，空间是核心收益）

| 方向 | 为什么 | 工作量/风险 | 状态 |
|---|---|---|---|
| **块大小退役 64KiB → 1MiB** | 两套独立基准（ratio-bench 真实路径 + algo-compare）裁定 64KiB 砍掉长程冗余 | 小 | ☑ **已落地**（`DEFAULT_CHUNK_SIZE=1MiB`，提交 18b2d25；Shadow 真实 5.43x→13.7x、Container 1.89x→8.84x） |
| **冷文件封存 seal** | 会话写完即冷、读为归档；活跃块 1MiB 随机访问甜点，冷文件大块重压逼近整流 | 中 | ☑ **已落地**（`zipfs seal` + `src/seal.rs`，提交 bb04640；shadow 8MiB/zstd-19→~25-30x，读路径零改动）。container 封存 + 单块>8MiB 的 --long 留后续 |
| **共享字典压缩** | 用 transcript 语料训练 zstd 字典补回 boilerplate 长程冗余 | 中（研究性） | ☑ **已实现 + 实测：收益次于大块**（提交 96e69a9/df47794；真实路径 64K+字典 10.24x 仍输纯 256K 11.2x；先前 CLI「16x」是单文件过拟合）。保留 opt-in `--dict`/`train-dict`，默认关 |
| **head 缓存（archive v2，发现读）** | 头尾 64KB 发现读现解压整个 1MiB（16x 放大）；源码实证访问面（[[claude-code-session-io-access-pattern]]） | 中偏高 | ✅ **已落地**（rmw 建/shadow 存读/rwfs 快路径 + 单测；discovery-bench HOT 砍 82%） |
| **algo/chunk 自适应** | 按文件类型/可压缩性选等级/lz4；不可压缩媒体走 verbatim | 中 | ◐ 块大小已据实测定 1MiB + `--level` 可配；lz4 codec 仍 unimplemented、自动选择未做 |
| **S 压实（append-only 空洞回收）** | §8.4 尾日志 raw + 旧块成空洞、文件增长；temp+rename 整文件重写回收 | 小/中 | ✅ `zipfs compact --backend shadow`（实测 16x 收缩） |
| **编码侧 zstd `--long`/更大窗口（长程匹配）** | 冗余主在**文件内长程**、重复点常 ≫1MiB 块距，放大 LDM 窗口是逼近 18–21x 单流上限**最便宜**的杠杆；ROI 高于去重 | 中 | ✅ **机制已落地 + 真实语料已实测 + 默认已定**（提交 e9643b6/993ed72；seal >8MiB 块自动开 LDM，windowLog clamp ≤27，热路径不开、零回归）。M2 实测（[results/ldm-ratio/REPORT.md](../bench/results/ldm-ratio/REPORT.md)）：64MiB 档 LDM 净增 **+5.26%**（21.72x→22.86x），**每大 transcript +8~16%**；8MiB 默认档几乎为零（+0.07%）。**决策 C（保守）**：保持 `DEFAULT_SEAL_CHUNK=8MiB` 不变，LDM 仅在用户显式 `seal --seal-chunk >8MiB` 时 opt-in——收益兑现，零默认风险。**未采纳** A（默认 8→64MiB+LDM，本语料 -11.9% 但代价冷读 RMW 放大、且收益绑定大文件主导 backing）/ B（默认 16–32MiB）：重开 A/B 前须补冷读放大实测 |
| **V 全局去重（内容寻址）—— 价值待证实** | 概念上跨会话共享前缀，但**实测定长块 0% 命中、同目录拼接增益仅 1.0x**，冗余主在文件内；须 CDC 且命中率未测 | 大 | ☐（G3 门控；**先做编码侧 --long**，dedup 价值由 CDC 命中率实测裁定） |
| **BV compact 自动化** | 覆盖写产生 MVCC 膨胀，需卸载时/后台 GC 兜底 | 小/中 | ☐ |

## T4 · 生产化 / 迁移（把它真正用起来）

| 方向 | 为什么 | 工作量/风险 | 状态 |
|---|---|---|---|
| **迁移 `~/.claude/projects`（分层）** | 目标范围已分层界定（[03-target-data-scope.md](./03-target-data-scope.md)）：**Tier 1a** projects/*.jsonl(8GB)→**1b** append 日志→**Tier 2** file-history(524MB)；plugins/已压缩类排除。灌入、校验、切换工具，可逆零丢失，活跃会话实时追加压测 | 中 | ☑ **切换工具落地** `zipfs enable`（TUI + list/apply/restore/remount/status/purge/autostart 子命令；`ingest --verify` + sidecar 提交标记使半灌可检测 + 活跃会话默认拦截 + 失败回滚到 Plain；`src/enable/`，取代旧 `bench/scripts/zipfs-*.sh`）；分层批量灌入策略与活跃会话长压测待续 |
| **可观测性** | 守护健康、实时压缩比、append 吞吐的监控，便于长期运行排障 | 中 | ◐ statfs 显压缩比（df）+ sd-notify 健康；吞吐/比值监控待扩 |
| **物理空间回收** | 压缩省的是逻辑量；WSL `ext4.vhdx` 物理回收需 `wsl --shutdown` + `Optimize-VHD`，需文档化/脚本化 | 小 | ☐ |

## 决策门汇总

- **G1（布局取向）**：T0 完成后定 V/S/并存。
- **G2（自写数据区）**：仅当 redb 在真实规模下空间/性能不达标（microbench 已给 256KiB 红线），才评估自写 extent 数据区——默认不做。
- **G3（去重投入）**：G1 选了 V（或并存含 V）后，再决定 T3 去重是否进主线。

## Stretch / 研究

> 已移至 [BACKLOG.md](./BACKLOG.md)「研究 / Stretch」段。T2–T4 表内的搁置项（BV 抖动、BV compact、物理回收）保留原位以维持优先级叙事，并在 BACKLOG 单向汇总。
