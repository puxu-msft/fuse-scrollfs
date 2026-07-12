# zipfs 推迟待办 / BACKLOG

> **本文回答「推迟做什么」**——已知但当前不做的可选特性、非关键改进、研究性方向。收敛「知道但没做」的信号，避免静默砍潜在需求（`defer-potential-demand-over-cut-it`）。
>
> 职责边界：进主线的方向看 [ROADMAP.md](./ROADMAP.md)；这里是**低优先 / 门控 / 研究**层。条目成熟或被触发时应上提到 ROADMAP。

## 搁置 / 低优先（有明确技术信号，暂不投入）

| 条目 | 为什么推迟 | 关联 |
|---|---|---|
| BV 写尾抖动定位 | rand-write-64k p99 抖到 28ms，疑 redb commit/MVCC；曾叫停，待需要时再 profile | [ROADMAP.md](./ROADMAP.md) T2 |
| BV compact 自动化 | 覆盖写产生 MVCC 膨胀，需卸载时/后台 GC 兜底；布局 V 非当前生产主路径 | [ROADMAP.md](./ROADMAP.md) T3 |
| 物理空间回收脚本化 | WSL `ext4.vhdx` 物理回收需 `wsl --shutdown` + `Optimize-VHD`，待文档化/脚本化 | [ROADMAP.md](./ROADMAP.md) T4 |

## 门控（等决策门放行）

| 条目 | 门 | 触发条件 |
|---|---|---|
| V 全局去重（内容寻址 / CDC） | G3 | G1 选含 V 后，且 CDC 命中率实测证明价值；实测已推翻定长块去重（见 [ADR.md](./ADR.md) §3） |
| 自写 extent 数据区 | G2 | 仅当 redb 真实规模空间/性能不达标；默认不做 |

## 研究 / Stretch（松散，无承诺）

- bcachefs / ZFS 透明压缩横向对照（内核态另一参照）。
- 静态加密层叠加（compression-then-encryption 顺序与安全）。
- `/mnt/c`（Windows 侧）用例——当前明确不在范围，未来若需另开。
