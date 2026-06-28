//! Core 写编排：随机写 / append / 截断的读改写（RMW），两布局共享（§3、§4.1）。
//!
//! 逻辑文件 = 定长逻辑块序列。一次 `write(off, data)`：
//! - 算出受影响的块区间 `[first, last]`（core::chunk::block_range）。
//! - **整块覆盖**（块被本次写完整覆盖且不越过当前 EOF 之外需保留的尾巴）→ 直接压缩新块写回，
//!   跳过读旧块（省一次解压）。
//! - **部分块**（首/末块非块对齐）→ `get_block` 解压旧块 → 在内存 patch → 重压 → 写回（RMW）。
//! - **append / 越 EOF / 空洞**：被写区间起点落在当前 EOF 之外时，中间缺块按**零填充**补齐，
//!   而非报错；末块延伸到新逻辑大小。
//! - 每块重过不可压缩启发式（codec::compress），RMW 时 verbatim flag 可翻转（§3）。
//!
//! `truncate(new_size)`：末块若被部分截断先 RMW 重压，再 `Store::truncate_blocks` 丢弃尾块。
//!
//! 压缩在本层（Core）完成，Store 只搬运不透明字节（§2）。

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::core::chunk::block_range;
use crate::core::codec::{compress_block, decompress_block, Algo, SharedDict};
use crate::store::{Store, StoredBlock};

/// 块压缩（重压）计数器：每次 `store_plain_block` 把一个逻辑块压缩落 Store 记一次。
///
/// 仅供基准量化「append 优化前后重压次数」用（§1.1）。关闭尾块缓冲的旧路径每次 append
/// 都重压尾块（计数 ≈ append 次数）；开启后仅满块/封块时压一次（计数 ≈ 满块数）。
/// 进程级累加，`Relaxed` 足够（仅做粗粒度统计，不参与同步）。基准跑前 `reset` 清零。
static BLOCK_COMPRESS_COUNT: AtomicU64 = AtomicU64::new(0);

/// 读取当前累计块压缩次数（基准埋点）。
pub fn block_compress_count() -> u64 {
    BLOCK_COMPRESS_COUNT.load(Ordering::Relaxed)
}

/// 清零块压缩计数（基准每段跑前调用，隔离 before/after）。
pub fn reset_block_compress_count() {
    BLOCK_COMPRESS_COUNT.store(0, Ordering::Relaxed);
}

/// 一次写所需的 codec 参数（算法 + zstd 等级 + 可选共享字典）。
///
/// `dict` 非空时所有块走字典压缩/解压（CDict 已含等级，`level` 仅无字典路径生效）。
/// 含 `Arc<SharedDict>` 故不再是 `Copy`；按引用 `&CodecParams` 传递（Arc clone 仅在需要持有时）。
#[derive(Debug, Clone)]
pub struct CodecParams {
    pub algo: Algo,
    pub level: i32,
    pub dict: Option<Arc<SharedDict>>,
}

/// 单次写允许的最大稀疏扩展跨度（字节）：写起点越过当前 EOF 超过此值则拒绝（ENOSPC 语义）。
///
/// 不可信的 FUSE `offset` 若取巨值（如 1TiB）会让中间空洞块被逐块物化（archive 须块连续），
/// 造成写放大 / 磁盘耗尽 / OOM（rust-review H4）。布局 S 当前不支持真稀疏文件，故对单次写的
/// 空洞跨度设上限（1GiB，远超本负载 append 步长）。真稀疏支持留作后续（seek-hole）。
pub const MAX_SPARSE_EXTENSION: u64 = 1 << 30;

/// 读取并解压第 `idx` 块的逻辑字节；缺块（越界/空洞）返回空 vec（由调用方零填充语义处理）。
pub(crate) fn load_plain_block(
    store: &dyn Store,
    ino: u64,
    idx: u64,
    params: &CodecParams,
) -> io::Result<Vec<u8>> {
    match store.get_block(ino, idx)? {
        Some(b) => decompress_block(
            &b.bytes,
            params.algo,
            b.stored_verbatim,
            params.dict.as_deref(),
        ),
        None => Ok(Vec::new()),
    }
}

/// 压缩并写回一个逻辑块（过不可压缩启发式，flag 由 codec 决定）。
pub(crate) fn store_plain_block(
    store: &dyn Store,
    ino: u64,
    idx: u64,
    plain: &[u8],
    new_size: u64,
    params: &CodecParams,
) -> io::Result<()> {
    let (bytes, verbatim) =
        compress_block(plain, params.algo, params.level, params.dict.as_deref())?;
    BLOCK_COMPRESS_COUNT.fetch_add(1, Ordering::Relaxed);
    store.put_block(
        ino,
        idx,
        StoredBlock {
            bytes,
            stored_verbatim: verbatim,
        },
        new_size,
    )?;
    maybe_build_head_cache(store, ino, idx, plain, new_size, params)?;
    Ok(())
}

/// head 缓存（发现读快路径，docs/02 §4.3）：仅当块 0 成为**满的不可变正文块**（其后还有内容，
/// `new_size > 块0逻辑长度`）且覆盖窗口（`>= HEAD_CACHE_BYTES`）时，把首 `HEAD_CACHE_BYTES` 字节
/// 单独压一份交 Store。块 0 明文此刻在手，免事后 `get_block`+解压回捞（审查 M2）。append 主负载下
/// 块 0 一旦满封即不再变，故缓存写一次永不失效；块 0 被 RMW 时本函数重建、Store 端脏会话期间
/// 读快路径自动回退（见 ShadowStore::read_head_cache）。非块 0 / 小文件（块 0 即末块）不建。
fn maybe_build_head_cache(
    store: &dyn Store,
    ino: u64,
    idx: u64,
    plain: &[u8],
    new_size: u64,
    params: &CodecParams,
) -> io::Result<()> {
    let head_bytes = crate::archive::HEAD_CACHE_BYTES;
    if idx != 0 || (plain.len() as u64) < head_bytes || new_size <= plain.len() as u64 {
        return Ok(());
    }
    let head = &plain[..head_bytes as usize];
    let (hbytes, hverbatim) =
        compress_block(head, params.algo, params.level, params.dict.as_deref())?;
    store.set_head_cache(ino, hbytes, hverbatim, head_bytes)
}

/// 在逻辑文件 `ino` 的偏移 `offset` 处写入 `data`，返回写入字节数（恒为 data.len()）。
///
/// 处理 append / 越 EOF / 空洞零填充 / 部分块 RMW / 整块覆盖。调用方须持该 inode 的写锁
/// （§4 per-inode 锁）以保证 RMW 原子。压缩参数由 `params` 给定。
pub fn write_at(
    store: &dyn Store,
    ino: u64,
    offset: u64,
    data: &[u8],
    params: &CodecParams,
) -> io::Result<usize> {
    if data.is_empty() {
        return Ok(0);
    }
    let Some((old_size, chunk_size)) = store.block_geometry(ino) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("write_at：ino={ino} 非可分块文件或不存在"),
        ));
    };
    let cs = chunk_size as u64;
    // offset 来自不可信 FUSE 回调：用 checked 加法防溢出（越界写在 release build 会 wrap
    // 成错误块号，debug build 直接 panic）。溢出即拒绝。
    let write_end = offset
        .checked_add(data.len() as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write offset+len 溢出"))?;
    let new_size = old_size.max(write_end);

    // 稀疏扩展上限：拒绝把空洞撑得过大（防写放大 / OOM，见 MAX_SPARSE_EXTENSION）。
    if offset > old_size && offset - old_size > MAX_SPARSE_EXTENSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "稀疏写空洞过大：offset={offset} 越过 EOF={old_size} 超过上限 {MAX_SPARSE_EXTENSION}"
            ),
        ));
    }

    let (first, last) = block_range(offset, data.len() as u64, cs);

    // 若写起点跨过了当前 EOF 形成空洞，且首块下方还有「不存在的块」，需把它们一并
    // 物化为零块，保持块索引连续（archive 格式要求块连续；container 多存零块影响极小）。
    // 起点取 min(first, 当前块数)：当前块数之前的块都已存在，无需补。
    let old_block_count = old_size.div_ceil(cs);
    let loop_start = first.min(old_block_count);

    for idx in loop_start..=last {
        let block_start = idx * cs;
        let block_end = block_start + cs;

        // 本块与写区间的交集（写入落在块内的部分）。空洞块此交集为空。
        let w_start = offset.max(block_start);
        let w_end = write_end.min(block_end).max(w_start);

        // 该块在新文件下应保留的逻辑长度：末块可不足 chunk_size。
        let block_logical_end = block_end.min(new_size);
        let block_logical_len = (block_logical_end - block_start) as usize;

        // 判断是否「整块覆盖」：写区间覆盖了本块在新文件下的全部逻辑字节，
        // 且不需要保留旧块尾巴（block_logical_end <= w_end）。此时跳过读旧块。
        let full_overwrite = w_start <= block_start && w_end >= block_logical_end;

        let mut plain: Vec<u8> = if full_overwrite {
            Vec::with_capacity(block_logical_len)
        } else {
            // 部分块 / 空洞块：先取旧块（含空洞→空 vec），保留未被覆盖的字节。
            load_plain_block(store, ino, idx, params)?
        };

        // 把 plain 补齐 / 截到本块逻辑长度，缺口零填充（空洞 / 越 EOF）。
        if plain.len() < block_logical_len {
            plain.resize(block_logical_len, 0);
        } else if plain.len() > block_logical_len {
            // 旧块比新逻辑长度长（理论上仅末块在文件被同次写延伸时出现，少见）：截断。
            plain.truncate(block_logical_len);
        }

        // 把本次写的数据片 patch 进去（空洞块无交集，跳过 patch，仅写零块保持连续）。
        if w_end > w_start {
            let src_off = (w_start - offset) as usize;
            let src_end = (w_end - offset) as usize;
            let dst_off = (w_start - block_start) as usize;
            let dst_end = (w_end - block_start) as usize;
            plain[dst_off..dst_end].copy_from_slice(&data[src_off..src_end]);
        }

        store_plain_block(store, ino, idx, &plain, new_size, params)?;
    }

    Ok(data.len())
}

/// 把逻辑文件 `ino` 截断（或零填充扩展）到 `new_size`。
///
/// - 缩小：末块若被部分截断 → 取旧末块解压、截到块内长度、重压写回；再丢弃其后的块。
/// - 扩展：末块零填充到 chunk_size（或新 EOF），后续块作为空洞由读路径零填充；
///   仅重写「被部分触及」的末块，不为纯空洞块写零块（保持稀疏，省空间）。
pub fn truncate(
    store: &dyn Store,
    ino: u64,
    new_size: u64,
    params: &CodecParams,
) -> io::Result<()> {
    let Some((old_size, chunk_size)) = store.block_geometry(ino) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("truncate：ino={ino} 非可分块文件或不存在"),
        ));
    };
    if new_size == old_size {
        return Ok(());
    }
    let cs = chunk_size as u64;

    if new_size < old_size {
        // 缩小。keep_from = 完整保留的块数 = ceil(new_size / cs) 之前的块均保留；
        // 落在 new_size 边界的块若非块对齐，需 RMW 截断其尾部。
        let last_kept_block = if new_size == 0 {
            0
        } else {
            (new_size - 1) / cs
        };
        let boundary_in_block = new_size % cs;
        if new_size != 0 && boundary_in_block != 0 {
            // 末块部分保留：解压、截断、重压写回（new_size 由本块写带入）。
            let mut plain = load_plain_block(store, ino, last_kept_block, params)?;
            let want = boundary_in_block as usize;
            if plain.len() > want {
                plain.truncate(want);
            } else if plain.len() < want {
                plain.resize(want, 0);
            }
            store_plain_block(store, ino, last_kept_block, &plain, new_size, params)?;
            // 丢弃 last_kept_block 之后的块。
            store.truncate_blocks(ino, last_kept_block + 1, new_size)
        } else {
            // new_size 块对齐（或为 0）：直接丢弃 keep_from 及之后的块。
            let keep_from = new_size.div_ceil(cs);
            store.truncate_blocks(ino, keep_from, new_size)
        }
    } else {
        // 扩展（越 EOF 截断 = 创建空洞）。只触碰原末块所在块：把它零填充到块边界，
        // 之后纯空洞块不落零块（读路径零填充）。仅当扩展跨过原末块边界时才需改原末块。
        let old_last_block = if old_size == 0 {
            0
        } else {
            (old_size - 1) / cs
        };
        let old_last_block_end = (old_last_block + 1) * cs;
        if old_size > 0 && old_size < old_last_block_end {
            // 原末块未满：把它零填充到 min(块边界, new_size)。
            let target_in_block = old_last_block_end.min(new_size);
            let mut plain = load_plain_block(store, ino, old_last_block, params)?;
            let want = (target_in_block - old_last_block * cs) as usize;
            if plain.len() < want {
                plain.resize(want, 0);
            }
            store_plain_block(store, ino, old_last_block, &plain, new_size, params)?;
        } else {
            // 原本块对齐（或空文件）：无需触碰任何旧块，只需把逻辑大小改大。
            // 用一次 0 长度的「大小提升」：truncate_blocks(keep_all, new_size) 仅改 size。
            let keep_all = old_size.div_ceil(cs);
            store.truncate_blocks(ino, keep_all, new_size)?;
        }
        // 若上面走了 store_plain_block 分支，new_size 已随块带入；这里确保 size 被提升到 new_size。
        // store_plain_block 传的 new_size 即目标，故无需再调。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::codec::decompress;
    use crate::store::tests_support::MemStore;

    fn params() -> CodecParams {
        CodecParams {
            algo: Algo::Zstd,
            level: 3,
            dict: None,
        }
    }

    /// 读回整个文件的逻辑字节（解压全部块拼接，缺块零填充到 size）。
    fn read_all(store: &dyn Store, ino: u64) -> Vec<u8> {
        let (size, cs) = store.block_geometry(ino).unwrap();
        let mut out = vec![0u8; size as usize];
        let cs = cs as u64;
        let nblocks = size.div_ceil(cs);
        for idx in 0..nblocks {
            if let Some(b) = store.get_block(ino, idx).unwrap() {
                let plain = decompress(&b.bytes, Algo::Zstd, b.stored_verbatim).unwrap();
                let start = (idx * cs) as usize;
                let end = (start + plain.len()).min(out.len());
                out[start..end].copy_from_slice(&plain[..end - start]);
            }
        }
        out
    }

    #[test]
    fn 顺序写单块_round_trip() {
        let store = MemStore::new(16);
        let ino = store.new_file();
        write_at(&store, ino, 0, b"hello", &params()).unwrap();
        assert_eq!(read_all(&store, ino), b"hello");
        assert_eq!(store.block_geometry(ino).unwrap().0, 5);
    }

    #[test]
    fn append_延伸跨块() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        write_at(&store, ino, 0, b"abcdefgh", &params()).unwrap(); // 块0满
        write_at(&store, ino, 8, b"ijkl", &params()).unwrap(); // append 块1
        assert_eq!(read_all(&store, ino), b"abcdefghijkl");
        assert_eq!(store.block_geometry(ino).unwrap().0, 12);
    }

    #[test]
    fn 中间块_rmw_保留两侧字节() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        write_at(&store, ino, 0, b"AAAAAAAABBBBBBBB", &params()).unwrap();
        // 覆盖块0中间 [2,5)。
        write_at(&store, ino, 2, b"xyz", &params()).unwrap();
        assert_eq!(read_all(&store, ino), b"AAxyzAAABBBBBBBB");
    }

    #[test]
    fn 越_eof_写产生空洞零填充() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        write_at(&store, ino, 0, b"ab", &params()).unwrap();
        // 在 offset 20 写（跨过空洞），中间 [2,20) 应为零。
        write_at(&store, ino, 20, b"Z", &params()).unwrap();
        let got = read_all(&store, ino);
        assert_eq!(got.len(), 21);
        assert_eq!(&got[0..2], b"ab");
        assert!(got[2..20].iter().all(|&b| b == 0), "空洞区应零填充");
        assert_eq!(got[20], b'Z');
    }

    #[test]
    fn truncate_缩小到块中间() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        write_at(&store, ino, 0, b"0123456789ABCDEF", &params()).unwrap(); // 16 字节 / 2 块
        truncate(&store, ino, 5, &params()).unwrap();
        assert_eq!(read_all(&store, ino), b"01234");
        assert_eq!(store.block_geometry(ino).unwrap().0, 5);
    }

    #[test]
    fn truncate_扩展产生空洞() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        write_at(&store, ino, 0, b"abc", &params()).unwrap();
        truncate(&store, ino, 10, &params()).unwrap();
        let got = read_all(&store, ino);
        assert_eq!(got.len(), 10);
        assert_eq!(&got[0..3], b"abc");
        assert!(got[3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn 整块覆盖跳过读仍正确() {
        let store = MemStore::new(4);
        let ino = store.new_file();
        write_at(&store, ino, 0, b"WXYZ", &params()).unwrap();
        // 整块覆盖块0。
        write_at(&store, ino, 0, b"1234", &params()).unwrap();
        assert_eq!(read_all(&store, ino), b"1234");
    }

    #[test]
    fn 稀疏空洞过大被拒() {
        let store = MemStore::new(64);
        let ino = store.new_file();
        // 在远超上限的 offset 写，应拒绝（防写放大 / OOM），而非物化海量零块。
        let huge_offset = MAX_SPARSE_EXTENSION + 1;
        let err = write_at(&store, ino, huge_offset, b"x", &params()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn 写偏移溢出被拒() {
        let store = MemStore::new(64);
        let ino = store.new_file();
        let err = write_at(&store, ino, u64::MAX, b"xx", &params()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
