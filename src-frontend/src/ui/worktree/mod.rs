//! WP12 — working tree & commit (reference UI §6 WIP panel + commit box, §11 stash dialog, §13 discard, toolbar
//! Stash / Pop).
//!
//! * `model` — pure logic (selection, tree grouping, labels, validation, merge prefill); unit tested.
//! * `files` — the file sections (conflicted / unstaged / staged), rows, context menu, stage / unstage / discard.
//! * `commit_box` — summary / description / commit button, signature check, Cmd+Enter.
//! * `stash` — stash dialog and pop.
//!
//! Engine state is never updated optimistically: every action calls the engine and the resulting
//! `GitFilesChanged` / `CommitsUpdated` / `StashesUpdated` events update the panel.

mod commit_box;
mod files;
pub(crate) mod model;
mod stash;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::PathBuf;

use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use gityas_core::AppEvent;
use gityas_core::diff::DiffTarget;
use gityas_core::models::Delta;

use self::model::Section;
use crate::app::{Center, DiffRequest, GitYasApp};
use crate::theme;

/// Stage / unstage / discard (one key: the section buttons are disabled while any of them runs).
pub const TASK_FILES: crate::bridge::TaskKey = "worktree_files";
pub const TASK_COMMIT: crate::bridge::TaskKey = "commit";
pub const TASK_STASH: crate::bridge::TaskKey = "stash";
pub const TASK_POP: crate::bridge::TaskKey = "stash_pop";

/// Global state of this module.
#[derive(Default)]
pub struct WorktreeUi {
    stash: Option<stash::StashDialog>,
    /// A commit / stash is waiting for the signature dialog (WP15's `request_signature`); cleared when the deferred
    /// retry runs. Shown as a hint in the commit box.
    pub(crate) awaiting_signature: bool,
}

/// Per-tab state: draft commit message (kept when switching tabs), file selection, list / tree toggle.
#[derive(Default)]
pub struct WorktreeTab {
    /// Draft commit summary. WP11's WIP graph row shows it (`// WIP <summary>`), so it is fixed in the skeleton.
    pub summary: String,
    pub description: String,
    pub(crate) selection: model::FileSelection,
    /// Tree view (grouped by directory) instead of the flat path list.
    pub(crate) tree: bool,
    /// Collapsed directories of the tree view.
    pub(crate) collapsed: HashSet<PathBuf>,
    pub(crate) prefill: model::MergePrefill,
}

/// A stage / unstage that takes the file open in the diff viewer out of its section (see [`shown_move`]).
pub(crate) struct ShownMove {
    shown: PathBuf,
    from: Section,
    /// Row of the same section to show instead, picked before the move (in on-screen order).
    next: Option<PathBuf>,
}

/// Call before staging / unstaging `moving` out of `from`: `Some` when the working-tree diff on screen is one of
/// them.
pub(crate) fn shown_move(app: &GitYasApp, from: Section, moving: &[Delta]) -> Option<ShownMove> {
    let tab = app.tab()?;
    let Center::Diff(request) = &tab.center else { return None };
    let shown = &request.file.path;
    if request.target != (DiffTarget::WorkingTree { staged: from.staged() }) || !moving.iter().any(|d| &d.path == shown) {
        return None;
    }
    let list = model::section_files(&app.store.repo.as_ref()?.files, from);
    Some(ShownMove {
        next: files::next_after_move(&tab.worktree, &list, shown, moving),
        shown: shown.clone(),
        from,
    })
}

/// After the move succeeded, while the moved file is still on screen: shows (and selects) the next file of the
/// same section, or, when none is left, closes the viewer (and deselects).
pub(crate) fn after_move(app: &mut GitYasApp, moved: Option<ShownMove>) {
    let Some(moved) = moved else { return };
    let still_shown = app
        .tab()
        .is_some_and(|t| matches!(&t.center, Center::Diff(r) if r.file.path == moved.shown));
    let Some(files) = app.store.repo.as_ref().map(|r| &r.files).filter(|_| still_shown) else {
        return;
    };
    let next = moved.next.as_deref().and_then(|next| {
        model::section_files(files, moved.from)
            .into_iter()
            .find(|d| d.path == next)
            .cloned()
    });
    let Some(file) = next else {
        crate::ui::diff::close(app);
        return;
    };
    if let Some(tab) = app.tab_mut() {
        tab.worktree.selection.select_only(moved.from, &file.path);
    }
    crate::ui::diff::open(
        app,
        DiffRequest {
            target: DiffTarget::WorkingTree {
                staged: moved.from.staged(),
            },
            file,
        },
    );
}

pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    match event {
        AppEvent::GitFilesChanged { files, .. } => {
            if let Some(tab) = app.tab_mut() {
                tab.worktree.selection.retain_existing(files);
            }
        }
        AppEvent::RepoStatusUpdated { status, .. } => {
            if let Some(tab) = app.tab_mut() {
                let wt = &mut tab.worktree;
                wt.prefill.sync(status, &mut wt.summary);
            }
        }
        // A stash dialog belongs to the repository it was opened for.
        AppEvent::SelectedRepositoryUpdated(id) if app.worktree_ui.stash.as_ref().is_some_and(|d| Some(&d.repo) != id.as_ref()) => {
            app.worktree_ui.stash = None;
        }
        _ => {}
    }
}

pub fn show_dialogs(ctx: &egui::Context, app: &mut GitYasApp) {
    stash::show(ctx, app);
}

/// Changes page when the WIP row is selected.
pub fn panel(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let Some(repo) = app.store.repo.as_ref() else { return };
    if repo.status.is_bare {
        ui.label(RichText::new("Bare repository: there is no working tree to commit from.").color(p.text_muted));
        return;
    }
    if !repo.loaded.files {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.label(RichText::new("Loading changes…").color(p.text_muted));
        });
        return;
    }
    let total = repo.files.staged.len() + repo.files.unstaged.len();
    let branch = repo.head.as_ref().map_or_else(|| "…".to_string(), |h| h.label());

    // Header: "Changes" + list / tree toggle, then "N file changes on ‹branch›".
    let tree = app.tab().is_some_and(|t| t.worktree.tree);
    let mut set_tree = None;
    crate::ui::trailing_row(
        ui,
        |ui| set_tree = view_toggle(ui, tree),
        |ui| {
            ui.add(egui::Label::new(RichText::new("Changes").size(16.0).strong()).truncate());
        },
    );
    ui.add_space(2.0);
    header_text(ui, p, total, &branch);
    if let (Some(tree), Some(tab)) = (set_tree, app.tab_mut()) {
        tab.worktree.tree = tree;
    }
    ui.add_space(10.0);

    // The commit box is pinned to the bottom; the file sections share the rest.
    egui::Panel::bottom("wip_commit_box")
        .frame(egui::Frame::new().inner_margin(egui::Margin {
            top: 10,
            ..Default::default()
        }))
        .show_separator_line(false)
        .resizable(false)
        .show(ui, |ui| commit_box::show(ui, app));
    egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show(ui, |ui| files::show(ui, app));
}

/// Segmented Path | Tree control of a fixed size (so the title beside it keeps its room). Returns the view picked.
fn view_toggle(ui: &mut Ui, tree: bool) -> Option<bool> {
    let p = theme::pal(ui);
    let font = egui::FontId::proportional(12.0);
    let segments = [
        (false, format!("{} Path", icon::LIST), "Flat list of paths"),
        (true, format!("{} Tree", icon::TREE_STRUCTURE), "Group files by directory"),
    ];
    let galleys: Vec<_> = segments
        .iter()
        .map(|(_, text, _)| ui.painter().layout_no_wrap(text.clone(), font.clone(), p.text))
        .collect();
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 16.0).collect();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(widths.iter().sum::<f32>() + 4.0, 26.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 8, p.bg_input);
    let mut x = rect.left() + 2.0;
    let mut picked = None;
    for (((value, text, tooltip), galley), w) in segments.iter().zip(galleys).zip(widths) {
        let seg = egui::Rect::from_min_size(egui::pos2(x, rect.top() + 2.0), egui::vec2(w, rect.height() - 4.0));
        let resp = ui.interact(seg, ui.id().with(("view_toggle", *value)), egui::Sense::click());
        let selected = *value == tree;
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, text));
        if selected {
            ui.painter().rect_filled(seg, 6, p.bg_selected);
        } else if resp.hovered() {
            ui.painter().rect_filled(seg, 6, p.bg_hover);
        }
        let color = if selected { p.text } else { p.text_muted };
        ui.painter()
            .galley_with_override_text_color(seg.center() - galley.size() / 2.0, galley, color);
        if resp.on_hover_text(*tooltip).clicked() && !selected {
            picked = Some(*value);
        }
        x += w;
    }
    picked
}

/// "N file changes on ‹branch›" on one line when it fits, else the branch wraps below and is elided.
fn header_text(ui: &mut Ui, p: &theme::Palette, total: usize, branch: &str) {
    let count = RichText::new(format!("{total} file {} on", if total == 1 { "change" } else { "changes" }))
        .size(12.0)
        .color(p.text_muted);
    let branch = RichText::new(format!("{} {branch}", icon::GIT_BRANCH)).size(12.0).color(p.accent);
    let width = |text: &RichText| {
        egui::WidgetText::from(text.clone())
            .into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body)
            .size()
            .x
    };
    if width(&count) + ui.spacing().item_spacing.x + width(&branch) <= ui.available_width() {
        ui.horizontal(|ui| {
            ui.label(count);
            ui.label(branch);
        });
    } else {
        ui.add(egui::Label::new(count).truncate());
        ui.add(egui::Label::new(branch).truncate());
    }
}

/// Opens the stash dialog for `files` (`None` = all changes). Toolbar Stash, STASHES `+`, "Stash changes" toasts.
pub fn open_stash_dialog(app: &mut GitYasApp, files: Option<Vec<Delta>>) {
    stash::open(app, files);
}

/// Commits the staged files with the active tab's draft summary (signature check included); `push_after` pushes
/// the current branch once the commit succeeded. Speed mode.
pub(crate) fn commit(app: &mut GitYasApp, push_after: bool) {
    commit_box::request(app, push_after);
}

/// Stages `files` (keeps the diff viewer in step like the Changes panel does). Speed mode.
pub(crate) fn stage(app: &mut GitYasApp, files: Vec<Delta>) {
    files::apply(app, files::Action::Stage(files), None);
}

/// Unstages `files`. Speed mode.
pub(crate) fn unstage(app: &mut GitYasApp, files: Vec<Delta>) {
    files::apply(app, files::Action::Unstage(files), None);
}

/// Asks before discarding `files` (§13). Speed mode.
pub(crate) fn discard(app: &mut GitYasApp, files: Vec<Delta>) {
    files::confirm_discard(app, files);
}

/// Toolbar Pop: pops `stash@{0}`.
pub fn pop_latest(app: &mut GitYasApp) {
    stash::pop_latest(app);
}
