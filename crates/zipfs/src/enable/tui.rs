//! 交互式 TUI（ratatui）：项目状态列表 + 选项 + 确认弹窗 + 批量；ingest 在 worker 线程跑。
//!
//! 键位（底部常驻）：↑/↓ 选择 · a 切换 · r 还原 · m 重挂 · A 全部切换 · E 全部还原 · o 选项 · g 刷新 · q 退出。
//! 活跃项目的切换需在弹窗键入 `APPLY` 放行（评审：默认拦截 + 显式放行）。
//! 挂载/灌入在后台线程执行，UI 不冻结；完成后自动刷新。

use std::io;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::enable::daemon::Mounter;
use crate::enable::discovery::{self, ProjectInfo};
use crate::enable::lifecycle;
use crate::enable::model::{Activity, ApplyOptions, Backend, Paths, ProjectStatus};
use crate::enable::systemd::select_mounter;

/// 块大小预设（选项弹窗循环）。
const CHUNK_PRESETS: [u32; 4] = [65536, 262144, 1048576, 4194304];
/// 等级预设。
const LEVEL_PRESETS: [i32; 4] = [1, 3, 9, 19];
/// 线程数预设（0=默认）。
const THREAD_PRESETS: [usize; 5] = [0, 2, 4, 8, 16];

/// 待执行动作（确认后交 worker 线程）。
#[derive(Clone)]
enum Pending {
    Apply { name: String, force: bool },
    Restore { name: String },
    Remount { name: String },
    ApplyAll,
    RestoreAll,
}

/// TUI 模式（弹窗状态）。
enum Mode {
    List,
    Options,
    Confirm { pending: Pending, prompt: String },
    TypeApply { name: String, buf: String },
    Working { what: String },
}

struct App {
    paths: Paths,
    items: Vec<ProjectInfo>,
    state: ListState,
    opts: ApplyOptions,
    mode: Mode,
    status: String,
    worker: Option<(JoinHandle<()>, Receiver<String>)>,
    quit: bool,
}

impl App {
    fn new(paths: Paths) -> io::Result<Self> {
        let items = discovery::scan(&paths)?;
        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(0));
        }
        // 选项起点 = 持久化默认（ZIPFS_HOME/config）。
        let opts = crate::enable::config::load_defaults(&paths);
        Ok(Self {
            paths,
            items,
            state,
            opts,
            mode: Mode::List,
            status: "就绪。a 切换 · r 还原 · m 重挂 · o 选项 · q 退出".into(),
            worker: None,
            quit: false,
        })
    }

    fn refresh(&mut self) {
        match discovery::scan(&self.paths) {
            Ok(items) => {
                let sel = self.state.selected().unwrap_or(0);
                self.items = items;
                if self.items.is_empty() {
                    self.state.select(None);
                } else {
                    self.state.select(Some(sel.min(self.items.len() - 1)));
                }
            }
            Err(e) => self.status = format!("刷新失败：{e}"),
        }
    }

    fn selected(&self) -> Option<&ProjectInfo> {
        self.state.selected().and_then(|i| self.items.get(i))
    }

    fn move_sel(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as i32;
        let n = self.items.len() as i32;
        let next = (cur + delta).rem_euclid(n);
        self.state.select(Some(next as usize));
    }

    /// 启动 worker 执行 pending（线程内跑真实 lifecycle）。
    fn spawn_worker(&mut self, pending: Pending) {
        let (tx, rx) = mpsc::channel();
        let paths = self.paths.clone();
        let opts = self.opts.clone();
        let what = describe(&pending);
        let handle = std::thread::spawn(move || {
            let m = select_mounter();
            let msg = run_pending(&paths, pending, opts, m.as_ref());
            let _ = tx.send(msg);
        });
        self.mode = Mode::Working { what: what.clone() };
        self.status = format!("执行中：{what}…");
        self.worker = Some((handle, rx));
    }

    /// 轮询 worker 完成。
    fn poll_worker(&mut self) {
        let done = if let Some((_, rx)) = &self.worker {
            rx.try_recv().ok()
        } else {
            None
        };
        if let Some(msg) = done {
            if let Some((h, _)) = self.worker.take() {
                let _ = h.join();
            }
            self.status = msg;
            self.mode = Mode::List;
            self.refresh();
        }
    }
}

/// pending 的人类描述。
fn describe(p: &Pending) -> String {
    match p {
        Pending::Apply { name, .. } => format!("切换 {name}"),
        Pending::Restore { name } => format!("还原 {name}"),
        Pending::Remount { name } => format!("重挂 {name}"),
        Pending::ApplyAll => "切换所有空闲 PLAIN".into(),
        Pending::RestoreAll => "还原所有 ZIPFS".into(),
    }
}

/// 在 worker 线程内执行 pending，返回结果摘要。
fn run_pending(paths: &Paths, p: Pending, opts: ApplyOptions, m: &dyn Mounter) -> String {
    match p {
        Pending::Apply { name, force } => match lifecycle::apply(paths, &name, opts, force, m) {
            Ok(o) => format!("✓ {name} 已切换（{} 文件 {:.2}x）", o.files, o.ratio()),
            Err(e) => format!("✗ {name} 切换失败：{e}"),
        },
        Pending::Restore { name } => match lifecycle::restore(paths, &name, m) {
            Ok(()) => format!("✓ {name} 已还原"),
            Err(e) => format!("✗ {name} 还原失败：{e}"),
        },
        Pending::Remount { name } => match lifecycle::remount(paths, &name, m) {
            Ok(()) => format!("✓ {name} 已重挂"),
            Err(e) => format!("✗ {name} 重挂失败：{e}"),
        },
        Pending::ApplyAll => {
            let mut ok = 0;
            let mut skip = 0;
            let mut fail = 0;
            let infos = discovery::scan(paths).unwrap_or_default();
            for info in infos {
                if info.status != ProjectStatus::Plain {
                    continue;
                }
                // 批量只挑空闲项目（活跃的跳过，绝不强挂）。
                if discovery::detect_activity(&paths.mountpoint(&info.name)).is_active() {
                    skip += 1;
                    continue;
                }
                match lifecycle::apply(paths, &info.name, opts.clone(), false, m) {
                    Ok(_) => ok += 1,
                    Err(_) => fail += 1,
                }
            }
            format!("批量切换：成功 {ok} 跳过活跃 {skip} 失败 {fail}")
        }
        Pending::RestoreAll => {
            let mut ok = 0;
            let mut fail = 0;
            let infos = discovery::scan(paths).unwrap_or_default();
            for info in infos {
                // 含 Broken：半灌/stale 项目恰恰最需还原（只要 orig 在，restore 即可修复）；
                // 与单项 restore 的可还原集合保持一致，绝不静默跳过最该处理的项目。
                if matches!(
                    info.status,
                    ProjectStatus::Active
                        | ProjectStatus::Stopped
                        | ProjectStatus::Broken
                        | ProjectStatus::Hung
                ) {
                    match lifecycle::restore(paths, &info.name, m) {
                        Ok(()) => ok += 1,
                        Err(_) => fail += 1,
                    }
                }
            }
            format!("批量还原：成功 {ok} 失败 {fail}")
        }
    }
}

/// 启动 TUI 主循环。
pub fn run(paths: &Paths) -> io::Result<()> {
    let mut app = App::new(paths.clone())?;
    let mut terminal = ratatui::init();
    let res = run_loop(&mut terminal, &mut app);
    ratatui::restore();
    res
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    while !app.quit {
        terminal.draw(|f| draw(f, app))?;
        app.poll_worker();
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(app, key.code);
                }
            }
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, code: KeyCode) {
    // Working 模式下只接受退出（worker 在跑，禁止新动作）。
    if let Mode::Working { .. } = app.mode {
        return;
    }
    match &mut app.mode {
        Mode::List => handle_list_key(app, code),
        Mode::Options => handle_options_key(app, code),
        Mode::Confirm { .. } => handle_confirm_key(app, code),
        Mode::TypeApply { .. } => handle_type_apply_key(app, code),
        Mode::Working { .. } => {}
    }
}

fn handle_list_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
        KeyCode::Char('g') => app.refresh(),
        KeyCode::Char('o') => app.mode = Mode::Options,
        KeyCode::Char('a') => start_apply(app),
        KeyCode::Char('r') => start_restore(app),
        KeyCode::Char('m') => start_remount(app),
        KeyCode::Char('A') => {
            app.mode = Mode::Confirm {
                pending: Pending::ApplyAll,
                prompt: "切换所有空闲 PLAIN 项目？(y/n)".into(),
            }
        }
        KeyCode::Char('E') => {
            app.mode = Mode::Confirm {
                pending: Pending::RestoreAll,
                prompt: "还原所有 ZIPFS/STOPPED 项目？(y/n)".into(),
            }
        }
        _ => {}
    }
}

fn start_apply(app: &mut App) {
    let Some(info) = app.selected().cloned() else {
        return;
    };
    if info.status != ProjectStatus::Plain {
        app.status = format!("{} 非 PLAIN，无法切换", info.name);
        return;
    }
    let mp = app.paths.mountpoint(&info.name);
    match discovery::detect_activity(&mp) {
        Activity::Active(reason) => {
            app.status = format!("⚠ {} 活跃（{reason}）：键入 APPLY 放行", info.name);
            app.mode = Mode::TypeApply {
                name: info.name,
                buf: String::new(),
            };
        }
        Activity::Idle => {
            app.mode = Mode::Confirm {
                pending: Pending::Apply {
                    name: info.name.clone(),
                    force: false,
                },
                prompt: format!("切换 {}？(y/n)", info.name),
            };
        }
    }
}

fn start_restore(app: &mut App) {
    let Some(info) = app.selected().cloned() else {
        return;
    };
    if !matches!(
        info.status,
        ProjectStatus::Active
            | ProjectStatus::Stopped
            | ProjectStatus::Broken
            | ProjectStatus::Hung
    ) {
        app.status = format!("{} 未切换，无需还原", info.name);
        return;
    }
    app.mode = Mode::Confirm {
        pending: Pending::Restore {
            name: info.name.clone(),
        },
        prompt: format!("还原 {}？(y/n)", info.name),
    };
}

fn start_remount(app: &mut App) {
    let Some(info) = app.selected().cloned() else {
        return;
    };
    if info.status != ProjectStatus::Stopped {
        app.status = format!("{} 非 STOPPED，无需重挂", info.name);
        return;
    }
    app.spawn_worker(Pending::Remount { name: info.name });
}

fn handle_options_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc | KeyCode::Char('o') | KeyCode::Enter => app.mode = Mode::List,
        KeyCode::Char('b') => {
            app.opts.backend = match app.opts.backend {
                Backend::Shadow => Backend::Container,
                Backend::Container => Backend::Shadow,
            }
        }
        KeyCode::Char('c') => app.opts.chunk_size = cycle(&CHUNK_PRESETS, app.opts.chunk_size),
        KeyCode::Char('l') => app.opts.level = cycle(&LEVEL_PRESETS, app.opts.level),
        KeyCode::Char('t') => app.opts.threads = cycle(&THREAD_PRESETS, app.opts.threads),
        KeyCode::Char('w') => app.opts.writeback = !app.opts.writeback,
        _ => {}
    }
}

fn handle_confirm_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Mode::Confirm { pending, .. } = &app.mode {
                let p = pending.clone();
                app.spawn_worker(p);
            }
        }
        _ => app.mode = Mode::List,
    }
}

fn handle_type_apply_key(app: &mut App, code: KeyCode) {
    if let Mode::TypeApply { name, buf } = &mut app.mode {
        match code {
            KeyCode::Esc => app.mode = Mode::List,
            KeyCode::Enter => {
                if buf == "APPLY" {
                    let name = name.clone();
                    app.spawn_worker(Pending::Apply { name, force: true });
                } else {
                    app.status = "未键入 APPLY，已取消".into();
                    app.mode = Mode::List;
                }
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => buf.push(c),
            _ => {}
        }
    }
}

/// 在预设数组里循环到下一个值。
fn cycle<T: Copy + PartialEq>(presets: &[T], cur: T) -> T {
    let idx = presets.iter().position(|&v| v == cur).unwrap_or(0);
    presets[(idx + 1) % presets.len()]
}

// ── 渲染 ─────────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(3),
        ])
        .split(f.area());

    draw_list(f, app, chunks[0]);
    draw_status(f, app, chunks[1]);
    draw_footer(f, chunks[2]);

    match &app.mode {
        Mode::Options => draw_options(f, app),
        Mode::Confirm { prompt, .. } => draw_popup(f, "确认", prompt, ""),
        Mode::TypeApply { buf, .. } => {
            draw_popup(f, "活跃项目放行", "键入 APPLY 后回车放行，Esc 取消：", buf)
        }
        Mode::Working { what } => draw_popup(f, "执行中", what, "请稍候…（不要退出）"),
        Mode::List => {}
    }
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .items
        .iter()
        .map(|info| {
            let color = match info.status {
                ProjectStatus::Plain => Color::Gray,
                ProjectStatus::Active => Color::Green,
                ProjectStatus::Stopped => Color::Yellow,
                ProjectStatus::Broken => Color::Red,
                ProjectStatus::Hung => Color::Magenta, // 卡死：与 Broken(红,僵尸/半灌) 视觉区分
            };
            let ratio = info
                .meta
                .as_ref()
                .filter(|_| info.status != ProjectStatus::Plain)
                .map(|m| format!("{:.2}x", m.ratio()))
                .unwrap_or_default();
            let line = Line::from(vec![
                Span::styled(
                    format!("{:<8}", info.status.label()),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{:<48} {:>8}", info.name, ratio)),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" zipfs · Claude projects "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = app.state;
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let threads = if app.opts.threads == 0 {
        "auto".to_string()
    } else {
        app.opts.threads.to_string()
    };
    let opt = format!(
        " 选项: backend={} chunk={}KiB level={} threads={} wb={} ",
        app.opts.backend.flag(),
        app.opts.chunk_size / 1024,
        app.opts.level,
        threads,
        if app.opts.writeback { "on" } else { "off" },
    );
    let p = Paragraph::new(Line::from(vec![
        Span::styled(opt, Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {}", app.status)),
    ]));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let help = "↑/↓ 选择 · a 切换 · r 还原 · m 重挂 · A 全切 · E 全还 · o 选项 · g 刷新 · q 退出";
    let p = Paragraph::new(help)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::TOP));
    f.render_widget(p, area);
}

fn draw_options(f: &mut Frame, app: &App) {
    let threads = if app.opts.threads == 0 {
        "auto".to_string()
    } else {
        app.opts.threads.to_string()
    };
    let dict = app
        .opts
        .dict
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "（无，用 CLI --dict 设置）".into());
    let body = format!(
        "b: 后端 = {}\nc: 块大小 = {}KiB\nl: 等级 = {}\nt: 线程 = {}\nw: 写回缓存 = {}\n   字典 = {}\n\nEsc/o/Enter 关闭",
        app.opts.backend.flag(),
        app.opts.chunk_size / 1024,
        app.opts.level,
        threads,
        if app.opts.writeback { "on" } else { "off" },
        dict,
    );
    draw_popup(f, "apply 选项", &body, "");
}

/// 居中弹窗。
fn draw_popup(f: &mut Frame, title: &str, body: &str, input: &str) {
    let area = centered_rect(60, 30, f.area());
    f.render_widget(Clear, area);
    let mut text = body.to_string();
    if !input.is_empty() || title.contains("放行") {
        text.push_str(&format!("\n\n> {input}"));
    }
    let p = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {title} "))
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(p, area);
}

/// 计算居中矩形（百分比）。
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vert[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_wraps() {
        assert_eq!(cycle(&CHUNK_PRESETS, 65536), 262144);
        assert_eq!(cycle(&CHUNK_PRESETS, 4194304), 65536);
        assert_eq!(cycle(&LEVEL_PRESETS, 19), 1);
    }
}
