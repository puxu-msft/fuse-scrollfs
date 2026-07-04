//! `zipfs enable`：把 Claude projects 目录可逆切换到透明压缩挂载的启用器。
//!
//! - 无子动作 → 交互式 ratatui TUI（`tui.rs`）。
//! - 子动作 `list/apply/restore/remount/status/purge/autostart` → 可脚本化（便于测试/批处理/自挂载）。
//!
//! 设计见 docs 计划与 ROADMAP T4「切换工具」。可逆配方移植自 crash-tested 的
//! `bench/scripts/zipfs-{cutover,rollback,mount}.sh`，并以 sidecar 提交标记强化半灌可检测性。

pub mod autostart;
pub mod config;
pub mod daemon;
pub mod discovery;
pub mod force_umount;
pub(crate) mod hang_free;
pub mod lifecycle;
pub mod model;
pub mod systemd;
pub mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::Subcommand;

use crate::enable::systemd::select_mounter;
use crate::reconcile::advisor::Recommendation;
use crate::reconcile::orchestrator::{
    reconcile, Confirm, ConfirmFn, ReconcileOptions, ReconcileReport,
};
pub use model::{ApplyOptions, Backend, Paths, ProjectStatus};

/// `zipfs enable` 的子动作。`None`（不给）→ 启动 TUI。
#[derive(Subcommand, Debug, Clone)]
pub enum EnableAction {
    /// 列出所有 Claude projects 的 zipfs 状态（PLAIN/ZIPFS/STOPPED/BROKEN）。
    List,
    /// 把某项目可逆切换到透明压缩挂载（mv 备份 → ingest --verify → 挂载）。
    Apply {
        /// 项目名（projects_root 下的目录名，path-encoded，如 `-home-xp-src-foo`）。
        name: String,
        /// 后端布局：shadow（默认，支持 symlink）或 container（redb 单文件）。
        #[arg(long, value_enum)]
        backend: Option<Backend>,
        /// 块大小（字节），默认 1MiB。
        #[arg(long)]
        chunk: Option<u32>,
        /// zstd 等级（1/3/9/19），默认 3。
        #[arg(long)]
        level: Option<i32>,
        /// 共享 zstd 字典文件（`zipfs train-dict` 产出）；持久化、remount 复用。
        #[arg(long)]
        dict: Option<PathBuf>,
        /// FUSE 工作线程数（0=默认 = CPU 数，下限 4）。
        #[arg(long)]
        threads: Option<usize>,
        /// 启用 FUSE 写回缓存（降写尾 p99）。
        #[arg(long, default_value_t = false)]
        writeback: bool,
        /// 最大单次 write 字节（0=内核默认 128KiB）。
        #[arg(long)]
        max_write: Option<u32>,
        /// 关闭未压缩开放尾块缓冲（仅基准对照用）。
        #[arg(long, default_value_t = false)]
        no_tail_buffer: bool,
        /// 允许其他用户访问挂载点（allow_other，需 /etc/fuse.conf 放行）。
        #[arg(long, default_value_t = false)]
        allow_other: bool,
        /// 进程退出自动卸载（AutoUnmount）。
        #[arg(long, default_value_t = false)]
        auto_unmount: bool,
        /// Prometheus textfile 指标输出路径（.prom）。
        #[arg(long)]
        metrics_file: Option<PathBuf>,
        /// 越过活跃会话防护（**危险**：会挂到活跃日志上）。
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// 还原某项目（卸载 → 还原源备份）。零丢失；backing 保留。
    Restore { name: String },
    /// 重挂某项目（守护崩溃/重启后）；`--all` 重挂所有 STOPPED 项目。
    Remount {
        name: Option<String>,
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// 显示某项目的详细状态。
    Status { name: String },
    /// 删除某项目的 backing（仅在已 restore 后；二次确认）。
    Purge {
        name: String,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// 离线压实某项目 backing（回收空间）：卸载 → compact → 重挂。
    Compact { name: String },
    /// 从源备份（orig）重新灌入某项目 backing（修复指令，仅 shadow）：ingest --verify → 卸载 →
    /// 换 backing（旧的留 .reingest-bak）→ 重挂。修旧版本灌出的 backing（如 mtime 缺陷）。
    Reingest {
        name: String,
        /// 越过活跃会话防护（**危险**：会在写入中重建）。
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// 冷文件封存某项目（仅 shadow：大块高等级重编码逼近整流）：卸载 → seal → 重挂。
    Seal {
        name: String,
        /// 封存块大小（字节），默认 8MiB。
        #[arg(long)]
        seal_chunk: Option<u32>,
        /// 封存等级，默认 19。
        #[arg(long)]
        level: Option<i32>,
    },
    /// 停用期回落写重合并（仅 shadow）：把挂载点 underlay 里 Claude 直接写进去的 jsonl 等安全并回
    /// backing。**须先卸载**——挂载态下读挂载点是 FUSE 视图而非 underlay，reconcile 会误判。逐条
    /// 交互确认 `[a]ccept/[k]eep-both/[s]kip`；`--dry-run` 只出建议单、零改动；非交互（stdin 非 tty）
    /// 且非 dry-run → 拒绝（策略 B：绝不自动落盘）。
    Reconcile {
        name: String,
        /// 只出建议单、零改动（不落盘、不交互）。
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// 越过活跃会话门禁（人工确认空闲后）。
        #[arg(long, default_value_t = false)]
        force: bool,
        /// 逐条落盘后从 orig 全量重建 backing 并重挂。
        #[arg(long, default_value_t = false)]
        rebuild: bool,
    },
    /// 查看 / 设置持久化默认选项（ZIPFS_HOME/config，apply 起点）。
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// 自挂载接线：生成 systemd user 模板并启用，或打印 wsl.conf 片段。
    Autostart {
        #[command(subcommand)]
        cmd: AutostartCmd,
    },
}

/// 默认配置子动作。
#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCmd {
    /// 打印当前默认配置。
    Show,
    /// 设置一个默认项（key value），如 `config set level 19`。
    Set { key: String, value: String },
}

/// 自挂载子动作。
#[derive(Subcommand, Debug, Clone)]
pub enum AutostartCmd {
    /// 安装并启用 systemd user 单元（每个 ZIPFS 项目一个实例）。
    Install {
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// 打印 wsl.conf `[boot]` 片段（root 文件，仅打印不自动改）。
    Print,
}

/// enable 子命令入口。`home` 由调用方传入（`$HOME`），便于测试隔离。
pub fn run(action: Option<EnableAction>, home: PathBuf) -> std::io::Result<()> {
    let paths = Paths::resolve(&home);
    // 入口 fail-closed：校验所有用户提供的项目名为单一目录段，杜绝 join 穿越导致的树外
    // remove_dir_all/rename（no-unconscious 数据丢失红线）。TUI 的 name 来自目录扫描，天然单段。
    if let Some(
        EnableAction::Apply { name, .. }
        | EnableAction::Restore { name }
        | EnableAction::Status { name }
        | EnableAction::Remount {
            name: Some(name), ..
        }
        | EnableAction::Purge { name, .. }
        | EnableAction::Compact { name }
        | EnableAction::Reingest { name, .. }
        | EnableAction::Reconcile { name, .. }
        | EnableAction::Seal { name, .. },
    ) = &action
    {
        model::validate_name(name)?;
    }
    match action {
        None => tui::run(&paths),
        Some(EnableAction::List) => cmd_list(&paths),
        Some(EnableAction::Status { name }) => cmd_status(&paths, &name),
        Some(EnableAction::Apply {
            name,
            backend,
            chunk,
            level,
            dict,
            threads,
            writeback,
            max_write,
            no_tail_buffer,
            allow_other,
            auto_unmount,
            metrics_file,
            force,
        }) => cmd_apply(
            &paths,
            &name,
            ApplyOverrides {
                backend,
                chunk,
                level,
                dict,
                threads,
                writeback,
                max_write,
                no_tail_buffer,
                allow_other,
                auto_unmount,
                metrics_file,
            },
            force,
        ),
        Some(EnableAction::Restore { name }) => {
            lifecycle::restore(&paths, &name, select_mounter().as_ref())?;
            println!("restore: {name} 已还原（backing 保留，可 `enable purge {name}` 清理）");
            Ok(())
        }
        Some(EnableAction::Remount { name, all }) => cmd_remount(&paths, name, all),
        Some(EnableAction::Purge { name, yes }) => {
            lifecycle::purge_backing(&paths, &name, yes)?;
            println!("purge: 已删除 {name} 的 backing");
            Ok(())
        }
        Some(EnableAction::Compact { name }) => cmd_compact(&paths, &name),
        Some(EnableAction::Reingest { name, force }) => cmd_reingest(&paths, &name, force),
        Some(EnableAction::Reconcile {
            name,
            dry_run,
            force,
            rebuild,
        }) => cmd_reconcile(&paths, &name, dry_run, force, rebuild),
        Some(EnableAction::Seal {
            name,
            seal_chunk,
            level,
        }) => cmd_seal(&paths, &name, seal_chunk, level),
        Some(EnableAction::Config { cmd }) => config::run(&paths, cmd),
        Some(EnableAction::Autostart { cmd }) => autostart::run(&home, cmd),
    }
}

/// `list`：状态表（NAME / STATUS / RATIO / 物理大小）。
fn cmd_list(paths: &Paths) -> std::io::Result<()> {
    let infos = discovery::scan(paths)?;
    if infos.is_empty() {
        println!("（{} 下无项目）", paths.projects_root.display());
        return Ok(());
    }
    println!(
        "{:<46} {:<24} {:<10} {:>8}  META",
        "NAME", "STATUS", "BACKEND", "RATIO"
    );
    for info in &infos {
        // Plain 但有遗留 backing（restore 后未 purge）→ 标 purgeable，不显示其旧 ratio（易误解）。
        let (ratio, note) = match (info.status, &info.meta) {
            (ProjectStatus::Plain, Some(_)) => ("-".to_string(), "purgeable backing"),
            (ProjectStatus::Plain, None) => ("-".to_string(), ""),
            (_, Some(m)) if m.committed => (format!("{:.2}x", m.ratio()), "committed"),
            (_, Some(_)) => ("-".to_string(), "UNCOMMITTED"),
            (_, None) => ("-".to_string(), ""),
        };
        let backend = if info.meta.is_some() {
            info.backend().flag()
        } else {
            "-"
        };
        println!(
            "{:<46} {:<24} {:<10} {:>8}  {}",
            info.name,
            info.status_display(),
            backend,
            ratio,
            note
        );
    }
    Ok(())
}

/// `status <name>`：单项目详情 + 活跃判定。
fn cmd_status(paths: &Paths, name: &str) -> std::io::Result<()> {
    let info = discovery::probe(paths, name);
    println!("项目: {name}");
    println!("状态: {}", info.status_display());
    println!("后端: {}", info.backend().flag());
    println!("挂载点: {}", paths.mountpoint(name).display());
    println!("backing: {}", paths.backing(name, info.backend()).display());
    if let Some(m) = &info.meta {
        println!(
            "  chunk={} level={} 逻辑={}B 物理={}B ratio={:.2}x committed={}",
            m.chunk_size,
            m.level,
            m.bytes_src,
            m.bytes_archive,
            m.ratio(),
            m.committed
        );
        println!(
            "  dict={} threads={} writeback={} max_write={} no_tail_buffer={} allow_other={} auto_unmount={} metrics_file={}",
            m.dict.as_deref().unwrap_or("无"),
            m.threads,
            m.writeback,
            m.max_write,
            m.no_tail_buffer,
            m.allow_other,
            m.auto_unmount,
            m.metrics_file.as_deref().unwrap_or("无"),
        );
    }
    match discovery::detect_activity(&paths.mountpoint(name)) {
        crate::enable::model::Activity::Active(r) => println!("活跃: 是（{r}）"),
        crate::enable::model::Activity::Idle => println!("活跃: 否"),
    }
    Ok(())
}

/// `apply` 的命令行覆盖项（None = 用配置/默认）。
struct ApplyOverrides {
    backend: Option<Backend>,
    chunk: Option<u32>,
    level: Option<i32>,
    dict: Option<PathBuf>,
    threads: Option<usize>,
    writeback: bool,
    max_write: Option<u32>,
    no_tail_buffer: bool,
    allow_other: bool,
    auto_unmount: bool,
    metrics_file: Option<PathBuf>,
}

/// `apply <name>`：切换并回显结果。
fn cmd_apply(paths: &Paths, name: &str, ov: ApplyOverrides, force: bool) -> std::io::Result<()> {
    // 起点 = 持久化默认（ZIPFS_HOME/config），命令行覆盖之。
    let mut opts = config::load_defaults(paths);
    if let Some(b) = ov.backend {
        opts.backend = b;
    }
    if let Some(c) = ov.chunk {
        opts.chunk_size = c;
    }
    if let Some(l) = ov.level {
        opts.level = l;
    }
    if ov.dict.is_some() {
        opts.dict = ov.dict;
    }
    if let Some(t) = ov.threads {
        opts.threads = t;
    }
    if ov.writeback {
        opts.writeback = true;
    }
    if let Some(mw) = ov.max_write {
        opts.max_write = mw;
    }
    if ov.no_tail_buffer {
        opts.no_tail_buffer = true;
    }
    if ov.allow_other {
        opts.allow_other = true;
    }
    if ov.auto_unmount {
        opts.auto_unmount = true;
    }
    if ov.metrics_file.is_some() {
        opts.metrics_file = ov.metrics_file;
    }
    // 字典文件须存在，否则守护挂载会失败（提前给清晰错误）。
    if let Some(d) = &opts.dict {
        if !d.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("字典文件不存在：{}", d.display()),
            ));
        }
    }
    // 活跃且 --force：醒目警告并指明持有进程（评审 L4）。
    if force {
        if let crate::enable::model::Activity::Active(r) =
            discovery::detect_activity(&paths.mountpoint(name))
        {
            eprintln!("⚠ 警告：项目活跃（{r}），--force 仍将挂到活跃数据上，可能损坏！");
        }
    }
    let backend = opts.backend;
    let out = lifecycle::apply(paths, name, opts, force, select_mounter().as_ref())?;
    println!(
        "apply: {name} 已切换并挂载（backend={} files={} {}B→{}B {:.2}x）",
        backend.flag(),
        out.files,
        out.bytes_src,
        out.bytes_archive,
        out.ratio()
    );
    Ok(())
}

/// `remount`：单项目或 `--all`。
fn cmd_remount(paths: &Paths, name: Option<String>, all: bool) -> std::io::Result<()> {
    if all {
        let results = lifecycle::remount_all(paths, select_mounter().as_ref())?;
        if results.is_empty() {
            println!("remount --all: 无 STOPPED 项目");
        }
        for (n, r) in results {
            match r {
                Ok(()) => println!("remount: {n} ✓"),
                Err(e) => eprintln!("remount: {n} ✗ {e}"),
            }
        }
        Ok(())
    } else {
        let name = name.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "remount 需 <name> 或 --all",
            )
        })?;
        lifecycle::remount(paths, &name, select_mounter().as_ref())?;
        println!("remount: {name} ✓");
        Ok(())
    }
}

/// `compact <name>`：卸载→压实→重挂。
fn cmd_compact(paths: &Paths, name: &str) -> std::io::Result<()> {
    let report = lifecycle::compact(paths, name, select_mounter().as_ref())?;
    println!("compact: {name} — {report}");
    Ok(())
}

/// `reingest <name> [--force]`：从 orig 重灌 backing（修复指令，仅 shadow）。
fn cmd_reingest(paths: &Paths, name: &str, force: bool) -> std::io::Result<()> {
    let report = lifecycle::reingest(paths, name, force, select_mounter().as_ref())?;
    println!("{report}");
    Ok(())
}

/// `seal <name>`：卸载→封存（仅 shadow）→重挂。
fn cmd_seal(
    paths: &Paths,
    name: &str,
    seal_chunk: Option<u32>,
    level: Option<i32>,
) -> std::io::Result<()> {
    let chunk = seal_chunk.unwrap_or(crate::seal::DEFAULT_SEAL_CHUNK);
    let lvl = level.unwrap_or(crate::seal::DEFAULT_SEAL_LEVEL);
    let report = lifecycle::seal(paths, name, chunk, lvl, select_mounter().as_ref())?;
    println!("seal: {name} — {report}");
    Ok(())
}

/// `reconcile <name> [--dry-run] [--force] [--rebuild]`：停用期回落写重合并（仅 shadow）。
///
/// 前置：**项目必须未挂载**。挂载态下读挂载点返回的是 FUSE 视图（backing 内容），而非 underlay 里
/// Claude 直接写下的回落文件，reconcile 会把合并后的 backing 内容误当回落写反复处理 → 误判。
///
/// 交互策略（策略 B，绝不自动落盘）：
/// - `--dry-run`：引擎只出建议单、零改动，confirm 回调不会被调用。
/// - 实跑且 stdin 为 tty：逐条打印 rel + 推荐动作/置信度/理由，从 stdin 读 `[a]ccept/[k]eep-both/[s]kip`。
/// - 实跑且 stdin 非 tty：拒绝并提示（不自动执行）。
fn cmd_reconcile(
    paths: &Paths,
    name: &str,
    dry_run: bool,
    force: bool,
    rebuild: bool,
) -> std::io::Result<()> {
    reconcile_not_mounted_guard(discovery::is_mounted(&paths.mountpoint(name)))?;
    let confirm = build_confirm(dry_run, std::io::stdin().is_terminal())?;
    let opts = ReconcileOptions {
        dry_run,
        force,
        rebuild,
        confirm,
    };
    let report = reconcile(paths, name, opts, select_mounter().as_ref())?;
    print_reconcile_report(&report, dry_run);
    Ok(())
}

/// reconcile 前置守卫：项目挂载中则拒（挂载态读挂载点是 FUSE 视图而非 underlay，会误判）。
fn reconcile_not_mounted_guard(mounted: bool) -> std::io::Result<()> {
    if mounted {
        return Err(std::io::Error::other(
            "项目在挂载中，先卸载/待其停用后再 reconcile（挂载态读挂载点是 FUSE 视图而非 underlay，会误判）",
        ));
    }
    Ok(())
}

/// 据 `dry_run`/`is_tty` 构造逐条目裁决回调（策略 B）：
/// - `dry_run` → 占位回调（恒 `Skip`，引擎 dry 分支实际不调用它）。
/// - 实跑 + tty → 交互回调（逐条读 stdin）。
/// - 实跑 + 非 tty → `Err`（非交互绝不自动落盘）。
fn build_confirm(dry_run: bool, is_tty: bool) -> std::io::Result<Box<ConfirmFn>> {
    if dry_run {
        return Ok(Box::new(|_, _| Confirm::Skip));
    }
    if !is_tty {
        return Err(std::io::Error::other(
            "非交互（stdin 非 tty）且非 --dry-run：拒绝自动落盘（策略 B）。请在终端交互运行，或先用 --dry-run 查看建议单",
        ));
    }
    Ok(Box::new(interactive_confirm))
}

/// 交互裁决单条目：打印 rel + 推荐动作/置信度/理由，循环从 stdin 读直至合法 `a`/`k`/`s`。
/// EOF/读错 → `Skip`（策略 B 保守：宁可不动也不误落盘）。
fn interactive_confirm(rel: &str, rec: &Recommendation) -> Confirm {
    use std::io::Write;
    println!("\n条目: {rel}");
    println!(
        "  推荐: {:?}（置信度 {:?}）— {}",
        rec.action, rec.confidence, rec.rationale
    );
    loop {
        print!("  采纳？[a]ccept / [k]eep-both / [s]kip > ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => return Confirm::Skip,
            Ok(_) => match parse_confirm(&line) {
                Some(c) => return c,
                None => eprintln!("  无法识别（请输入 a/k/s）"),
            },
        }
    }
}

/// 把用户一行输入映射到 `Confirm`：取首个非空白字符、大小写不敏感。`a`→Accept、`k`→KeepBoth、
/// `s`→Skip；空行/无法识别 → `None`（调用方重问）。抽成纯函数便于单测（CLI 交互靠手验）。
fn parse_confirm(input: &str) -> Option<Confirm> {
    match input.trim().chars().next().map(|c| c.to_ascii_lowercase()) {
        Some('a') => Some(Confirm::Accept),
        Some('k') => Some(Confirm::KeepBoth),
        Some('s') => Some(Confirm::Skip),
        _ => None,
    }
}

/// 打印 `ReconcileReport`：逐条目 decision/action/notes + stash 目录 + 未完成条目的后续提示。
fn print_reconcile_report(report: &ReconcileReport, dry_run: bool) {
    if dry_run {
        println!("reconcile --dry-run 建议单（零改动）：");
    } else {
        println!("reconcile 报告：");
    }
    for e in &report.entries {
        println!("  {} [{}] {}", e.name, e.decision, e.action);
        for note in &e.notes {
            println!("      - {note}");
        }
    }
    println!("stash 目录: {}", report.stash_dir.display());
    // 未完成条目提示：underlay 保留 / deferred / 待人工者需人工核查后重试或手动处理。
    let needs_followup = report.entries.iter().any(|e| {
        e.action.contains("kept") || e.action.contains("deferred") || e.action.contains("warn")
    });
    if needs_followup {
        println!(
            "注意: 部分条目未完成（underlay 保留 / deferred）——需人工核查 stash/quarantine 后重跑 reconcile 或手动处理。"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::advisor::{Action, Confidence};

    #[test]
    fn parse_confirm_maps_first_char_case_insensitive() {
        assert_eq!(parse_confirm("a"), Some(Confirm::Accept));
        assert_eq!(parse_confirm("Accept\n"), Some(Confirm::Accept));
        assert_eq!(parse_confirm("  k "), Some(Confirm::KeepBoth));
        assert_eq!(parse_confirm("S"), Some(Confirm::Skip));
        assert_eq!(parse_confirm("skip\n"), Some(Confirm::Skip));
        assert_eq!(parse_confirm(""), None);
        assert_eq!(parse_confirm("x"), None);
    }

    #[test]
    fn not_mounted_guard_rejects_mounted_allows_unmounted() {
        assert!(reconcile_not_mounted_guard(true).is_err());
        assert!(reconcile_not_mounted_guard(false).is_ok());
    }

    #[test]
    fn build_confirm_rejects_non_tty_non_dry_run() {
        // 策略 B：非交互（非 tty）且非 dry-run → 拒绝，绝不自动落盘。
        assert!(build_confirm(false, false).is_err());
    }

    #[test]
    fn build_confirm_allows_dry_run_without_tty() {
        assert!(build_confirm(true, false).is_ok());
    }

    #[test]
    fn build_confirm_allows_interactive_tty() {
        assert!(build_confirm(false, true).is_ok());
    }

    #[test]
    fn dry_run_placeholder_confirm_never_applies() {
        // dry_run 占位回调恒 Skip（引擎 dry 分支实际不调它，即使调也不落盘）。
        let confirm = build_confirm(true, false).unwrap();
        let rec = Recommendation {
            action: Action::UnionIntoBase,
            confidence: Confidence::High,
            rationale: "x".into(),
        };
        assert_eq!(confirm("s.jsonl", &rec), Confirm::Skip);
    }
}
