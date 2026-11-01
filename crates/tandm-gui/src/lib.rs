//! The tandm window: machines as columns, repos as rows, one sync-state cell each.
//!
//! This machine's status is read in-process; peers answer `tandm status --json` over ssh. Actions
//! run `tandm <args>` on the target machine (this binary locally, ssh for peers), so the GUI and the
//! CLI can never disagree about what an action does.

mod theme;

use std::collections::{BTreeSet, HashSet};
use std::process::Command;
use std::time::Duration;

use gpui::{
    App, Application, Bounds, ClickEvent, Context, FocusHandle, FontWeight, KeyBinding,
    KeyDownEvent, Menu, MenuItem, SharedString, Stateful, Window, WindowBounds, WindowOptions,
    actions, div, prelude::*, px, size,
};

use tandm_core::config::{Config, MachineConfig};
use tandm_core::remote;
use tandm_core::state::StateFiles;
use tandm_core::status::{self, CellState, MachineStatus, RepoStatus};
use tandm_core::{paths, util};

use theme::{Palette, alpha};

actions!(tandm, [Quit, Refresh, CloseOverlay]);

const POLL: Duration = Duration::from_secs(30);
const REPO_COL_W: f32 = 190.;
const CELL_MIN_W: f32 = 230.;

/// gpui 0.2.2 renders through Vulkan on Linux and dies with an opaque GPU error without a driver;
/// say what's missing instead.
#[cfg(target_os = "linux")]
fn check_vulkan() -> anyhow::Result<()> {
    if std::env::var_os("VK_ICD_FILENAMES").is_some() || std::env::var_os("VK_DRIVER_FILES").is_some() {
        return Ok(());
    }
    let mut dirs: Vec<std::path::PathBuf> = ["/usr/share", "/usr/local/share", "/etc"]
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    if let Some(x) = std::env::var_os("XDG_DATA_DIRS") {
        dirs.extend(std::env::split_paths(&x));
    }
    dirs.push(paths::home_dir().join(".local/share"));
    let found = dirs.iter().any(|d| {
        std::fs::read_dir(d.join("vulkan/icd.d"))
            .map(|mut it| it.any(|e| e.is_ok()))
            .unwrap_or(false)
    });
    if found {
        Ok(())
    } else {
        anyhow::bail!(
            "no Vulkan driver found (no vulkan/icd.d/*.json); the GUI needs one on Linux. \
             On Arch: `pacman -S vulkan-intel` / `vulkan-radeon` (or `vulkan-swrast` for software \
             rendering). The CLI and service work without it: try `tandm status --all`."
        )
    }
}

pub fn run() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    check_vulkan()?;
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("cmd-r", Refresh, None),
            KeyBinding::new("ctrl-r", Refresh, None),
            KeyBinding::new("f5", Refresh, None),
            KeyBinding::new("escape", CloseOverlay, None),
        ]);
        cx.set_menus(vec![Menu {
            name: "tandm".into(),
            items: vec![
                MenuItem::action("Refresh", Refresh),
                MenuItem::separator(),
                MenuItem::action("Quit tandm", Quit),
            ],
        }]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1000.), px(680.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("tandm".into()),
                    ..Default::default()
                }),
                app_id: Some("tandm".into()),
                window_min_size: Some(size(px(560.), px(360.))),
                ..Default::default()
            },
            |window, cx| {
                // TitlebarOptions.title doesn't reach the Wayland toplevel in gpui 0.2.2.
                window.set_window_title("tandm");
                let view = cx.new(|cx| TandmView::new(window, cx));
                let focus = view.read(cx).root_focus.clone();
                window.focus(&focus);
                view
            },
        );
        if let Err(e) = opened {
            eprintln!("tandm: could not open a window: {e:#}");
            cx.quit();
        }
        cx.activate(true);
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Model

#[derive(Clone)]
enum Target {
    Local,
    Peer(MachineConfig),
}

enum ColStatus {
    Loading,
    Ok(MachineStatus),
    Unreachable(String),
}

struct Column {
    title: String,
    target: Target,
    status: ColStatus,
}

/// What one background refresh gathers.
struct Snapshot {
    cfg: Config,
    local: MachineStatus,
    peers: Vec<(MachineConfig, Result<MachineStatus, String>)>,
    at: i64,
}

fn gather() -> Result<Snapshot, String> {
    let cfg = Config::load().map_err(|e| format!("{e:#}"))?;
    let state_dir = paths::state_dir();
    let (local, peers) = std::thread::scope(|s| {
        let peers = s.spawn(|| remote::all_status(&cfg.machines));
        let local = status::local_status(&cfg, &state_dir);
        (local, peers.join().unwrap_or_default())
    });
    let peers = peers
        .into_iter()
        .map(|(m, r)| (m, r.map_err(|e| format!("{e:#}"))))
        .collect();
    Ok(Snapshot {
        cfg,
        local,
        peers,
        at: util::now(),
    })
}

/// Run `tandm <args>` on a machine and return its output.
fn exec(target: &Target, args: &[String]) -> Result<String, String> {
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match target {
        Target::Local => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let mut c = Command::new(exe);
            c.args(&argv);
            let out = util::run_with_timeout(c, remote::ACTION_TIMEOUT).map_err(|e| format!("{e:#}"))?;
            let text = util::excerpt(&format!("{}{}", out.stdout, out.stderr), 6);
            if out.success() { Ok(text) } else { Err(text) }
        }
        Target::Peer(m) => remote::run(m, &argv, remote::ACTION_TIMEOUT)
            .map(|s| util::excerpt(&s, 6))
            .map_err(|e| format!("{e:#}")),
    }
}

fn fetch_log(target: &Target, repo: &str) -> Result<Vec<String>, String> {
    match target {
        Target::Local => Ok(StateFiles::new(&paths::state_dir(), repo).log_tail(40)),
        Target::Peer(m) => {
            let out = remote::run(m, &["log", repo, "-n", "40", "--json"], remote::STATUS_TIMEOUT)
                .map_err(|e| format!("{e:#}"))?;
            serde_json::from_str(&out).map_err(|e| format!("bad log JSON: {e}"))
        }
    }
}

/// `/home/u/projects/x` or `/Users/u/projects/x` → `~/projects/x`, to suggest the same checkout on
/// another machine.
fn suggest_path(p: &str) -> String {
    for prefix in ["/home/", "/Users/"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            if let Some((_user, tail)) = rest.split_once('/') {
                return format!("~/{tail}");
            }
        }
    }
    paths::contract_tilde(p)
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CellKey {
    col: usize,
    repo: String,
}

struct AddForm {
    col: usize,
    /// Set when adding an existing row's repo on another machine, so the row name matches.
    name: Option<String>,
    input: String,
}

struct LogView {
    key: CellKey,
    lines: Option<Result<Vec<String>, String>>,
}

pub struct TandmView {
    columns: Vec<Column>,
    config_error: Option<String>,
    last_refresh: Option<i64>,
    refreshing: bool,
    selected: Option<CellKey>,
    log: Option<LogView>,
    busy: HashSet<CellKey>,
    flash: Option<(bool, String)>,
    add: Option<AddForm>,
    confirm_remove: Option<CellKey>,
    root_focus: FocusHandle,
    input_focus: FocusHandle,
    mono: SharedString,
}

impl TandmView {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let fonts = cx.text_system().all_font_names();
        let mono = ["Menlo", "SF Mono", "JetBrainsMono Nerd Font", "Adwaita Mono", "DejaVu Sans Mono", "Liberation Mono"]
            .into_iter()
            .find(|f| fonts.iter().any(|n| n == f))
            .unwrap_or("monospace");

        // Poll forever; the task ends when the view is dropped.
        cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |v, cx| v.refresh(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(POLL).await;
            }
        })
        .detach();

        let cfg = Config::load().unwrap_or_default();
        let mut view = TandmView {
            columns: Vec::new(),
            config_error: None,
            last_refresh: None,
            refreshing: false,
            selected: None,
            log: None,
            busy: HashSet::new(),
            flash: None,
            add: None,
            confirm_remove: None,
            root_focus: cx.focus_handle(),
            input_focus: cx.focus_handle(),
            mono: mono.into(),
        };
        view.set_columns(&cfg);
        view
    }

    fn set_columns(&mut self, cfg: &Config) {
        let mut cols = vec![Column {
            title: cfg.machine_name(),
            target: Target::Local,
            status: ColStatus::Loading,
        }];
        cols.extend(cfg.machines.iter().map(|m| Column {
            title: m.name.clone(),
            target: Target::Peer(m.clone()),
            status: ColStatus::Loading,
        }));
        // Keep what we already know about columns that still exist.
        for c in &mut cols {
            if let Some(old) = self.columns.iter_mut().find(|o| o.title == c.title) {
                c.status = std::mem::replace(&mut old.status, ColStatus::Loading);
            }
        }
        self.columns = cols;
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let snap = cx.background_executor().spawn(async move { gather() }).await;
            this.update(cx, |v, cx| {
                v.refreshing = false;
                match snap {
                    Ok(s) => v.apply(s),
                    Err(e) => v.config_error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply(&mut self, s: Snapshot) {
        self.config_error = None;
        self.set_columns(&s.cfg);
        self.columns[0].status = ColStatus::Ok(s.local);
        for (m, r) in s.peers {
            if let Some(c) = self.columns.iter_mut().find(|c| c.title == m.name) {
                c.status = match r {
                    Ok(ms) => ColStatus::Ok(ms),
                    Err(e) => ColStatus::Unreachable(e),
                };
            }
        }
        self.last_refresh = Some(s.at);
        if let Some(sel) = &self.selected {
            if sel.col >= self.columns.len() {
                self.selected = None;
            }
        }
    }

    fn rows(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        for c in &self.columns {
            if let ColStatus::Ok(ms) = &c.status {
                names.extend(ms.repos.iter().map(|r| r.name.clone()));
            }
        }
        names.into_iter().collect()
    }

    fn repo_at(&self, col: usize, repo: &str) -> Option<&RepoStatus> {
        match &self.columns.get(col)?.status {
            ColStatus::Ok(ms) => ms.repos.iter().find(|r| r.name == repo),
            _ => None,
        }
    }

    fn now_for(&self, col: usize) -> i64 {
        match self.columns.get(col).map(|c| &c.status) {
            Some(ColStatus::Ok(ms)) if ms.now > 0 => ms.now,
            _ => util::now(),
        }
    }

    fn cell_state(&self, col: usize, repo: &str) -> CellState {
        match &self.columns[col].status {
            ColStatus::Loading => CellState::Unknown,
            ColStatus::Unreachable(e) => CellState::Unreachable(e.clone()),
            ColStatus::Ok(ms) => match ms.repos.iter().find(|r| r.name == repo) {
                Some(r) => CellState::of(r),
                None => CellState::NotAdded,
            },
        }
    }

    // --- actions -----------------------------------------------------------------------------

    fn select(&mut self, key: CellKey, cx: &mut Context<Self>) {
        self.confirm_remove = None;
        if self.selected.as_ref() == Some(&key) {
            self.selected = None;
            self.log = None;
            self.add = None;
            return;
        }
        self.selected = Some(key.clone());
        self.add = None;
        self.log = None;
        if self.repo_at(key.col, &key.repo).is_some() {
            self.load_log(key, cx);
        } else if matches!(self.columns[key.col].status, ColStatus::Ok(_)) {
            // Not on that machine yet: offer to add it, path guessed from another machine.
            let guess = (0..self.columns.len())
                .filter_map(|c| self.repo_at(c, &key.repo))
                .map(|r| suggest_path(&r.path))
                .next()
                .unwrap_or_default();
            self.add = Some(AddForm {
                col: key.col,
                name: Some(key.repo.clone()),
                input: guess,
            });
        }
    }

    fn load_log(&mut self, key: CellKey, cx: &mut Context<Self>) {
        let target = self.columns[key.col].target.clone();
        self.log = Some(LogView {
            key: key.clone(),
            lines: None,
        });
        cx.spawn(async move |this, cx| {
            let repo = key.repo.clone();
            let lines = cx
                .background_executor()
                .spawn(async move { fetch_log(&target, &repo) })
                .await;
            this.update(cx, |v, cx| {
                if let Some(l) = &mut v.log {
                    if l.key == key {
                        l.lines = Some(lines);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn act(&mut self, key: CellKey, args: Vec<String>, cx: &mut Context<Self>) {
        let target = self.columns[key.col].target.clone();
        let what = format!("{} on {}", args.join(" "), self.columns[key.col].title);
        self.busy.insert(key.clone());
        self.flash = Some((true, format!("running: tandm {what}…")));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { exec(&target, &args) })
                .await;
            this.update(cx, |v, cx| {
                v.busy.remove(&key);
                v.flash = Some(match res {
                    Ok(out) if out.is_empty() => (true, format!("done: tandm {what}")),
                    Ok(out) => (true, out),
                    Err(e) => (false, format!("tandm {what} failed: {e}")),
                });
                if v.selected.as_ref() == Some(&key) && v.repo_at(key.col, &key.repo).is_some() {
                    v.load_log(key.clone(), cx);
                }
                v.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn open_add(&mut self, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.add = Some(AddForm {
            col,
            name: None,
            input: String::new(),
        });
        self.selected = None;
        self.log = None;
        window.focus(&self.input_focus);
        cx.notify();
    }

    fn submit_add(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.add.take() else { return };
        let path = form.input.trim().to_string();
        if path.is_empty() {
            self.add = Some(form);
            return;
        }
        let mut args = vec!["add".to_string(), path];
        let repo = match &form.name {
            Some(n) => {
                args.push("--name".into());
                args.push(n.clone());
                n.clone()
            }
            None => String::new(),
        };
        self.act(CellKey { col: form.col, repo }, args, cx);
    }

    fn on_input_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.add else { return };
        let k = &ev.keystroke;
        let cmd = k.modifiers.platform || k.modifiers.control;
        match k.key.as_str() {
            "enter" => {
                self.submit_add(cx);
            }
            "escape" => self.add = None,
            "backspace" if cmd => form.input.clear(),
            "backspace" => {
                form.input.pop();
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    form.input.push_str(t.lines().next().unwrap_or("").trim());
                }
            }
            "u" if k.modifiers.control => form.input.clear(),
            _ => {
                if !cmd {
                    if let Some(ch) = &k.key_char {
                        form.input.push_str(ch);
                    }
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering

fn button(id: impl Into<SharedString>, label: impl Into<SharedString>, p: &Palette) -> Stateful<gpui::Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .px_2()
        .py(px(3.))
        .rounded_md()
        .border_1()
        .border_color(p.border)
        .bg(p.surface2)
        .text_sm()
        .text_color(p.text)
        .cursor_pointer()
        .hover(|s| s.border_color(p.accent))
        .child(label.into())
}

fn pill(label: String, color: gpui::Hsla) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py(px(1.))
        .rounded_full()
        .bg(alpha(color, 0.16))
        .text_color(color)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .max_w_full()
        .overflow_hidden()
        .child(div().size(px(6.)).rounded_full().bg(color).flex_shrink_0())
        .child(div().truncate().child(label))
}

impl TandmView {
    fn render_toolbar(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let status = if self.refreshing {
            "refreshing…".to_string()
        } else if let Some(at) = self.last_refresh {
            format!("updated {}", status::ago(at, util::now()))
        } else {
            "loading…".into()
        };
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(p.border)
            .bg(p.surface)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .text_color(p.text)
                    .child("tandm"),
            )
            .child(div().text_sm().text_color(p.muted).child(status))
            .child(div().flex_1())
            .child(
                button("refresh", "Refresh", p)
                    .on_click(cx.listener(|v, _: &ClickEvent, _, cx| v.refresh(cx))),
            )
            .child(
                button("add-repo", "Add repo…", p).on_click(cx.listener(
                    |v, _: &ClickEvent, window, cx| v.open_add(0, window, cx),
                )),
            )
            .into_any_element()
    }

    fn render_header(&self, p: &Palette) -> gpui::AnyElement {
        div()
            .flex()
            .border_b_1()
            .border_color(p.border)
            .bg(p.surface)
            .child(
                div()
                    .w(px(REPO_COL_W))
                    .flex_shrink_0()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(p.muted)
                    .child("REPO  ╲  MACHINE"),
            )
            .children(self.columns.iter().enumerate().map(|(i, c)| {
                let (sub, color) = match &c.status {
                    ColStatus::Loading => ("loading…".to_string(), p.muted),
                    ColStatus::Unreachable(e) if e.starts_with("unreachable") => (e.clone(), p.bad),
                    ColStatus::Unreachable(e) => (format!("unreachable: {e}"), p.bad),
                    ColStatus::Ok(ms) => {
                        if ms.service.active {
                            (format!("{} · tandm {} · service on", ms.os, ms.tandm_version), p.muted)
                        } else {
                            (format!("service not running ({})", ms.service.detail), p.warn)
                        }
                    }
                };
                div()
                    .flex_1()
                    .min_w(px(CELL_MIN_W))
                    .px_3()
                    .py_2()
                    .border_l_1()
                    .border_color(p.border)
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_baseline()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text)
                                    .child(c.title.clone()),
                            )
                            .when(i == 0, |d| {
                                d.child(div().text_xs().text_color(p.muted).child("this machine"))
                            }),
                    )
                    .child(div().text_xs().text_color(color).truncate().child(sub))
            }))
            .into_any_element()
    }

    fn render_cell(&self, col: usize, repo: &str, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let key = CellKey {
            col,
            repo: repo.to_string(),
        };
        let selected = self.selected.as_ref() == Some(&key);
        let busy = self.busy.contains(&key);
        let state = self.cell_state(col, repo);
        let tone = p.tone(state.tone());
        let r = self.repo_at(col, repo);
        let now = self.now_for(col);

        let mut lines: Vec<gpui::AnyElement> = Vec::new();
        let label = if busy {
            "working…".to_string()
        } else {
            state.label()
        };
        let mut top = div().flex().items_center().gap_2().child(pill(label, if busy { p.accent } else { tone }));
        if let Some(r) = r {
            if let Some(l) = &r.live {
                if l.dirty > 0 {
                    top = top.child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .flex_shrink_0()
                            .child(format!("dirty {}", l.dirty)),
                    );
                }
            }
        }
        lines.push(top.into_any_element());

        match (r, &state) {
            (Some(r), _) => {
                if let Some(l) = &r.live {
                    let matches = l.head_matches_upstream();
                    lines.push(
                        div()
                            .flex()
                            .gap_1()
                            .text_xs()
                            .font_family(self.mono.clone())
                            .child(div().text_color(p.text).child(status::short_sha(&l.head)))
                            .child(
                                div()
                                    .text_color(if matches { p.good } else { p.warn })
                                    .child(if matches { "=" } else { "≠" }),
                            )
                            .child(div().text_color(p.muted).child(format!("{}/{}", r.remote, r.branch)))
                            .into_any_element(),
                    );
                }
                let when = match &r.last_run {
                    Some(run) => format!("synced {}", status::ago(run.at, now)),
                    None => "never synced".into(),
                };
                lines.push(div().text_xs().text_color(p.muted).child(when).into_any_element());
            }
            (None, CellState::NotAdded) => {
                lines.push(
                    div()
                        .text_xs()
                        .text_color(p.muted)
                        .child("click to add here")
                        .into_any_element(),
                );
            }
            _ => {}
        }

        div()
            .id(SharedString::from(format!("cell-{col}-{repo}")))
            .flex_1()
            .min_w(px(CELL_MIN_W))
            .flex()
            .flex_col()
            .gap(px(3.))
            .px_3()
            .py_2()
            .border_l_1()
            .border_color(p.border)
            .overflow_hidden()
            .cursor_pointer()
            .when(selected, |d| d.bg(alpha(p.selected, 0.12)))
            .hover(|s| s.bg(alpha(p.selected, 0.07)))
            .on_click(cx.listener(move |v, _: &ClickEvent, window, cx| {
                v.select(key.clone(), cx);
                if v.add.is_some() {
                    window.focus(&v.input_focus);
                }
                cx.notify();
            }))
            .children(lines)
            .into_any_element()
    }

    fn render_grid(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let rows = self.rows();
        let mut grid = div()
            .id("grid")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll();
        if rows.is_empty() {
            let msg = if self.columns.iter().all(|c| matches!(c.status, ColStatus::Loading)) {
                "loading…"
            } else {
                "No repos yet. Use “Add repo…” or `tandm add <path>`."
            };
            grid = grid.child(div().p_4().text_sm().text_color(p.muted).child(msg));
        }
        for (i, name) in rows.iter().enumerate() {
            grid = grid.child(
                div()
                    .flex()
                    .border_b_1()
                    .border_color(p.border)
                    .when(i % 2 == 1, |d| d.bg(alpha(p.surface2, 0.35)))
                    .child(
                        div()
                            .w(px(REPO_COL_W))
                            .flex_shrink_0()
                            .px_3()
                            .py_2()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text)
                            .truncate()
                            .child(name.clone()),
                    )
                    .children((0..self.columns.len()).map(|c| self.render_cell(c, name, p, cx))),
            );
        }
        grid.into_any_element()
    }

    fn render_add_form(&self, form: &AddForm, window: &Window, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let focused = self.input_focus.is_focused(window);
        let title = match &form.name {
            Some(n) => format!("Add {n} on {}", self.columns[form.col].title),
            None => "Add a repo".to_string(),
        };
        let mut machines = div().flex().gap_2().items_center().child(div().text_sm().text_color(p.muted).child("on"));
        if form.name.is_none() {
            for (i, c) in self.columns.iter().enumerate() {
                let active = i == form.col;
                machines = machines.child(
                    button(format!("add-col-{i}"), c.title.clone(), p)
                        .when(active, |d| d.border_color(p.accent).text_color(p.accent))
                        .on_click(cx.listener(move |v, _: &ClickEvent, window, cx| {
                            if let Some(f) = &mut v.add {
                                f.col = i;
                            }
                            window.focus(&v.input_focus);
                            cx.notify();
                        })),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title))
            .when(form.name.is_none(), |d| d.child(machines))
            .child(
                div()
                    .id("path-input")
                    .track_focus(&self.input_focus)
                    .on_key_down(cx.listener(Self::on_input_key))
                    .on_click(cx.listener(|v, _: &ClickEvent, window, cx| {
                        window.focus(&v.input_focus);
                        cx.notify();
                    }))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(if focused { p.accent } else { p.border })
                    .bg(p.bg)
                    .font_family(self.mono.clone())
                    .text_sm()
                    .text_color(p.text)
                    .cursor_text()
                    .child(if form.input.is_empty() && !focused {
                        div().text_color(p.muted).child("path, e.g. ~/projects/my-repo  (click to type)")
                    } else {
                        div().child(format!("{}{}", form.input, if focused { "▏" } else { "" }))
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button("add-submit", "Add", p).on_click(cx.listener(|v, _: &ClickEvent, _, cx| v.submit_add(cx))))
                    .child(button("add-cancel", "Cancel", p).on_click(cx.listener(|v, _: &ClickEvent, _, cx| {
                        v.add = None;
                        cx.notify();
                    })))
                    .child(div().text_xs().text_color(p.muted).child("Enter to add · Esc to cancel · ⌘/Ctrl-V pastes")),
            )
            .into_any_element()
    }

    fn render_details(&self, window: &Window, p: &Palette, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if let Some(form) = &self.add {
            return Some(self.render_add_form(form, window, p, cx));
        }
        let key = self.selected.clone()?;
        let col = &self.columns[key.col];
        let state = self.cell_state(key.col, &key.repo);
        let mut d = div().flex().flex_col().gap_1();
        d = d.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .child(format!("{} on {}", key.repo, col.title)),
                )
                .child(pill(state.label(), p.tone(state.tone()))),
        );
        let kv = |k: &str, v: String| {
            div()
                .flex()
                .gap_2()
                .text_xs()
                .child(div().w(px(110.)).flex_shrink_0().text_color(p.muted).child(k.to_string()))
                .child(div().text_color(p.text).child(v))
        };
        match (&col.status, self.repo_at(key.col, &key.repo)) {
            (ColStatus::Unreachable(e), _) => {
                d = d.child(kv("ssh", e.clone()));
            }
            (_, Some(r)) => {
                let now = self.now_for(key.col);
                d = d.child(kv("path", r.path.clone()));
                if let Some(l) = &r.live {
                    d = d.child(kv(
                        "HEAD",
                        format!(
                            "{} ({})",
                            l.head.clone().unwrap_or_else(|| "–".into()),
                            l.branch.clone().unwrap_or_else(|| "detached".into())
                        ),
                    ));
                    d = d.child(kv(
                        &format!("{}/{}", r.remote, r.branch),
                        format!(
                            "{}  ·  ahead {} · behind {} · dirty {}{}",
                            l.upstream.clone().unwrap_or_else(|| "–".into()),
                            l.ahead,
                            l.behind,
                            l.dirty,
                            if l.head_matches_upstream() { "  ·  HEAD matches" } else { "" }
                        ),
                    ));
                }
                if let Some(e) = &r.live_error {
                    d = d.child(kv("error", e.clone()));
                }
                if let Some(run) = &r.last_run {
                    d = d.child(kv("last run", format!("{} — {}", status::ago(run.at, now), run.message)));
                    if !run.overlap.is_empty() {
                        d = d.child(kv("overlap", run.overlap.join("  ")));
                    }
                    if !run.secret_hits.is_empty() {
                        let hits: Vec<String> = run.secret_hits.iter().map(|h| h.to_string()).collect();
                        d = d.child(kv("secret-looking", hits.join("  ")));
                    }
                    if let Some(det) = &run.detail {
                        d = d.child(kv("git said", det.clone()));
                    }
                }
                if let (Some(at), Some(what)) = (r.last_change_at, &r.last_change) {
                    d = d.child(kv("last change", format!("{} — {what}", status::ago(at, now))));
                }

                let paused = r.paused;
                let busy = self.busy.contains(&key);
                let confirming = self.confirm_remove.as_ref() == Some(&key);
                let (k1, k2, k3) = (key.clone(), key.clone(), key.clone());
                let name = key.repo.clone();
                let mut actions = div().flex().gap_2().pt_1();
                if !paused {
                    actions = actions.child(button("act-sync", "Sync now", p).when(busy, |b| b.opacity(0.5)).on_click(cx.listener(
                        move |v, _: &ClickEvent, _, cx| {
                            if !v.busy.contains(&k1) {
                                v.act(k1.clone(), vec!["sync".into(), k1.repo.clone()], cx);
                            }
                        },
                    )));
                }
                actions = actions.child(
                    button("act-pause", if paused { "Resume" } else { "Pause" }, p).on_click(cx.listener(
                        move |v, _: &ClickEvent, _, cx| {
                            let verb = if paused { "resume" } else { "pause" };
                            v.act(k2.clone(), vec![verb.into(), k2.repo.clone()], cx);
                        },
                    )),
                );
                actions = actions.child(
                    button("act-rm", if confirming { "Click again to remove" } else { "Remove from this machine" }, p)
                        .when(confirming, |b| b.border_color(p.bad).text_color(p.bad))
                        .on_click(cx.listener(move |v, _: &ClickEvent, _, cx| {
                            if v.confirm_remove.as_ref() == Some(&k3) {
                                v.confirm_remove = None;
                                v.selected = None;
                                v.log = None;
                                v.act(k3.clone(), vec!["rm".into(), name.clone()], cx);
                            } else {
                                v.confirm_remove = Some(k3.clone());
                                cx.notify();
                            }
                        })),
                );
                d = d.child(actions);

                let log: gpui::AnyElement = match self.log.as_ref().filter(|l| l.key == key).and_then(|l| l.lines.as_ref()) {
                    None => div().text_xs().text_color(p.muted).child("loading log…").into_any_element(),
                    Some(Err(e)) => div().text_xs().text_color(p.bad).child(format!("log: {e}")).into_any_element(),
                    Some(Ok(lines)) if lines.is_empty() => {
                        div().text_xs().text_color(p.muted).child("log is empty").into_any_element()
                    }
                    Some(Ok(lines)) => div()
                        .flex()
                        .flex_col()
                        .font_family(self.mono.clone())
                        .text_xs()
                        .text_color(p.text)
                        .children(lines.iter().rev().map(|l| div().child(l.clone())))
                        .into_any_element(),
                };
                d = d.child(div().pt_1().text_xs().text_color(p.muted).child("log (newest first)"));
                d = d.child(log);
            }
            _ => {}
        }
        Some(d.into_any_element())
    }
}

impl Render for TandmView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_appearance(window.appearance());
        let details = self.render_details(window, &p, cx);
        let flash = self.flash.clone();
        let config_error = self.config_error.clone();

        div()
            .id("root")
            .track_focus(&self.root_focus)
            .on_action(cx.listener(|v, _: &Refresh, _, cx| v.refresh(cx)))
            .on_action(cx.listener(|v, _: &CloseOverlay, _, cx| {
                v.add = None;
                v.selected = None;
                v.log = None;
                v.confirm_remove = None;
                cx.notify();
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .text_color(p.text)
            .child(self.render_toolbar(&p, cx))
            .when_some(config_error, |d, e| {
                d.child(div().px_4().py_1().bg(alpha(p.bad, 0.15)).text_sm().text_color(p.bad).child(format!("config: {e}")))
            })
            .child(self.render_header(&p))
            .child(self.render_grid(&p, cx))
            .when_some(details, |d, det| {
                d.child(
                    div()
                        .id("details")
                        .max_h(px(300.))
                        .overflow_y_scroll()
                        .flex_shrink_0()
                        .border_t_1()
                        .border_color(p.border)
                        .bg(p.surface)
                        .px_4()
                        .py_3()
                        .child(det),
                )
            })
            .when_some(flash, |d, (ok, msg)| {
                d.child(
                    div()
                        .id("flash")
                        .flex()
                        .gap_2()
                        .px_4()
                        .py_1()
                        .border_t_1()
                        .border_color(p.border)
                        .bg(if ok { p.surface2 } else { alpha(p.bad, 0.15) })
                        .text_xs()
                        .text_color(if ok { p.muted } else { p.bad })
                        .cursor_pointer()
                        .on_click(cx.listener(|v, _: &ClickEvent, _, cx| {
                            v.flash = None;
                            cx.notify();
                        }))
                        .child(div().flex_1().child(msg))
                        .child("✕"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::suggest_path;

    #[test]
    fn suggests_home_relative_paths() {
        assert_eq!(suggest_path("/home/alice/projects/x"), "~/projects/x");
        assert_eq!(suggest_path("/Users/alice/projects/x"), "~/projects/x");
        assert_eq!(suggest_path("/srv/x"), "/srv/x");
    }
}
