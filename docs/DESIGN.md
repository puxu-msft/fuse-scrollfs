# zipfs 跨模块内部设计 / DESIGN

> **本文回答「怎么做」**——跨模块的内部机制、算法、数据模型、内部契约。随项目复杂度增长，内部机制越来越不直观，本文集中导航「机制在哪、核心契约是什么」，细节正文留在各编号专题（它们是随实现演进的领域知识文档）。
>
> 职责边界：**是什么/在哪里**看 [ARCH.md](./ARCH.md)；**为什么**看 [ADR.md](./ADR.md)。

## 1. 分块与读改写（`core/`）

逻辑文件 = 定长逻辑块序列（`DEFAULT_CHUNK_SIZE=1MiB`，见 [ADR.md](./ADR.md) D8）。每块独立压缩记录压缩后长度；随机读定位覆盖块→解压→切片；随机写走 **RMW**（解压整块→打补丁→重压→写回），首尾部分块 RMW、整块覆盖跳读。不可压缩启发式：`clen >= raw*阈值` 则原样存 + flag。

- 核心契约与块大小权衡：[01-zipfs-design.md](./01-zipfs-design.md) §3。
- 分层分块（head/body/tail 按访问模式分层）+ head 缓存快路径：[02-layered-chunking.md](./02-layered-chunking.md)。
- 解压块缓存（压力感知）：[08-observability.md](./08-observability.md) 命中率指标 + [plan/decompress-cache.md](./plan/decompress-cache.md)。

## 2. 布局 S 的每文件 archive（`archive/`）

底层目录树镜像逻辑树，每个后端文件是**分块压缩包**（非单 zstd 流），索引置尾部 footer，使 append 只需末尾增量写。

- 格式 / 追加路径 / footer 结构：[01-zipfs-design.md](./01-zipfs-design.md) §7。
- **崩溃安全提交协议**（双 superblock + 不可变块 + in-archive 尾日志 + 写序 barrier）：[04-crash-safe-commit.md](./04-crash-safe-commit.md)。这是 archive 模块的核心内部契约。
- 冷文件封存 seal（大块重压 + LDM opt-in）与 append-only 压实 compact：[ADR.md](./ADR.md) D10/D11。

## 3. 布局 V 的容器后端（`store/container.rs`）

整棵树落一个后端对象，把「命名空间/元数据 + 变长 blob 存储 + 空闲管理/崩溃一致」逼进同一文件。redb 全包默认，写批处理是必备项（一次 write 回调多块合一事务，fsync/flush 才 commit）。

- 三档形态 + microbench 裁决：[01-zipfs-design.md](./01-zipfs-design.md) §6、[ADR.md](./ADR.md) D6、[exp/container-backend-selection/REPORT.md](../exp/container-backend-selection/REPORT.md)。

## 4. 并发与锁纪律（`store/lock.rs` + `core/`）

`fuser` 多线程派发；单文件 RMW 用 per-inode RwLock（多读并发、写排他堵 torn-read）；跨 inode 操作按全局锁顺序（ino 升序）避免死锁。lookup-count/forget 延迟回收保 POSIX 打开语义。

- 前端标记锁正确性 + 后端整改：[plan/concurrency-remediation.md](./plan/concurrency-remediation.md)（D1–D6 缺陷修复 + loom 证明）。

## 5. 故障注入测试架构（`blockio.rs` + `fixture.rs`）

两层：进程内 `BlockIo` 接缝 + `FaultIo` 确定性崩溃模拟器（EIO/撕裂/掉电/乱序 × barrier 软化），穷举 kill/tmpfs 测不到的崩溃点；Tier 2 dm-flakey/dm-log-writes/container 真实块层门（root 门控）。

- 完整规格：[05-fault-injection-testing.md](./05-fault-injection-testing.md)。

## 6. 生产化机制（`enable/`）

发现 `~/.claude/projects/*` → 可逆切换（ingest `--verify` + sidecar 提交标记使半灌可检测 + 活跃会话拦截 + 失败回滚 Plain）→ 守护（systemd + sd-notify）→ 分档卸载（clean/lazy/abort/auto，全程 hang-free）。

- 启用器与 TUI：[plan/enable-tool.md](./plan/enable-tool.md)；探测层加固：[plan/enable-probe-hardening.md](./plan/enable-probe-hardening.md)。
- Hang-free 分档卸载：[07-hangfree-umount.md](./07-hangfree-umount.md)。
- 可观测性 / Prometheus 指标：[08-observability.md](./08-observability.md)。

## 7. 会话感知回落写重合并（`reconcile/`）

停用期落进裸挂载点的回落写，安全无损并回 archive；纯合并核（record 解析→无损并集→advisor 推荐，全无 IO 单测）+ orchestrator（活跃门禁/快照/锁/原子替换/超集删除许可）+ 真挂载入口守卫。

- 完整设计：[09-session-reconcile.md](./09-session-reconcile.md)；undo + memory 短路：[plan/reconcile-undo.md](./plan/reconcile-undo.md)。
