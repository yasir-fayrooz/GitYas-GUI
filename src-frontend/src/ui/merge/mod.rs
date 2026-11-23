//! WP14 — merge-conflict editor (reference UI §9).
//!
//! * [`model`]: pure merge model (chunk choices → output, hand edits, undo / redo, navigation, line mapping).
//! * [`dock`]: pure pane layout (a tree of row / column splits; docking, swapping, resizing, presets).
//! * [`labels`]: pure labels (ours / theirs names, action card message and buttons).
//! * [`rows`]: row painter for the source panes (spine bars, line-number pick chips).
//! * [`view`]: the egui screens (result-first editor, action card, loading / error / "no longer conflicted" cards).
//!
//! This file: per-tab state, loading, saving, resolving and reacting to engine events.

mod dock;
mod labels;
mod model;
mod rows;
#[cfg(test)]
mod tests;
mod view;

use std::path::{Path, PathBuf};

use egui::Ui;
use gityas_core::models::{DeletedMergeAction, DeltaChange, MergeConflict};
use gityas_core::{AppEvent, GitYasError, RepoId};

use crate::app::{Center, GitYasApp};
use crate::bridge::TaskKey;
use crate::dialogs::Confirm;

pub use model::MergeModel;

const TASK_LOAD: TaskKey = "merge.load";
const TASK_SAVE: TaskKey = "merge.save";
const TASK_ACTION: TaskKey = "merge.action";

/// Per-tab state: the open path and its editor (survives tab switches).
#[derive(Default)]
pub struct MergeTab {
    path: Option<PathBuf>,
    /// Bumped on every load / close so late results of a superseded load are dropped.
    generation: u64,
    state: State,
}

impl MergeTab {
    /// The text editor's model, if a text conflict is loaded.
    pub fn model(&self) -> Option<&MergeModel> {
        match &self.state {
            State::Editor(editor) => Some(&editor.model),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn reset(&mut self) {
        self.path = None;
        self.generation += 1;
        self.state = State::Idle;
    }
}

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Loading,
    Failed(String),
    Editor(Box<view::Editor>),
    Action(Box<MergeConflict>),
    /// The path is no longer conflicted (resolved elsewhere, merge aborted).
    Gone,
}

impl State {
    fn is_dirty(&self) -> bool {
        matches!(self, State::Editor(editor) if editor.model.is_dirty())
    }
}

/// What the view asks for after drawing a frame (applied with full access to the app).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Request {
    Close,
    Save,
    Resolve(DeletedMergeAction),
    Retry,
}

pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    let AppEvent::GitFilesChanged { files, .. } = event else { return };
    let busy = app.bridge.is_running(TASK_SAVE) || app.bridge.is_running(TASK_ACTION);
    let Some(tab) = app.tab_mut() else { return };
    let Some(path) = tab.merge.path.clone() else { return };
    let conflicted = files
        .unstaged
        .iter()
        .chain(files.staged.iter())
        .any(|d| d.change == DeltaChange::Conflicted && d.path == path);
    match tab.merge.state {
        State::Editor(_) | State::Action(_) | State::Loading if !conflicted && !busy => tab.merge.state = State::Gone,
        State::Gone if conflicted => load(app, path),
        _ => {}
    }
}

/// Opens the merge editor for a conflicted, repository-relative `path` (WP12's conflicted list calls this).
pub fn open(app: &mut GitYasApp, path: PathBuf) {
    let Some(tab) = app.tab_mut() else { return };
    tab.center = Center::Merge(path.clone());
    let keep = tab.merge.path.as_ref() == Some(&path) && matches!(tab.merge.state, State::Loading | State::Editor(_) | State::Action(_));
    if !keep {
        load(app, path);
    }
}

fn load(app: &mut GitYasApp, path: PathBuf) {
    let Some(id) = app.store.active.clone() else { return };
    let tab = app.tabs.entry(id.clone()).or_default();
    tab.merge.generation += 1;
    let generation = tab.merge.generation;
    tab.merge.path = Some(path.clone());
    tab.merge.state = State::Loading;
    app.bridge.run(
        TASK_LOAD,
        move |engine| async move {
            let conflict = engine.merge_conflict(path.clone()).await?;
            let syntax = view::SideSyntax::compute(&path, &conflict);
            Ok((conflict, syntax))
        },
        move |app, result| {
            let Some(tab) = app.tabs.get_mut(&id) else { return };
            if tab.merge.generation != generation {
                return;
            }
            tab.merge.state = match result {
                Ok((conflict, syntax)) => state_for(conflict, syntax),
                Err(GitYasError::NotFound(_)) => State::Gone,
                Err(err) => State::Failed(err.to_string()),
            };
        },
    );
}

fn state_for(conflict: MergeConflict, syntax: view::SideSyntax) -> State {
    if labels::needs_action_card(&conflict) {
        return State::Action(Box::new(conflict));
    }
    match MergeModel::new(&conflict) {
        Ok(model) => State::Editor(Box::new(view::Editor::new(model, syntax))),
        Err(err) => State::Failed(format!("The conflict can't be shown: {err}")),
    }
}

/// Centre area while the merge editor is open.
pub fn view(ui: &mut Ui, app: &mut GitYasApp, path: &Path) {
    let Some(id) = app.store.active.clone() else { return };
    let needs_load = app
        .tabs
        .get(&id)
        .is_none_or(|t| t.merge.path.as_deref() != Some(path) || matches!(t.merge.state, State::Idle));
    if needs_load {
        load(app, path.to_path_buf());
    }
    let Some(tab) = app.tabs.get_mut(&id) else { return };
    let mut state = std::mem::take(&mut tab.merge.state);
    let request = view::show(ui, app, path, &mut state);
    if let Some(tab) = app.tabs.get_mut(&id) {
        tab.merge.state = state;
    }
    match request {
        Some(Request::Close) => close(app, &id),
        Some(Request::Save) => save(app, path.to_path_buf(), false),
        Some(Request::Resolve(action)) => resolve_with_action(app, path.to_path_buf(), action, false),
        Some(Request::Retry) => load(app, path.to_path_buf()),
        None => {}
    }
}

/// `✕` / Esc: asks first when the output has unsaved changes.
fn close(app: &mut GitYasApp, id: &RepoId) {
    let Some(tab) = app.tabs.get(id) else { return };
    if tab.merge.state.is_dirty() {
        let name = tab.merge.path.as_deref().map(file_label).unwrap_or_default();
        let id = id.clone();
        app.confirm(Confirm::new(
            "Discard your resolution?",
            format!("Your changes to the merge output of {name} haven't been saved."),
            "Discard",
            move |app| close_now(app, &id),
        ));
    } else {
        close_now(app, id);
    }
}

fn close_now(app: &mut GitYasApp, id: &RepoId) {
    if let Some(tab) = app.tabs.get_mut(id) {
        if matches!(tab.center, Center::Merge(_)) {
            tab.center = Center::Graph;
        }
        tab.merge.reset();
    }
}

fn file_label(path: &Path) -> String {
    path.display().to_string()
}

/// Save: writes the output with `resolve_conflict`. Unless `force`, asks first when chunks are undecided or the
/// output contains conflict markers.
fn save(app: &mut GitYasApp, path: PathBuf, force: bool) {
    let Some(id) = app.store.active.clone() else { return };
    if app.bridge.is_running(TASK_SAVE) {
        return;
    }
    let Some(model) = app.tabs.get(&id).and_then(|t| t.merge.model()) else {
        return;
    };
    if !force && let Some(warning) = save_warning(model) {
        let again = path.clone();
        app.confirm(
            Confirm::new(
                "Save with unresolved conflicts?",
                format!("Save the output as the resolution of {}?", file_label(&path)),
                "Save anyway",
                move |app| save(app, again, true),
            )
            .with_warning(warning),
        );
        return;
    }
    let content = model.text().to_owned();
    let label = file_label(&path);
    app.bridge.run(
        TASK_SAVE,
        move |engine| async move { engine.resolve_conflict(path, content).await },
        move |app, result| match result {
            Ok(()) => {
                app.notify.success(format!("Resolved {label}"));
                close_now(app, &id);
            }
            Err(err) => app.report_error("Could not save the resolution", &err),
        },
    );
}

/// Why saving deserves a second look, if it does.
fn save_warning(model: &MergeModel) -> Option<String> {
    let undecided = model.undecided();
    let mut parts = Vec::new();
    if undecided > 0 {
        parts.push(format!(
            "{undecided} of {} conflicts are unresolved; they keep the base version.",
            model.chunk_count()
        ));
    }
    if model.has_markers() {
        parts.push("The output still contains conflict markers (<<<<<<< / >>>>>>>).".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Action card buttons. Delete file is confirmed first.
fn resolve_with_action(app: &mut GitYasApp, path: PathBuf, action: DeletedMergeAction, confirmed: bool) {
    let Some(id) = app.store.active.clone() else { return };
    if app.bridge.is_running(TASK_ACTION) {
        return;
    }
    if action == DeletedMergeAction::DeleteFile && !confirmed {
        let again = path.clone();
        app.confirm(
            Confirm::new(
                format!("Delete {}?", file_label(&path)),
                "This resolves the conflict by deleting the file from the working tree and staging the deletion.",
                "Delete file",
                move |app| resolve_with_action(app, again, DeletedMergeAction::DeleteFile, true),
            )
            .with_warning("Both versions of the file are discarded from the merge result."),
        );
        return;
    }
    let done = labels::action_done(&file_label(&path), action, &view::labels_for(app));
    app.bridge.run(
        TASK_ACTION,
        move |engine| async move { engine.resolve_conflict_with_action(path, action).await },
        move |app, result| match result {
            Ok(()) => {
                app.notify.success(done);
                close_now(app, &id);
            }
            Err(err) => app.report_error("Could not resolve the conflict", &err),
        },
    );
}
