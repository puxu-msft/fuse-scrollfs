//! 落盘原语：fsync 文件 / 目录 / 目录链 + 原子写。
use std::io;

use super::*;

/// fsync 单个文件（确保 stash 内容落盘，崩溃不丢快照）。
pub(crate) fn fsync_path(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// fsync 目录项（确保新建条目在父目录中可见落盘）。
pub(crate) fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// fsync 从 stash 文件父目录 `from` 逐级向上到 `stash_root`（含）的每层目录。
///
/// 补齐 `create_dir_all` 新建的 `<rel>`/`underlay`/`<ts>` 各级 dirent，使 stash 拷贝的整条目录链
/// 崩溃可恢复、不被孤儿化（本项目对崩溃持久化敏感）。`from` 恒为 `stash_root` 的后代，向上必达上界。
pub(crate) fn fsync_dir_chain(from: &Path, stash_root: &Path) -> io::Result<()> {
    let mut dir = from;
    loop {
        fsync_dir(dir)?;
        if dir == stash_root {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    Ok(())
}

/// 原子写：`bytes` → `<dst>.tmp`（`sync_all`）→ `rename(tmp, dst)` → fsync 父目录。
///
/// 崩溃安全的「全有或全无」落盘（复用 `lifecycle::fsync_parent` 思路）：先把内容写进同目录临时
/// 文件并 fsync 其内容，再原子 rename 覆盖 `dst`，最后 fsync 父目录持久化这次 rename 的 dirent。
/// 任一步崩溃时 `dst` 要么是旧内容要么是完整新内容，绝不出现半截写入。临时文件名恒为 `<dst>.tmp`
/// （同目录 → rename 同文件系统内原子），与 reconcile 删除许可链的 readback 基准配套。
pub fn atomic_write(dst: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp_os = dst.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);

    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dst)?;
    if let Some(parent) = dst.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_then_readback() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.jsonl");
        atomic_write(&p, b"line1\nline2\n").unwrap();
        assert!(readback_eq(&p, b"line1\nline2\n").unwrap());
        assert!(
            !p.with_extension("jsonl.tmp").exists(),
            "tmp 应已 rename 消失"
        );
    }

}
