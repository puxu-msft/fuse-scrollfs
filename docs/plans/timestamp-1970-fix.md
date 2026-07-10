# 修复 zipfs 挂载文件时间戳全为 1970

## Context（为什么做这个改动）

在 `~/.claude/projects/-home-xp-src-neighbors`（`fuse.zipfs-shadow` 挂载）上，`ls` 显示**所有文件**的 atime/mtime/ctime 都是 `1970-01-01 08:00`（UNIX_EPOCH+08 时区），而底层原始目录 `…neighbors.zipfs-orig/` 里的真实日期（如 `2026-06-24 04:47`）完好无损。

根因（已实证定位，非推测）：

1. **store 层 `Attr` 结构体根本没有时间戳字段** —— [fuse/src/store/mod.rs:34](fuse/src/store/mod.rs#L34) 只有 `ino/size/kind/perm/uid/gid/chunk_size`。
2. 前端 `to_file_attr` 把四个时间**写死成 `UNIX_EPOCH`** —— [fuse/src/rwfs.rs:152-155](fuse/src/rwfs.rs#L152-L155)。
3. shadow 后端 `attr_from_meta` 手里就握着底层文件真实 `meta`（含正确 mtime/atime/ctime），却只取了 size/perm/uid/gid，**丢弃了时间** —— [fuse/src/store/shadow.rs:187-208](fuse/src/store/shadow.rs#L187-L208)。
4. `setattr` 的 `_atime/_mtime/_ctime` 三参数被前缀 `_` 忽略 —— [fuse/src/rwfs.rs:575-577](fuse/src/rwfs.rs#L575-L577)，shadow setattr 也只落 perm —— [fuse/src/store/shadow.rs:520-528](fuse/src/store/shadow.rs#L520-L528)。

影响：Claude Code 按 mtime 排序/识别「最近会话」，全 1970 会打乱 `/resume` 顺序与近期判定；任何依赖文件日期的工具都失真。

期望结果：挂载点文件呈现底层真实时间；`touch -d` / `cp -p` / `rsync -a` 设置的时间被持久化；容器（V）布局同样支持。

**可复用的现成模式**：只读 `passthrough.rs` 已正确实现本特性 —— `system_time_from(secs, nsec)` 助手 [fuse/src/passthrough.rs:257](fuse/src/passthrough.rs#L257) 与 `meta.mtime()/mtime_nsec()` 取值 [fuse/src/passthrough.rs:241-243](fuse/src/passthrough.rs#L241-L243)，并在 [:380](fuse/src/passthrough.rs#L380) 留有 futimens 的 setattr TODO。本计划即把同一模式补进读写前端 `rwfs` + `Store` 接缝。

## 实现方案

### 1) 数据模型：`Attr` 加时间字段 — `fuse/src/store/mod.rs`
在 `Attr` 增加 `pub mtime: SystemTime`、`pub atime: SystemTime`、`pub ctime: SystemTime`（`use std::time::SystemTime`）。
- 不加 `crtime`（Linux 罕用且底层多不支持）；前端 crtime 复用 ctime。
- 加字段后所有 `Attr {…}` 字面量构造点（约 15 处，编译错误会逐个点出）按语义补值：
  - shadow `attr_from_meta`：由 `meta` 取（见 §2）。
  - container `row_to_attr`：由行取（见 §3）。
  - 前端 create/mkdir/symlink 构造给 store 的 attr（[rwfs.rs:433/475/751](fuse/src/rwfs.rs#L433)）：`SystemTime::now()`。
  - 合成/基准/夹具（`bin/*-bench.rs`、`ingest.rs`、`seal.rs`、`compact.rs`、`tests_support.rs`）：`now()` 或 `UNIX_EPOCH` 皆可，取就近。

### 2) 读路径（直接消除 1970）
- **shadow** `attr_from_meta` — 复用 `system_time_from(meta.mtime(), meta.mtime_nsec())` 等填 mtime/atime/ctime（`MetadataExt` 已 `use`，[shadow.rs:27](fuse/src/store/shadow.rs#L27)）。把 `system_time_from` 从 passthrough 提到共享位置（如 `core` 或 `store`）复用，不复制。
- **前端** `to_file_attr` [rwfs.rs:147](fuse/src/rwfs.rs#L147) — 改用 `a.mtime/a.atime/a.ctime`，`crtime: a.ctime`。

### 3) 容器后端格式 — `fuse/src/store/container.rs`
- `InodeRow` 加 `mtime/atime/ctime`，每个存 `i64 secs + u32 nsec`（12B×3=36B），`INODE_ROW_LEN` 23 → **59**。
- `encode` 写新布局；`decode` **长度容忍**：`len==23`（旧档）→ 时间填 `UNIX_EPOCH`；`len==59` → 正常读。保证已存在的 V 档案不被破坏（可逆）。
- `create`/`mkdir` 建行时时间置 `SystemTime::now()`；`row_to_attr` 回填。

### 4) 写回路径（setattr）
- **前端** `setattr` [rwfs.rs:567](fuse/src/rwfs.rs#L567) — 解析 `_atime/_mtime`（`TimeOrNow::Now → SystemTime::now()`，`SpecificTime(t) → t`）与 `_ctime`；写入 `a`，并把「有时间更新」纳入调用 `store.setattr` 的触发条件（当前仅 perm/uid/gid 触发）。
- **shadow** `setattr` [shadow.rs:520](fuse/src/store/shadow.rs#L520) — 经 `libc::utimensat`(已依赖 `libc`) 把 mtime/atime 落到底层文件。ctime 在 Linux 无法直接设定（元数据变更即「now」），跳过其写回并注释说明。
- **container** `setattr` [container.rs:454](fuse/src/store/container.rs#L454) — 把三时间写进 `InodeRow`。

## 测试（TDD，先红后绿）

- **shadow 读**：以已知 mtime 建底层文件（测试内 utimensat 设一个固定时间）→ `getattr_ino`/`attr_from_meta` 断言 `Attr.mtime` 与之相等（验证不再 epoch）。
- **shadow 写回**：`setattr` 设定 mtime → 重新 `getattr_ino` 往返一致，且底层文件 `meta.modified()` 同步变化。
- **container 往返**：`InodeRow` encode→decode 含三时间一致；**旧档兼容** —— 构造 23B 旧行喂 `decode`，断言成功且时间为 `UNIX_EPOCH`。
- **container setattr**：设 mtime → `getattr_ino` 反映新值（跨 reopen 持久）。
- **前端映射**：`TimeOrNow::Now/SpecificTime` → 正确 SystemTime（小单元）。
- 既有套件（`passthrough/mount_rw/model_based/fault_injection/enable`）须仍绿。

## 端到端验证

```
cargo test -p <fuse-crate>           # 全套件含新增时间用例
cargo clippy -- -D warnings && cargo fmt --check
```
重挂 neighbors（或临时挂 .zipfs-orig 子集）后：
```
ls -la --time-style=long-iso <挂载点>    # 应显示 2026-… 真实日期，非 1970
touch -d '2025-01-02 03:04' <挂载点>/某文件 && ls -la …   # 写回生效
```
对照底层 `…neighbors.zipfs-orig/` 同名文件日期应一致。

## 风险

- 容器 `INODE_ROW_LEN` 变更属磁盘格式演进：靠 decode 长度容忍保持对旧 V 档案**只读兼容、可逆**；新写出的档案旧版本 zipfs 无法读（向后不兼容，符合「加字段」预期）。用户部署为 shadow 后端，不受影响。
- `system_time_from` 提取为共享助手时勿改 passthrough 现有行为（保持 `secs<0 → epoch`）。
