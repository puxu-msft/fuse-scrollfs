# T4：可逆切换 + Prometheus 监控 + writeback(opt-in)

## Context
ingest 已能流式灌入+校验、statfs/sd-notify 有骨架。补生产化最后两块：可逆切换工具（真正用起来）+ Prometheus 指标导出；并验证 writeback（已确认 fuser 0.17 支持 FUSE_WRITEBACK_CACHE，无需 libfuse3）。

## 1. 可逆切换（bench/scripts/zipfs-cutover.sh）
脚本编排现成件：`mv 源→源.orig`（备份，可逆）→ `zipfs ingest --src 源.orig --backing --verify` → mount 到原路径 → 校验读回。回滚 `zipfs-rollback.sh`：卸载→`mv 源.orig 源`。零丢失（verify 通过才删/换）。

## 2. Prometheus 指标（dep-free textfile）
`zipfs-stats.{c,r}s`：ZipfsRw 暴露累计计数（seal_count 已有 wsession.rs:73、block_compress_count rmw.rs:32、compression_stats）。新 `--metrics-file <path>`：守护起后台线程周期写 `.prom`（node_exporter textfile collector 格式，零 HTTP 依赖）：zipfs_logical_bytes/physical_bytes/ratio/seals。

## 3. writeback（opt-in，独立）
`--writeback`：init `config.add_capabilities(FUSE_WRITEBACK_CACHE)` + 写 fd 去 direct_io（与 mmap 只读分支并存）。降写尾 p99；须重测 RMW 正确性 + harness。默认关（direct_io 求精确）。

## 关键文件
main.rs（--metrics-file/--writeback CLI + 后台导出）、rwfs.rs（init add_capabilities、metrics 句柄、open 写 fd 模式）、新 bench/scripts/zipfs-cutover.sh + zipfs-rollback.sh。

## 验收
切换：灌入临时目录→挂→读回逐字节→回滚还原。metrics：挂载后 cat .prom 见指标。writeback：开关各跑 harness 10/10 + 大行写一致。各独立提交、subagent review。
