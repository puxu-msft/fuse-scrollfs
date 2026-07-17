# bench — scrollz 首轮对照基准脚手架

本目录是 [docs/00-overview.md](../docs/00-overview.md) §5「首轮最小实验」的可执行落地：对四个条件 **C0 / A / B0 / B2** 跑统一的 fio 负载，量出「FUSE 税」与「整文件压缩模型在随机写下的劣化」。脚手架本身不绑定具体挂载方式——它接受「条件名 → 挂载点」映射，谁就绪就跑谁，未就绪的显式跳过。

> 实验意图、判据、数据集分层见总纲；本文只讲「怎么跑」。环境事实见 [docs/environment-snapshot.md](../docs/environment-snapshot.md)，会随系统漂移，跑前先 `probe-env.sh` 刷新认知。

## 对照条件与就绪状态

| 代号 | 条件 | 挂载方式 | 当前状态 |
|---|---|---|---|
| C0 | 裸 ext4（无压缩，吞吐地板） | 在 ext4 后端建个普通子目录即可 | **就绪**：建目录就能跑 |
| A | btrfs + zstd:N（loop image） | `setup-btrfs.sh` 建并挂 | **就绪（需 root + 先 modprobe btrfs）** |
| B0 | FUSE 透传（不压缩，隔离纯 FUSE 开销） | scrollz（crates/scrollz）的 Rust passthrough 二进制 `scrollz`，由 `mount-b0.sh` 挂起 | **就绪**：`cargo build --release` 后 `mount-b0.sh` 即可挂 |
| B2 | FUSE + zstd 整文件（`fuse-zstd`） | 待构建/安装 `fuse-zstd` 并挂载 | **待装**：`fuse-zstd` 未在 PATH |

脚手架对「待实现/待装」的条件是**优雅跳过 + 显式 log**，不会因为某条不在而中断其余条件。

## 依赖

按角色分层安装，并**记录确切版本以复现**（本机已验版本见括注，跑前用 `--version` 复核）：

| 依赖 | 角色 | 安装（分层） | 版本/权限要点 |
|---|---|---|---|
| **fio**（必需） | 跑全部 fio 负载 | `brew install fio` | 3.42（撰写时最新）；纯用户态，无需 root |
| **btrfs-progs**（条件 A） | `mkfs.btrfs` 建 loop image | `brew install btrfs-progs` | 7.0；`mkfs.btrfs` **可非 root** 对 image 执行，仅 `mount` 需 sudo 且走**内核模块** |
| **btrfs-compsize**（测 A 压缩比） | 量 btrfs 真实物理 vs 逻辑 | `sudo apt install btrfs-compsize` | 走系统包；读 btrfs 元数据 |
| **python3**（汇总） | `collect.py` 解析 fio JSON | 通常自带 | 标准库即可 |
| **Rust toolchain**（B0） | 构建 scrollz 透传二进制 | 项目已有 `cargo` | `cargo build --release -p scrollz`（仓库根），产物 `target/release/scrollz` |
| **fuse-zstd**（B2，待装） | 整文件 zstd 对照 | 从源码构建（见待办） | 未在 PATH 时 `probe-env.sh` 会报 |

> **libfuse3-dev 不需要**：scrollz（crates/scrollz）crate 用 `fuser` 且 `default-features = false`，挂载走 `fusermount3` 二进制而非链接 libfuse3，故无需 `libfuse3-dev`/`fuse3.pc`。
>
> **跑前一次性准备（条件 A，需内核模块）**：`sudo modprobe btrfs`（WSL 每次启动都要重加载，见总纲 §7）。脚本遵循「不擅自 modprobe」，未加载会显式提示退出。
>
> **版本复现**：把各工具 `--version` 输出连同结果一并归档（`probe-env.sh` 已收集大部分），确保跨机对照可复现。

先跑探测确认缺口：

```bash
bash bench/scripts/probe-env.sh
```

## 脚本一览

| 文件 | 作用 | 是否改系统 |
|---|---|---|
| `scripts/probe-env.sh` | 探测内核/btrfs/fuse/fio/工具链/CPU/内存，输出报告 | 否（只读、幂等） |
| `scripts/setup-btrfs.sh` | 建稀疏 loop image → `mkfs.btrfs` → `mount -o loop,compress-force=zstd:N`（默认 `FORCE=1` 强制压；`FORCE=0` 退回 btrfs 默认启发式对照） | 是（需 root） |
| `scripts/mount-bv.sh` / `mount-bs.sh` | 把 backing 挂成 BV(容器/redb) / BS(影子树) 读写挂载点 | 否（用户 FUSE，无需 root） |
| `scripts/umount-bv.sh` / `umount-bs.sh` | 卸载 BV / BS | 否 |
| `scripts/measure-a-ratio.sh` | 测 A(btrfs) 真实压缩比：写数据集进探针 → `sudo compsize` → 算比值 → 清理 | 部分（compsize 需 sudo） |
| `scripts/mount-b0.sh` | 用 scrollz 透传二进制把 backing 目录挂成 B0 挂载点 | 否（普通用户 FUSE，无需 root） |
| `scripts/umount-b0.sh` | 卸载 B0（`fusermount3 -u` + 收尾守护进程） | 否（无需 root） |
| `scripts/teardown.sh` | 安全卸载 btrfs（A），可选删 image（重重设防，绝不通配符 rm） | 是（需 root） |
| `datasets/fetch-claude-projects.sh` | 把 `~/.claude/projects` 的**只读副本**取到 `datasets/claude-projects/`（默认代表性子集） | 否（只读源，cp -a/rsync -a） |
| `scripts/run-suite.sh` | 对「条件→挂载点」映射跑全部 fio job，输出 JSON | 跑 IO；冷缓存需 root |
| `scripts/collect.py` | 解析 fio JSON → 汇总 CSV | 否 |
| `fio/*.fio` | fio job 模板（目标目录由 `DIR` 环境变量注入） | 否 |

## 怎么跑（首轮最小流程）

### 1. 探测环境

```bash
bash bench/scripts/probe-env.sh
```

### 2. 准备各条件的挂载点

**C0（裸 ext4）**——直接建目录：

```bash
mkdir -p /home/xp/src/zipfs/bench/.mnt/c0   # 任意 ext4 后端目录均可
```

**A（btrfs+zstd）**——先加载模块（脚本不擅自 modprobe），再建挂：

```bash
sudo modprobe btrfs                       # WSL 每次启动都要，见总纲 §7
IMG=/home/xp/src/zipfs/bench/results/btrfs.img \
SIZE=20G MNT=/mnt/scrollz-btrfs ZSTD_LEVEL=3 \
  sudo -E bash bench/scripts/setup-btrfs.sh
```

**B0（FUSE 透传）**——先构建二进制，再挂起 backing 目录（backing 应在 ext4 上）：

```bash
( cd /home/xp/src/zipfs && cargo build --release -p scrollz )   # 产物 target/release/scrollz
bash bench/scripts/mount-b0.sh                            # 默认 backing=bench/.b0-backing, MNT=bench/.mnt/b0
# 自定: BACKING=/path/on/ext4 MNT=/path/mnt bash bench/scripts/mount-b0.sh
# 卸载: bash bench/scripts/umount-b0.sh
```

二进制不存在时 `mount-b0.sh` 会优雅报错并提示先 `cargo build --release`，不擅自构建。

**B2**——待 `fuse-zstd` 就位后挂到其挂载点（见下「待办」）。

### 3. 跑基准

把就绪的条件用 `CONDITIONS` 传进去（`名称=挂载点`，空格分隔）：

```bash
CONDITIONS="C0=/home/xp/src/zipfs/bench/.mnt/c0 A=/mnt/scrollz-btrfs B0=/home/xp/src/zipfs/bench/.mnt/b0" \
  bash bench/scripts/run-suite.sh
```

未列出的、或挂载点不存在的条件会被显式跳过。结果落 `results/<UTC时间戳>/<条件>/<job>.json`。

> **默认 1 轮（用户要求减少测试量）**：`run-suite.sh` 默认 `ROUNDS=1`，单项目标 ~1-5 分钟。需要稳定性统计再加轮数：`ROUNDS=3 bash bench/scripts/run-suite.sh`，多轮时每轮落 `<时间戳>/r<N>/<条件>/` 子目录（单轮不加 `r<N>/`，向后兼容）。

> **冷缓存**：`run-suite.sh` 每个 job 前尝试 `sync + drop_caches`（需 root / 免密 sudo）。无权限时**降级为热缓存并打印告警**，不会静默。需要严格冷缓存对比时，以 root 运行整个 suite。

### 4. 汇总成 CSV

```bash
python3 bench/scripts/collect.py results/<那个时间戳目录>
# → results/<时间戳>/summary.csv，列含 condition/job/bw/iops/lat p50,p99/cpu
```

### 5. 压缩比（条件 A）

吞吐之外，压缩比单独量。**A 用 `measure-a-ratio.sh`**（写数据集进探针 → `sudo compsize` → 算逻辑/物理比 → 清理探针）：

```bash
bash bench/scripts/measure-a-ratio.sh                       # compsize 那步要 sudo
DATASET=~/.claude/projects bash bench/scripts/measure-a-ratio.sh   # 测完整目标负载
```

> **必须用 `compress-force`**：目标负载是 append-only 可压缩 jsonl，btrfs 默认启发式 `compress` 会误判跳过大量可压数据（实测 676M 子集漏压 212M，仅 2.44x）；`compress-force=zstd:3` 下同一数据 **6.74x**。`setup-btrfs.sh` 已默认 force；若挂载是旧的启发式，先 `sudo mount -o remount,compress-force=zstd:3 /mnt/scrollz-btrfs` 再测。
> BV/BS 压缩比：卸载后对 backing（`bench/.bv-backing` 容器文件 / `bench/.bs-backing` 影子树）`du` 对比逻辑大小；BV 的 redb 是稀疏文件，用 `du -sh`（实际磁盘块）而非 `du -sb`（apparent）。

### 6. 收尾

```bash
# B0（FUSE 透传）卸载（无需 root）:
bash bench/scripts/umount-b0.sh
# A（btrfs）卸载（保留 image）:
MNT=/mnt/scrollz-btrfs sudo -E bash bench/scripts/teardown.sh
# A 卸载并删除 image（显式 DELETE_IMG=1，且校验路径以 .img 结尾、非链接、未被占用）:
MNT=/mnt/scrollz-btrfs IMG=/home/xp/src/zipfs/bench/results/btrfs.img DELETE_IMG=1 \
  sudo -E bash bench/scripts/teardown.sh
```

## 旗舰真实数据集（`~/.claude/projects` 只读副本）

总纲 §4.4 / 设计 §1.1 的旗舰负载：`~/.claude/projects`（8.7GB，jsonl/txt/json，双峰大小，追加写为主，跨会话高冗余，zstd:3 实测 31x）。`datasets/fetch-claude-projects.sh` 把它的**只读副本**取到 `datasets/claude-projects/`：

```bash
# 默认代表性子集（约 1-2GB，覆盖双峰：小文件密集目录 + 至少一个含巨型 jsonl 的目录）
bash bench/datasets/fetch-claude-projects.sh
# 取全部约 8.7GB:
bash bench/datasets/fetch-claude-projects.sh --full
# 自定子集上限:
bash bench/datasets/fetch-claude-projects.sh --size-cap 3G
```

要点：

- **源绝对只读**：用 `rsync -a`（回退 `cp -a`）读取，永不修改/删除/移动源数据。
- **确定性子集**：强制纳入「含最大单文件」的巨文件锚点目录（巨型 jsonl 特征），其余按总大小升序填充至 cap（小文件密集目录优先）。绝不随机。
- **不静默截断**：打印选中/跳过的目录与字节数；锚点撑破 cap 时显式告警。
- 副本目录已在 `.gitignore` 忽略（`datasets/claude-projects/`），不入库。
- 用于基准时，把它拷进各条件挂载点（或对其跑 `grep -r`/`git status` 等真实负载，见总纲 §4.3）。

## fio job 模板说明

| job 文件 | 内容 | 块大小 |
|---|---|---|
| `seq-write.fio` | 顺序写（备份/拷贝类） | 1M |
| `rand-read.fio` | 随机读，两个子 job | 4k + 64k |
| `rand-write.fio` | 随机写（路线 B 痛点） | 4k + 64k |

要点：

- 目标目录由环境变量 `DIR` 注入（`directory=${DIR}`），`run-suite.sh` 会在每个挂载点下开 `fio-work/` 子目录传入。
- 写负载用 `buffer_compress_percentage=50`，让数据约一半可压缩，贴近「混合树」数据集；否则 fio 默认全随机数据会让 A/B2 的压缩路径失真为「不可压缩」。
- `size=1G`、`numjobs=1`。并发对照（总纲 §4.5 的 `numjobs` 1 vs N）后续可通过复制 job 或加参数扩展。
- 输出 JSON（`--output-format=json`），供 `collect.py` 解析。

## append 优化微基准（开放尾块缓冲，§1.1）

针对目标负载「逐行 append 小记录到增长文件 + 周期 fsync」的专项微基准，量化**未压缩开放尾块缓冲**优化前后的差异（吞吐 / 尾块重压次数 / 压缩比）。直接驱动 Core+Store（免 FUSE 挂载噪声），跑在 BS（影子树）与 BV（容器）上：

```bash
( cargo build --release -p scrollz-bench --bin append-bench )
target/release/append-bench                                   # 默认 64KiB 块 / 1KB 行 / 20000 行，两后端各跑 on/off
target/release/append-bench --chunk-size 4096 --line-size 512 # 小块场景
# 可调：--lines --line-size --chunk-size --fsync-every --level --backend {shadow|container|both}
```

**默认单次短跑**（约 1-5 分钟内完成两后端 × on/off 四次），输出 before/after 对照表。结果与分析见 [`results/append-opt/REPORT.md`](results/append-opt/REPORT.md)。`--no-tail-buffer`（挂载 CLI）或 bench 的 OFF 段走旧路径（每次 append 重压尾块）。

## 常见坑 / Troubleshooting

- **`teardown.sh` 报 `MNT 为空`**：teardown 要的是 **`MNT`（挂载点）**，不是 `IMG`，且 umount 需 root。正确：`sudo MNT=/mnt/scrollz-btrfs bash bench/scripts/teardown.sh`。删镜像再加 `IMG=...img DELETE_IMG=1`。
- **`setup-btrfs.sh` 报 `已是挂载点`**：该挂载点已挂着 btrfs；要重建先 teardown 卸载。若只是想换压缩模式，**不必重建**——`sudo mount -o remount,compress-force=zstd:3 <挂载点>` 即可（对后续写入生效）。
- **A 压缩比偏低（~2.44x）**：多半是 btrfs 用了**默认启发式** `compress`（漏压）。改 `compress-force`（`setup-btrfs.sh` 已默认；旧挂载用上面的 remount），同数据应到 ~6.74x。
- **compsize 报 `SEARCH_V2: Operation not permitted`**：compsize 需 root，用 `sudo compsize ...`（`measure-a-ratio.sh` 已内置 sudo）。
- **WSL 重启后 btrfs 挂不上**：`sudo modprobe btrfs`（每次 `wsl --shutdown` 后都要；脚本不擅自 modprobe）。

## 当前待办 / 风险

- **B0（FUSE 透传）已就绪**：scrollz（crates/scrollz）的 Rust `fuser` passthrough 二进制 `scrollz`，`cargo build --release` 后由 `mount-b0.sh` 挂起、`umount-b0.sh` 卸载，已纳入 `CONDITIONS`。隔离「纯 FUSE 税」的关键条件。
- **B2（fuse-zstd）待安装**：`probe-env.sh` 若报 `fuse-zstd 未在 PATH`，需先 `cargo install` 或从 `Big-Dig-Data/fuse-zstd` 源码构建，挂载后纳入。
- **冷缓存依赖 root**：非 root 且无免密 sudo 时会降级热缓存，跨条件对比会偏乐观——严格对比请整体以 root 跑。
- **WSL btrfs 模块不持久**：每次 `wsl --shutdown` 后需重新 `sudo modprobe btrfs`。`setup-btrfs.sh` 检测未加载会直接退出并提示，不擅自加载。
- **并发矩阵尚未铺开**：当前 job 为 `numjobs=1`；总纲要求的 1 vs N 并发、zstd 等级扫描（A:1/3/9/15）还需扩展 job 或参数化。
- **旗舰数据集已接入，分层其余项待补**：`fetch-claude-projects.sh` 已取 `~/.claude/projects` 旗舰真实负载；总纲 §4.4 其余分层（Linux 源码树 / 预压缩媒体 / 纯海量小文件）仍需在 `datasets/` 下补脚本。当前合成路径仍用 fio（50% 可压缩）。
