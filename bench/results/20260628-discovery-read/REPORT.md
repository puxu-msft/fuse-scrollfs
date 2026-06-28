# 发现读 micro-bench（docs/02 §6.1，head 缓存门控）

测量 2026-06-28 · 旗舰 96MB jsonl · 1MiB 块 / zstd-3 · ext4(/dev/sdd) · iters=15 取中位 · 冷态用 `posix_fadvise(DONTNEED)` 驱逐页缓存（已核实 /tmp 与项目盘均 ext4 实块设备，非 tmpfs，fadvise 有效）。

archive_total=5MiB(19x) · block0_clen=67KiB(首 1MiB→67KiB，高度可压样板，印证 §1.1) · head_cache 压缩后=20KiB(64KiB→20KiB)。

| 段(中位) | HOT | COLD |
|---|---|---|
| open 解析（footer+index+CRC+越界，**head 缓存救不了**） | 10us | 527us |
| 块 0 读+解压取 64KB（现状第二项） | 342us | 842us |
| 独立 head 流读+解压取 64KB（缓存第二项） | 51us | 265us |
| **单文件节省 (block0−head)** | **291us** | **577us** |
| 现状/文件 = open+block0 | 353us | 1369us |
| 带缓存/文件 = open+head | 62us | 792us |
| **节省占比** | **82%** | **42%** |

选择器单次扫描外推（首尾各需解块的上界）：

| N 文件 | HOT 现状→带缓存 | COLD 现状→带缓存 |
|---|---|---|
| 50  | 17.6→3.1ms（省 14.5ms） | 68.5→39.6ms（省 28.9ms） |
| 200 | 70.5→12.4ms（省 58.1ms） | 273.9→158.5ms（省 115.4ms） |
| 500 | 176.3→31.0ms（省 145.3ms） | 684.7→396.1ms（省 288.6ms） |

## 裁决：通过门槛，head 缓存值得落地

- **收益真实且显著**：单文件发现读 HOT 砍 82% / COLD 砍 42%；典型选择器扫描（N=200–500）省 58–289ms，对交互式操作可感知。
- **诚实边界（审查 H2 坐实）**：`open 解析`是不可省的地板，HOT 可忽略(10us)、**COLD 达 527us 成主导固定项**，把冷态收益封在 42%。即 head 缓存在页缓存温热（反复开选择器）时最有效，冷首扫收益减半。
- 决策量 block0(342us)−head(51us) 远大于 open(10us)，HOT 下缓存captures几乎全部可省成本，方向正确。
