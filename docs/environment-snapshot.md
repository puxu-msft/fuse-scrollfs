# 环境快照 / Environment Snapshot

> 采集日期：2026-06-27。这些是**实测事实**，会随系统变化而过期；后续在新机器或重装后应重新跑一遍 `bench/scripts/probe-env.sh`（待编写）并更新本文件。与「总纲」分开，是为了让意图稳定、事实可刷新。

## 主机

| 项目 | 值 |
|---|---|
| 平台 | WSL2 |
| 发行版 | Ubuntu 24.04.4 LTS |
| 内核 | `6.18.33.1-microsoft-standard-WSL2` |
| CPU | 20 逻辑核 |
| 内存 | ~196 GiB (`MemTotal: 206123520 kB`) |
| 根文件系统 | ext4，块设备 `/dev/sdd`，挂载于 `/`（即 WSL 的 `ext4.vhdx`） |

## 方案一（btrfs + zstd）相关

| 检查项 | 结果 | 说明 |
|---|---|---|
| `btrfs.ko` 内核模块 | **存在** | `/lib/modules/6.18.33.1-microsoft-standard-WSL2/kernel/fs/btrfs/btrfs.ko` |
| btrfs 已加载 | 否 | `/proc/filesystems` 里暂无 btrfs；需 `sudo modprobe btrfs` |
| `modprobe btrfs`（无 sudo） | 失败 | `Operation not permitted`——权限问题，非缺模块；带 `sudo` 预期可加载 |
| `btrfs-progs`（`mkfs.btrfs` 等） | **缺** | 需 `sudo apt install btrfs-progs` |
| `compsize` | 缺 | 测压缩比要用，需 `sudo apt install btrfs-compsize` |

结论：方案一在本机**可行**，只差「加载模块 + 装用户态工具」两步。注意挂载 loop image 需要 root。

## 方案四（FUSE 透明压缩）相关

| 检查项 | 结果 | 说明 |
|---|---|---|
| `/dev/fuse` | 存在，`crw-rw-rw-` | 普通用户可用，无需 root 即可挂载 FUSE |
| `fusermount` / `fusermount3` | 均存在 | `/usr/bin/` 下 |
| `libfuse3-dev` 头文件 | **缺** | 若用 C/C++ 写 FUSE，需 `sudo apt install libfuse3-dev`；Rust `fuser` / Go `hanwen/go-fuse` 走自带绑定，可不依赖系统头 |
| `libzstd` dev | **就绪** `1.5.5` | `pkg-config --modversion libzstd` 通过；C/Rust/Go 都能直接用 |
| `mksquashfs` | 存在 | 只读对照（SquashFS）可立即用 |
| `squashfuse` | 缺 | 如需 FUSE 方式只读挂 squashfs 镜像再装 |

## 构建工具链

| 工具 | 路径 |
|---|---|
| `cargo` | `/home/xp/.local/cargo/bin/cargo` |
| `go` | `/home/xp/.local/go/bin/go` |
| `gcc` | `/usr/bin/gcc` |
| `zstd` (CLI) | `/home/linuxbrew/.linuxbrew/bin/zstd` |

→ 自研 FUSE scrollz 用 **Rust（`fuser` + `zstd` crate）** 或 **Go（`hanwen/go-fuse` + `klauspost/compress`）** 都具备条件。

## 一键补齐依赖（草案，待核对版本）

```bash
sudo apt update
sudo apt install -y btrfs-progs btrfs-compsize fio libfuse3-dev
sudo modprobe btrfs && grep -q btrfs /proc/filesystems && echo "btrfs ready"
```

路线 B（Rust）核心 crate（实测 2026-06-27 经 `fuse-zstd` 确认可用组合）：`fuser`（FUSE 绑定，事实标准）、`zstd` `0.13`、`lz4_flex`（lz4 对照）。无需系统 FUSE 头即可用 `fuser`；`libfuse3-dev` 仅在用 C/C++ 或某些 `fuse3` 配置时需要。

> 只读方案（`squashfuse` 等）因负载为读写已排除，不在依赖清单内。
> WSL 重启后 `modprobe` 不保留。若要持久，考虑写入启动脚本或 `/etc/wsl.conf` 的 `[boot] command`，细节在总纲「WSL 特有注意事项」里展开。
