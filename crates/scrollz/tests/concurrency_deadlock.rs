//! 并发死锁回归（live 症状：已启用目录的 shadow 挂载 **wedge**，对挂载点 `ls`/读必 hang）。
//!
//! 两类已确认根因各配一个压力测试，**全部经真实挂载点用普通 syscall 触发**（最忠实地复现
//! 「挂载卡死」这一线上症状，且不依赖任何 crate 内部类型 / 不碰 src 文件）：
//!
//!   1. **ShadowStore `inodes`/`sessions` AB-BA**：`unlink`（持 `inodes` 守卫——edition 2021 里
//!      `if let` scrutinee 的 `MutexGuard` 跨 body 存活——再取 `sessions`/`readers`）与
//!      RMW/truncate（持 `sessions` 经 `ensure_session`→`abs_of_ino`→`rel_of` 再取 `inodes`）
//!      构成反序环。两线程交错即死锁，`inodes` 被永久持有 → 之后**任何** lookup/getattr/readdir
//!      全部阻塞 → 整挂载 wedge。
//!   2. **FUSE notify 重入**：`fsync`/`flush` 在仍持 per-inode 写 `RwLock` 时调 `inval_inode`，
//!      与并发 `read` 同 inode 在内核页缓存锁上形成跨层反序环（worker 线程逐个被吃光 → 饿死）。
//!
//! 机制：每个测试在挂载点上起多线程压力跑固定时长，主线程用**看门狗超时**把「worker 永不收尾」
//! 的死锁转成断言失败（而非让整套测试无限 hang）。死锁发生时 worker 阻塞在 D 态 syscall，无法被
//! 信号/`timeout(1)` 收割——故清理顺序是**先 SIGKILL 守护进程**（解开内核 FUSE 连接、令阻塞
//! syscall 返回 ENOTCONN），worker 随即退出，再 join。
//!
//! 无 `/dev/fuse` / 无 `fusermount` 优雅跳过，不 panic（对齐 `mount_rw.rs` 约定）。

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

// ---- 环境探测 / 挂载脚手架（与 mount_rw.rs 同款，集成测试间不共享辅助，故各自带一份）----

fn scrollz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_scrollz"))
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(bin))
        .find(|p| p.is_file())
}

fn skip_reason() -> Option<String> {
    if !Path::new("/dev/fuse").exists() {
        return Some("/dev/fuse 不存在".to_string());
    }
    let has = ["fusermount3", "fusermount"]
        .iter()
        .any(|b| which(b).is_some());
    if !has {
        return Some("找不到 fusermount3/fusermount".to_string());
    }
    None
}

fn wait_mounted(mountpoint: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            if mounts
                .lines()
                .any(|l| l.split_whitespace().nth(1) == mountpoint.to_str())
            {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn still_mounted(mountpoint: &Path) -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|m| {
            m.lines()
                .any(|l| l.split_whitespace().nth(1) == mountpoint.to_str())
        })
        .unwrap_or(false)
}

fn unmount(mountpoint: &Path) {
    for attempt in 0..5 {
        // 守护已被 SIGKILL 时内核常已自动摘下挂载 → 不在 /proc/mounts 就别再喊 fusermount
        // （否则刷一屏 "entry not found in /etc/mtab" 噪声）。
        if !still_mounted(mountpoint) {
            return;
        }
        for bin in ["fusermount3", "fusermount"] {
            if which(bin).is_none() {
                continue;
            }
            // -u 优先；wedge 状态下可能 EBUSY，再用 -uz 惰性摘下。
            let _ = Command::new(bin)
                .arg("-u")
                .arg(mountpoint)
                .stderr(std::process::Stdio::null())
                .status();
            if !still_mounted(mountpoint) {
                return;
            }
            let _ = Command::new(bin)
                .arg("-uz")
                .arg(mountpoint)
                .stderr(std::process::Stdio::null())
                .status();
        }
        if attempt < 4 {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// 强制 abort 该挂载点的 FUSE 连接：从 `/proc/self/mountinfo`（只读文本、不 stat，wedge 安全）
/// 取其 `major:minor`，向 `/sys/fs/fuse/connections/<minor>/abort` 写入，令内核**强制了结所有挂起
/// FUSE 请求**。这是「内核 notify 重入死锁」下唯一能唤醒卡在 `inval_inode` 的 **D 态**守护线程的手段
/// （`fusermount -uz` 惰性卸载不 abort 挂起请求）——也正是 enable 层 wedge 恢复应具备的原语。
fn abort_fuse_conn(mountpoint: &Path) {
    let Ok(mi) = fs::read_to_string("/proc/self/mountinfo") else {
        return;
    };
    let mp = mountpoint.to_str().unwrap_or("\0");
    for line in mi.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // mountinfo：mount_id parent_id major:minor root mount_point ...
        if f.len() > 4 && f[4] == mp {
            if let Some((_maj, min)) = f[2].split_once(':') {
                let _ = fs::write(format!("/sys/fs/fuse/connections/{min}/abort"), "1");
            }
        }
    }
}

/// 一次 shadow 挂载会话。`child` 为守护进程；`Drop` 兜底卸载 + 杀守护。
struct ShadowMount {
    child: Option<Child>,
    mountpoint: PathBuf,
    _backing: tempfile::TempDir,
    _mountdir: tempfile::TempDir,
}

impl ShadowMount {
    /// 启动一个 shadow 挂载。返回 None 表示环境不允许 FUSE（调用方应跳过）。
    fn start(chunk_size: u32) -> Option<ShadowMount> {
        let backing = tempfile::tempdir().ok()?;
        let mountdir = tempfile::tempdir().ok()?;
        let mountpoint = mountdir.path().to_path_buf();
        let child = Command::new(scrollz_bin())
            .arg("--backend")
            .arg("shadow")
            .arg("--backing")
            .arg(backing.path())
            .arg("--mountpoint")
            .arg(&mountpoint)
            .arg("--chunk-size")
            .arg(chunk_size.to_string())
            .env("RUST_LOG", "warn")
            .spawn()
            .ok()?;
        let mut m = ShadowMount {
            child: Some(child),
            mountpoint,
            _backing: backing,
            _mountdir: mountdir,
        };
        if !wait_mounted(&m.mountpoint, Duration::from_secs(5)) {
            // 起不来：清理后让调用方跳过。
            m.force_kill();
            return None;
        }
        Some(m)
    }

    /// 恢复顺序对 wedge 至关重要：
    /// - 普通死锁（守护 worker 卡在用户态锁）：SIGKILL 守护即可，内核 abort 连接、client 解阻塞。
    /// - **内核 notify 重入死锁**（守护线程卡在 `inval_inode` 的**内核** D 态）：SIGKILL 唤不醒 D 态
    ///   线程，必须先 `fusermount -uz` **惰性卸载**——它 abort FUSE 连接、强制了结所有挂起请求，
    ///   才能解开 client 的 D 态。故先 `-uz`，再杀守护，再常规卸载兜底。可重复调用。
    fn force_kill(&mut self) {
        // 先 abort FUSE 连接（唯一能解开内核 notify 重入 D 态的手段），再惰性卸载，再杀守护。
        abort_fuse_conn(&self.mountpoint);
        for bin in ["fusermount3", "fusermount"] {
            let _ = Command::new(bin)
                .arg("-uz")
                .arg(&self.mountpoint)
                .stderr(std::process::Stdio::null())
                .status();
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        unmount(&self.mountpoint);
    }
}

impl Drop for ShadowMount {
    fn drop(&mut self) {
        self.force_kill();
    }
}

/// 看门狗：从 `rx` 等 `n` 个完成信号，总预算 `timeout`。全部按时收到返回 true；任一超时返回
/// false（即检测到死锁——不再 join，避免主线程也一起 hang）。
fn await_workers(rx: &mpsc::Receiver<()>, n: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    for _ in 0..n {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        if rx.recv_timeout(deadline - now).is_err() {
            return false;
        }
    }
    true
}

/// 压力时长：worker 在挂载点上跑这么久的高频并发操作；死锁在远小于此的时间内即触发。
const WORK: Duration = Duration::from_secs(6);
/// 看门狗预算：> WORK，留足正常收尾余量；死锁则在此触发失败。
const WATCHDOG: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// 测试 1：ShadowStore inodes/sessions AB-BA（unlink vs RMW/truncate）
// ---------------------------------------------------------------------------

#[test]
fn shadow_mount_unlink_vs_rmw_no_deadlock() {
    if let Some(reason) = skip_reason() {
        eprintln!("[SKIP] shadow_mount_unlink_vs_rmw_no_deadlock：{reason}");
        return;
    }
    let Some(mut mount) = ShadowMount::start(4096) else {
        eprintln!("[SKIP] 5s 内未观察到 shadow 挂载，疑似环境不允许 FUSE");
        return;
    };
    let mp = mount.mountpoint.clone();

    // 一个多块热文件，供 RMW/truncate 线程反复随机写（→ put_block / truncate_blocks，持
    // sessions 经 ensure_session 取 inodes）。
    let hot = mp.join("hot.dat");
    fs::write(&hot, vec![7u8; 4096 * 4]).expect("初始化热文件");

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let deadline = Instant::now() + WORK;
    let mut handles = Vec::new();

    // RMW/truncate 线程：seek+write 命中已封内部块 → RMW 走 put_block；set_len 走 truncate_blocks；
    // sync_all 触发 commit_session。三者都经 ensure_session 取 inodes（持 sessions）。
    for w in 0..2 {
        let hot = hot.clone();
        let stop = Arc::clone(&stop);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let _ = (|| -> std::io::Result<()> {
                    let mut f = fs::OpenOptions::new().write(true).open(&hot)?;
                    let off = ((i % 3) * 4096 + (w as u64) * 17) % (4096 * 3);
                    f.seek(SeekFrom::Start(off))?;
                    f.write_all(b"RMW-PAYLOAD")?; // 改写已封内部块 → put_block
                    f.sync_all()?; // fsync → commit_session
                    if i.is_multiple_of(8) {
                        // 截断到不同大小 → truncate_blocks（也经 ensure_session 取 inodes）。
                        let len = 4096 * 4 - (i % 4) * 512;
                        let g = fs::OpenOptions::new().write(true).open(&hot)?;
                        g.set_len(len)?;
                    }
                    Ok(())
                })();
                i += 1;
            }
            let _ = tx.send(());
        }));
    }

    // unlink 线程：create+unlink 各自独占文件名（保证 remove_file 必成功 → unlink 进到
    // `if let Some(ino)=self.inodes.lock()...` 持 inodes 取 sessions/readers 的反序段）。
    for u in 0..2 {
        let mp = mp.clone();
        let stop = Arc::clone(&stop);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            let name = format!("victim-{u}.bin");
            let p = mp.join(&name);
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let _ = (|| -> std::io::Result<()> {
                    fs::write(&p, b"x")?; // create
                    fs::remove_file(&p)?; // unlink → inodes→sessions→readers
                    Ok(())
                })();
            }
            let _ = tx.send(());
        }));
    }
    drop(tx);

    let ok = await_workers(&rx, handles.len(), WATCHDOG);
    // 无论成败：先杀守护解阻塞，再 join（死锁的 worker 此时才能退出）。
    stop.store(true, Ordering::Relaxed);
    mount.force_kill();
    for h in handles {
        let _ = h.join();
    }
    assert!(
        ok,
        "DEADLOCK：ShadowStore inodes/sessions AB-BA（unlink 持 inodes 取 sessions vs \
         RMW/truncate 持 sessions 经 ensure_session 取 inodes）——挂载已 wedge"
    );
    eprintln!("[OK] unlink × RMW/truncate 高并发无死锁");
}

// ---------------------------------------------------------------------------
// 测试 2：FUSE notify 重入（fsync/flush 持 per-inode 写锁调 inval_inode）vs 并发 read
// ---------------------------------------------------------------------------

#[test]
fn shadow_mount_fsync_vs_read_no_deadlock() {
    if let Some(reason) = skip_reason() {
        eprintln!("[SKIP] shadow_mount_fsync_vs_read_no_deadlock：{reason}");
        return;
    }
    let Some(mut mount) = ShadowMount::start(4096) else {
        eprintln!("[SKIP] 5s 内未观察到 shadow 挂载，疑似环境不允许 FUSE");
        return;
    };
    let mp = mount.mountpoint.clone();

    let hot = mp.join("hot.log");
    fs::write(&hot, vec![b'a'; 4096 * 3]).expect("初始化热日志");

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let deadline = Instant::now() + WORK;
    let mut handles = Vec::new();

    // 写线程：append 一行 + fsync（fsync handler 在持 per-inode 写锁时调 inval_inode）。
    {
        let hot = hot.clone();
        let stop = Arc::clone(&stop);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let _ = (|| -> std::io::Result<()> {
                    let mut f = fs::OpenOptions::new().append(true).open(&hot)?;
                    f.write_all(b"{\"role\":\"user\",\"line\":\"append-and-fsync\"}\n")?;
                    f.sync_all()?; // fsync → inval_inode（持锁）
                    Ok(())
                })();
            }
            let _ = tx.send(());
        }));
    }

    // 读线程（×2）：只读 open（KEEP_CACHE → 填充内核页缓存，使 fsync 的 inval_inode 真要摘页）+
    // 反复全量读同 inode → 与写线程的持锁 inval_inode 形成跨层反序环的另一半。
    for _ in 0..2 {
        let hot = hot.clone();
        let stop = Arc::clone(&stop);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let _ = (|| -> std::io::Result<()> {
                    let mut f = fs::File::open(&hot)?;
                    let mut buf = Vec::new();
                    f.read_to_end(&mut buf)?;
                    Ok(())
                })();
            }
            let _ = tx.send(());
        }));
    }
    drop(tx);

    let ok = await_workers(&rx, handles.len(), WATCHDOG);
    stop.store(true, Ordering::Relaxed);
    mount.force_kill();
    for h in handles {
        let _ = h.join();
    }
    assert!(
        ok,
        "DEADLOCK：fsync/flush 在持 per-inode 写锁时调 inval_inode，与并发 read 同 inode 在内核\
         页缓存锁上死锁（FUSE notify 重入）——挂载已 wedge"
    );
    eprintln!("[OK] fsync × read 同 inode 高并发无死锁");
}

// ---------------------------------------------------------------------------
// 测试 3：贴近真实 Claude 会话的混合负载（append+fsync+read+偶发 unlink/rename/truncate）
// 一次同时给两类根因施压，是对「neighbors 挂载实测 wedge」最接近的复现。
// ---------------------------------------------------------------------------

#[test]
fn shadow_mount_session_like_workload_no_deadlock() {
    if let Some(reason) = skip_reason() {
        eprintln!("[SKIP] shadow_mount_session_like_workload_no_deadlock：{reason}");
        return;
    }
    let Some(mut mount) = ShadowMount::start(4096) else {
        eprintln!("[SKIP] 5s 内未观察到 shadow 挂载，疑似环境不允许 FUSE");
        return;
    };
    let mp = mount.mountpoint.clone();

    let session = mp.join("session.jsonl");
    fs::write(&session, b"{\"start\":true}\n").expect("初始化会话日志");

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let deadline = Instant::now() + WORK;
    let mut handles = Vec::new();

    // append+fsync 主写流（模拟会话日志持续追加）。
    {
        let session = session.clone();
        let stop = Arc::clone(&stop);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let _ = (|| -> std::io::Result<()> {
                    let mut f = fs::OpenOptions::new().append(true).open(&session)?;
                    writeln!(
                        f,
                        "{{\"i\":{n},\"text\":\"a fairly long transcript line ...\"}}"
                    )?;
                    f.sync_all()?;
                    Ok(())
                })();
                n += 1;
            }
            let _ = tx.send(());
        }));
    }

    // 并发读会话日志（resume 读 / 工具读）。
    {
        let session = session.clone();
        let stop = Arc::clone(&stop);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let _ = (|| -> std::io::Result<()> {
                    let mut buf = Vec::new();
                    fs::File::open(&session)?.read_to_end(&mut buf)?;
                    Ok(())
                })();
            }
            let _ = tx.send(());
        }));
    }

    // 旁路文件流：create / write-then-rename / unlink（临时文件、原子替换、清理）。
    {
        let mp = mp.clone();
        let stop = Arc::clone(&stop);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            let mut k = 0u64;
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let _ = (|| -> std::io::Result<()> {
                    let tmp = mp.join(format!("tmp-{}.part", k % 4));
                    let dst = mp.join(format!("obj-{}.bin", k % 4));
                    fs::write(&tmp, vec![(k % 251) as u8; 1500])?;
                    fs::rename(&tmp, &dst)?; // 覆盖式 rename（失效旧 ino 的 reader/session）
                    if k.is_multiple_of(3) {
                        let _ = fs::remove_file(&dst); // unlink → inodes→sessions→readers
                    }
                    Ok(())
                })();
                k += 1;
            }
            let _ = tx.send(());
        }));
    }
    drop(tx);

    let ok = await_workers(&rx, handles.len(), WATCHDOG);
    stop.store(true, Ordering::Relaxed);
    mount.force_kill();
    for h in handles {
        let _ = h.join();
    }
    assert!(
        ok,
        "DEADLOCK：会话式混合负载（append+fsync+read+rename/unlink）下挂载 wedge——\
         两类根因（inodes/sessions AB-BA、inval_inode 重入）至少其一触发"
    );
    eprintln!("[OK] 会话式混合负载高并发无死锁");
}
