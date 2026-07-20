# LDM 压缩比补测 / LDM Ratio Measurement (M2)

> 日期：2026-07-06。工具：`fuse/src/bin/ldm-ratio.rs`（逐块 `compress_with_params`，`CompressParams::sized` 同基准只切 `enable_ldm`）。驱动：`bench/scripts/ldm-ratio-matrix.sh`。
> 目的：量化编码侧 zstd 长程匹配（LDM / `--long`，提交 `e9643b6`）在**真实 `~/.claude/projects` 语料**上的压缩比收益，作为「是否调大默认 `DEFAULT_SEAL_CHUNK`（现 8MiB）」的决策依据。回填缺陷审查台账 / ROADMAP T3 里「机制已落地、真实收益未实测」的保留项。

## 方法学 / Method

- LDM **只对 ≥8MiB 的单块**有效：每块独立压缩，块 <8MiB 时 windowLog<23、落在 zstd 默认窗口内，LDM 近乎 no-op。故收益只可能来自语料里的大 transcript 文件。
- 同基准对照：LDM 开/关两组用**同一方法**（逐块 `compress_with_params`，level 19，尊重 verbatim），唯一变量是 `enable_ldm`。
- 首个 ≥8MiB 文件逐块 round-trip（`decompress_block` window_log_max=27）逐字节校验通过（防「测量帧解不出」）。

## 语料 / Corpus

`bench/datasets/claude-projects`：408 文件 / 676.3MiB，中位 107KiB，最大 96.4MiB。**19 个 ≥8MiB 大文件（会话 jsonl）占 89.6% 字节**——LDM 有真实发挥空间。

| 阈值 | 文件数 | 字节占比 |
|---|---|---|
| ≥8MiB | 19 | 89.6% |
| ≥16MiB | 15 | 83.0% |
| ≥32MiB | 9 | 64.2% |
| ≥64MiB | 1 | 14.3% |

## 聚合比值矩阵 / Aggregate ratio (全语料 676.3MiB，level 19)

| chunk | LDM off | LDM on | 纯 LDM 提升 |
|---|---|---|---|
| 8MiB  | 20.152x | 20.167x | **+0.07%**（windowLog=23=默认窗口，LDM 白开——即现默认的处境）|
| 16MiB | 20.971x | 21.294x | +1.54% |
| 32MiB | 21.474x | 22.195x | +3.36% |
| 64MiB | 21.717x | 22.860x | **+5.26%** |

> 64MiB 行为符号链接修复后的干净数（files=408）；8/16/32 行为初测（files=431，含 1 个 symlink 多计的 23 个外部小 md，对聚合比值影响 <0.1%，LDM 增量不受影响）。趋势：LDM 收益随 chunk **单调放大**（windowLog 23→24→25→26 覆盖更远重复距离）。

## 每文件比值 / Per-file (top 5 大 transcript @ chunk=64MiB)

| 文件 | 大小 | off | on | 提升 |
|---|---|---|---|---|
| d2ab6c11 | 96.3MiB | 29.331x | 34.077x | **+16.2%** |
| 15448d41 | 62.4MiB | 37.308x | 41.557x | +11.4% |
| eb4f1c47 | 48.8MiB | 44.375x | 50.133x | +13.0% |
| 20ce48f4 | 41.9MiB | 30.941x | 33.739x | +9.0% |
| 413d295c | 38.8MiB | 26.627x | 28.828x | +8.3% |

## 结论 / Conclusion

**LDM 在本语料兑现收益，但聚合被稀释、真正收益集中在大文件。**

- **聚合**：64MiB 档 LDM 净增 +5.26%；8MiB 档几乎为零（+0.07%）——坐实「`DEFAULT_SEAL_CHUNK=8MiB` 时 LDM 白开」。
- **每大 transcript**：64MiB 下 +8%～+16%，最大 96MiB 文件省 16.2%。这是 LDM 的正当发挥场景。
- **调大默认 seal 块的收益**：从「8MiB + 无 LDM」提到「64MiB + LDM」，本语料 sealed 物理占用 33.6MiB→29.6MiB（**-11.9%**，聚合 20.15x→22.86x，+13.4%），其中提块本身贡献 ~+7.6%、LDM 再叠 +5.26%。代价：seal 块更大 → 冷读 RMW 放大 + 单块解压内存上升（seal 是冷归档路径，可接受）。

## 红线 / Caveat

以上收益的前提是**本 backing 被大 transcript 主导**（89.6% 字节 ≥8MiB）。若某库以小文件为主，LDM 与调大 seal 块的收益都趋近于零——**调大默认应绑定「典型 backing 是否含大会话文件」，而非无条件**。

## 复现 / Reproduce

```bash
cd fuse && cargo build --release --bin ldm-ratio
bash bench/scripts/ldm-ratio-matrix.sh bench/datasets/claude-projects
# 或单配置：
fuse/target/release/ldm-ratio --input bench/datasets/claude-projects \
  --chunk-size 67108864 --level 19 --long --max-bytes 1073741824
```
