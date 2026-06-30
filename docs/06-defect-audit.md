# 06 · 缺陷审查台账 / Defect Audit — 单一信息源

> 状态：第二轮全面审查完成并修复（23 提交，188 lib + 31 集成测试全绿，fmt/clippy 干净）。日期：2026-06-30。
> 上游：第一轮数据损坏事故（Bug A/B/C/D）见计划 `~/.claude/plans/cheeky-hatching-clock.md` 与提交 `95775d3..e70bed9`。本文收敛**第二轮**（数据安全四梯队 + enable 编排竞态）的全部发现、修复、未做项与判断依据，作为后续路线图的缺陷侧单一信息源。

## §0 背景 / Background

第一轮（A/B/C/D）修掉了反复 apply 撞孤儿守护导致的会话日志覆盖事故。第二轮是一次**主动全面审查**：6 路并行 reviewer（5 个 `rust-reviewer` 按模块集群 + 1 个 `Explore` 跨切面）扫描全仓，主代理对每条 CRITICAL **独立核实代码事实**后再动手（不盲信审查结论，曾据此下调 C1 的严重性定级）。

修复纪律：每条 RED→GREEN→`cargo build`+`clippy`+`fmt`→精确 pathspec 提交。可确定性单测的都补了失败先行的回归；真实 FUSE/systemd/进程竞态路径靠 inspection 正确性 + 已有回归 + A3 终极锁防线兜底（见 §4）。

## §1 严重度口径 / Severity

| 级别 | 含义 | 处置 |
|---|---|---|
| CRITICAL | 数据丢失 / 静默损坏 | 必修，已全部修复 |
| HIGH | bug / 安全洞 / 显著质量问题 | 已全部修复 |
| MEDIUM | 可维护性 / 回报与现实不一致 | 已修主要项 |
| LOW | 风格 / best-effort 日志 | 部分修复，余记 §3 |

## §2 已修缺陷台账 / Fixed Defects

### 梯队一 · 崩溃一致性与数据安全（CRITICAL/HIGH）

| 编号 | 位置 | 缺陷 | 提交 |
|---|---|---|---|
| A1 | `enable/lifecycle.rs` apply | `rename(mp→orig)` 与 `create_dir(mp)` 之间失败无回滚 → 项目目录"蒸发"到 `.zipfs-orig`（list 扫不到），用户恐慌二次误操作 | `2708ea9` |
| A2 | `store/shadow.rs::create` | 新建 archive 只 fsync 文件、未 fsync 父目录 → 崩溃后新文件整体丢失。新 `core::fsync_dir_of` 助手，seal/compact 统一改用（带 warn，并修 L2 静默吞错） | `b2f91d1` |
| A3 | `compact.rs`/`seal.rs`/`enable/lifecycle.rs::maintain` | **Bug A 在维护路径复发**：compact/seal 裸函数不取 flock，与活守护并发 temp+rename 互覆盖。`lock::backing_lock_path/acquire_backing` 下沉互斥域；maintain 对 shadow 也要求守护退出 | `d6cec70` |
| B1/B2 | `archive.rs::truncate` + `store/shadow.rs::read_head_cache` | truncate 不失效越界 head 缓存 → 发现读返回已截掉的旧前缀。truncate 在 `new_size<rawlen` 时清缓存 + 读路径 `covered` clamp 到 `uncompressed_size` | `5750463` |

### 梯队二 · 正确性（CRITICAL/HIGH）

| 编号 | 位置 | 缺陷 | 提交 |
|---|---|---|---|
| C1 | `ingest.rs::verify_file{,_in_store}` | 不校验源 EOF / 总长 → archive 短于源时尾部字节漏检、静默通过，使"逐字节校验"形同虚设。补 EOF + `total==size` 断言 | `0a78cc7` |
| D1 | `rwfs.rs` | `ZipfsRw` 无 `forget` → `locks`/`tails` 映射随只追加不删除会话无界增长。实现 forget：先 seal+flush（保数据）再丢缓冲 + evict 锁 | `8a6d91a` |
| D3 | `store/container.rs::rmdir` | `rmdir`=`unlink` 不查空目录 → 子项 dirent/inode 成孤儿（无法 lookup 又永占空间）。补 ENOTEMPTY | `b11d940` |
| E1 | `store/{shadow,container}.rs` + `store/mod.rs` | Store API 不校验 name → `ingest_dir_into_store` 绕内核喂 `..`/`/` 可逃逸 backing / 污染键空间。新 `store::validate_name`，guard create/mkdir/symlink/rename | `5621c67` |

### 梯队三 · 加固（HIGH/MEDIUM）

| 编号 | 位置 | 缺陷 | 提交 |
|---|---|---|---|
| D2 | `store/container.rs` 删块 | `range((x,0)..(x+1,0))` 在 `x==u64::MAX` 溢出（release 回绕成空范围 → 块漏删）。改 `..=(x, u64::MAX)` RangeInclusive | `e92e841` |
| H2 | `core/codec.rs::decompress_block` | 解压无 window/输出上限 → 恶意/损坏块可炸成任意大 OOM（CRC 可被蓄意重算）。加 `window_log_max=27` + 256MiB 输出帽，降级为 InvalidData | `06abdf0` |
| — | `core/mod.rs` + `main.rs` + `enable/lifecycle.rs` | CLI `--chunk-size` 无上限 → `vec![0u8; chunk_size]` 数 GB OOM。`MAX_CHUNK_SIZE=64MiB` 校验入口 | `92efb33` |
| C2 | `main.rs::run_ingest` | 独立 `zipfs ingest` 退出码不反映 `errors`/`skipped` → 外层脚本据此删源丢数据。非零退出 | `92efb33` |
| B3 | `archive.rs::commit{,_journal}` | 契约仅靠注释维系：index 变更后误走 commit_journal → 新块不可达；journal 未重置走 commit → 陈旧 delta 污染封块。加 `index_dirty` 守卫双不变量 | `f228a53` |
| — | `tests/append_tail_buffer.rs` | 测试盲区：现有最大 ~200KiB，从不跨 1MiB 默认块。补大文件多块 + 跨块 append 逐字节回归 | `8108aad` |

### 梯队四 · enable 编排竞态（HIGH/MEDIUM）

| 编号 | 位置 | 缺陷 | 提交 |
|---|---|---|---|
| M4 | `enable/discovery.rs::write_meta` + `config.rs` | **唯一真安全洞**：含 `\n` 的 dict/metrics_file 路径可注入伪造 `committed=1`（parse 末键胜出）篡改挂载闸门。fail-closed 拒控制字符 | `7942034` |
| M2 | `enable/lifecycle.rs::rollback_to_plain` | backing 残留却返回 true（谎称已回滚）。返回纳入 `!backing.exists() && !meta.exists()` | `1934678` |
| A4/C2 | `enable/discovery.rs::is_mounted` | 整路径 canonicalize 失败退化为原路径 → stale endpoint 漏判已挂载。退而规范化父目录拼回末段 | `63082ba` |
| M1 | `enable/lifecycle.rs::remount` | 清 stale endpoint 后盲目 spawn 撞占用。复核 `is_mounted` 仍占则 fail | `82030e3` |
| H4 | `enable/systemd.rs::SystemdMounter::unmount` | mounter 漂移（apply 走 Real、restore 走 Systemd）→ stop 不存在的 unit 卡还原。unit 不存在且仍挂载时回退 `unmount_path`(fusermount) | `e9c37bb` |
| D4/H2 | `enable/lifecycle.rs::wait_daemon_exit` | pid-file 单次读失败即判退出 → systemd 重启的删除→重写窗口里误判、在锁仍被占时换 backing。要求**连续缺失 3 次**才判退出 | `24b4a41` |
| M3/H1/C3 | `enable/lifecycle.rs` + `discovery.rs` + `main.rs` | write_meta 失败给"backing 完好、reingest/restore"指引而非裸 error；活跃检测 EACCES 跳过计数 warn（盲区可观测）；managed mount/umount 错误附原始 `%i` 实例名（unescape 有损便于反查） | `a8cf2f7` |
| H3 | `enable/systemd.rs::SystemdMounter::spawn` | 单次 is_mounted 快照在守护抖动时误报失败（systemd 几秒后又挂上）。改 20×100ms 短轮询就绪 | `8fe559f` |

## §3 未修项与判断依据 / Not Fixed (with rationale)

均**不丢数据**，属结构性或 best-effort 噪声：

- **活跃检测 TOCTOU**（`detect_activity`→`rename` 间隙）：inherent，无法用进程内检测彻底消除（窗口内 Claude 可开 fd 开始 append）。已由 C3 收窄（紧贴 rename 前再查）+ EACCES 盲区告警缓解。本设计下 orig 是金副本不丢数据，仅"挂载期写入与 orig 分叉"——记为已知边界。
- **L1** `enable/daemon.rs` SIGTERM 后 `child.wait()` best-effort，不校验真退出/卸载干净。
- **L2** `enable/autostart.rs::migrate_off_aggregate_unit`：只要有 systemctl 就跑一次无害但噪声的 `disable`。
- **L3** `enable/discovery.rs` mtime 在未来时 `unwrap_or(true)` 判活跃——安全方向（宁可误拦），已注释为有意。
- **L4** `enable/tui.rs` 批量 `scan().unwrap_or_default()` 失败静默当空集。
- **container M2** `decode_block` 空 blob 静默返回空块而非 InvalidData——改签名侵入，"理论不该出现"，低值。
- **inode.rs `LogicalAttr::new` 的 `todo!()`**：P0 遗留死占位，零调用方、运行时不可达、无 panic 风险，建议清理（非缺陷）。
- **依赖 CVE**：`cargo-audit` 未安装，transitive 依赖无 CVE 结论（依赖面小而新，无明显 unmaintained）。建议安装后复跑。

## §4 测试性边界 / Testability Boundary

部分修复无法确定性单测，处置如下：

- **真实 FUSE/systemd/进程竞态**（M1/H4/H3/D4）：FakeMounter 无法模拟 broken-FUSE endpoint（`endpoint_ok` 是真 FS 检查）、systemd unit 状态、PID 复用窗口。这些是 inspection-correct 防御性修复，靠已有 maintain/remount 回归 + **A3 的 backing flock 终极防线**兜底：compact/seal 持锁，即便 `wait_daemon_exit` 误判退出也不会 clobber（操作 fail-closed 得 WouldBlock）。
- **可确定性单测的都补了 RED→GREEN**：A1（back_root 占位文件令 create_dir 失败）、B1（archive 直接 truncate）、C1（手造短 archive，python 临时改源验证 RED）、D1（cs 对齐的 tails append + forget）、D3（mkdir+create 后 rmdir）、E1（`create("..")`）、D2（容器删块）、H2（解压炸弹帧）、M4（含 `\n` 路径）、M2（只读目录注入，root 跳过）、A4（symlink 父目录）。
- **debug_assert（B3）在 `--release` 不触发**：必须用 `cargo test`（debug）跑 `append_tail_buffer`/`model_based` 确认不变量在真实 seal/journal/truncate 路径成立——已验证全绿。

## §5 验证状态 / Verification

```
cargo test --release            # 188 lib + 31 集成（13 二进制）全绿，0 失败
cargo test --test model_based   # debug：B3 断言在差分负载下成立
cargo clippy --release --all-targets --features fault-injection   # 0 warning
cargo fmt --check               # clean
```

## §6 相关 / See Also

- [04-crash-safe-commit.md](./04-crash-safe-commit.md) — 双 superblock + 尾日志（A3/B3 的格式基础）
- [05-fault-injection-testing.md](./05-fault-injection-testing.md) — 故障注入两层架构
- [ROADMAP.md](./ROADMAP.md) — T0–T4 路线图（本台账是其缺陷侧输入）
- 第一轮 A/B/C/D：提交 `95775d3..e70bed9`
