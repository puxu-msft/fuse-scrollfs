# zipfs 架构视图 / ARCH

> **本文回答「是什么 / 在哪里」**——zipfs 当前的架构骨架：组件、边界、数据流、技术栈。反映**现状**（`crates/zipfs/` 实态，含 workspace 化、hangfree、reconcile、enable、head 缓存等已落地部分），而非历史设计快照。
>
> 职责边界：**为什么**看 [ADR.md](./ADR.md)；**怎么做**（算法/内部契约细节）看 [DESIGN.md](./DESIGN.md) 及其索引的编号专题；**下一步**看 [ROADMAP.md](./ROADMAP.md)。
>
> 设计快照（多为 2026-06-27 冻结、含推演背景）见 [01-zipfs-design.md](./01-zipfs-design.md) 等编号文档；本文与之偏差处**以本文为现状准**。

## 1. 定位

zipfs 是自研的 Rust FUSE 透明压缩文件系统，目标负载 = `~/.claude/projects`（append 为主、高冗余、运行时活跃写的会话 jsonl）。起源是一次「btrfs+zstd vs FUSE 透明压缩」横评（见 [00-overview.md](./00-overview.md)），FUSE 路线转正为产品。

## 2. 技术栈

| 层 | 选型 | 备注 |
|---|---|---|
| FUSE 绑定 | `fuser` 0.17 | 多线程派发（`n_threads = available_parallelism`，`clone_fd`）；writeback/passthrough 待升级 |
| 压缩 codec | `zstd`（多等级 + `--long`/LDM）、`lz4_flex`（对照，codec 部分 unimplemented） | 见 [ADR.md](./ADR.md) D4 |
| 容器后端（布局 V） | `redb`（默认全包）、`rusqlite`（空间敏感备选） | microbench 裁定，见 [ADR.md](./ADR.md) D6 |
| 构建 | Cargo workspace | 见 §5 |

## 3. 组件与边界

数据流自上而下：POSIX 调用 → FUSE 内核 → `rwfs`（`impl fuser::Filesystem`：inode 缓存 / 句柄表 / 每-inode RwLock）→ **压缩内核 `core`**（分块数学 · RMW · codec · 块缓存）→ **`trait Store` 接缝**（唯一后端差异面）→ 布局 V（`ContainerStore`，redb/sqlite 容器）或布局 S（`ShadowStore`，底层目录树 + 每文件分块 archive）。

**关键边界**：分块/压缩/codec 全在 `core`；`Store` 只管「不透明已压缩 blob 的放置 + 命名空间 + 空闲管理 + 该后端持久化原语」，不碰压缩。`fsync(ino)` 与 `sync_all()` 分离，保证单文件 fsync 语义可比。详见 [01-zipfs-design.md](./01-zipfs-design.md) §5 的 `Store` trait 定义。

## 4. 模块地图（`crates/zipfs/src/`）

| 模块 | 职责 | 边界内文档 |
|---|---|---|
| `rwfs.rs` `passthrough.rs` | FUSE 前端：读写文件系统 / 透传 | [01](./01-zipfs-design.md) §4 |
| `core/`（`chunk` `codec` `rmw` `blockcache` `inode` `wsession` `metrics`） | 共享压缩内核：分块、编解码、读改写、解压块缓存、写会话 | [02-layered-chunking.md](./02-layered-chunking.md)、[08-observability.md](./08-observability.md) |
| `store/`（`container` `shadow` `lock`） | 两布局后端 + per-inode 标记锁 | [01](./01-zipfs-design.md) §6/§7 |
| `archive/`（`format` `writer` `reader` `updater` `journal` `superblock`） | 布局 S 每文件分块包：格式、尾日志、双 superblock、head 缓存 | [04-crash-safe-commit.md](./04-crash-safe-commit.md)、[02](./02-layered-chunking.md) |
| `seal.rs` `compact.rs` | 冷文件封存（大块重压 + LDM）、append-only 空洞压实 | [ADR.md](./ADR.md) D10 |
| `ingest.rs` `blockio.rs` `fixture.rs` | 灌入 + 校验、可注入块 IO 接缝、测试夹具 | [05-fault-injection-testing.md](./05-fault-injection-testing.md) |
| `enable/`（`discovery` `lifecycle` `daemon` `systemd` `autostart` `force_umount` `hang_free` `config` `model`） | 生产化启用器：发现 / 可逆切换 / 守护 / systemd / 分档卸载 | [07-hangfree-umount.md](./07-hangfree-umount.md)、[reversible-switch-prometheus.md](./plan/reversible-switch-prometheus.md) |
| `reconcile/`（`merge` `advisor` `guard` `record` `orchestrator/`） | 会话感知的停用期回落写重合并 | [09-session-reconcile.md](./09-session-reconcile.md) |

## 5. Workspace 布局

```
zipfs/（Cargo workspace，根 profile.release: lto=thin + codegen-units=1）
  crates/zipfs/        # 主 crate（FUSE 文件系统 + enable/reconcile）
  crates/zipfs-bench/  # 基准工具
  exp/                 # 一次性 PoC / 设计闸门（如 container-backend-selection）
```

两布局并存（V + S），布局取向决策门 G1 未关（见 [ADR.md](./ADR.md)）。当前生产路径主用布局 S（shadow + rwfs）。技术演进历史见 [CHANGELOG.md](./CHANGELOG.md)，骨架现代化决策见 [ADR.md](./ADR.md) D12 与 [plan/workspace-restructure.md](./plan/workspace-restructure.md)。
