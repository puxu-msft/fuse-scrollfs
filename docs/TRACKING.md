# zipfs 进行中工作跟踪 / TRACKING

> **本文回答「正在做什么」**——跨会话的进行中工作（WIP）指针。只放**指向权威源的链接 + 一句现状**，不复制/镜像状态，避免与 [ROADMAP.md](./ROADMAP.md) 双写漂移。
>
> 职责边界：长期方向与优先级看 [ROADMAP.md](./ROADMAP.md)（T0–T4）；推迟项看 [BACKLOG.md](./BACKLOG.md)；已完成看 [CHANGELOG.md](./CHANGELOG.md)。短期任务级进度（单会话内）仍由 Claude memory 承载，本文只登记**需跨会话交接**的活。

## 进行中（◐，链接回 ROADMAP 权威行）

| 工作 | 现状一句 | 权威源 |
|---|---|---|
| FUSE 写尾延迟优化 | 多线程派发已落地；writeback/passthrough 待 `fuser` 升级 | [ROADMAP.md](./ROADMAP.md) T2 |
| mmap 跨 fd 并发写陈旧页 | 只读 fd 已启 mmap；写 fd 仍 direct_io，跨 fd 陈旧页待 `notify_inval` | [ROADMAP.md](./ROADMAP.md) T2 |
| algo/chunk 自适应 | 块大小已定 1MiB + `--level` 可配；lz4 codec 仍 unimplemented、自动选择未做 | [ROADMAP.md](./ROADMAP.md) T3 |
| 可观测性扩展 | statfs 压缩比 + sd-notify 健康已有；吞吐/比值监控待扩 | [ROADMAP.md](./ROADMAP.md) T4、[08-observability.md](./08-observability.md) |
| enable 分层批量灌入 + 活跃会话长压测 | `zipfs enable` 切换工具已落地；分层批量策略与长压测待续 | [ROADMAP.md](./ROADMAP.md) T4 |

## 开放决策门（待触发，详情在 ROADMAP / ADR）

- **G1 布局取向**（V / S / 并存）——T0 收尾评估补齐后定。
- **G2 自写数据区**——仅当 redb 真实规模不达标；默认不做。
- **G3 去重投入**——G1 选含 V 后再定；先做编码侧 `--long`。

详见 [ADR.md](./ADR.md) §2 与 [ROADMAP.md](./ROADMAP.md) 决策门汇总。
