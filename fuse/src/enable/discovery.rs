//! enable 的探测层（IO）：扫描 projects、判挂载、判活跃、读写 backing 提交标记 sidecar。
//!
//! 纯解析逻辑（mountinfo 行匹配、sidecar 解析）抽成无 IO 函数以便单测；真正读 `/proc`、`stat`
//! 挂载点的部分靠集成/手测覆盖。

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::enable::model::{
    classify, Activity, ApplyOptions, Backend, Paths, ProjectStatus, ACTIVITY_MTIME_SECS,
};

/// 单个项目的探测快照（list/TUI 一行）。
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub name: String,
    pub status: ProjectStatus,
    pub meta: Option<Meta>,
}

impl ProjectInfo {
    /// 已记录的后端（无 meta 时回退 shadow）。
    pub fn backend(&self) -> Backend {
        self.meta
            .as_ref()
            .map(|m| m.backend)
            .unwrap_or(Backend::Shadow)
    }
}

/// 提交标记 sidecar（`back/<name>.zipfs.meta`）的解析结果。`committed` 为 true 才算灌入完成、可挂载
/// （评审 C2）。后端无关位置，记录 `backend` 供探测时反推 backing 形态（dir / redb 文件）。
#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub backend: Backend,
    pub chunk_size: u32,
    pub level: i32,
    pub bytes_src: u64,
    pub bytes_archive: u64,
    pub applied_at: u64,
    pub committed: bool,
    /// 持久化的挂载选项（remount 复用，与 ApplyOptions 一一对应）。
    pub dict: Option<String>,
    pub threads: usize,
    pub writeback: bool,
    pub max_write: u32,
    pub no_tail_buffer: bool,
    pub allow_other: bool,
    pub auto_unmount: bool,
    pub metrics_file: Option<String>,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            backend: Backend::Shadow,
            chunk_size: 0,
            level: 0,
            bytes_src: 0,
            bytes_archive: 0,
            applied_at: 0,
            committed: false,
            dict: None,
            threads: 0,
            writeback: false,
            max_write: 0,
            no_tail_buffer: false,
            allow_other: false,
            auto_unmount: false,
            metrics_file: None,
        }
    }
}

impl Meta {
    /// 压缩比（逻辑/物理）。
    pub fn ratio(&self) -> f64 {
        if self.bytes_archive == 0 {
            0.0
        } else {
            self.bytes_src as f64 / self.bytes_archive as f64
        }
    }

    /// apply 后的提交选项（remount 原样复用全部挂载参数）。
    pub fn options(&self) -> ApplyOptions {
        ApplyOptions {
            backend: self.backend,
            chunk_size: self.chunk_size,
            level: self.level,
            dict: self.dict.as_ref().map(std::path::PathBuf::from),
            threads: self.threads,
            writeback: self.writeback,
            max_write: self.max_write,
            no_tail_buffer: self.no_tail_buffer,
            allow_other: self.allow_other,
            auto_unmount: self.auto_unmount,
            metrics_file: self.metrics_file.as_ref().map(std::path::PathBuf::from),
        }
    }

    /// 由 apply 选项 + 灌入统计构造提交标记（committed=true）。
    pub fn from_apply(
        opts: &ApplyOptions,
        bytes_src: u64,
        bytes_archive: u64,
        applied_at: u64,
    ) -> Self {
        Self {
            backend: opts.backend,
            chunk_size: opts.chunk_size,
            level: opts.level,
            bytes_src,
            bytes_archive,
            applied_at,
            committed: true,
            dict: opts.dict.as_ref().map(|p| p.to_string_lossy().into_owned()),
            threads: opts.threads,
            writeback: opts.writeback,
            max_write: opts.max_write,
            no_tail_buffer: opts.no_tail_buffer,
            allow_other: opts.allow_other,
            auto_unmount: opts.auto_unmount,
            metrics_file: opts
                .metrics_file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        }
    }
}

/// 扫描 projects_root 下所有目录（跳过 `*.zipfs-orig` 备份与点文件），探测状态。
pub fn scan(paths: &Paths) -> io::Result<Vec<ProjectInfo>> {
    let mut out = Vec::new();
    let rd = match fs::read_dir(&paths.projects_root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for dent in rd {
        let dent = dent?;
        let name = match dent.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue, // 非 UTF-8 名跳过（Claude 项目名是 ASCII path-encoded）。
        };
        if name.ends_with(super::model::ORIG_SUFFIX) || name.starts_with('.') {
            continue;
        }
        if !dent.file_type()?.is_dir() {
            continue;
        }
        out.push(probe(paths, &name));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 探测单个项目状态。meta（后端无关 sidecar）先读 → 反推 backend → 派生 backing。
pub fn probe(paths: &Paths, name: &str) -> ProjectInfo {
    let mp = paths.mountpoint(name);
    let orig_exists = paths.orig(name).exists();
    let endpoint_ok = endpoint_ok(&mp);
    let mounted = endpoint_ok && is_mounted(&mp);
    let meta = read_meta(&paths.meta_path(name)).ok().flatten();
    let committed = meta.as_ref().map(|m| m.committed).unwrap_or(false);
    let status = classify(orig_exists, mounted, endpoint_ok, committed);
    ProjectInfo {
        name: name.to_string(),
        status,
        meta,
    }
}

/// 挂载点是否可 stat（stale FUSE endpoint 返回 ENOTCONN → false）。其他错误不算 stale。
pub fn endpoint_ok(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) => e.raw_os_error() != Some(libc::ENOTCONN),
    }
}

/// `path` 是否为活的 fuse 挂载点：解析 `/proc/self/mountinfo`，精确匹配挂载点且 fstype=fuse。
pub fn is_mounted(path: &Path) -> bool {
    let target = canonicalized_target(path);
    let Ok(content) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    // 同一挂载点可能多行（overmount），取最后一条为准。
    let mut mounted = false;
    for line in content.lines() {
        if let Some((mp, is_fuse)) = parse_mountinfo_line(line) {
            if Path::new(&mp) == target {
                mounted = is_fuse;
            }
        }
    }
    mounted
}

/// 规范化挂载点用于与 mountinfo（内核规范路径）精确比对。整路径 `canonicalize` 失败时
/// （如 stale ENOTCONN endpoint，挂载点自身已无法 stat），退而规范化**父目录**再拼回末段——
/// 父目录通常仍可解析，避免回退到未规范化原路径导致与 mountinfo 失配、漏判已挂载（评审 A4/C2）。
fn canonicalized_target(path: &Path) -> std::path::PathBuf {
    if let Ok(p) = fs::canonicalize(path) {
        return p;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(cp) = fs::canonicalize(parent) {
            return cp.join(name);
        }
    }
    path.to_path_buf()
}

/// 解析一行 mountinfo，返回（反转义后的挂载点, fstype 是否 fuse 系）。格式：
/// `id pid major:minor root mountpoint opts... - fstype source superopts`。
fn parse_mountinfo_line(line: &str) -> Option<(String, bool)> {
    let fields: Vec<&str> = line.split(' ').collect();
    if fields.len() < 7 {
        return None;
    }
    let mountpoint = unescape_octal(fields[4]);
    // 找 ` - ` 分隔符后的 fstype。
    let sep = fields.iter().position(|&f| f == "-")?;
    let fstype = fields.get(sep + 1)?;
    let is_fuse = fstype.starts_with("fuse");
    Some((mountpoint, is_fuse))
}

/// 反转义 mountinfo 的八进制转义（空格 \040、tab \011、换行 \012、反斜杠 \134）。
fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // 需要完整 4 字节 `\ooo`（含末尾恰好以转义结尾的挂载点）。
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            let oct = &s[i + 1..i + 4];
            if let Ok(v) = u8::from_str_radix(oct, 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 活跃判定：扫 `/proc/*/fd` 与 `/cwd` 命中 `P` 子树（catch 任何活跃写者，含当前会话的 claude），
/// 再辅以 `*.jsonl`/`*.log` 近期 mtime。任一命中即 `Active`（带原因）。
pub fn detect_activity(path: &Path) -> Activity {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(reason) = scan_proc_for_holders(&target) {
        return Activity::Active(reason);
    }
    if let Some(reason) = recent_log_write(&target) {
        return Activity::Active(reason);
    }
    Activity::Idle
}

/// 扫 `/proc/[pid]/fd/*` 与 `/proc/[pid]/cwd`，若有进程在 `target` 子树持 fd 或以其为 cwd 则返回原因。
fn scan_proc_for_holders(target: &Path) -> Option<String> {
    let rd = fs::read_dir("/proc").ok()?;
    for dent in rd.flatten() {
        let pid_name = dent.file_name();
        let pid_str = pid_name.to_string_lossy();
        if !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let base = dent.path();
        // cwd
        if let Ok(cwd) = fs::read_link(base.join("cwd")) {
            if cwd.starts_with(target) {
                return Some(format!("pid {pid_str} 以此为 cwd"));
            }
        }
        // 打开的 fd（他人进程目录 EACCES → 静默跳过）。
        if let Ok(fds) = fs::read_dir(base.join("fd")) {
            for fd in fds.flatten() {
                if let Ok(p) = fs::read_link(fd.path()) {
                    if p.starts_with(target) {
                        let comm = fs::read_to_string(base.join("comm"))
                            .map(|s| s.trim().to_string())
                            .unwrap_or_default();
                        return Some(format!("pid {pid_str} ({comm}) 持有打开 fd"));
                    }
                }
            }
        }
    }
    None
}

/// `target` 下任一 `*.jsonl`/`*.log` 的 mtime 在活跃窗口内 → 返回原因。
fn recent_log_write(target: &Path) -> Option<String> {
    let now = SystemTime::now();
    let window = Duration::from_secs(ACTIVITY_MTIME_SECS);
    recent_log_write_rec(target, now, window, 0)
}

fn recent_log_write_rec(
    dir: &Path,
    now: SystemTime,
    window: Duration,
    depth: u32,
) -> Option<String> {
    if depth > 8 {
        return None; // 防极深树。
    }
    let rd = fs::read_dir(dir).ok()?;
    for dent in rd.flatten() {
        let path = dent.path();
        // 单个 entry 取类型失败：跳过此条，继续扫同级其余文件（绝不放弃整层 → 防活跃误判）。
        let Ok(ft) = dent.file_type() else { continue };
        if ft.is_dir() {
            if let Some(r) = recent_log_write_rec(&path, now, window, depth + 1) {
                return Some(r);
            }
        } else if ft.is_file() {
            let is_log = path
                .extension()
                .map(|e| e == "jsonl" || e == "log")
                .unwrap_or(false);
            if !is_log {
                continue;
            }
            if let Ok(md) = dent.metadata() {
                if let Ok(mt) = md.modified() {
                    if now.duration_since(mt).map(|d| d < window).unwrap_or(true) {
                        return Some(format!("{} 近期写入", path.display()));
                    }
                }
            }
        }
    }
    None
}

// ── sidecar 提交标记（手搓 key=value，无 serde） ──────────────────────────────

/// 写提交标记 sidecar 到 `path`（`back/<name>.zipfs.meta`）。**调用方**负责在 fsync backing
/// 之后再调本函数；本函数自身 fsync sidecar + 其父目录，使「sidecar 存在且 committed=1」成为
/// 可信提交点（评审 C1/C2）。
pub fn write_meta(path: &Path, meta: &Meta) -> io::Result<()> {
    // 评审 M4：sidecar 是数据安全信任根（committed 是挂载闸门）。自由文本值 dict/metrics_file
    // 是路径，Unix 路径可含换行——含 `\n` 的路径会注入伪造行（如 `\ncommitted=1`），parse 末键
    // 胜出即可把半灌 backing 伪造成权威挂出。写入端 fail-closed 拒绝含控制字符的值。
    let dict = sidecar_value(meta.dict.as_deref().unwrap_or(""))?;
    let metrics_file = sidecar_value(meta.metrics_file.as_deref().unwrap_or(""))?;
    let tmp = with_ext(path, "tmp");
    let body = format!(
        "backend={}\nchunk_size={}\nlevel={}\nbytes_src={}\nbytes_archive={}\napplied_at={}\ncommitted={}\ndict={}\nthreads={}\nwriteback={}\nmax_write={}\nno_tail_buffer={}\nallow_other={}\nauto_unmount={}\nmetrics_file={}\n",
        meta.backend.flag(),
        meta.chunk_size,
        meta.level,
        meta.bytes_src,
        meta.bytes_archive,
        meta.applied_at,
        if meta.committed { 1 } else { 0 },
        dict,
        meta.threads,
        if meta.writeback { 1 } else { 0 },
        meta.max_write,
        if meta.no_tail_buffer { 1 } else { 0 },
        if meta.allow_other { 1 } else { 0 },
        if meta.auto_unmount { 1 } else { 0 },
        metrics_file,
    );
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // fsync 父目录使 rename 持久（dirent durability）。
    crate::core::fsync_dir_of(path);
    Ok(())
}

/// 校验一个 sidecar key=value 的 value 不含会破坏 key=value 解析 / 伪造键的控制字符
/// （`\n`/`\r`）。含则 fail-closed 报错（评审 M4）。返回原值便于内联使用。
fn sidecar_value(v: &str) -> io::Result<&str> {
    if v.contains('\n') || v.contains('\r') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("sidecar 值含换行/回车，拒绝写入（防伪造提交标记）：{v:?}"),
        ));
    }
    Ok(v)
}

/// 读提交标记 sidecar。不存在 → Ok(None)。解析未知键忽略，缺失键取默认（parse-don't-validate）。
pub fn read_meta(path: &Path) -> io::Result<Option<Meta>> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(Some(parse_meta(&content)))
}

/// 给路径换扩展名后缀（`.zipfs.meta` → `.zipfs.meta.tmp`），保持同目录原子 rename。
fn with_ext(path: &Path, ext: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    std::path::PathBuf::from(s)
}

/// 纯解析：key=value 行 → Meta（单测覆盖）。
fn parse_meta(content: &str) -> Meta {
    let mut m = Meta::default();
    let opt = |v: &str| {
        if v.is_empty() {
            None
        } else {
            Some(v.to_string())
        }
    };
    for line in content.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "backend" => m.backend = Backend::parse(v).unwrap_or(Backend::Shadow),
            "chunk_size" => m.chunk_size = v.parse().unwrap_or(0),
            "level" => m.level = v.parse().unwrap_or(0),
            "bytes_src" => m.bytes_src = v.parse().unwrap_or(0),
            "bytes_archive" => m.bytes_archive = v.parse().unwrap_or(0),
            "applied_at" => m.applied_at = v.parse().unwrap_or(0),
            "committed" => m.committed = v == "1",
            "dict" => m.dict = opt(v),
            "threads" => m.threads = v.parse().unwrap_or(0),
            "writeback" => m.writeback = v == "1",
            "max_write" => m.max_write = v.parse().unwrap_or(0),
            "no_tail_buffer" => m.no_tail_buffer = v == "1",
            "allow_other" => m.allow_other = v == "1",
            "auto_unmount" => m.auto_unmount = v == "1",
            "metrics_file" => m.metrics_file = opt(v),
            _ => {}
        }
    }
    m
}

/// 当前 unix 时间戳（秒），失败回 0。
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn unescape_octal_handles_space_and_backslash() {
        assert_eq!(unescape_octal("/mnt/a"), "/mnt/a");
        assert_eq!(unescape_octal("/mnt/a\\040b"), "/mnt/a b"); // \040 = space
        assert_eq!(unescape_octal("/x\\134y"), "/x\\y"); // \134 = backslash
        assert_eq!(unescape_octal("/x\\040"), "/x "); // 末尾恰好转义（M2 边界）
    }

    #[test]
    fn parse_mountinfo_exact_match_and_fstype() {
        // 真实 fuse 挂载行（zipfs）。
        let line =
            "123 45 0:50 / /home/u/.claude/projects/foo rw,nosuid - fuse.zipfs-shadow zipfs rw";
        let (mp, is_fuse) = parse_mountinfo_line(line).unwrap();
        assert_eq!(mp, "/home/u/.claude/projects/foo");
        assert!(is_fuse);
        // 非 fuse（ext4）。
        let ext = "1 2 0:1 / /data rw - ext4 /dev/sda1 rw";
        assert!(!parse_mountinfo_line(ext).unwrap().1);
    }

    #[test]
    fn parse_meta_round_trip_via_write_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proj.zipfs.meta");
        let meta = Meta {
            backend: Backend::Container,
            chunk_size: 1048576,
            level: 3,
            bytes_src: 1000,
            bytes_archive: 100,
            applied_at: 42,
            committed: true,
            dict: Some("/x/shared.dict".to_string()),
            threads: 8,
            writeback: true,
            max_write: 4194304,
            no_tail_buffer: true,
            allow_other: true,
            auto_unmount: true,
            metrics_file: Some("/m/z.prom".to_string()),
        };
        write_meta(&path, &meta).unwrap();
        let got = read_meta(&path).unwrap().unwrap();
        assert_eq!(got, meta);
        assert!((got.ratio() - 10.0).abs() < 1e-9);
        // options() 透传全部选项（含 backend）。
        let o = got.options();
        assert_eq!(o.backend, Backend::Container);
        assert_eq!(o.threads, 8);
        assert!(o.writeback && o.no_tail_buffer && o.allow_other && o.auto_unmount);
        assert_eq!(
            o.dict.as_deref(),
            Some(std::path::Path::new("/x/shared.dict"))
        );
        assert_eq!(
            o.metrics_file.as_deref(),
            Some(std::path::Path::new("/m/z.prom"))
        );
    }

    #[test]
    fn write_meta_rejects_newline_injection_in_paths() {
        // 评审 M4：含换行的路径会注入伪造行（如 `\ncommitted=1`）篡改提交闸门。须 fail-closed。
        let dir = tempfile::tempdir().unwrap();
        let mut meta = Meta {
            committed: false,
            ..Meta::default()
        };
        meta.dict = Some("/tmp/x\ncommitted=1".to_string());
        let res = write_meta(&dir.path().join("p.zipfs.meta"), &meta);
        assert!(res.is_err(), "含换行的 dict 路径须拒绝写入");
        // 干净路径正常写入并读回，committed 保持 false（未被伪造）。
        meta.dict = Some("/tmp/clean-dict".to_string());
        let p = dir.path().join("q.zipfs.meta");
        write_meta(&p, &meta).unwrap();
        let back = read_meta(&p).unwrap().unwrap();
        assert!(!back.committed, "干净写入后 committed 应仍为 false");
        assert_eq!(back.dict.as_deref(), Some("/tmp/clean-dict"));
    }

    #[test]
    fn read_meta_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_meta(&dir.path().join("none.zipfs.meta"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn canonicalized_target_resolves_symlinked_parent_for_stale_endpoint() {
        // 评审 A4/C2：挂载点 endpoint 自身不可 stat（stale）时，仍应经父目录解析出规范路径，
        // 而非回退未规范化原路径（会与 mountinfo 失配漏判已挂载）。
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // link/ep 不存在（模拟 stale endpoint）：整路径 canonicalize 失败，须经父（link→real）解析。
        let stale = link.join("ep");
        let got = canonicalized_target(&stale);
        let want = fs::canonicalize(&real).unwrap().join("ep");
        assert_eq!(got, want, "应经规范化父目录拼回末段，而非回退原路径");
    }

    #[test]
    fn parse_meta_tolerates_legacy_sidecar_without_new_fields() {
        // 升级路径：apply 增挂载选项前写的旧 sidecar 没有 dict/threads/writeback/max_write 行。
        // 解析须容忍缺失（留 Default），新字段不得污染旧值，committed 仍可信。
        let legacy = "chunk_size=1048576\nlevel=3\nbytes_src=1000\nbytes_archive=100\napplied_at=42\ncommitted=1\n";
        let m = parse_meta(legacy);
        assert_eq!(m.chunk_size, 1048576);
        assert_eq!(m.level, 3);
        assert!(m.committed, "committed 仍应可信");
        // 新字段回落默认（remount 据此用默认挂载参数，与升级前行为一致）。
        assert_eq!(m.dict, None);
        assert_eq!(m.threads, 0);
        assert!(!m.writeback);
        assert_eq!(m.max_write, 0);
    }

    #[test]
    fn uncommitted_meta_parses_committed_false() {
        let m = parse_meta("chunk_size=65536\nlevel=3\ncommitted=0\n");
        assert!(!m.committed);
        assert_eq!(m.chunk_size, 65536);
    }

    #[test]
    fn endpoint_ok_true_for_normal_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(endpoint_ok(dir.path()));
    }

    #[test]
    fn detect_activity_idle_for_quiet_nonlog_tree() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), b"hi").unwrap(); // 非 jsonl/log，不触发 mtime 分支
        assert_eq!(detect_activity(dir.path()), Activity::Idle);
    }

    #[test]
    fn detect_activity_active_when_fd_held() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("note.txt"); // 用非 log 文件，确保命中的是 fd 分支而非 mtime
        fs::write(&f, b"hi").unwrap();
        let mut handle = fs::File::open(&f).unwrap();
        let act = detect_activity(dir.path());
        assert!(act.is_active(), "本进程持有 fd 应判活跃：{act:?}");
        // 触碰 handle 防止被提前 drop/优化。
        let mut buf = [0u8; 1];
        let _ = handle.read(&mut buf);
    }

    #[test]
    fn detect_activity_active_when_recent_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("s.jsonl"), b"{}\n").unwrap(); // 新建 → mtime=now → 活跃
        assert!(detect_activity(dir.path()).is_active());
    }

    #[test]
    fn recent_log_write_scans_all_siblings_and_subdirs() {
        // 回归：单个 entry 出错不得放弃同级其余文件；近期 .jsonl 即便排在其它条目之后、
        // 或嵌在子目录里，也必须被扫到（活跃误判 → 零丢失漏洞）。
        let dir = tempfile::tempdir().unwrap();
        // 先放若干非 log 文件与一个子目录，最后才是近期 .jsonl。
        fs::write(dir.path().join("a.txt"), b"x").unwrap();
        fs::write(dir.path().join("b.bin"), b"x").unwrap();
        let sub = dir.path().join("nested");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("recent.jsonl"), b"{}\n").unwrap(); // 子目录里的近期日志
        assert!(
            detect_activity(dir.path()).is_active(),
            "子目录中的近期 .jsonl 应判活跃"
        );
    }
}
