# zipfs `fuse/` —— 路线 B（用户态 FUSE）Rust 实现

> 实验背景见 [`../docs/00-overview.md`](../docs/00-overview.md)，磁盘布局与分阶段设计见 [`../docs/01-zipfs-design.md`](../docs/01-zipfs-design.md)。

本 crate 是 zipfs「方案四 / 路线 B」的 Rust 实现骨架。当前处于 **P0：fuser 透传（passthrough，零压缩）** 阶段，对应基准矩阵里的 **B0（隔离纯 FUSE 税）**。

## 当前 P0 范围

P0 的目的不是压缩，而是**在没有压缩复杂度的前提下打通 FUSE 语义骨架**：inode 分配、句柄表、并发与锁顺序、lookup-count / forget。这样后续把「FUSE 语义 bug」与「压缩 / RMW bug」分离开（设计文档 §12 P0、§4 的 C4 难点都在这层）。

已实现（全部转发到底层 backing 目录对应路径或 fd）：

| FUSE 操作 | 行为 |
|---|---|
| `lookup` / `getattr` | stat backing 路径，分配/复用 inode |
| `setattr` | chmod / chown / truncate（atime/mtime 待 P4） |
| `create` / `mkdir` | 在 backing 下创建，返回新 inode + 句柄 |
| `read` / `write` | 经句柄表的 fd 做 `pread` / `pwrite` |
| `open` / `release` / `flush` / `fsync` | 句柄生命周期 + `fsync`/`fdatasync` |
| `unlink` / `rmdir` / `rename` | 转发到 backing，同步 inode 路径映射 |
| `readdir` | 列底层目录，`.`/`..` + 各子项（含 inode 复用） |
| `statfs` | `statvfs` backing |

架构上同时**预留了 P1+ 的接缝**（均可编译，内部 `todo!()`）：

- `src/core/`：`chunk`（分块数学，`block_range` 已实现并测试）、`codec`（zstd/lz4 + 不可压缩启发式占位）、`inode`（逻辑属性占位）。
- `src/store/`：`Store` trait（§5 签名，含 `fsync(ino)` / `sync_all` / `get_block` / `put_block` / `truncate_blocks`）、`ContainerStore`（布局 V / redb）、`ShadowStore`（布局 S / 影子树）占位实现。

## 构建与测试

环境前提：Linux / WSL，`/dev/fuse` 存在，`fusermount3`（或 `fusermount`）在 `PATH` 中。

```bash
cd fuse
cargo build            # 编译
cargo test             # 12 个单元测试 + 1 个集成测试
cargo fmt              # 格式化
cargo clippy --all-targets
```

测试构成：

- **单元测试**：`InodeTable` 的 ino/路径映射、lookup-count / forget 语义、`block_range` 分块数学、mode→FileType 推导。纯逻辑，不需挂载。
- **集成测试 `tests/passthrough.rs`**：若环境允许挂载，则把 zipfs 挂到临时目录，做 create / write / read / readdir / mkdir / unlink 的 round-trip 校验（并对照 backing 目录），结束后**务必卸载**。若挂载失败（权限/环境受限），测试**优雅跳过并打印原因**，不会 panic 让整个 test 套失败。

> 该集成测试通过启动已编译的 `zipfs` 二进制来挂载（用 `CARGO_BIN_EXE_zipfs`），贴近真实使用路径，也避免引入额外的 lib target。无论断言是否 panic，都会卸载并回收子进程。

## 手动挂载试跑

```bash
# 准备底层目录与挂载点
mkdir -p /tmp/zipfs-backing /tmp/zipfs-mnt

# 前台运行（Ctrl-C 退出）
RUST_LOG=info cargo run -- --backing /tmp/zipfs-backing --mountpoint /tmp/zipfs-mnt

# 另开一个终端验证透传
echo hello > /tmp/zipfs-mnt/a.txt
cat /tmp/zipfs-backing/a.txt      # 应看到 hello（数据落在 backing）
ls -l /tmp/zipfs-mnt

# 卸载
fusermount3 -u /tmp/zipfs-mnt
```

参数：

- `--backing <DIR>`：底层目录，所有数据实际落在这里。
- `--mountpoint <DIR>`：挂载点。
- `--auto-unmount`：进程退出自动卸载。**默认关闭**——本版 `fuser` 的 `AutoUnmount` 要求同时具备 `allow_other`/`allow_root`，否则挂载会以 `auto_unmount requires acl != Owner` 失败。需要时配合 `--allow-other`（且 `/etc/fuse.conf` 放行 `user_allow_other`）。
- `--allow-other`：允许其他用户访问挂载点。

## 已知简化点（P0 故意不做，均引用设计文档）

- **lookup-count / 延迟回收（§4）**：维护了 lookup-count 与 forget，但**未实现 unlink-while-open 的 orphan 延迟回收**。透传场景下底层 FS 已天然保证「已打开 fd 在文件被 unlink 后仍可读写」，故 P0 数据风险低；完整 POSIX 语义（置 orphan、待 forget + 句柄全关再回收）留待 **P4**。代码内已用 `TODO(§4)` 标注。
- **写模型 `direct_io`（§4.1）**：`open`/`create` 返回 `FOPEN_DIRECT_IO`，求正确（offset/size 精确、语义简单）。`writeback cache` 作为后续优化项。
- **`rename` 子树缓存（§4）**：`rename` 同步精确匹配项的路径映射；深层子孙的已缓存路径依赖内核 rename 后重新 lookup 纠正。`TODO` 标注递归重写。
- **`setattr` 时间戳**：atime/mtime 设定（`futimens`）P0 暂略，待 P4。
- **mmap（§4）**：未支持。写时 mmap 复杂且与 `direct_io` 互斥，列为后续。
- **特殊文件类型**：`readdir` 对 FIFO/socket/device 回退为普通文件类型（透传下少见）。

## 后续 P1–P5 待办（设计文档 §12）

| 阶段 | 目标 |
|---|---|
| **P1** | 只读 + 顺序读（两 Store 都通），离线 fixture 生成预置数据，`getattr`/`readdir` 正确 |
| **P2** | 写：create / 顺序 write / truncate，round-trip 一致 |
| **P3** | 随机写（RMW）：随机 offset 写后整文件校验；不可压缩启发式 + flag 翻转；空洞/越 EOF 零填充 |
| **P4** | 完整元数据：rename / unlink / mkdir / fsync 的 POSIX 语义；hardlink=ENOTSUP、unlink-while-open、跨目录 rename 一致性边界 |
| **P5** | 基准接入 BV/BS + 块大小/等级扫描，产出场景适配表 |

落地时遵循 **TDD（先写测试 RED → 实现 GREEN → 重构）**，正确性测试优先用「随机操作序列 vs 内存参照模型」做差分（model-based test），参考模型须覆盖 hardlink / sparse / 截断 / 越 EOF 写等边界。

## 依赖说明

- `fuser`（0.17）以 `default-features = false` 引入：本机未装 `libfuse3-dev`（无 `fuse3.pc`），关闭 libfuse 链接、改用 `fusermount3` 二进制挂载，避免 build 因 pkg-config 找不到 fuse3 而失败。
- P0 **不引** `zstd` / `lz4_flex` / `redb` / `rusqlite`——透传零压缩无需它们，按设计 §11 在 P1+ 引入。

## 长期运行 / 自启（生产化，ROADMAP T1）

守护是前台阻塞 `mount2`；长期运行用 `--auto-unmount` + `--pid-file` 配脚本/ systemd：

- **多线程**：`--threads N`（默认 = CPU 数，下限 4），降写尾 p99；per-inode RwLock 保并发安全。
- **幂等自挂载**：`bench/scripts/zipfs-mount.sh <backing> <mnt> [chunk]`——已挂跳过、清 stale endpoint、后台挂载写 PID。
- **systemd 自启 + 崩溃重挂**：`bench/scripts/zipfs.service`（`Restart=on-failure`）→ `~/.config/systemd/user/`。
- **WSL 无 systemd**：`/etc/wsl.conf` 加 `[boot] command = .../zipfs-mount.sh <backing> <mnt>`。
