# zipfs（crates/zipfs）—— 路线 B（用户态 FUSE）Rust 实现

> 实验背景见 [`../../docs/00-overview.md`](../../docs/00-overview.md)，磁盘布局与分阶段设计见 [`../../docs/01-scrollz-design.md`](../../docs/01-scrollz-design.md)。
> **进度与缺陷的单一信息源**是 [`../../docs/ROADMAP.md`](../../docs/ROADMAP.md)（T0–T4 优先级）与 [`../../docs/06-defect-audit.md`](../../docs/06-defect-audit.md)（两轮审查台账）。本 README 只描述「这个 crate 现在是什么、怎么用」，不重复路线图的优先级排序。

本 crate 是 zipfs「方案四 / 路线 B」的 Rust 实现。设计文档 §12 的分阶段骨架 **P0–P4 已全部落地**（P0 透传基线 + P1 只读/顺序读 + P2 顺序写/截断 + P3 随机写 RMW + P4 元数据 POSIX 语义），代码内 **无 `todo!()` 占位**；当前投入已转向 ROADMAP 的 **T1 可靠性 / T2 性能 / T3 空间 / T4 生产化**。

## 阶段完成度（设计文档 §12 P0–P5）

| 阶段 | 目标 | 状态 | 落点 |
|---|---|---|---|
| **P0** | fuser 透传（零压缩，B0 基线，隔离纯 FUSE 税） | ✅ 完成 | `src/passthrough.rs` |
| **P1** | 只读 + 顺序读（两 Store 都通），离线 fixture 预置，`getattr`/`readdir` 正确 | ✅ 完成 | `src/store/{shadow,container}.rs`、`src/fixture.rs`、`bin/mkfixture` |
| **P2** | 写：create / 顺序 write / truncate，round-trip 一致 | ✅ 完成 | `src/rwfs.rs` + `core::rmw::{write_at,truncate}` |
| **P3** | 随机写（RMW）：随机 offset 写后整文件校验；不可压缩启发式 + flag 翻转；空洞/越 EOF 零填充 | ✅ 完成 | `src/core/rmw.rs` + `src/core/codec.rs`；差分测试 `tests/model_based.rs` |
| **P4** | 完整元数据：rename / unlink / mkdir / fsync 的 POSIX 语义；hardlink 不支持、跨目录 rename 一致性边界 | ◐ 大体完成 | `src/rwfs.rs` + `src/store/*`；**残留边界见下文** |
| **P5** | 基准接入 BV/BS + 块大小/等级扫描，产出场景适配表 | ◐ 进行中 | `bin/{ratio,discovery,append}-bench`；扫描见 ROADMAP T0/T3 |

**P4 的一处诚实标注**（不阻断当前用途，详见「已知边界」）：

- **unlink-while-open**：透传层靠底层 FS 天然兜底（已打开 fd 在 unlink 后仍可读写）；但布局 S/V 的 Store 层 `unlink` 是直接删 dirent + inode + 块，**未实现 orphan 延迟回收**，该 POSIX 边界尚无专门回归测试。

（hardlink 已对齐设计：布局 S/V 的 `link` handler 显式返回 `ENOTSUP`，与 docs/01 §4、ROADMAP T1 一致，并有 `tests/mount_rw.rs` 回归守卫；详见「已知边界」。）

## 三种后端布局

`--backend` 选择，所有后端共用同一套 FUSE 语义层（`rwfs` / `passthrough`）。

| 后端 | 布局 | 读写 | 说明 |
|---|---|---|---|
| `passthrough` | —（透传） | 读写 | P0 基线：FUSE 操作直接转发到底层目录对应路径/fd，零压缩。隔离纯 FUSE 税。 |
| `shadow` | 布局 S（影子树） | 读写 | 每文件一个分块压缩 archive（双 superblock + append-only 尾日志 §8.3/§8.4）。目标负载首选。 |
| `container` | 布局 V（redb 全包） | 读写 | 整个树装进一个 redb 容器，写批处理为一事务（§6.1）。 |

## 已实现的 FUSE 语义（`rwfs`，布局 S/V 共用）

| FUSE 操作 | 行为 |
|---|---|
| `lookup` / `getattr` | 经 Store 取逻辑属性，分配/复用 inode；尾缓冲大小叠加到 size |
| `create` / `mkdir` | Store 建项 + fsync 父目录，返回新 inode + 句柄 |
| `read` | 组装 `[off, off+size)` 逻辑字节：head 缓存快路径 / 逐块解压 / 缺块越 EOF 零填充 |
| `write` | `core::rmw::write_at`（整块覆盖 / 部分块 RMW / append / 空洞零填充），持 per-inode 写锁 |
| `setattr` | chmod / chown / truncate（零填充扩展或截短）/ 时间戳 |
| `open` / `release` / `flush` / `fsync` | 句柄生命周期 + 尾块封存 + `fsync`/`fdatasync` |
| `unlink` / `rmdir` / `rename` | 转 Store；`rmdir` 非空目录回 `ENOTEMPTY`；入口校验 name 防路径穿越 |
| `symlink` / `readlink` | 已支持（hardlink 不支持，见上文） |
| `forget` | 回收 per-inode 锁 / 尾缓冲，**先封尾再丢弃**防数据丢失 |
| `readdir` / `statfs` | 列目录（含 inode 复用）；`statfs` 在 shadow 下经压缩比折算（`df` 可见） |

并发模型：**per-inode `RwLock`**——不同 inode 并行、同 inode 读读并行、写排他堵 torn-read；`--threads` 多线程派发降写尾 p99。

## 架构与模块

- `src/core/`：`chunk`（分块数学）、`codec`（zstd + 不可压缩启发式 + 可选共享字典；lz4 仍 TODO）、`rmw`（读-改-写 / 零填充 / 截断）、`wsession`（脏块写会话）、`blockcache`（解压明文块缓存，压力感知）、`inode`（逻辑属性）。
- `src/store/`：`Store` trait（§5 签名，含 `get_block`/`put_block`/`truncate_blocks`/尾日志 `append_tail`/`seal_tail_block`/head 缓存 `set/read_head_cache`/`fsync`/`sync_all`）；`ContainerStore`（redb）、`ShadowStore`（影子树）两实现 + `lock`（跨进程 flock）。
- `src/archive/`：布局 S 的每文件 archive 格式——双 superblock 原子提交、per-block CRC、append-only 尾日志、head 缓存区。按职责拆为 `format`（crc32/整数编解码）、`superblock`、`journal`、`reader`、`writer`、`updater` 子模块。
- `src/blockio.rs`：`BlockIo` 接缝 + `FaultIo` 确定性崩溃模拟器（`fault-injection` feature，见测试）。
- 离线工具：`compact`（回收 MVCC / append-only 空洞）、`seal`（冷文件大块高等级重编码）、`ingest`（迁移灌入 + `--verify`）、`fixture`（测试预置数据）。
- `src/enable/`：Claude projects 透明压缩启用器（可逆切换 / systemd 自挂载 / TUI），取代旧 `bench/scripts/zipfs-*.sh`。
- `src/reconcile/`：停用期回落写重合并。`orchestrator/` 按流水线阶段拆为 `preconditions`/`io`/`delete_gate`/`reingest`/`plan`/`quarantine`/`routes`/`apply`/`manifest`/`prune`/`driver`（`reconcile` 主入口）/`undo`（`reconcile_undo` 入口）等子模块。

## 构建与测试

环境前提：Linux / WSL，`/dev/fuse` 存在，`fusermount3`（或 `fusermount`）在 `PATH` 中。

```bash
cd crates/zipfs
cargo build                              # 编译
cargo test                               # 229 个测试（202 单元 + 27 集成）
cargo test --features fault-injection    # 额外 8 个故障注入测试（共 237）
cargo fmt
cargo clippy --all-targets
```

测试构成：

- **单元测试（202）**：inode 映射 / lookup-count / forget、`block_range` 分块数学、codec 启发式与 flag、RMW 各分支、archive 双 SB 提交与 CRC、head 缓存、Store 不变量、name 校验等。纯逻辑，不需挂载。
- **集成测试（27）**：`passthrough` / `mount_rw`（真实挂载 round-trip，环境受限则**优雅跳过**不 panic）、`model_based`（随机操作序列 vs 内存参照模型差分，覆盖随机写/截断/越 EOF/可压缩与不可压缩两路）、`append_growth` / `append_tail_buffer`（尾日志增长边界）、`enable` / `systemd_mount`（启用器与托管挂载）。
- **故障注入（8，feature 门控）**：`FaultIo` × archive 双 superblock 提交协议的格式层不变量穷举（EIO / 撕裂 / 掉电 / 乱序 × barrier 软化），独立 oracle 复用生产 reader，见 [`../../docs/05-fault-injection-testing.md`](../../docs/05-fault-injection-testing.md)。崩溃端到端门由 `bench/scripts/crash-test.sh`（10/10）与 Tier 2 `dm-flakey`/`dm-log-writes` 脚本（root 门控）覆盖。

落地遵循 **TDD**（先写测试 RED → 实现 GREEN → 重构），正确性优先用 model-based 差分测试。

## 命令总览

无子命令 = 挂载（向后兼容原 `zipfs --backend ... --backing ...` 用法）。

| 子命令 | 用途 |
|---|---|
| _(默认)_ | 挂载：`--backend {passthrough\|shadow\|container} --backing <obj> --mountpoint <dir>` |
| `compact` | 离线压实：container 回收 redb MVCC、shadow 回收 append-only 空洞（须未挂载） |
| `seal` | 冷文件封存：shadow archive 用更大块 + 高等级离线重编码（~16x → ~25–30x） |
| `ingest` | 迁移灌入：源目录流式转布局 S，`--verify` 逐字节校验 |
| `train-dict` | 从语料训练共享 zstd 字典（T3 研究项，opt-in `--dict`） |
| `enable` | Claude projects 透明压缩启用器（TUI 或 `list/apply/restore/remount/status/purge/autostart`） |
| `mount-managed` / `umount-managed` | systemd 模板 `zipfs@<inst>.service` 内部调用 |

挂载常用参数：`--chunk-size`（默认 **1MiB**，实测裁决退役 64KiB）、`--level`（zstd 等级，默认 3）、`--threads`、`--block-cache-bytes`（解压块缓存，默认 **128MiB**，压力感知）、`--writeback`、`--max-write`、`--pid-file`、`--metrics-file`（Prometheus textfile）、`--dict`、`--auto-unmount`、`--allow-other`。

## 手动挂载试跑

```bash
# 准备底层对象与挂载点（shadow：backing 是目录树）
mkdir -p /tmp/zipfs-backing /tmp/zipfs-mnt

# 前台运行布局 S（Ctrl-C 退出）
RUST_LOG=info cargo run -- \
  --backend shadow --backing /tmp/zipfs-backing --mountpoint /tmp/zipfs-mnt

# 另开终端验证读写 round-trip（数据以压缩 archive 落在 backing）
echo hello > /tmp/zipfs-mnt/a.txt
cat /tmp/zipfs-mnt/a.txt          # 应看到 hello（解压后）
ls -l /tmp/zipfs-backing          # 应看到 a.txt 的 archive（非明文）
df -h /tmp/zipfs-mnt              # statfs 折算压缩比

# 卸载
fusermount3 -u /tmp/zipfs-mnt
```

把 `--backend shadow` 换成 `passthrough`（数据明文落 backing，B0 基线）或 `container`（`--backing` 改为 redb 容器文件路径）即可切后端。

`--auto-unmount` 默认关闭：本版 `fuser` 的 `AutoUnmount` 要求同时具备 `allow_other`/`allow_root`，否则挂载以 `auto_unmount requires acl != Owner` 失败。需要时配合 `--allow-other`（且 `/etc/fuse.conf` 放行 `user_allow_other`）。

## 已知边界与简化点（引用设计文档）

- **hardlink（§4）**：布局 S/V 正式不支持，`link` handler 显式返回 `ENOTSUP`（`cp -al` / git 会触发）；`passthrough` 基线未透传 `link`，由内核 VFS 回退 `EPERM`。决策见 ROADMAP T1、docs/01 §4。
- **unlink-while-open orphan 回收（§4）**：Store 层未做延迟回收，详见上文 P4 标注。
- **写模型 `direct_io`（§4.1）**：写 fd 默认 `direct_io` 求 RMW 精确；`--writeback` 可启内核写回缓存（降写尾 p99，但放宽精确性）。
- **mmap（§4）**：只读 fd 经 `KEEP_CACHE` 已可 mmap；写 fd 仍 `direct_io`，跨 fd 并发写陈旧页未保证（待 `notify_inval`）。
- **`rename` 子树缓存（§4）**：精确匹配项路径映射已同步；深层子孙缓存路径依赖内核 rename 后重新 lookup 纠正。
- **lz4 codec / algo 自适应**：codec 仅 zstd，lz4 与按文件类型自动选等级仍 TODO（ROADMAP T3）。
- **特殊文件类型**：`readdir` 对 FIFO/socket/device 回退为普通文件类型（透传下少见）。

## 依赖说明

- `fuser`（0.17）以 `default-features = false` 引入：本机未装 `libfuse3-dev`（无 `fuse3.pc`），关闭 libfuse 链接、改用 `fusermount3` 二进制挂载，避免 build 因 pkg-config 找不到 fuse3 而失败。
- P1+ 已按设计 §11 引入 `zstd`（压缩 + 字典）、`redb`（布局 V 容器）；布局 S 的 archive 格式为本 crate 自实现（`src/archive.rs`），不依赖外部容器库。
- `fault-injection` 为可选 feature，仅故障注入测试需要。

## 长期运行 / 自启（生产化，ROADMAP T4）

守护是前台阻塞 `mount2`；长期运行推荐 `zipfs enable`（取代旧脚本）：

- **一键启用**：`zipfs enable`（TUI）或 `apply`/`restore`/`remount`/`status`——可逆切换 Claude projects 透明压缩，半灌（未提交 sidecar）可检测、活跃会话默认拦截、失败回滚到 Plain。
- **systemd 自启 + 崩溃重挂**：`enable autostart` 装 per-project 模板 `zipfs@<inst>.service`（`Restart=on-failure`）到 `~/.config/systemd/user/`。
- **多线程**：`--threads N`（默认 = CPU 数，下限 4）降写尾 p99；per-inode RwLock 保并发安全。
- **WSL 无 systemd**：`/etc/wsl.conf` 加 `[boot] command = ...` 自挂载。
- **可观测性**：`statfs` 显压缩比（`df`）+ `--metrics-file` 写 Prometheus textfile + sd-notify 健康。
