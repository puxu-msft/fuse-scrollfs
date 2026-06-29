//! `BlockIo`：archive 写/提交/打开读链所依赖的**唯一定位 IO 差异面**（故障注入接缝，docs/05 §3）。
//!
//! 今天 `ArchiveUpdater` 与共享只读自由函数（`load_active`/`read_sb_slot`/…）直接打在具体
//! `std::fs::File` 上，没有抽象接缝，无法在「fsync 已确认却被掉电丢弃/重排」「写中途 EIO」这类
//! 真实进程杀不到的层面注入故障。本 trait 把这些定位读写收口成一个 `&self` 接口：
//! 生产经 `impl BlockIo for File`（转调 `FileExt`，零行为变化），测试经 `FaultIo`（确定性崩溃
//! 模拟器，docs/05 §4）。
//!
//! 设计要点（docs/05 §3 评审定稿）：
//! - 方法取 `&self`：贴合 `FileExt::{write_all_at, read_exact_at, sync_all}`（均 `&self`、pwrite/pread
//!   不移动游标），免改 `ArchiveUpdater::sync(&self)` 签名；内部可变（`FaultIo` 计数器/计划）用
//!   `Mutex`/原子封装，故 `&self` 不碍注入。
//! - `Send + Sync`：`File` 已满足，零成本；为日后 `ArchiveReader` 泛型化（`Arc` 跨线程并发读）留路。
//! - **`impl BlockIo for File`（取代「包 FileIo」）**：让 `File` 直接实现 `BlockIo`，使 `ArchiveReader`
//!   的 `file: File` 字段**原地**满足 `&impl BlockIo`，4 处 `read_exact_at(&self.file, …)` 与共享自由
//!   函数零所有权改动即可复用。

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

/// 定位读写 + durability barrier 的抽象接缝。`write_at`/`read_at` 用**绝对偏移**（pwrite/pread
/// 语义，不移动文件游标），`sync` 是唯一 durability barrier。
///
/// `len`/`set_len` 为压实预留（append-only 写路径当前不用 `set_len`）。
// `len` 返回 `io::Result<u64>`（非裸 usize），`is_empty` 语义不适用于「可能 IO 失败的字节长度
// 查询」，故显式豁免 clippy::len_without_is_empty。
#[allow(clippy::len_without_is_empty)]
pub trait BlockIo: Send + Sync {
    /// 把 `buf` 写到绝对偏移 `off`（pwrite，不移动游标）。
    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<()>;
    /// 从绝对偏移 `off` 读满 `buf`（pread，不移动游标）。
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    /// durability barrier：把已写字节落到稳定存储（唯一 barrier）。
    fn sync(&self) -> io::Result<()>;
    /// 当前字节长度。
    fn len(&self) -> io::Result<u64>;
    /// 截断/扩展到 `len` 字节（append-only 写路径暂不用，留待压实）。
    fn set_len(&self, len: u64) -> io::Result<()>;
}

/// 生产实现：`File` 直接实现 `BlockIo`，逐方法转调 `FileExt`/inherent，与改造前逐字节等价。
impl BlockIo for File {
    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.write_all_at(buf, off)
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.read_exact_at(buf, off)
    }
    fn sync(&self) -> io::Result<()> {
        self.sync_all()
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        // inherent File::set_len（UFCS 避免与本 trait 同名方法递归）。
        File::set_len(self, len)
    }
}

// ===========================================================================
// FaultIo：Tier 1 确定性崩溃模拟器（docs/05 §4）
// ===========================================================================
//
// 门控 `#[cfg(any(test, feature = "fault-injection"))]`：`#[cfg(test)]` 对 tests/ 独立 crate
// 不可见，集成测试唯一途径是 feature（docs/05 §6）。生产构建不编译本段，无死代码。
#[cfg(any(test, feature = "fault-injection"))]
mod fault {
    use super::BlockIo;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// 一次定位写：自上次成功 `sync` 以来的脏写（内核页缓存层，尚未落 `durable`）。
    #[derive(Clone)]
    struct PendingWrite {
        off: u64,
        bytes: Vec<u8>,
    }

    struct FaultState {
        /// 已 sync、能扛崩溃的盘面字节。
        durable: Vec<u8>,
        /// 自上次 sync 起的脏写覆盖层（模拟内核页缓存），按写序排列。
        dirty: Vec<PendingWrite>,
        /// 注入：下一次写入 `[lo, hi)` 区间的 `write_at` 返 EIO（fire-once，docs/05 §4 按语义
        /// offset 区间调度）。
        fail_write: Option<(u64, u64)>,
        /// 注入：第 `fail_sync_at` 次 `sync`（1-based，累计计数）返 EIO（fire-once）。
        fail_sync_at: Option<usize>,
        /// 注入：下一次写入 `[lo, hi)` 区间的 `write_at` **撕裂**——只把前 `prefix` 字节落入页缓存
        /// （`prefix` 由调用方量化到 512B 扇区，免造真实块设备不产生的字节边界）。撕裂写**静默成功**
        /// （部分落盘，进程以为写成功），半截只在崩溃镜像暴露（fire-once，docs/05 §4）。
        tear_write: Option<(u64, u64, u64)>,
        /// 累计 sync 调用次数（含失败次）。
        sync_calls: usize,
        /// 注入：接下来 `soft_syncs` 次 `sync` **软化**——返 Ok 但**不合并** dirty（模拟 fsync 撒谎/
        /// 被重排：写仍在页缓存、可乱序回写）。用于 2.5「barrier 软化 × 乱序子集」构造「SB 进 durable
        /// 而其依赖 index 未进」的危险镜像（docs/05 §4）。
        soft_syncs: usize,
        /// durable 状态阶梯：构造时的初始 durable + 每次成功 `sync` 后的 durable 快照。供穷举崩溃点
        /// 测试取「每个 durable 转移点」的悲观崩溃镜像（durable 仅在 sync 改变，故阶梯即全部不同
        /// 悲观镜像；写边界崩溃 = 最近一格，已覆盖，docs/05 §4 / 任务 2.4）。
        history: Vec<Vec<u8>>,
    }

    /// 确定性崩溃模拟器（docs/05 §4）：`durable` + `dirty` 页缓存模型。`sync` 合并 dirty→durable
    /// （唯一 durability barrier）；`crash_with_mask` 产出「durable + dirty 任意子集」的崩溃后磁盘
    /// 镜像——**支持脏页乱序子集持久化**（评审 CRITICAL ②），否则永远碰不到「SB 先于其数据落盘」。
    ///
    /// `#[derive(Clone)]` 经内部 `Arc<Mutex<_>>` 共享状态：注入时把 `clone()` 交给
    /// `ArchiveUpdater::from_io`，测试侧持原句柄在崩溃点取镜像/查脏写。
    #[derive(Clone)]
    pub struct FaultIo {
        state: Arc<Mutex<FaultState>>,
    }

    /// 把一次写应用到镜像缓冲（绝对偏移，越界零填充扩展，仿 `File` 语义）。
    fn apply(buf: &mut Vec<u8>, off: u64, bytes: &[u8]) {
        let end = off as usize + bytes.len();
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[off as usize..end].copy_from_slice(bytes);
    }

    impl FaultIo {
        /// 以初始盘面字节构造（durable = initial，dirty 空）。
        pub fn from_bytes(initial: Vec<u8>) -> Self {
            let history = vec![initial.clone()];
            Self {
                state: Arc::new(Mutex::new(FaultState {
                    durable: initial,
                    dirty: Vec::new(),
                    fail_write: None,
                    fail_sync_at: None,
                    tear_write: None,
                    sync_calls: 0,
                    soft_syncs: 0,
                    history,
                })),
            }
        }

        /// 武装：下一次写入 `[lo, hi)` 区间的 `write_at` 返 EIO（fire-once）。按 SB_A/SB_B 槽区间、
        /// 数据区（index/journal）区间调度——offset 区间是格式契约、稳定（docs/05 §4 防脆弱）。
        pub fn fail_write_in(&self, lo: u64, hi: u64) {
            self.state.lock().unwrap().fail_write = Some((lo, hi));
        }

        /// 武装：从此刻起第 `nth_from_now` 次 `sync` 返 EIO（fire-once）。与历史 sync 次数无关，
        /// 便于「武装后即将到来的 commit 的 barrier 1/2」精确打点（nth=1 → barrier 1，nth=2 → barrier 2）。
        pub fn fail_sync_in(&self, nth_from_now: usize) {
            let mut st = self.state.lock().unwrap();
            st.fail_sync_at = Some(st.sync_calls + nth_from_now);
        }

        /// 武装：下一次写入 `[lo, hi)` 区间的 `write_at` 撕裂——只落前 `prefix` 字节（fire-once）。
        /// `prefix` 应由调用方量化到 512B 扇区（如 512、1024）。撕裂写静默成功，半截只在崩溃镜像暴露。
        pub fn tear_write_in(&self, lo: u64, hi: u64, prefix: u64) {
            self.state.lock().unwrap().tear_write = Some((lo, hi, prefix));
        }

        /// 武装：接下来 `count` 次 `sync` 软化——返 Ok 但不合并 dirty（写留在可乱序回写的脏层）。
        /// 用于「barrier 软化 × 乱序子集」交叉：使 commit 的 index/SB 写在崩溃枚举时仍可被任意子集
        /// 持久化，构造「SB 进 durable 而 index 未进」的危险镜像（docs/05 §4 / 任务 2.5）。
        pub fn soften_syncs(&self, count: usize) {
            self.state.lock().unwrap().soft_syncs = count;
        }

        /// 自上次成功 sync 起的脏写条数（= 崩溃点/重排子集枚举窗口大小）。
        pub fn dirty_count(&self) -> usize {
            self.state.lock().unwrap().dirty.len()
        }

        /// 崩溃后磁盘镜像 = `durable` + 按 `mask` 选中的 dirty 子集（bit i 置位 → 应用 dirty[i]，
        /// 按写序）。只尊重 sync 屏障（已合并进 durable 的必在）；dirty 子集模拟脏页回写的任意子集
        /// 持久化（重排）。
        pub fn crash_with_mask(&self, mask: u64) -> Vec<u8> {
            let st = self.state.lock().unwrap();
            let mut img = st.durable.clone();
            for (i, w) in st.dirty.iter().enumerate() {
                if mask & (1u64 << i) != 0 {
                    apply(&mut img, w.off, &w.bytes);
                }
            }
            img
        }

        /// durable 状态阶梯：初始 durable + 每次成功 sync 后的 durable 快照（穷举悲观崩溃点）。
        pub fn history(&self) -> Vec<Vec<u8>> {
            self.state.lock().unwrap().history.clone()
        }
    }

    impl BlockIo for FaultIo {
        fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<()> {
            let mut st = self.state.lock().unwrap();
            // 撕裂注入优先：只落前 prefix 字节（静默成功，半截只在崩溃镜像暴露）。
            if let Some((lo, hi, prefix)) = st.tear_write {
                let end = off + buf.len() as u64;
                if off < hi && lo < end {
                    st.tear_write = None;
                    let k = (prefix as usize).min(buf.len());
                    if k > 0 {
                        st.dirty.push(PendingWrite {
                            off,
                            bytes: buf[..k].to_vec(),
                        });
                    }
                    return Ok(());
                }
            }
            if let Some((lo, hi)) = st.fail_write {
                let end = off + buf.len() as u64;
                if off < hi && lo < end {
                    // 区间相交 → 注入 EIO（fire-once）。写不落 dirty（仿真实 EIO：字节未写入）。
                    st.fail_write = None;
                    return Err(io::Error::from_raw_os_error(5)); // 5 = EIO（Linux）
                }
            }
            st.dirty.push(PendingWrite {
                off,
                bytes: buf.to_vec(),
            });
            Ok(())
        }
        fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
            let st = self.state.lock().unwrap();
            // 当前可见视图 = durable + 全部 dirty（仿真实 File 看得见自己未 sync 的写）。
            let mut view = st.durable.clone();
            for w in &st.dirty {
                apply(&mut view, w.off, &w.bytes);
            }
            let end = off as usize + buf.len();
            if end > view.len() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read 越界"));
            }
            buf.copy_from_slice(&view[off as usize..end]);
            Ok(())
        }
        fn sync(&self) -> io::Result<()> {
            let mut st = self.state.lock().unwrap();
            st.sync_calls += 1;
            if st.soft_syncs > 0 {
                // 软 sync：返 Ok 但不合并 dirty（fsync 撒谎/重排：写仍在页缓存，可乱序回写）。
                st.soft_syncs -= 1;
                return Ok(());
            }
            if st.fail_sync_at == Some(st.sync_calls) {
                // 注入 EIO（fire-once）：dirty **不合并** → 这些写非 durable（barrier 未完成）。
                st.fail_sync_at = None;
                return Err(io::Error::from_raw_os_error(5)); // 5 = EIO（Linux）
            }
            let dirty = std::mem::take(&mut st.dirty);
            for w in dirty {
                apply(&mut st.durable, w.off, &w.bytes);
            }
            let snap = st.durable.clone();
            st.history.push(snap); // 记录每次成功 sync 后的 durable（穷举崩溃点阶梯）。
            Ok(())
        }
        fn len(&self) -> io::Result<u64> {
            let st = self.state.lock().unwrap();
            let mut n = st.durable.len();
            for w in &st.dirty {
                n = n.max(w.off as usize + w.bytes.len());
            }
            Ok(n as u64)
        }
        fn set_len(&self, len: u64) -> io::Result<()> {
            // append-only 写路径从不调用；最简实现：truncate/extend durable（崩溃模型不依赖）。
            self.state.lock().unwrap().durable.resize(len as usize, 0);
            Ok(())
        }
    }
}

#[cfg(any(test, feature = "fault-injection"))]
pub use fault::FaultIo;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn file_impl_write_sync_read_len_setlen_round_trip() {
        // 预置 5 字节内容，使 set_len 截断可观测。
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"xxxxx").unwrap();
        tmp.flush().unwrap();
        let f = tmp.reopen().unwrap();

        // 用 UFCS 显式调 BlockIo（避免与 FileExt 同名方法歧义）。
        BlockIo::write_at(&f, 0, b"abc").unwrap();
        BlockIo::sync(&f).unwrap();
        let mut buf = [0u8; 3];
        BlockIo::read_at(&f, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"abc", "write_at→sync→read_at 应回读写入字节");

        assert_eq!(BlockIo::len(&f).unwrap(), 5, "len 应为文件字节数");
        BlockIo::set_len(&f, 1).unwrap();
        assert_eq!(BlockIo::len(&f).unwrap(), 1, "set_len 后 len 应缩短");
    }

    // ----- FaultIo 页缓存崩溃模型（docs/05 §4 / 任务 2.1）-----

    #[test]
    fn faultio_未sync的写_crash不含_sync后含() {
        let io = FaultIo::from_bytes(vec![0u8; 8]);
        BlockIo::write_at(&io, 0, b"AB").unwrap();
        // 未 sync：最悲观镜像（mask=0，不应用任何 dirty）不含该写。
        let before = io.crash_with_mask(0);
        assert_eq!(
            &before[..2],
            &[0, 0],
            "未 sync 的写不应进入 crash(mask=0) 镜像"
        );
        // sync 后：写已合并进 durable，mask=0 也含（dirty 已清空）。
        BlockIo::sync(&io).unwrap();
        let after = io.crash_with_mask(0);
        assert_eq!(&after[..2], b"AB", "sync 后写应 durable，crash 必含");
    }

    #[test]
    fn faultio_乱序子集_能产出含a不含b() {
        // 钉死模型未退化成「全丢 dirty」：两条未 sync 的写 A(off0)、B(off4)，crash 能产出含 A 不含 B
        // （评审 CRITICAL ②：否则 2.5 的乱序剪枝可能悄悄废掉整条链）。
        let io = FaultIo::from_bytes(vec![0u8; 8]);
        BlockIo::write_at(&io, 0, b"AA").unwrap(); // dirty[0] = A
        BlockIo::write_at(&io, 4, b"BB").unwrap(); // dirty[1] = B
        assert_eq!(io.dirty_count(), 2);
        let img = io.crash_with_mask(0b01); // 只应用 A，不应用 B
        assert_eq!(&img[0..2], b"AA", "应含 A");
        assert_eq!(&img[4..6], &[0, 0], "不应含 B（乱序子集能力存在）");
    }
}
