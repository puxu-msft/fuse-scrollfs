# scrollz 推迟待办 / BACKLOG

> **本文回答「推迟做什么」**——已知但当前不做的可选特性、非关键改进、研究性方向。收敛「知道但没做」的信号，避免静默砍潜在需求（`defer-potential-demand-over-cut-it`）。
>
> 职责边界：进主线的方向看 [ROADMAP.md](./ROADMAP.md)；这里是**低优先 / 门控 / 研究**层。条目成熟或被触发时应上提到 ROADMAP。

## 搁置 / 低优先（有明确技术信号，暂不投入）

| 条目 | 为什么推迟 | 关联 |
|---|---|---|
| BV 写尾抖动定位 | rand-write-64k p99 抖到 28ms，疑 redb commit/MVCC；曾叫停，待需要时再 profile | [ROADMAP.md](./ROADMAP.md) T2 |
| BV compact 自动化 | 覆盖写产生 MVCC 膨胀，需卸载时/后台 GC 兜底；布局 V 非当前生产主路径 | [ROADMAP.md](./ROADMAP.md) T3 |
| shadow commit_lock 等待的确定性测试 | 现「第二个 fsync 被阻塞」中间断言靠 20ms 负断言（只证明未完成、非严格证明已阻塞在 commit_lock）；欲严格证明需仅测试用「到达取锁前」钩子或把 commit 序列抽成可注入协调器。最终 archive=B 断言 + 无歧义锁结构已足够，此为测试完备性改进 | reviewer 2026-07-13 Minor |
| 物理空间回收脚本化 | WSL `ext4.vhdx` 物理回收需 `wsl --shutdown` + `Optimize-VHD`，待文档化/脚本化 | [ROADMAP.md](./ROADMAP.md) T4 |

## 实现语义缺口（来自 2026-07-13 gpt-souls 分析，低频/非阻塞，非数据正确性）

| 条目 | 现状与位置 | 关联 |
|---|---|---|
| 压缩前端 unlink-while-open | `ZipfsRw::open` 恒返回 `fh=0`，无句柄表/open count/orphan；unlink 后立即删数据，已 open fd 无法继续访问。lookup-count/延迟回收仅在 passthrough 简化 inode 表中部分存在。`rwfs.rs:517-531,681-696` | POSIX 语义缺口；append-only transcript 主路径不高频 |
| reconcile `--rebuild` W2 自启窗口 | 清 `.reconciling` marker 后到重建/重挂完成之间，外部 systemd 理论可挂到重建中 backing。orig 仍权威、单文件替换原子，风险是短暂暴露混合代/双挂载。`reconcile/orchestrator/driver.rs:174-179` 已标 W2 | 需区分「reconcile 内部可信重挂」与外部自启，使 marker 覆盖整个 rebuild |
| 自动 compact 策略 | Shadow 在线 RMW/index/head cache/journal 均只追加，旧版本仅靠 compact 回收；当前无自动触发 | 长期运行需定期 compact |
| hang-free stat 遗留 D 状态线程 | 首次 endpoint stat 超时可能遗留一个 D 状态线程；熔断只限制重复泄漏频率，非根除 | [07-hangfree-umount.md](./07-hangfree-umount.md) |

## 门控（等决策门放行）

| 条目 | 门 | 触发条件 |
|---|---|---|
| V 全局去重（内容寻址 / CDC） | G3 | G1 选含 V 后，且 CDC 命中率实测证明价值；实测已推翻定长块去重（见 [ADR.md](./ADR.md) §3） |
| 自写 extent 数据区 | G2 | 仅当 redb 真实规模空间/性能不达标；默认不做 |

## 研究 / Stretch（松散，无承诺）

- bcachefs / ZFS 透明压缩横向对照（内核态另一参照）。
- 静态加密层叠加（compression-then-encryption 顺序与安全）。
- `/mnt/c`（Windows 侧）用例——当前明确不在范围，未来若需另开。
