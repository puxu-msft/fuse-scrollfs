//! enable 子命令的数据模型：路径解析、项目状态、apply 选项、活跃判定。
//!
//! 这里只放**纯数据 + 纯函数**（无 IO），尤其是 `classify` —— 状态分类的单一真值源，
//! 便于真值表单测覆盖（见文件末 tests）。IO 扫描在 `discovery.rs`，生命周期在 `lifecycle.rs`。

use std::path::{Path, PathBuf};

use crate::core::{DEFAULT_CHUNK_SIZE, DEFAULT_ZSTD_LEVEL};

/// 备份后缀：apply 时 `P` 改名为 `P.zipfs-orig`，restore 时还原（与 cutover.sh 一致）。
pub const ORIG_SUFFIX: &str = ".zipfs-orig";
/// 守护 pid 文件后缀：`<mountpoint>.zipfs.pid`（与 cutover.sh / mount.sh 一致）。
pub const PID_SUFFIX: &str = ".zipfs.pid";
/// 提交标记 sidecar 文件后缀（评审 C2：committed=1 才算灌入完成、可挂载）。后端无关，
/// 位于 `back/<name>.zipfs.meta`（与 backing 同级），故 container 的 redb 文件外也有提交点。
pub const META_SUFFIX: &str = ".zipfs.meta";
/// NEEDS-RECONCILE sentinel 后缀（Task 12）：guard-check 检出 underlay 非空时落
/// `back_root/<name>.needs-reconcile`，给脚本/人明确信号「自动挂载已被阻止、需人工 reconcile」。
pub const NEEDS_RECONCILE_SUFFIX: &str = ".needs-reconcile";

/// guard-check 检出 underlay 非空（需人工 reconcile）时的独特退出码（Task 12）。
///
/// systemd 单元配 `RestartPreventExitStatus=75` 拦住它：underlay 非空是「需人工介入」的**稳定态**，
/// 重启无益，若不拦会与 `Restart=on-failure` 组成 crash-loop 风暴。取 75（sysexits.h `EX_TEMPFAIL`
/// 的数值）只为一个不易与通用失败（`1`）混淆的独特码，语义由本项目定义（= 需人工 reconcile）。
pub const GUARD_CHECK_NEEDS_RECONCILE_EXIT: i32 = 75;

/// 近期写入活跃判定窗口（秒）：jsonl/log 在此窗口内被改 → 视为活跃会话。
///
/// 取 5min 偏保守：主防线是 `/proc` 打开 fd 扫描（catch 任何持有该子树 fd 的写者），但 Claude
/// 可能在轮次间关闭 jsonl fd，此时仅靠 mtime；放宽窗口缩小「空闲思考>窗口 → 误判 Idle → apply
/// 抢走活跃会话」的假阴性。仍非万无一失 —— 真正不确定时用 `--force` 由人确认（见 lifecycle apply）。
pub const ACTIVITY_MTIME_SECS: u64 = 300;

/// 路径布局：projects 根（被管理的 Claude 项目目录）与 zipfs_home（backing 命名空间）。
///
/// 默认 `~/.claude/projects` 与 `~/.claude-zip`，分别可由 env `CLAUDE_PROJECTS` / `ZIPFS_HOME`
/// 覆盖（测试与隔离烟测靠这两个 env，绝不碰真实 `~/.claude`）。
#[derive(Debug, Clone)]
pub struct Paths {
    pub projects_root: PathBuf,
    pub zipfs_home: PathBuf,
}

impl Paths {
    /// 从环境解析。`home` 显式传入以便测试（生产取 `$HOME`）。
    pub fn resolve(home: &Path) -> Self {
        let projects_root = std::env::var_os("CLAUDE_PROJECTS")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude").join("projects"));
        let zipfs_home = std::env::var_os("ZIPFS_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude-zip"));
        Self {
            projects_root,
            zipfs_home,
        }
    }

    /// 项目挂载点 = projects_root/name（apply 后即此处的透明压缩挂载）。
    pub fn mountpoint(&self, name: &str) -> PathBuf {
        self.projects_root.join(name)
    }

    /// 源备份目录 = mountpoint + ORIG_SUFFIX。
    pub fn orig(&self, name: &str) -> PathBuf {
        let mut p = self.mountpoint(name).into_os_string();
        p.push(ORIG_SUFFIX);
        PathBuf::from(p)
    }

    /// backing 根目录 = zipfs_home/back（meta sidecar 与各 backing 的父目录）。
    pub fn back_root(&self) -> PathBuf {
        self.zipfs_home.join("back")
    }

    /// backing：shadow → 目录 `back/name`；container → redb 文件 `back/name.redb`。
    pub fn backing(&self, name: &str, backend: Backend) -> PathBuf {
        match backend {
            Backend::Shadow => self.back_root().join(name),
            Backend::Container => self.back_root().join(format!("{name}.redb")),
        }
    }

    /// 提交标记 sidecar = `back/name.zipfs.meta`（**后端无关**的同名兄弟文件，使 container 的
    /// redb 文件外也有可信提交点；记录 backend 供探测时反推 backing 形态）。
    pub fn meta_path(&self, name: &str) -> PathBuf {
        self.back_root().join(format!("{name}{META_SUFFIX}"))
    }

    /// pid 文件 = mountpoint + PID_SUFFIX。
    pub fn pid_file(&self, name: &str) -> PathBuf {
        let mut p = self.mountpoint(name).into_os_string();
        p.push(PID_SUFFIX);
        PathBuf::from(p)
    }

    /// reconcile 快照暂存目录 = `zipfs_home/reconcile-stash/<name>/<ts>`。
    /// underlay 拍下的不可变快照落此处（合并输入与删前复核的唯一基准）；按 `ts` 分代便于审计/清理。
    pub fn reconcile_stash(&self, name: &str, ts: &str) -> PathBuf {
        self.zipfs_home.join("reconcile-stash").join(name).join(ts)
    }

    /// per-generation reconcile manifest = `reconcile_stash(name,ts)/manifest`（undo 依赖，§10.1）。
    /// 首行 `ts`，其后每行真实 `rel\tclass`（逆转类），供 `reconcile_undo` 逐条目反向还原。
    pub fn reconcile_manifest(&self, name: &str, ts: &str) -> PathBuf {
        self.reconcile_stash(name, ts).join("manifest")
    }

    /// reconcile 隔离区 = `zipfs_home/reconcile-quarantine/<name>/<ts>`。
    /// 合并冲突/超限降级为 KeepBoth 的条目搬此处保全（绝不静默丢弃），供人工核查。
    pub fn quarantine(&self, name: &str, ts: &str) -> PathBuf {
        self.zipfs_home
            .join("reconcile-quarantine")
            .join(name)
            .join(ts)
    }

    /// reconcile 串行锁 = `back_root/<name>.reconcile.lock`。**独立于** backing `.zipfs.lock`：
    /// 仅串行化并发 reconcile 彼此，不参与挂载互斥（后者靠 underlay-empty 守卫 + reconciling 标记）。
    pub fn reconcile_lock(&self, name: &str) -> PathBuf {
        self.back_root().join(format!("{name}.reconcile.lock"))
    }

    /// reconcile 进行中标记 sidecar = `back_root/<name>.reconciling`（评审 I-4）。
    ///
    /// **独立于** `committed` 提交标记：reconcile 会原子改写 orig（半改写窗口），此标记存在即示意
    /// 生命周期维护操作（restore/reingest/compact/seal/remount）让路，避免作用在半改写的 orig 上。
    /// 绝不改 `Meta.committed`（committed 语义是「backing 已灌入完成、可挂载」，与 reconcile 正交）。
    pub fn reconciling_marker(&self, name: &str) -> PathBuf {
        self.back_root().join(format!("{name}.reconciling"))
    }

    /// NEEDS-RECONCILE sentinel = `back_root/<name>.needs-reconcile`（Task 12）。
    ///
    /// guard-check 检出挂载点 underlay 含停用期回落写时落此文件：给脚本/人明确信号「该项目自动挂载
    /// 已被阻止，需 `zipfs enable reconcile <name>` 重合并」。underlay 清空后由下次 guard-check 通过
    /// 时自愈清除。**独立于** `.reconciling`（那是半改写维护互斥）与 `.zipfs.meta`（提交标记）。
    pub fn needs_reconcile_sentinel(&self, name: &str) -> PathBuf {
        self.back_root()
            .join(format!("{name}{NEEDS_RECONCILE_SUFFIX}"))
    }
}

/// 校验用户提供的项目 `name` 为 projects_root 下的**单一目录段**，拒绝路径穿越。
///
/// 命中 no-unconscious 数据丢失红线：`name` 直接喂给 `Paths::{mountpoint,orig,backing}`
/// 的 `join`，而 `join` 对绝对路径会**整体替换**基目录、对 `..` 会**逃出**基目录 ——
/// 一个前导 `/` 或粘贴带 `..` 的手误就能让下游 `remove_dir_all`/`rename` 落到树外真实目录。
/// 入口 fail-closed：只允许单段、非 `.`/`..`、不含分隔符或 NUL 的名字（Claude path-encoded
/// 项目名天然满足）。
pub fn validate_name(name: &str) -> std::io::Result<()> {
    let invalid = |msg: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("非法项目名 {name:?}：{msg}"),
        )
    };
    if name.is_empty() {
        return Err(invalid("不能为空"));
    }
    if name.contains('\0') {
        return Err(invalid("含 NUL"));
    }
    if name == "." || name == ".." {
        return Err(invalid("不能为 . 或 .."));
    }
    // 必须恰好是一个 Normal 组件：绝对路径、含 `/`（或平台分隔符）、含 `..` 均被拒。
    let mut comps = Path::new(name).components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Normal(c)), None) if c == name => Ok(()),
        _ => Err(invalid(
            "必须是 projects_root 下的单一目录段（不得含 / 或 ..）",
        )),
    }
}

/// 项目当前状态（`classify` 的输出，TUI 状态色块与 list 表的依据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    /// 未被 zipfs 管理（普通目录）。
    Plain,
    /// 正常透明压缩挂载中。
    Active,
    /// 已切换但守护已停（backing 已提交，可安全 remount）。
    Stopped,
    /// 半状态需人工：stale endpoint（ENOTCONN，daemon 死）或 backing 未提交（半灌）。续 restore 或 re-ingest。
    Broken,
    /// daemon 无响应（wedge/卡死）：endpoint 探测超时，挂载条目可能仍在。区别于 Broken（僵尸/半灌），
    /// 卡死可能只是 daemon 暂时无响应，也可能真死——需卸载（`Auto` 档必要时 abort）修复。
    Hung,
}

impl ProjectStatus {
    /// 简短英文标签（list/CLI 输出）。
    pub fn label(self) -> &'static str {
        match self {
            ProjectStatus::Plain => "PLAIN",
            ProjectStatus::Active => "ZIPFS",
            ProjectStatus::Stopped => "STOPPED",
            ProjectStatus::Broken => "BROKEN",
            ProjectStatus::Hung => "HUNG",
        }
    }
}

/// 挂载点 endpoint 的健康三态（`discovery` 探测，喂 `classify` 区分「僵尸」与「卡死」）。
/// 只关心「健康与否」的消费点用 `endpoint_ok` 薄封装即可；此三态仅供分类/展示层区分故障成因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointHealth {
    /// 可 stat：daemon 活着并响应。
    Healthy,
    /// ENOTCONN：daemon 已死，挂载是僵尸 stale endpoint。
    Stale,
    /// 探测超时（或熔断缓存命中）：daemon 无响应（wedge/卡死）。
    Hung,
}

/// 纯状态分类（评审 C2：以 `backing_committed` 区分「守护死可重挂」与「半灌需人工」）。
///
/// 输入全是探测事实（由 `discovery.rs` 提供）：
/// - `orig_exists`：`P.zipfs-orig` 备份是否在（= 是否被 apply 过/切换中）。
/// - `mounted`：`P` 是否为活的 zipfs 挂载点（mountinfo 命中 **且** endpoint 健康）。
/// - `health`：`P` 的 endpoint 健康三态。`mounted` 已蕴含 `Healthy`（探测端 `matches!(health,Healthy) && is_mounted`），
///   故 `Stale`/`Hung` 只可能落 `(true,false)` 分支：`Hung → Hung`、`Stale → Broken`。
/// - `backing_committed`：backing 内 sidecar 存在且 `committed=1`。
pub fn classify(
    orig_exists: bool,
    mounted: bool,
    health: EndpointHealth,
    backing_committed: bool,
) -> ProjectStatus {
    match (orig_exists, mounted) {
        // 未切换：无备份且未挂载 → 普通目录。
        (false, _) => ProjectStatus::Plain,
        // 已切换且在挂：mounted 蕴含 Healthy → Active（防御性对非 Healthy 归 Broken，实际不可达）。
        (true, true) => match health {
            EndpointHealth::Healthy => ProjectStatus::Active,
            EndpointHealth::Hung => ProjectStatus::Hung,
            EndpointHealth::Stale => ProjectStatus::Broken,
        },
        // 已切换但未挂载：卡死优先标 Hung；否则 backing 提交完整且健康才可安全重挂，其余半灌/stale → Broken。
        (true, false) => match health {
            EndpointHealth::Hung => ProjectStatus::Hung,
            EndpointHealth::Healthy if backing_committed => ProjectStatus::Stopped,
            _ => ProjectStatus::Broken,
        },
    }
}

/// 后端布局选择。enable 默认 shadow（projects 负载最适），可选 container（redb 单文件容器）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    /// 布局 S：影子树，backing 是真实目录树（支持 symlink，append 友好，崩溃 fail-closed）。
    Shadow,
    /// 布局 V：容器，backing 是单个 redb 文件（便于搬运；不支持 symlink；覆盖写有 MVCC 膨胀）。
    Container,
}

impl Backend {
    /// 底层 `--backend` flag 值。
    pub fn flag(self) -> &'static str {
        match self {
            Backend::Shadow => "shadow",
            Backend::Container => "container",
        }
    }
    /// 解析（sidecar / CLI）。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "shadow" => Some(Backend::Shadow),
            "container" => Some(Backend::Container),
            _ => None,
        }
    }
}

/// apply 选项：后端布局 + 压缩/挂载参数。全量持久化到 backing sidecar，remount 原样复用，
/// 与底层 `zipfs --backend ... ` 一一对应。
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// 后端布局（shadow/container）。
    pub backend: Backend,
    /// 逻辑块大小（字节）。
    pub chunk_size: u32,
    /// zstd 等级（1/3/9/19；大块叠高等级提比值）。
    pub level: i32,
    /// 共享 zstd 字典文件路径（`zipfs train-dict` 产出）；None 不启用。
    pub dict: Option<PathBuf>,
    /// FUSE 工作线程数（0=默认 = CPU 数，下限 4）。
    pub threads: usize,
    /// 启用 FUSE 写回缓存（合并小写、降写尾 p99）。
    pub writeback: bool,
    /// 协商最大单次 write 字节（0=内核默认 128KiB；调大减大行 append 拆分）。
    pub max_write: u32,
    /// 关闭未压缩开放尾块缓冲（仅基准对照用；默认开启优化即 false）。
    pub no_tail_buffer: bool,
    /// 允许其他用户访问挂载点（allow_other，需 /etc/fuse.conf 放行）。
    pub allow_other: bool,
    /// 进程退出自动卸载（AutoUnmount）。
    pub auto_unmount: bool,
    /// Prometheus textfile 指标输出路径（.prom）；None 不输出。
    pub metrics_file: Option<PathBuf>,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            backend: Backend::Shadow,
            chunk_size: DEFAULT_CHUNK_SIZE as u32,
            level: DEFAULT_ZSTD_LEVEL,
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

/// 活跃判定结果。`Active` 携带人类可读原因（指明持有进程，便于 `--force` 前警告，评审 L4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Active(String),
}

impl Activity {
    pub fn is_active(&self) -> bool {
        matches!(self, Activity::Active(_))
    }
    pub fn reason(&self) -> Option<&str> {
        match self {
            Activity::Active(r) => Some(r.as_str()),
            Activity::Idle => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 真值表：classify 全态覆盖（评审 C2 核心 + 阶段 A 的 Hung 分离）。
    #[test]
    fn classify_plain_when_no_backup() {
        // 无备份 → 永远 Plain，与挂载/健康/提交无关。
        use EndpointHealth::*;
        for &m in &[true, false] {
            for &h in &[Healthy, Stale, Hung] {
                for &c in &[true, false] {
                    assert_eq!(classify(false, m, h, c), ProjectStatus::Plain);
                }
            }
        }
    }

    #[test]
    fn classify_active_when_mounted_and_endpoint_ok() {
        assert_eq!(
            classify(true, true, EndpointHealth::Healthy, true),
            ProjectStatus::Active
        );
        assert_eq!(
            classify(true, true, EndpointHealth::Healthy, false),
            ProjectStatus::Active
        );
    }

    #[test]
    fn classify_broken_when_mounted_but_stale_endpoint() {
        // 挂载条目在但 endpoint ENOTCONN（stale）→ 半状态（防御分支，实际 mounted 蕴含 Healthy）。
        assert_eq!(
            classify(true, true, EndpointHealth::Stale, true),
            ProjectStatus::Broken
        );
    }

    #[test]
    fn classify_stopped_when_committed_and_unmounted() {
        assert_eq!(
            classify(true, false, EndpointHealth::Healthy, true),
            ProjectStatus::Stopped
        );
    }

    #[test]
    fn classify_broken_when_uncommitted_backing() {
        // 半灌 backing（无 committed）即便未挂载也不可自动重挂。
        assert_eq!(
            classify(true, false, EndpointHealth::Healthy, false),
            ProjectStatus::Broken
        );
        // stale endpoint 同样 Broken（僵尸）。
        assert_eq!(
            classify(true, false, EndpointHealth::Stale, true),
            ProjectStatus::Broken
        );
    }

    #[test]
    fn classify_hung_when_endpoint_probe_times_out() {
        // 阶段 A：endpoint 探测超时（daemon 无响应/wedge）→ Hung，区别于 Broken（stale/半灌）。
        // 卡死优先：即便 backing 未提交也标 Hung（先解卡死才谈重挂）。
        assert_eq!(
            classify(true, false, EndpointHealth::Hung, true),
            ProjectStatus::Hung
        );
        assert_eq!(
            classify(true, false, EndpointHealth::Hung, false),
            ProjectStatus::Hung
        );
    }

    #[test]
    fn paths_layout_from_explicit_home() {
        // 清掉可能干扰的 env，验证默认布局。
        // SAFETY: 单线程测试内临时改 env；其他 enable 测试不依赖这两个 env 的缺省。
        let prev_p = std::env::var_os("CLAUDE_PROJECTS");
        let prev_z = std::env::var_os("ZIPFS_HOME");
        std::env::remove_var("CLAUDE_PROJECTS");
        std::env::remove_var("ZIPFS_HOME");
        let p = Paths::resolve(Path::new("/home/u"));
        assert_eq!(p.projects_root, Path::new("/home/u/.claude/projects"));
        assert_eq!(p.zipfs_home, Path::new("/home/u/.claude-zip"));
        assert_eq!(
            p.mountpoint("proj-x"),
            Path::new("/home/u/.claude/projects/proj-x")
        );
        assert_eq!(
            p.orig("proj-x"),
            Path::new("/home/u/.claude/projects/proj-x.zipfs-orig")
        );
        assert_eq!(
            p.backing("proj-x", Backend::Shadow),
            Path::new("/home/u/.claude-zip/back/proj-x")
        );
        assert_eq!(
            p.backing("proj-x", Backend::Container),
            Path::new("/home/u/.claude-zip/back/proj-x.redb")
        );
        assert_eq!(
            p.meta_path("proj-x"),
            Path::new("/home/u/.claude-zip/back/proj-x.zipfs.meta")
        );
        assert_eq!(
            p.pid_file("proj-x"),
            Path::new("/home/u/.claude/projects/proj-x.zipfs.pid")
        );
        assert_eq!(
            p.needs_reconcile_sentinel("proj-x"),
            Path::new("/home/u/.claude-zip/back/proj-x.needs-reconcile")
        );
        if let Some(v) = prev_p {
            std::env::set_var("CLAUDE_PROJECTS", v);
        }
        if let Some(v) = prev_z {
            std::env::set_var("ZIPFS_HOME", v);
        }
    }

    #[test]
    fn activity_helpers() {
        assert!(!Activity::Idle.is_active());
        let a = Activity::Active("pid 42".into());
        assert!(a.is_active());
        assert_eq!(a.reason(), Some("pid 42"));
    }

    #[test]
    fn validate_name_accepts_path_encoded_project_names() {
        // Claude 项目名是 path-encoded 单段（前导 `-`、内嵌 `-`、点、下划线、数字）。
        for ok in ["-home-xp-src-foo", "proj-x", "a.b_c-1", "x"] {
            assert!(validate_name(ok).is_ok(), "应接受: {ok}");
        }
    }

    #[test]
    fn validate_name_rejects_traversal_and_absolute_and_empty() {
        // 命中 no-unconscious 数据丢失红线：这些 name 经 join 会逃出/替换基目录，
        // 最终落到 remove_dir_all/rename 的树外路径。必须在入口 fail-closed。
        for bad in [
            "",                   // 空
            ".",                  // 当前目录
            "..",                 // 上级
            "../etc",             // 相对逃逸
            "../../home/xp/data", // 多级逃逸
            "/etc",               // 绝对：join 整体替换基目录
            "/home/xp/important", // 绝对
            "a/b",                // 内嵌分隔符（非单段）
            "foo/../bar",         // 内嵌 ..
            "a\0b",               // NUL
        ] {
            assert!(validate_name(bad).is_err(), "应拒绝: {bad:?}");
        }
    }
}
