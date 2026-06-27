//! 布局 S 的**只读** FUSE 前端（P1：只读 + 顺序读）。
//!
//! 把 `fuser::Filesystem` 的读侧回调映射到 `Store`（ShadowStore）+ `core::codec`：
//! - lookup / getattr / readdir：直接转 Store 的元数据查询（底层镜像目录真实 stat）。
//! - read：算出覆盖的逻辑块范围（core::chunk）→ 逐块 `get_block` 取压缩字节 →
//!   `codec::decompress` 解压（**压缩在 Core**，§2）→ 拼接切片返回。顺序读跨 chunk
//!   边界由块循环天然处理。
//! - open/release：只读语义，仅校验目标存在，不持后端 fd（archive 每次 get_block 重开；
//!   P1 求正确，缓存留作后续优化）。
//! - 所有写操作（write/create/mkdir/unlink/setattr/...）返回 **EROFS**（§7：P1 只读）。
//!
//! 设计见 docs/01-zipfs-design.md §4、§7、§12 P1。

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::Duration;

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, OpenFlags, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use log::debug;

use crate::core::chunk::block_range;
use crate::core::codec::{decompress, Algo};
use crate::store::{Attr, Store};

/// attr / entry 缓存 TTL。与 passthrough 取同值（§4.1：BV/BS 间固定同值）。
const TTL: Duration = Duration::from_secs(1);

/// 只读影子树前端。持有一个 `Store`（按 ino 提供元数据 + 压缩块）与压缩算法。
pub struct ShadowRoFs {
    store: Arc<dyn Store>,
    /// 后端 archive 块采用的压缩算法（fixture 用什么这里就用什么）。P1 固定 zstd。
    algo: Algo,
}

impl ShadowRoFs {
    pub fn new(store: Arc<dyn Store>, algo: Algo) -> Self {
        Self { store, algo }
    }

    /// 把 Store 的 `Attr` 转成 FUSE `FileAttr`。时间戳走 getattr 时机的底层 stat 已在
    /// Store 内消化为 perm/uid/gid；P1 时间戳给 epoch 占位（只读读路径不依赖精确 mtime，
    /// 后续可在 Attr 里补 mtime 字段透传）。
    fn to_file_attr(&self, a: &Attr) -> FileAttr {
        let blksize = 4096u32;
        FileAttr {
            ino: INodeNo(a.ino),
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: std::time::SystemTime::UNIX_EPOCH,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            ctime: std::time::SystemTime::UNIX_EPOCH,
            crtime: std::time::SystemTime::UNIX_EPOCH,
            kind: a.kind,
            perm: a.perm,
            nlink: 1,
            uid: a.uid,
            gid: a.gid,
            rdev: 0,
            flags: 0,
            blksize,
        }
    }

    /// 顺序/随机读核心：组装 `[offset, offset+size)` 区间的逻辑字节。
    ///
    /// 跨 chunk 边界由 `block_range` + 块循环处理；缺块（越 EOF）按零长处理而非错误。
    fn read_range(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let Some((uncompressed_size, chunk_size)) = self.store.block_geometry(ino) else {
            return Err(Errno::ENOENT);
        };
        if offset >= uncompressed_size || size == 0 {
            return Ok(Vec::new());
        }
        // 实际可读长度被文件逻辑大小截断。
        let end = (offset + size as u64).min(uncompressed_size);
        let want = (end - offset) as usize;

        let cs = chunk_size as u64;
        let (first, last) = block_range(offset, (end - offset).max(1), cs);

        let mut out = Vec::with_capacity(want);
        for idx in first..=last {
            // 该块覆盖的逻辑区间。
            let block_start = idx * cs;
            let block_end = block_start + cs;
            // 与请求区间求交。
            let copy_start = offset.max(block_start);
            let copy_end = end.min(block_end);
            if copy_start >= copy_end {
                continue;
            }

            let stored = match self.store.get_block(ino, idx) {
                Some(b) => b,
                None => {
                    // 块缺失（空洞 / 越界）：按零填充该区间（§4.1）。
                    out.resize(out.len() + (copy_end - copy_start) as usize, 0);
                    continue;
                }
            };
            let plain = decompress(&stored.bytes, self.algo, stored.stored_verbatim)
                .map_err(|e| io_to_errno(&e))?;

            // 块内相对偏移切片。
            let in_block_start = (copy_start - block_start) as usize;
            let in_block_end = ((copy_end - block_start) as usize).min(plain.len());
            if in_block_start < in_block_end {
                out.extend_from_slice(&plain[in_block_start..in_block_end]);
            }
            // 若该块解压后比逻辑预期短（理论上不应，除非末块），用零补齐到目标长度。
            let produced = in_block_end.saturating_sub(in_block_start);
            let expected = (copy_end - copy_start) as usize;
            if produced < expected {
                out.resize(out.len() + (expected - produced), 0);
            }
        }
        Ok(out)
    }
}

/// io::Error → Errno，无 raw_os_error 时回退 EIO。
pub fn io_to_errno(e: &std::io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

impl Filesystem for ShadowRoFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.store.lookup(parent.0, name) {
            Some(a) => reply.entry(&TTL, &self.to_file_attr(&a), Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.store.getattr_ino(ino.0) {
            Some(a) => reply.attr(&TTL, &self.to_file_attr(&a)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        // 只读校验：目标存在即可。不持后端 fd（P1 每次 get_block 重开 archive）。
        if self.store.getattr_ino(ino.0).is_some() {
            // direct_io：与 passthrough 一致（§4.1），offset/size 精确，便于跨块读校验。
            reply.opened(FileHandle(0), fuser::FopenFlags::FOPEN_DIRECT_IO);
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        match self.read_range(ino.0, offset, size) {
            Ok(buf) => reply.data(&buf),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        // 只读：无脏数据可刷，直接成功（避免 fuser 打 Not Implemented 警告）。
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        // . 与 .. 在前，随后是 Store 列出的子项。offset 是「下一个条目下标」。
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (ino.0, FileType::Directory, "..".to_string()),
        ];
        for de in self.store.readdir(ino.0) {
            entries.push((de.ino, de.kind, de.name));
        }
        for (i, (eino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(eino), (i + 1) as u64, kind, name.as_str()) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        // 只读：给一组保守只读统计（具体容量数字对读路径基准非关键，P5 再精确化）。
        debug!("statfs（只读 shadow）");
        reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
    }

    // ----- 写路径：只读挂载一律 EROFS（§7 P1 只读） -----

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
    }
}
