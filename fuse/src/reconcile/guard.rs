//! 挂载前 underlay 守卫：真正挂载前的最后一道，非空 fall-through 即拒（评审 C1）。
use std::ffi::OsStr;
use std::io;
use std::path::Path;

/// 无害隐藏项白名单：FUSE 删除占位（`.fuse_hidden*`）、macOS 目录元数据（`.DS_Store`）、
/// 编辑器交换/备份文件（`.*.swp` / `.*.swx` / `.*~`）。命中即视为「非停用期回落写」放行。
pub fn is_harmless(name: &OsStr) -> bool {
    let n = name.to_string_lossy();
    n.starts_with(".fuse_hidden")
        || n == ".DS_Store"
        || (n.starts_with('.') && (n.ends_with(".swp") || n.ends_with(".swx") || n.ends_with('~')))
}

/// underlay 是否含**停用期回落写**（fall-through 语义条目）：跳过 `is_harmless` 白名单，
/// 只要余下任一条目即返回 `true`。挂载点不存在视为空（`Ok(false)`）。
pub fn underlay_has_fallthrough(mp: &Path) -> io::Result<bool> {
    let rd = match std::fs::read_dir(mp) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    for dent in rd {
        let dent = dent?;
        if !is_harmless(&dent.file_name()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 挂载前守卫：underlay 含非白名单 fall-through 条目即拒挂载（防静默盖住停用期回落写）。
/// 错误信息指向 `enable reconcile` 重合并流程（测试依赖 "reconcile" 关键字）。
pub fn ensure_underlay_empty(mp: &Path) -> io::Result<()> {
    if underlay_has_fallthrough(mp)? {
        return Err(io::Error::other(format!(
            "{} 挂载点 underlay 含停用期回落写，拒绝挂载（防静默盖住）；先 `zipfs enable reconcile` 重合并",
            mp.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_dir_ok() {
        let d = tempfile::tempdir().unwrap();
        assert!(ensure_underlay_empty(d.path()).is_ok());
    }
    #[test]
    fn harmless_hidden_ignored() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".fuse_hidden0001"), b"").unwrap();
        std::fs::write(d.path().join(".DS_Store"), b"").unwrap();
        assert!(ensure_underlay_empty(d.path()).is_ok(), "无害隐藏项应放行");
    }
    #[test]
    fn fallthrough_jsonl_blocks() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("s.jsonl"), b"{}").unwrap();
        let e = ensure_underlay_empty(d.path()).unwrap_err();
        assert!(e.to_string().contains("reconcile"), "错误应指向 reconcile");
    }
}
