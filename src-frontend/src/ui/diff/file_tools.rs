//! The full-screen **file history** view (opened with the diff viewer's History / Blame buttons).
//!
//! Like the merge editor it takes the whole window: the side panels are hidden and ✕ (or Esc) in the corner goes
//! back to the diff it was opened from. On the left, pinned, is every version of the file: the one the diff showed
//! when it is not a commit of the history (uncommitted or staged changes, a stash), then every commit that changed
//! the file (renames followed), newest first. The right shows the selected version:
//!
//! * History: that commit's diff of the file (the diff viewer, without its close and history buttons).
//! * Blame: the file as that version has it, each line with the commit that last changed it. A click on a line's
//!   commit selects that commit and shows its diff; hovering one highlights every line it changed.
//!
//! The history loads when the view opens and the blame when it is first shown for a version; both stay cached in
//! the tab until another file's history is opened. Working-tree versions reload when the files change.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{Align, Align2, Color32, FontId, Layout, Margin, Pos2, RichText, Sense, Ui, vec2};
use egui_phosphor::regular as icon;
use gityas_core::diff::DiffTarget;
use gityas_core::models::{BlameHunk, Delta, DeltaChange, FileBlame, FileHistoryEntry};
use gityas_core::{Oid, RepoId};

use super::rows::{self, LineSpec, Metrics};
use crate::app::{Center, DiffRequest, GitYasApp};
use crate::bridge::TaskKey;
use crate::syntax::HighlightedText;
use crate::theme::{self, Palette};
use crate::ui::history;

pub const TASK_HISTORY: TaskKey = "diff-file-history";
pub const TASK_BLAME: TaskKey = "diff-file-blame";
const TASK_OPEN: TaskKey = "diff-blame-open";

const ROW: f32 = 46.0;
const LIST_WIDTH: f32 = 320.0;
const LIST_RANGE: std::ops::RangeInclusive<f32> = 240.0..=560.0;
/// Width of the blame annotation column (commit, author, age, summary), at most this share of the view.
const ANNOTATION_WIDTH: f32 = 320.0;
const ANNOTATION_SHARE: f32 = 0.45;

/// What the right of the view shows for the selected version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    History,
    Blame,
}

impl Pane {
    pub const ALL: [(Pane, &'static str, &'static str); 2] = [
        (Pane::History, "History", "The selected commit's changes to the file"),
        (Pane::Blame, "Blame", "Who last changed each line, as of the selected version"),
    ];
}

enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// One cached result: what it was loaded for, and the load it belongs to (older completions are dropped).
struct Slot<K, T> {
    key: K,
    generation: u64,
    load: Load<T>,
}

/// A loaded blame with what drawing it needs (computed off the UI thread).
pub struct Blamed {
    blame: FileBlame,
    /// Byte ranges of the lines of `blame.text`, without terminators (as `str::lines`).
    lines: Vec<Range<usize>>,
    syntax: HighlightedText,
}

impl Blamed {
    fn new(path: &Path, blame: FileBlame) -> Self {
        let mut lines = Vec::new();
        let mut at = 0;
        for line in blame.text.split_inclusive('\n') {
            let content = line.strip_suffix('\n').unwrap_or(line);
            let content = content.strip_suffix('\r').unwrap_or(content);
            lines.push(at..at + content.len());
            at += line.len();
        }
        let syntax = HighlightedText::new(path, &blame.text);
        Self { blame, lines, syntax }
    }

    fn line(&self, i: usize) -> &str {
        self.lines.get(i).and_then(|r| self.blame.text.get(r.clone())).unwrap_or("")
    }
}

/// State of a tab's file history view (kept while it is left, reset when another file's history is opened).
#[derive(Default)]
pub struct FileTools {
    pub pane: Pane,
    /// The diff the view was opened from (`Center::File` holds the same request).
    origin: Option<DiffRequest>,
    /// The version shown on the right.
    pub selected: Option<DiffRequest>,
    history: Option<Slot<PathBuf, Arc<[FileHistoryEntry]>>>,
    blame: Option<Slot<(DiffTarget, PathBuf), Arc<Blamed>>>,
    generation: u64,
    /// The commit whose blame annotation the pointer was over last frame (its lines are highlighted).
    hovered: Option<Oid>,
    /// Scroll the list to the selected row next frame.
    scroll_to_selected: bool,
    /// Opened on Blame for a version without lines (a deleted file): select the newest commit that has the file
    /// once the history is in.
    pick_blamable: bool,
}

impl FileTools {
    /// The working tree changed: a working-tree / index blame is reloaded when shown.
    pub fn invalidate_blame(&mut self) {
        if self
            .blame
            .as_ref()
            .is_some_and(|s| matches!(s.key.0, DiffTarget::WorkingTree { .. }))
        {
            self.blame = None;
        }
    }

    /// The history is reloaded when shown (a commit may have changed the file).
    pub fn invalidate_history(&mut self) {
        self.history = None;
    }

    fn entries(&self) -> Option<Arc<[FileHistoryEntry]>> {
        match self.history.as_ref().map(|s| &s.load) {
            Some(Load::Ready(entries)) => Some(entries.clone()),
            _ => None,
        }
    }

    /// The rows of the list: the origin first when it is not one of the history's commits, then the history.
    fn rows(&self) -> Vec<DiffRequest> {
        let entries = self.entries();
        let entries = entries.as_deref().unwrap_or_default();
        let mut out = Vec::with_capacity(entries.len() + 1);
        if let Some(origin) = &self.origin
            && !entries.iter().any(|e| DiffTarget::Commit(e.commit.id.clone()) == origin.target)
        {
            out.push(origin.clone());
        }
        out.extend(entries.iter().map(entry_request));
        out
    }
}

fn entry_request(entry: &FileHistoryEntry) -> DiffRequest {
    DiffRequest {
        target: DiffTarget::Commit(entry.commit.id.clone()),
        file: entry.file.clone(),
    }
}

/// Whether the file has a version on this side of the diff to blame.
pub fn can_blame(file: &Delta) -> bool {
    !matches!(
        file.change,
        DeltaChange::Deleted | DeltaChange::Conflicted | DeltaChange::Ignored | DeltaChange::Unreadable
    )
}

// ---- Cross-module API --------------------------------------------------------------------------------------

/// Opens the file history view for the file of `request` (the diff on screen) on `pane`.
pub fn open(app: &mut GitYasApp, request: DiffRequest, pane: Pane) {
    let Some(tab) = app.tab_mut() else { return };
    let t = &mut tab.file;
    let same_file = t.origin.as_ref().is_some_and(|o| o.file.path == request.file.path);
    let history = if same_file { t.history.take() } else { None };
    *t = FileTools {
        pane,
        origin: Some(request.clone()),
        selected: Some(request.clone()),
        history,
        generation: t.generation,
        scroll_to_selected: true,
        pick_blamable: pane == Pane::Blame && !can_blame(&request.file),
        ..FileTools::default()
    };
    tab.center = Center::File(request);
}

/// ✕ / Esc: back to the diff the view was opened from.
pub fn exit(app: &mut GitYasApp) {
    let Some(tab) = app.tab_mut() else { return };
    if let Center::File(origin) = &tab.center {
        tab.center = Center::Diff(origin.clone());
    }
}

/// Whether `request` is the diff on screen (in the diff viewer or in this view's History pane).
pub fn shows_diff(app: &GitYasApp, request: &DiffRequest) -> bool {
    app.tab().is_some_and(|t| match &t.center {
        Center::Diff(r) => r == request,
        Center::File(_) => t.file.pane == Pane::History && t.file.selected.as_ref() == Some(request),
        _ => false,
    })
}

/// Starts loading what the view needs (the history, and the selected version's blame on the Blame pane), if it is
/// not loaded (or loading) yet.
fn ensure_loaded(app: &mut GitYasApp) {
    let Some(repo) = app.store.active.clone() else { return };
    let Some(tab) = app.tabs.get_mut(&repo) else { return };
    let t = &mut tab.file;
    let Some(origin) = t.origin.clone() else { return };
    let path = origin.file.path.clone();
    if t.history.as_ref().is_none_or(|s| s.key != path) {
        t.generation += 1;
        let generation = t.generation;
        t.history = Some(Slot {
            key: path.clone(),
            generation,
            load: Load::Loading,
        });
        let repo = repo.clone();
        app.bridge.run(
            TASK_HISTORY,
            move |engine| async move { engine.file_history(path).await },
            move |app, result| {
                let load = match result {
                    Ok(list) => Load::Ready(Arc::from(list)),
                    Err(err) => Load::Failed(err.to_string()),
                };
                with_slot(
                    app,
                    &repo,
                    |t| t.history.as_mut().filter(|s| s.generation == generation).map(|s| &mut s.load),
                    load,
                );
            },
        );
    }
    let Some(tab) = app.tabs.get_mut(&repo) else { return };
    let t = &mut tab.file;
    if t.pane != Pane::Blame {
        return;
    }
    let Some(selected) = t.selected.clone().filter(|s| can_blame(&s.file)) else {
        return;
    };
    let key = (selected.target.clone(), selected.file.path.clone());
    if t.blame.as_ref().is_some_and(|s| s.key == key) {
        return;
    }
    t.generation += 1;
    let generation = t.generation;
    t.blame = Some(Slot {
        key: key.clone(),
        generation,
        load: Load::Loading,
    });
    let (target, path) = key;
    app.bridge.run(
        TASK_BLAME,
        move |engine| async move {
            let blame = engine.file_blame(target, path.clone()).await?;
            Ok(Arc::new(Blamed::new(&path, blame)))
        },
        move |app, result| {
            let load = match result {
                Ok(blamed) => Load::Ready(blamed),
                Err(err) => Load::Failed(err.to_string()),
            };
            with_slot(
                app,
                &repo,
                |t| t.blame.as_mut().filter(|s| s.generation == generation).map(|s| &mut s.load),
                load,
            );
        },
    );
}

/// Stores a completed load into the slot `pick` finds, if the repository is still the active one (otherwise the
/// engine may have answered for another repository: the slot is dropped and reloaded when shown).
fn with_slot<T>(app: &mut GitYasApp, repo: &RepoId, pick: impl FnOnce(&mut FileTools) -> Option<&mut Load<T>>, load: Load<T>) {
    let active = app.store.active.as_ref() == Some(repo);
    let Some(tab) = app.tabs.get_mut(repo) else { return };
    let t = &mut tab.file;
    if !active {
        t.history = None;
        t.blame = None;
        return;
    }
    if let Some(slot) = pick(t) {
        *slot = load;
    }
}

// ---- Actions ----------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ToolAct {
    Exit,
    Pane(Pane),
    /// Show this version (a list row).
    Select(DiffRequest),
    /// Up / Down in the list.
    Step(isize),
    /// Show `commit`'s diff of the file, which had `path` there (a blame annotation).
    ShowCommit {
        commit: Oid,
        path: PathBuf,
    },
    Reveal(Oid),
    CopySha(Oid),
    Retry,
}

pub fn apply(app: &mut GitYasApp, ctx: &egui::Context, act: ToolAct) {
    match act {
        ToolAct::Exit => exit(app),
        ToolAct::Pane(pane) => {
            if let Some(tab) = app.tab_mut() {
                tab.file.pane = pane;
            }
        }
        ToolAct::Select(request) => select(app, request, None),
        ToolAct::Step(by) => {
            let Some(tab) = app.tab_mut() else { return };
            let rows = tab.file.rows();
            let at = rows.iter().position(|r| Some(r) == tab.file.selected.as_ref());
            let next = match at {
                Some(at) => at.saturating_add_signed(by).min(rows.len().saturating_sub(1)),
                None => 0,
            };
            if let Some(request) = rows.get(next).cloned() {
                select(app, request, None);
            }
        }
        ToolAct::ShowCommit { commit, path } => {
            let found = app
                .tab()
                .and_then(|t| t.file.entries()?.iter().find(|e| e.commit.id == commit).map(entry_request));
            match found {
                Some(request) => select(app, request, Some(Pane::History)),
                None => show_commit(app, commit, path),
            }
        }
        ToolAct::Reveal(id) => {
            if let Some(tab) = app.tab_mut() {
                tab.center = Center::Graph;
            }
            history::run(app, vec![history::Intent::Reveal(id)]);
        }
        ToolAct::CopySha(id) => ctx.copy_text(id.to_hex()),
        ToolAct::Retry => {
            if let Some(tab) = app.tab_mut() {
                match &tab.file.history {
                    Some(Slot { load: Load::Failed(_), .. }) => tab.file.history = None,
                    _ => tab.file.blame = None,
                }
            }
        }
    }
}

fn select(app: &mut GitYasApp, request: DiffRequest, pane: Option<Pane>) {
    let Some(tab) = app.tab_mut() else { return };
    let t = &mut tab.file;
    if let Some(pane) = pane {
        t.pane = pane;
    }
    if t.selected.as_ref() != Some(&request) {
        t.selected = Some(request);
        t.scroll_to_selected = true;
        t.hovered = None;
    }
}

/// Shows `commit`'s diff of the file it had at `path` when the commit is not in the list (its change type comes
/// from the commit's file list).
fn show_commit(app: &mut GitYasApp, commit: Oid, path: PathBuf) {
    let id = commit.clone();
    app.bridge.run(
        TASK_OPEN,
        move |engine| async move { engine.commit_files(id).await },
        move |app, result| {
            let file = match result {
                Ok(files) => files
                    .into_iter()
                    .find(|d| d.path == path || d.old_path.as_ref() == Some(&path))
                    .unwrap_or_else(|| Delta::new(path.clone(), DeltaChange::Modified)),
                Err(err) => {
                    app.report_error("Could not open the commit", &err);
                    return;
                }
            };
            if app.tab().is_some_and(|t| matches!(t.center, Center::File(_))) {
                let request = DiffRequest {
                    target: DiffTarget::Commit(commit),
                    file,
                };
                select(app, request, Some(Pane::History));
            }
        },
    );
}

/// The working tree changed while the view is open: working-tree versions are reloaded, and the history too once
/// the file has no changes left (it was most likely committed).
pub fn on_files_changed(app: &mut GitYasApp, files: &gityas_core::models::FileState) {
    let Some(tab) = app.tab_mut() else { return };
    let Center::File(origin) = &tab.center else { return };
    let changed = files.staged.iter().chain(files.unstaged.iter()).any(|d| d.path == origin.file.path);
    tab.file.invalidate_blame();
    if !changed {
        tab.file.invalidate_history();
    }
    let selected = tab.file.selected.clone();
    if let Some(request) = selected.filter(|r| matches!(r.target, DiffTarget::WorkingTree { .. }))
        && tab.diff.request.as_ref() == Some(&request)
    {
        super::refetch(app, request);
    }
}

// ---- Drawing ----------------------------------------------------------------------------------------------------

/// The whole view: header, the pinned list on the left, the selected version on the right.
pub fn view(ui: &mut Ui, app: &mut GitYasApp, origin: &DiffRequest) {
    let Some(repo) = app.store.active.clone() else { return };
    let stale = app
        .tabs
        .get(&repo)
        .is_none_or(|t| t.file.origin.as_ref() != Some(origin) || t.file.selected.is_none());
    if stale {
        let pane = app.tabs.get(&repo).map(|t| t.file.pane).unwrap_or_default();
        open(app, origin.clone(), pane);
    }
    pick_blamable(app);
    ensure_loaded(app);

    let p = theme::pal(ui);
    let keys_free = app.confirm.is_none() && ui.ctx().memory(|m| m.top_modal_layer().is_none()) && !ui.ctx().egui_wants_keyboard_input();
    let mut acts: Vec<ToolAct> = Vec::new();
    if keys_free {
        ui.ctx().input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                acts.push(ToolAct::Exit);
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                acts.push(ToolAct::Step(1));
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                acts.push(ToolAct::Step(-1));
            }
        });
    }

    let Some(tab) = app.tabs.get_mut(&repo) else { return };
    header(ui, p, origin, &tab.file, &mut acts);
    egui::Panel::left("file-history-list")
        .resizable(true)
        .default_size(LIST_WIDTH)
        .size_range(LIST_RANGE)
        .frame(egui::Frame::new().fill(p.bg_panel).stroke(egui::Stroke::new(1.0, p.border)))
        .show(ui, |ui| list(ui, p, &mut tab.file, &mut acts));

    let pane = tab.file.pane;
    let selected = tab.file.selected.clone();
    match (pane, selected) {
        (Pane::History, Some(request)) => super::view_embedded(ui, app, &request),
        (Pane::Blame, Some(request)) => {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(p.bg_graph))
                .show(ui, |ui| {
                    let Some(tab) = app.tabs.get_mut(&repo) else { return };
                    blame_body(ui, p, &request, &mut tab.file, &mut acts);
                });
        }
        (_, None) => {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(p.bg_graph))
                .show(ui, |_| {});
        }
    }

    let ctx = ui.ctx().clone();
    for act in acts {
        apply(app, &ctx, act);
    }
}

/// Opened on Blame for a version without lines: once the history is in, select its newest version that has them.
fn pick_blamable(app: &mut GitYasApp) {
    let Some(tab) = app.tab_mut() else { return };
    let t = &mut tab.file;
    if !t.pick_blamable {
        return;
    }
    let Some(entries) = t.entries() else { return };
    t.pick_blamable = false;
    if let Some(entry) = entries.iter().find(|e| can_blame(&e.file)) {
        t.selected = Some(entry_request(entry));
        t.scroll_to_selected = true;
    }
}

fn header(ui: &mut Ui, p: &Palette, origin: &DiffRequest, t: &FileTools, acts: &mut Vec<ToolAct>) {
    egui::Panel::top("file-history-header")
        .frame(
            egui::Frame::new()
                .fill(p.bg_toolbar)
                .inner_margin(Margin::symmetric(12, 7))
                .stroke(egui::Stroke::new(1.0, p.border)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::CLOCK_COUNTER_CLOCKWISE).color(p.accent).size(16.0));
                let path = &origin.file.path;
                let full = path.display().to_string();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| full.clone());
                let dir = path
                    .parent()
                    .map(|d| d.display().to_string())
                    .filter(|d| !d.is_empty())
                    .map(|d| format!("{d}/"));
                ui.spacing_mut().item_spacing.x = 0.0;
                if let Some(dir) = dir {
                    ui.label(RichText::new(dir).color(p.text_muted)).on_hover_text(&full);
                }
                ui.label(RichText::new(name).strong()).on_hover_text(&full);
                ui.add_space(10.0);
                ui.spacing_mut().item_spacing.x = 6.0;
                if let Some(entries) = t.entries() {
                    let n = entries.len();
                    let word = if n == 1 { "commit" } else { "commits" };
                    let more = if n >= gityas_engine::FILE_HISTORY_MAX {
                        " (newest shown)"
                    } else {
                        ""
                    };
                    ui.label(RichText::new(format!("{n} {word}{more}")).color(p.text_muted));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let close = crate::ui::icon_button(ui, icon::X, "Close file history (Esc)");
                    close.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close file history"));
                    if close.clicked() {
                        acts.push(ToolAct::Exit);
                    }
                    ui.add_space(8.0);
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        // Right-to-left layout: add in reverse.
                        for (pane, label, tip) in Pane::ALL.into_iter().rev() {
                            let resp = ui.add(egui::Button::selectable(t.pane == pane, label)).on_hover_text(tip);
                            if resp.clicked() && t.pane != pane {
                                acts.push(ToolAct::Pane(pane));
                            }
                        }
                    });
                });
            });
        });
}

fn loading(ui: &mut Ui, p: &Palette, what: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(40.0);
        ui.add(egui::Spinner::new());
        ui.label(RichText::new(what).color(p.text_muted));
    });
}

fn failed(ui: &mut Ui, p: &Palette, title: &str, message: &str, acts: &mut Vec<ToolAct>) {
    super::panel(ui, p, icon::WARNING, title, Some(message), |ui| {
        if ui.button("Retry").clicked() {
            acts.push(ToolAct::Retry);
        }
    });
}

/// The pinned list of versions.
fn list(ui: &mut Ui, p: &Palette, t: &mut FileTools, acts: &mut Vec<ToolAct>) {
    let entries = match t.history.as_ref().map(|s| &s.load) {
        Some(Load::Failed(message)) => {
            let message = message.clone();
            return failed(ui, p, "Could not load the history", &message, acts);
        }
        Some(Load::Ready(entries)) => Some(entries.clone()),
        None | Some(Load::Loading) => None,
    };
    let rows = t.rows();
    let scroll = std::mem::take(&mut t.scroll_to_selected);
    egui::ScrollArea::vertical()
        .id_salt("file-history-list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for request in &rows {
                let current = t.selected.as_ref() == Some(request);
                let entry = entries
                    .as_deref()
                    .and_then(|e| e.iter().find(|e| DiffTarget::Commit(e.commit.id.clone()) == request.target));
                let resp = match entry {
                    Some(entry) => history_row(ui, p, entry, current, acts),
                    None => origin_row(ui, p, request, current),
                };
                if resp.clicked() && !current {
                    acts.push(ToolAct::Select(request.clone()));
                }
                if current && scroll {
                    resp.scroll_to_me(None);
                }
            }
            match &entries {
                None => loading(ui, p, "Loading the file's history…"),
                Some(entries) if entries.is_empty() => {
                    ui.add_space(16.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("No commits have changed this file yet").color(p.text_muted));
                        ui.label(
                            RichText::new("Only commits reachable from HEAD are searched.")
                                .small()
                                .color(p.text_faint),
                        );
                    });
                }
                Some(_) => {}
            }
        });
}

/// A row's background, accent bar and separator; `None` when the row is scrolled out of view.
fn row_frame(ui: &mut Ui, p: &Palette, current: bool) -> (egui::Rect, egui::Response, Option<egui::Painter>) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::click());
    if !ui.is_rect_visible(rect) {
        return (rect, resp, None);
    }
    let painter = ui.painter_at(rect);
    if current {
        painter.rect_filled(rect, 0.0, p.bg_selected);
        painter.rect_filled(egui::Rect::from_min_size(rect.min, vec2(3.0, rect.height())), 0.0, p.accent);
    } else if resp.hovered() {
        painter.rect_filled(rect, 0.0, p.bg_hover);
    }
    painter.hline(rect.x_range(), rect.bottom() - 0.5, egui::Stroke::new(1.0, p.border));
    (rect, resp, Some(painter))
}

/// The version the diff showed when it is not a commit of the history (working tree, index, stash).
fn origin_row(ui: &mut Ui, p: &Palette, request: &DiffRequest, current: bool) -> egui::Response {
    let (title, detail) = match &request.target {
        DiffTarget::WorkingTree { staged: false } => ("Uncommitted changes", "Working tree".to_owned()),
        DiffTarget::WorkingTree { staged: true } => ("Staged changes", "Index".to_owned()),
        DiffTarget::Stash(id) => ("Stash", id.short(7)),
        DiffTarget::Commit(id) => ("This commit", id.short(7)),
    };
    let (rect, resp, painter) = row_frame(ui, p, current);
    if let Some(painter) = painter {
        let inner = rect.shrink2(vec2(14.0, 6.0));
        let top = inner.top() + 8.0;
        let bottom = inner.bottom() - 7.0;
        painter.text(
            Pos2::new(inner.left(), top),
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(13.0),
            p.orange,
        );
        painter.text(
            Pos2::new(inner.left(), bottom),
            Align2::LEFT_CENTER,
            detail,
            FontId::proportional(11.5),
            p.text_muted,
        );
        let (_, color) = crate::ui::change_icon(p, request.file.change);
        painter.text(
            Pos2::new(inner.right(), bottom),
            Align2::RIGHT_CENTER,
            request.file.change.label(),
            FontId::proportional(11.0),
            color,
        );
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, title));
    resp
}

fn history_row(ui: &mut Ui, p: &Palette, entry: &FileHistoryEntry, current: bool, acts: &mut Vec<ToolAct>) -> egui::Response {
    let commit = &entry.commit;
    let (rect, resp, painter) = row_frame(ui, p, current);
    if let Some(painter) = painter {
        let inner = rect.shrink2(vec2(14.0, 6.0));
        let top = inner.top() + 8.0;
        let bottom = inner.bottom() - 7.0;
        let age = crate::ui::format_relative(commit.author_time);
        let age_rect = painter.text(
            Pos2::new(inner.right(), top),
            Align2::RIGHT_CENTER,
            &age,
            FontId::proportional(11.5),
            p.text_faint,
        );
        crate::ui::paint_truncated(
            &painter,
            Pos2::new(inner.left(), top),
            &commit.summary,
            FontId::proportional(13.0),
            p.text,
            (age_rect.left() - inner.left() - 12.0).max(0.0),
        );
        let sha = painter.text(
            Pos2::new(inner.left(), bottom),
            Align2::LEFT_CENTER,
            commit.short_id(),
            FontId::monospace(11.5),
            p.accent,
        );
        let (_, color) = crate::ui::change_icon(p, entry.file.change);
        let change = match (&entry.file.change, &entry.file.old_path) {
            (DeltaChange::Renamed, Some(_)) => "Renamed".to_owned(),
            (change, _) => change.label().to_owned(),
        };
        let change_rect = painter.text(
            Pos2::new(inner.right(), bottom),
            Align2::RIGHT_CENTER,
            change,
            FontId::proportional(11.0),
            color,
        );
        crate::ui::paint_truncated(
            &painter,
            Pos2::new(sha.right() + 10.0, bottom),
            &commit.author_name,
            FontId::proportional(11.5),
            p.text_muted,
            (change_rect.left() - sha.right() - 22.0).max(0.0),
        );
    }
    let renamed = match (&entry.file.change, &entry.file.old_path) {
        (DeltaChange::Renamed, Some(old)) => format!("\nRenamed from {}", old.display()),
        _ => String::new(),
    };
    let summary = commit.summary.clone();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &summary));
    let resp = resp.on_hover_text(format!(
        "{}\n{} · {}{renamed}",
        commit.summary,
        commit.author_name,
        crate::ui::format_time(commit.author_time)
    ));
    resp.context_menu(|ui| {
        if ui.button("Show commit in graph").clicked() {
            acts.push(ToolAct::Reveal(commit.id.clone()));
            ui.close();
        }
        if ui.button("Copy SHA").clicked() {
            acts.push(ToolAct::CopySha(commit.id.clone()));
            ui.close();
        }
    });
    resp
}

/// One line above the blame: which version it is of.
fn blame_caption(ui: &mut Ui, p: &Palette, request: &DiffRequest, t: &FileTools) {
    let text = match &request.target {
        DiffTarget::WorkingTree { staged: false } => "As of your working tree".to_owned(),
        DiffTarget::WorkingTree { staged: true } => "As of the staged version".to_owned(),
        DiffTarget::Stash(id) => format!("As of stash {}", id.short(7)),
        DiffTarget::Commit(id) => {
            let summary = t
                .entries()
                .and_then(|e| e.iter().find(|e| &e.commit.id == id).map(|e| e.commit.summary.clone()));
            match summary {
                Some(summary) => format!("As of {} · {summary}", id.short(7)),
                None => format!("As of {}", id.short(7)),
            }
        }
    };
    egui::Frame::new()
        .fill(p.bg_toolbar)
        .inner_margin(Margin::symmetric(12, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(text).small().color(p.text_muted));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(request.file.path.display().to_string()).small().color(p.text_faint));
                });
            });
        });
}

fn blame_body(ui: &mut Ui, p: &Palette, request: &DiffRequest, t: &mut FileTools, acts: &mut Vec<ToolAct>) {
    if !can_blame(&request.file) {
        let detail = if t.pick_blamable {
            "Looking for the last version of the file…"
        } else {
            "This version of the file has no lines (it was deleted, or is in conflict). Pick another one on the left."
        };
        super::panel(ui, p, icon::FILE_X, "Nothing to blame", Some(detail), |_| {});
        return;
    }
    blame_caption(ui, p, request, t);
    let blamed = match t.blame.as_ref().map(|s| &s.load) {
        None | Some(Load::Loading) => return loading(ui, p, "Loading blame…"),
        Some(Load::Failed(message)) => {
            let message = message.clone();
            return failed(ui, p, "Could not blame this file", &message, acts);
        }
        Some(Load::Ready(blamed)) => blamed.clone(),
    };
    if blamed.lines.is_empty() {
        super::panel(ui, p, icon::FILE, "The file is empty", None, |_| {});
        return;
    }
    if blamed.blame.lossy {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(
                RichText::new(format!("{} Not valid UTF-8: invalid bytes are shown as boxes.", icon::WARNING))
                    .small()
                    .color(p.orange),
            );
        });
    }
    let m = Metrics::new(ui, u32::try_from(blamed.lines.len()).unwrap_or(u32::MAX));
    let annotation = ANNOTATION_WIDTH.min(ui.available_width() * ANNOTATION_SHARE);
    let hovered = t.hovered.take();
    let mut hovering = None;
    rows::virtual_rows(ui, "diff-blame", blamed.lines.len(), None, |ui, i, rect| {
        let Some((index, hunk)) = hunk_of(&blamed.blame, i) else { return };
        let first = hunk.start == i;
        let ann = egui::Rect::from_min_max(rect.min, Pos2::new(rect.left() + annotation, rect.bottom()));
        let code = egui::Rect::from_min_max(Pos2::new(ann.right(), rect.top()), rect.max);
        let same = hunk.commit.is_some() && hunk.commit == hovered;
        // Alternate shades per hunk so runs of lines from one commit read as a group.
        let shade = if index % 2 == 0 { p.bg_panel } else { p.bg_toolbar };
        ui.painter().rect_filled(ann, 0.0, if same { p.bg_selected } else { shade });
        if same {
            ui.painter().rect_filled(code, 0.0, p.bg_hover);
        }
        if first {
            ui.painter()
                .hline(rect.x_range(), rect.top() + 0.5, egui::Stroke::new(1.0, p.border));
            paint_annotation(ui.painter(), p, ann, hunk);
        }
        // The commit's stripe, on every line of the hunk.
        let stripe = egui::Rect::from_min_size(ann.min, vec2(3.0, ann.height()));
        ui.painter().rect_filled(stripe, 0.0, stripe_color(p, hunk));
        let resp = ui.interact(ann, ui.id().with(("blame", i)), Sense::click());
        if resp.hovered() {
            hovering = hunk.commit.clone();
        }
        if let Some(commit) = hunk.commit.clone() {
            let resp = resp.on_hover_text(format!(
                "{}\n{} · {}\n{}\n\nClick to show this commit's changes to the file",
                hunk.summary,
                hunk.author_name,
                crate::ui::format_time(hunk.author_time),
                commit.short(10)
            ));
            if resp.clicked() {
                acts.push(ToolAct::ShowCommit {
                    commit: commit.clone(),
                    path: hunk.path.clone(),
                });
            }
            resp.context_menu(|ui| {
                if ui.button("Show changes").clicked() {
                    acts.push(ToolAct::ShowCommit {
                        commit: commit.clone(),
                        path: hunk.path.clone(),
                    });
                    ui.close();
                }
                if ui.button("Show commit in graph").clicked() {
                    acts.push(ToolAct::Reveal(commit.clone()));
                    ui.close();
                }
                if ui.button("Copy SHA").clicked() {
                    acts.push(ToolAct::CopySha(commit.clone()));
                    ui.close();
                }
            });
        } else {
            resp.on_hover_text("Changed since the last commit");
        }
        let text = blamed.line(i);
        let syntax = blamed.lines.get(i).map(|r| blamed.syntax.colors(r.clone(), p)).unwrap_or_default();
        let number = u32::try_from(i + 1).ok();
        rows::paint_line(
            ui.painter(),
            code,
            &m,
            &LineSpec {
                bg: Color32::TRANSPARENT,
                numbers: &[number],
                number_color: p.text_faint,
                sign: "",
                sign_color: p.text_faint,
                text,
                text_color: p.text,
                syntax: &syntax,
                highlights: &[],
                highlight_bg: Color32::TRANSPARENT,
                italic: false,
                selected: None,
                selection_bg: Color32::TRANSPARENT,
            },
            0.0,
        );
    });
    t.hovered = hovering;
}

/// The hunk covering line `i` and its position in the list.
fn hunk_of(blame: &FileBlame, i: usize) -> Option<(usize, &BlameHunk)> {
    let at = blame.hunks.partition_point(|h| h.start + h.lines <= i);
    blame.hunks.get(at).filter(|h| h.start <= i).map(|h| (at, h))
}

fn stripe_color(p: &Palette, hunk: &BlameHunk) -> Color32 {
    match &hunk.commit {
        Some(id) => {
            let hash = id
                .as_bytes()
                .iter()
                .fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(usize::from(*b)));
            p.lane(hash)
        }
        None => p.orange,
    }
}

/// The first line of a hunk: short SHA, author, age and summary, truncated to the column.
fn paint_annotation(painter: &egui::Painter, p: &Palette, rect: egui::Rect, hunk: &BlameHunk) {
    let painter = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
    let y = rect.center().y;
    let left = rect.left() + 10.0;
    let right = rect.right() - 8.0;
    let Some(commit) = &hunk.commit else {
        crate::ui::paint_truncated(
            &painter,
            Pos2::new(left, y),
            &hunk.author_name,
            FontId::proportional(11.5),
            p.orange,
            right - left,
        );
        return;
    };
    let sha = painter.text(
        Pos2::new(left, y),
        Align2::LEFT_CENTER,
        commit.short(7),
        FontId::monospace(11.0),
        p.accent,
    );
    let age = painter.text(
        Pos2::new(right, y),
        Align2::RIGHT_CENTER,
        crate::ui::format_relative(hunk.author_time),
        FontId::proportional(11.0),
        p.text_faint,
    );
    let room = (age.left() - sha.right() - 16.0).max(0.0);
    let author = crate::ui::paint_truncated(
        &painter,
        Pos2::new(sha.right() + 8.0, y),
        &hunk.author_name,
        FontId::proportional(11.5),
        p.text_muted,
        room * 0.4,
    );
    crate::ui::paint_truncated(
        &painter,
        Pos2::new(author.right() + 8.0, y),
        &hunk.summary,
        FontId::proportional(11.5),
        p.text,
        (age.left() - author.right() - 16.0).max(0.0),
    );
}

/// Fills the open view's history (and, with `blame`, the blame of the selected version) as if the engine had
/// answered, for the mock app's snapshots.
#[cfg(test)]
pub fn inject(app: &mut GitYasApp, entries: Vec<FileHistoryEntry>, blame: Option<FileBlame>) {
    let Some(tab) = app.tab_mut() else { return };
    let t = &mut tab.file;
    let Some(selected) = t.selected.clone() else { return };
    let path = selected.file.path.clone();
    t.history = Some(Slot {
        key: path.clone(),
        generation: t.generation,
        load: Load::Ready(Arc::from(entries)),
    });
    if let Some(blame) = blame {
        t.blame = Some(Slot {
            key: (selected.target.clone(), path.clone()),
            generation: t.generation,
            load: Load::Ready(Arc::new(Blamed::new(&path, blame))),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blamed_lines_match_str_lines() {
        let blame = FileBlame {
            text: "a\r\nbb\n\nccc".into(),
            lossy: false,
            hunks: Vec::new(),
        };
        let blamed = Blamed::new(Path::new("f.txt"), blame);
        let lines: Vec<&str> = (0..blamed.lines.len()).map(|i| blamed.line(i)).collect();
        assert_eq!(lines, "a\r\nbb\n\nccc".lines().collect::<Vec<_>>());
    }

    #[test]
    fn blame_needs_a_version_of_the_file() {
        assert!(can_blame(&Delta::new("a", DeltaChange::Modified)));
        assert!(can_blame(&Delta::new("a", DeltaChange::Untracked)));
        assert!(!can_blame(&Delta::new("a", DeltaChange::Deleted)));
        assert!(!can_blame(&Delta::new("a", DeltaChange::Conflicted)));
    }
}
