# scrollz 数据损坏事故根因修复(TDD,A+B+C+D)

## Context(为什么做这次修复)

一次真实事故:`enable apply` 启用的 `-home-xp-src-neighbors`(1.6G / 1529 文件)反复出现 backing 被清空、挂载点变空。根因复盘(已用代码 + 运行时双重证实):

- `enable apply` 用 `RealMounter::spawn` re-exec 自身 + `libc::setsid()` detach 出守护([daemon.rs:50-98](../../src/scrollz/fuse/src/enable/daemon.rs#L50));父进程退出后守护成 ppid=1 **孤儿**,无人监管。
- **shadow backing 打开时完全无并发锁**([shadow.rs:142-171](../../src/scrollz/fuse/src/store/shadow.rs#L142) 只查 `is_dir`)。于是孤儿守护 + 新建守护**两个进程同时持有同一 backing**,孤儿用它启动时的空内存视图周期性覆盖,把刚 ingest 的 146M **清空** —— 数据损坏的直接机制。
- 每次重建走 `enable apply`,其 mount 步骤撞上孤儿守护持有的挂载点而失败,触发回滚 [lifecycle.rs:159-163](../../src/scrollz/fuse/src/enable/lifecycle.rs#L159) → `rollback_to_plain` → `remove_dir_all(backing)`([lifecycle.rs:439](../../src/scrollz/fuse/src/enable/lifecycle.rs#L439)),把**已 committed 的有效 backing** 也删了,放大损坏。
- 附带发现:shadow 后端 ingest 不保留原文件 mtime,挂载点文件时间变成注入时刻,会打乱 Claude Code 按时间排序会话。

数据已通过手动 systemd 托管恢复(逐字节 verify,4 副本冗余:挂载点/backing/orig/rescue)。本计划修掉**导致事故的代码缺陷**,使其不再复发。

四个 bug:
- **A【CRITICAL】** shadow backing 无并发互斥 → 数据损坏直接成因
- **B【HIGH】** apply 回滚误删已提交 backing
- **C【架构】** 守护孤儿 + 无监管 → 改 systemd per-project 模板托管
- **D【HIGH】** shadow ingest 不保留原文件 mtime

全程 TDD:每个 bug 先写失败测试(RED)→ 最小实现(GREEN)→ 重构。遵守 `cargo fmt` + `cargo clippy -D warnings`。

---

## Bug A — shadow backing 并发互斥锁(flock)

**根因**:`ShadowStore::open_with_chunk_size`([shadow.rs:142-171](../../src/scrollz/fuse/src/store/shadow.rs#L142))只做 `is_dir` 检查,无任何跨进程锁。两个守护可并发持有同一目录树。container 后端靠 redb 的 `Database::create`([container.rs:177](../../src/scrollz/fuse/src/store/container.rs#L177))自带文件锁,隐式受保护;shadow 裸奔。

**修复**:shadow open 时获取一把 **advisory flock 排他锁**(`flock(LOCK_EX|LOCK_NB)` on `<backing>/.scrollz.lock`),把 lock fd 持有在 `ShadowStore` 结构里直到 drop。第二个守护 open 同一 backing → `EWOULDBLOCK` → 返回 Err(明确报"backing 已被另一守护持有")。flock 在进程退出(含 SIGKILL)时由内核自动释放,正好解决"僵尸守护被 kill 后锁不残留"。

**TDD(纯单测,CI 可跑)**:
- RED:新建 `tempdir` backing,`ShadowStore::open_with_chunk_size` 两次 → 断言第二次 `is_err`(当前会两次都成功 → 失败)。
- 同时给 container 加一个回归守卫:`ContainerStore::open` 同路径双开,断言第二次 Err(锁定 redb 隐式锁假设)。
- 用 `tempfile::tempdir()`(已是 dev-dep)。flock 是 per-OFD,同进程不同 open 也会冲突,故同进程即可证伪。

**文件**:`src/store/shadow.rs`(open 加锁 + 结构存 lock fd + drop 释放);可抽 `src/store/lock.rs` 放可测的 `acquire_exclusive(path)->io::Result<File>` 纯原语。`src/store/container.rs` 仅加测试。

---

## Bug B — apply 回滚不删已提交 backing

**根因**:`write_meta(committed=1)` 在 [lifecycle.rs:153-155](../../src/scrollz/fuse/src/enable/lifecycle.rs#L153) 写入,**早于** mount([lifecycle.rs:159](../../src/scrollz/fuse/src/enable/lifecycle.rs#L159))。mount 失败(`spawn` Err)走 [lifecycle.rs:159-163](../../src/scrollz/fuse/src/enable/lifecycle.rs#L159) → `rollback_to_plain`([lifecycle.rs:449-459](../../src/scrollz/fuse/src/enable/lifecycle.rs#L449)) → `remove_backing`([lifecycle.rs:455](../../src/scrollz/fuse/src/enable/lifecycle.rs#L455))**无条件删 backing**。一个**已 ingest 完整 + 校验通过 + committed** 的 146M backing 被当作半灌垃圾删掉,强制全量重灌。现有测试 `apply_rolls_back_to_plain_on_mount_failure`([lifecycle.rs:644-674](../../src/scrollz/fuse/src/enable/lifecycle.rs#L644))反而**锁定了这个错误行为**。

**修复**:把"回滚"按 commit 点分两类:
- mount 失败但 **backing 已 committed** → 不删 backing/meta;把 orig 还原回挂载点(数据可用),保留 backing 为 **STOPPED 状态**(可 `remount` 直接复用,无需重灌)。给用户清晰提示"backing 完好,运行 `enable remount <name>` 重挂"。
- ingest 失败(commit 前,[lifecycle.rs:93/114/127/141](../../src/scrollz/fuse/src/enable/lifecycle.rs#L93))→ 维持现状:删半灌 backing 回 Plain。

实现:`rollback_to_plain` 拆出 commit 感知逻辑,或在 mount 失败分支改调"保留 backing 的回滚"。返回状态让 `rollback_msg` 给对应提示。

**TDD(FakeMounter 纯单测)**:
- 改写 `apply_rolls_back_to_plain_on_mount_failure`([lifecycle.rs:644](../../src/scrollz/fuse/src/enable/lifecycle.rs#L644)):用 `FakeMounter{fail_spawn:true}` 触发 mount 失败,断言**已提交 backing 仍存在**(`paths.backing("demo",Shadow).exists()` == true)+ 状态为 STOPPED(可 remount),与旧断言相反。
- 保留 `apply_rolls_back_to_plain_on_ingest_failure`([lifecycle.rs:612](../../src/scrollz/fuse/src/enable/lifecycle.rs#L612)):commit 前失败仍删 backing。
- 复用 `paths_in`/`make_project`([lifecycle.rs:515,523](../../src/scrollz/fuse/src/enable/lifecycle.rs#L515))+ `discovery::read_meta().committed` 断言。

**文件**:`src/enable/lifecycle.rs`(`apply` mount 失败分支 + `rollback_to_plain` 改造 + 测试)。

---

## Bug C — 守护改 systemd per-project 模板托管(大重构)

**根因**:裸 spawn + setsid 产生无人监管的孤儿守护([daemon.rs:50-98](../../src/scrollz/fuse/src/enable/daemon.rs#L50));autostart 是聚合 oneshot([autostart.rs:29-43](../../src/scrollz/fuse/src/enable/autostart.rs#L29)),拉起的守护仍脱离 systemd。[main.rs:623-631](../../src/scrollz/fuse/src/main.rs#L623) 已有 sd_notify READY/WATCHDOG,但空发。

**修复方案**(Plan agent 蓝图):per-project systemd user 模板实例 `scrollz@<name>.service` 托管,保持 `Mounter` trait 接口、只换实现 + 运行时按环境选择,无 systemd 自动降级 RealMounter(叠加 Bug A 的 flock 兜底)。

**关键设计决策**:
1. **模板拿参数 = 新子命令 `scrollz mount-managed --name %I`**:它 `Paths::resolve` + `discovery::read_meta(sidecar)` 自解析 backend/chunk/level/backing/mountpoint,复用 `run_mount` 挂载逻辑。sidecar meta 保持唯一真值源(对齐 `remount` 既有模式)。对称加 `umount-managed --name %I` 供 ExecStop(避免硬编码路径,兼容 `CLAUDE_PROJECTS` 覆盖)。
2. **实例名转义**:project name 形如 `-home-xp-src-neighbors`(前导/内嵌 `-` 是 systemd 特殊字符),必须 `systemd_escape`。Rust 侧自实现转义规则(`/`→`-`,非 `[0-9a-zA-Z:_.]`→`\x<hex>`,前导 `.`→`\x2e`)。unit 内用 `%I`(unescaped)还原回原名。
3. **`SystemdMounter` 实现 `Mounter`**:`spawn`=`systemctl --user start scrollz@<esc>`(Type=notify,start 阻塞到 main.rs:623 的 READY,比轮询更可靠);`unmount`=`systemctl --user stop`;`is_mounted`=仍用 `discovery::is_mounted`(查 /proc mountinfo 地面真值,非 unit active)。`MountSpec` 加 `name` 字段(RealMounter 忽略)。
4. **选择策略**:`select_mounter()` 探测 `/run/systemd/system` + `systemctl --user is-system-running` → SystemdMounter 否则 RealMounter。单点放在 `enable::run`(mod.rs)构造 mounter 处。
5. **模板 unit `~/.config/systemd/user/scrollz@.service`**:`Type=notify`、`ExecStart=scrollz mount-managed --name %I`、`ExecStop=scrollz umount-managed --name %I`、`Restart=on-failure`、`WatchdogSec=30`(启用 main.rs:624 心跳)、`WantedBy=default.target`。`autostart install` 改为装模板 + 对每个 committed project `systemctl --user enable scrollz@<esc>`。
6. **trait 扩展**:`Mounter` 加 `enable_autostart`/`disable_autostart`(default no-op,RealMounter 兼容);apply 成功后 enable,restore/purge disable。

**TDD**:
- 纯单测(CI):`systemd_escape`(对拍真实 `systemd-escape` 的硬编码 oracle + 幂等)、`resolve_managed_spec`(sidecar→MountSpec,committed=false→Err)、`systemctl_args`(命令构造,仿 [mount_argv](../../src/scrollz/fuse/src/enable/daemon.rs#L120) 可测模式)、`unit_body` 模板字段(改 [autostart.rs:114](../../src/scrollz/fuse/src/enable/autostart.rs#L114))、`select_mounter`(抽 `probe:bool` 纯函数)。`FakeMounter` 加 autostart 调用记录。
- 集成(需 systemd+/dev/fuse,CI SKIP):新 `tests/systemd_mount.rs`,仿 [mount_rw.rs:15](../../src/scrollz/fuse/tests/mount_rw.rs#L15) `skip_reason`,env 隔离(`CLAUDE_PROJECTS`/`ZIPFS_HOME`)。测 start→挂载→读写→stop→挂载消失 **且 backing 数据保留**(防 Bug B 回归)。

**新增文件**:`src/enable/systemd.rs`(escape/SystemdMounter/select_mounter)、`tests/systemd_mount.rs`。
**修改**:`src/main.rs`(MountManaged/UmountManaged 子命令 + resolve_managed_spec)、`src/enable/daemon.rs`(MountSpec.name + trait 扩展 + FakeMounter)、`src/enable/autostart.rs`(模板 + install)、`src/enable/lifecycle.rs`(mount_spec 填 name + autostart 调用)、`src/enable/mod.rs`(select_mounter)。

**迁移**:`autostart install` 先 `disable --now` 旧 `scrollz-projects.service` 再装模板。已裸 spawn 挂着的项目不强制迁移(flock 已防损坏),文档说明用 restore+apply 或 remount 平滑接管。**手动建的 `scrollz-neighbors.service`(本次事故恢复用)在 C 完成后迁移到模板 `scrollz@-home-xp-src-neighbors.service`**。

---

## Bug D — shadow ingest 保留原文件 mtime

**根因**:container ingest 用 `file_attr(&meta)`([ingest.rs:220](../../src/scrollz/fuse/src/ingest.rs#L220))从源 metadata 构造 Attr **含原 mtime/atime/ctime**([ingest.rs:212-214](../../src/scrollz/fuse/src/ingest.rs#L212))经 `store.create` 存入 → 正确。shadow ingest 的 `ingest_file`([ingest.rs:94-136](../../src/scrollz/fuse/src/ingest.rs#L94))直接 `ArchiveWriter::create` 写 archive 文件,虽读了 `src_meta`([ingest.rs:102](../../src/scrollz/fuse/src/ingest.rs#L102))却**从不设 dst 文件时间**。shadow getattr "由底层文件 meta 取真值"([mod.rs:41-43](../../src/scrollz/fuse/src/store/mod.rs#L41)),于是挂载点文件 mtime = archive 创建时刻(注入时间)。

**修复**:`ingest_file` 在 `writer.finish()` 后,用 `src_meta` 的 mtime/atime 设 dst archive 文件时间(`utimensat`,libc 已是依赖;或加 `filetime` crate)。同理 `ingest_dir` 目录时间(次要,可选)。软链时间一般无关。

> 实现前先确认:shadow getattr 的 mtime 确实读 archive **文件 fs metadata**(则设文件 mtime 即可);若读 footer 内字段,则改 footer 存 mtime(更大改动)。从 [mod.rs:41-43](../../src/scrollz/fuse/src/store/mod.rs#L41) 注释看是前者。

**TDD(纯单测)**:
- RED:`tempdir` 建源文件,`utimensat` 设一个已知的过去 mtime(如 2020-01-01),`ingest_tree`(shadow),断言 dst archive 文件 mtime == 源 mtime(当前 = 注入时间 → 失败)。更强:`ShadowStore::open` 后 `getattr_ino` 断言 mtime == 源。
- 复用现有 ingest 测试([ingest.rs](../../src/scrollz/fuse/src/ingest.rs) 末尾 mod tests)的 fixture 模式。

**文件**:`src/ingest.rs`(`ingest_file` 设时间 + 测试);若需 `filetime` 则 `Cargo.toml`。

---

## 实现顺序(每步独立编译 + 测试绿)

1. **Bug A**(backing flock):小、关键、纯单测。先做——直接堵死数据损坏机制。
2. **Bug D**(ingest mtime):小、独立、纯单测。
3. **Bug B**(回滚保护已提交):中、FakeMounter 纯单测。
4. **Bug C**(systemd 重构):大,按 Plan 蓝图 8 子步(escape→MountSpec.name→mount-managed→SystemdMounter→模板/autostart→mod.rs 切换→集成测试→迁移)。前 5 子步纯单测可在任何 CI 跑,第 6 步起在有 systemd 环境生效、无 systemd 自动降级。

A/B/D 完成即消除数据损坏与时间错乱(纵深防御不依赖 C);C 根治孤儿、提供单实例 + 监管。

---

## 验证

- `cargo fmt && cargo clippy --all-targets -- -D warnings`
- `cargo test`(全部纯单测,含新增 A/B/C/D 单测;mount_rw/passthrough 在无 /dev/fuse 时自 SKIP)
- `cargo test --features fault-injection`(Tier-1 一致性)
- 有 /dev/fuse + systemd 环境:`cargo test`(集成测试真实跑)+ 手动 `scrollz mount-managed --name <test>` 冒烟
- **不碰当前 neighbors 挂载**(已 systemd 托管稳定);C 完成后再迁移到模板 unit,迁移时逐字节 diff vs rescue 金副本确认。
- 覆盖率目标 80%(`cargo llvm-cov`,聚焦新增逻辑)。

## 风险

- flock 是 advisory:仅防 scrollz 守护互相(都走 open 路径),足够;非 scrollz 进程不受约束(可接受)。
- systemd 重构面大:严格分步、每步绿、Mounter trait 接口不变降低爆炸半径;无 systemd 环境靠 `select_mounter` 降级 + flock 兜底,行为不劣于今天。
- Bug B 改了被测试锁定的行为:需同步更新断言,确保语义是"已提交不删、半灌仍删"。
