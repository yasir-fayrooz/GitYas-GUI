//! Repositories & tabs: the tab bar and welcome screen, the repository picker, the add / clone dialog, removing a
//! repository and the tab shortcuts.
//!
//! * [`tabs`]: the tab bar (switch, close, middle-click, drag to reorder, context menu).
//! * [`picker`]: the repository popup under the tab bar's `+` and the context bar's switcher (filter, open,
//!   reorder, ⋯ menu).
//! * [`add_dialog`]: the add-repository dialog (Open local / Clone).
//! * [`welcome`]: the centre when no repository is open.
//! * [`logic`]: UI-free bookkeeping and validation (unit-tested).
//!
//! **Session operations are serialised.** Opening, closing and removing repositories all change the engine's single
//! active session; started concurrently they can finish in any order (the engine's command lock is not FIFO with
//! respect to validation), leaving the session on a repository the tab bar doesn't show as active, and an
//! `open_repository` racing `set_open_tabs` can persist the wrong active tab. So every session change
//! goes through one queue ([`ReposUi::queue`], one bridge task at a time, key [`TASK_SESSION`]); a newer switch
//! replaces a queued one. The tab bar itself ([`Store::open_tabs`](crate::store::Store)) is UI state the user edits
//! directly; it is persisted with `set_open_tabs` after every change and after every session operation, again one
//! write at a time ([`TASK_PERSIST_TABS`]) so the last write is always the newest state.

mod add_dialog;
pub(crate) mod logic;
mod picker;
mod tabs;
#[cfg(test)]
mod tests;
mod welcome;

use std::collections::VecDeque;

use egui::Ui;
use gityas_core::{AppEvent, Provider, RepoId};

use crate::app::GitYasApp;
use crate::dialogs::Confirm;

pub use logic::AfterClose;

/// Bridge key of the running session operation (open / close / remove a repository).
pub const TASK_SESSION: &str = "repos.session";
/// Bridge key of the `set_open_tabs` write.
pub const TASK_PERSIST_TABS: &str = "repos.persist_tabs";
/// Bridge key of add / clone / reorder / provider changes.
pub const TASK_REGISTRY: &str = "repos.registry";

/// Global state of this module (picker popup, add / clone dialog, session queue).
#[derive(Default)]
pub struct ReposUi {
    /// The repository popup, when open.
    picker: Option<picker::PickerState>,
    /// Where the popup opens: under the `+` of the tab bar (screen position recorded while drawing it).
    plus_anchor: Option<egui::Pos2>,
    /// The tab last scrolled into view in the tab bar (scrolled again only when another tab becomes active).
    revealed_tab: Option<RepoId>,
    add: Option<add_dialog::AddDialog>,
    /// Session operations waiting for the running one (see the module docs).
    queue: VecDeque<SessionOp>,
    /// The repository being opened (running or queued switch), for the spinner and the tab highlight.
    opening: Option<RepoId>,
    /// The tabs changed while a `set_open_tabs` write was running: write again when it finishes.
    persist_again: bool,
}

/// Tab of the add-repository dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddTab {
    Local,
    Clone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionOp {
    Open(RepoId),
    Close,
    /// Unregister `id`; then open `then` (the neighbour of its tab when it was the active one).
    Remove {
        id: RepoId,
        name: String,
        then: Option<RepoId>,
    },
}

pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    match event {
        AppEvent::SelectedRepositoryUpdated(Some(id)) if !app.store.open_tabs.is_open(id) => {
            // The engine opened something on its own (start): keep the tab bar in sync.
            app.store.open_tabs.open(id.clone());
        }
        _ => {}
    }
}

pub fn show_dialogs(ctx: &egui::Context, app: &mut GitYasApp) {
    picker::show(ctx, app);
    add_dialog::show(ctx, app);
}

/// Top tab bar (34 px), one tab per open repository, `+` opens the picker.
pub fn tab_bar(ui: &mut Ui, app: &mut GitYasApp) {
    tabs::show(ui, app);
}

/// Central area when no tab is open.
pub fn welcome(ui: &mut Ui, app: &mut GitYasApp) {
    welcome::show(ui, app);
}

/// Opens the repository picker popup (Cmd+T, `+`).
pub fn open_picker(app: &mut GitYasApp) {
    app.repos_ui.picker = Some(picker::PickerState::new());
}

/// Opens the add-repository dialog on `tab` (Cmd+O, welcome buttons).
pub fn open_add_dialog(app: &mut GitYasApp, tab: AddTab) {
    app.repos_ui.picker = None;
    match app.repos_ui.add.as_mut() {
        // Don't throw away a running clone or typed input; just switch tabs (unless cloning).
        Some(dialog) => dialog.select_tab(tab),
        None => app.repos_ui.add = Some(add_dialog::AddDialog::new(tab)),
    }
}

/// "Opening <name>…" while a tab switch is pending, for the centre's loading state (the graph module draws the
/// centre once a repository is open: it should show this with a spinner while `store.repo` isn't `is_ready()`).
pub fn opening_label(app: &GitYasApp) -> Option<String> {
    let id = app
        .repos_ui
        .opening
        .as_ref()
        .or_else(|| app.store.repo.as_ref().filter(|r| !r.is_ready()).map(|r| &r.id))?;
    let name = app.store.repository(id).map_or("repository", |r| r.name.as_str());
    Some(format!("Opening {name}…"))
}

/// A tab switch is queued or running and the engine has not switched to that repository yet (until then the store
/// still holds the previous repository, which must not be shown under the new tab).
pub fn switch_pending(app: &GitYasApp) -> bool {
    app.repos_ui
        .opening
        .as_ref()
        .is_some_and(|id| app.store.active.as_ref() != Some(id))
}

/// Opens `id` in a tab (adding the tab if needed), makes it active and persists the tabs.
pub fn switch_to(app: &mut GitYasApp, id: RepoId) {
    app.store.open_tabs.open(id.clone());
    if shown_active(app) == Some(&id) {
        // Already the active repository, or already being opened: nothing to reload.
        persist_tabs(app);
        return;
    }
    enqueue(app, SessionOp::Open(id));
}

/// Cmd+1…9 (index 0-based).
pub fn switch_to_index(app: &mut GitYasApp, index: usize) {
    if let Some(id) = app.store.open_tabs.open.get(index).cloned() {
        switch_to(app, id);
    }
}

/// Ctrl+Tab / Ctrl+Shift+Tab.
pub fn switch_relative(app: &mut GitYasApp, delta: isize) {
    let tabs = &app.store.open_tabs.open;
    let current = shown_active(app).and_then(|id| tabs.iter().position(|t| t == id));
    if let Some(next) = logic::relative_index(tabs.len(), current, delta).and_then(|i| tabs.get(i).cloned()) {
        switch_to(app, next);
    }
}

/// Cmd+W: closes the active tab (not the registration).
pub fn close_active_tab(app: &mut GitYasApp) {
    if let Some(id) = shown_active(app).cloned() {
        close_tab(app, &id);
    }
}

/// The tab shown as active: the one being opened, else the engine's active repository.
fn shown_active(app: &GitYasApp) -> Option<&RepoId> {
    app.repos_ui.opening.as_ref().or(app.store.active.as_ref())
}

/// Closes `id`'s tab (the repository stays registered). The active tab hands over to its neighbour.
fn close_tab(app: &mut GitYasApp, id: &str) {
    sync_active(app);
    let after = logic::close_tab(&mut app.store.open_tabs, id);
    apply_after_close(app, after);
}

fn close_other_tabs(app: &mut GitYasApp, keep: &str) {
    sync_active(app);
    let after = logic::close_others(&mut app.store.open_tabs, keep);
    apply_after_close(app, after);
}

fn apply_after_close(app: &mut GitYasApp, after: AfterClose) {
    match after {
        AfterClose::Nothing => persist_tabs(app),
        AfterClose::Open(next) => enqueue(app, SessionOp::Open(next)),
        AfterClose::CloseSession => enqueue(app, SessionOp::Close),
    }
}

/// Makes `open_tabs.active` the tab the user sees as active before bookkeeping on it.
fn sync_active(app: &mut GitYasApp) {
    let shown = shown_active(app).cloned();
    if shown.as_ref().is_some_and(|id| app.store.open_tabs.is_open(id)) || shown.is_none() {
        app.store.open_tabs.active = shown;
    }
}

/// Moves the tab at `from` to `to` (index after removal) and persists the order.
fn move_tab(app: &mut GitYasApp, from: usize, to: usize) {
    if app.store.open_tabs.move_tab(from, to) {
        persist_tabs(app);
    }
}

/// Asks to unregister `id`; closes its tab first, even when it is the active one.
fn confirm_remove(app: &mut GitYasApp, id: RepoId) {
    let Some(name) = app.store.repository(&id).map(|r| r.name.clone()) else {
        return;
    };
    app.repos_ui.picker = None;
    let title = format!("Remove {name} from GitYas?");
    let body = format!("Remove {name} from GitYas? The folder on disk is not deleted.");
    app.confirm(Confirm::new(title, body, "Remove repository", move |app| {
        remove_repository(app, id, name)
    }));
}

fn remove_repository(app: &mut GitYasApp, id: RepoId, name: String) {
    sync_active(app);
    let then = match logic::close_tab(&mut app.store.open_tabs, &id) {
        AfterClose::Open(next) => Some(next),
        AfterClose::Nothing | AfterClose::CloseSession => None,
    };
    // The removed tab must not be persisted again by a write racing the removal.
    app.repos_ui.queue.retain(|op| op != &SessionOp::Open(id.clone()));
    enqueue(app, SessionOp::Remove { id, name, then });
}

/// Moves the repository at `from` in the registry order to `to` (index after removal); the list updates when
/// `RepositoriesUpdated` arrives (no optimistic reorder).
fn reorder_repository(app: &mut GitYasApp, from: usize, to: usize) {
    let mut ids: Vec<RepoId> = app.store.repositories.iter().map(|r| r.id.clone()).collect();
    if logic::move_item(&mut ids, from, to) {
        app.bridge
            .run_reporting(TASK_REGISTRY, "Could not reorder the repositories", move |engine| async move {
                engine.reorder_repositories(ids).await
            });
    }
}

fn set_provider(app: &mut GitYasApp, id: RepoId, provider: Option<Provider>) {
    app.bridge
        .run_reporting(TASK_REGISTRY, "Could not change the provider", move |engine| async move {
            engine.set_repository_provider(id, provider).await
        });
}

fn reveal(app: &mut GitYasApp, id: &str) {
    let Some(path) = app.store.repository(id).map(|r| r.path.clone()) else {
        return;
    };
    if let Err(err) = gityas_platform::reveal_in_file_manager(&path) {
        app.report_error(&format!("Could not show {}", path.display()), &err);
    }
}

/// Queues a session operation; a newer open / close replaces a queued (not yet started) one.
fn enqueue(app: &mut GitYasApp, op: SessionOp) {
    let ui = &mut app.repos_ui;
    ui.queue.retain(|queued| matches!(queued, SessionOp::Remove { .. }));
    ui.opening = match &op {
        SessionOp::Open(id) => Some(id.clone()),
        SessionOp::Remove { then, .. } => then.clone(),
        SessionOp::Close => None,
    };
    ui.queue.push_back(op);
    run_next(app);
}

fn run_next(app: &mut GitYasApp) {
    if app.bridge.is_running(TASK_SESSION) {
        return;
    }
    let Some(op) = app.repos_ui.queue.pop_front() else {
        return;
    };
    match op {
        SessionOp::Open(id) => {
            let target = id.clone();
            app.bridge.run(
                TASK_SESSION,
                move |engine| async move { engine.open_repository(target).await },
                move |app, result| {
                    if let Err(err) = result {
                        let name = app.store.repository(&id).map_or(id.clone(), |r| r.name.clone());
                        if app.repos_ui.queue.is_empty() {
                            // The engine kept the previous session: show it as active again.
                            app.store.open_tabs.active = app.store.active.clone();
                        }
                        app.report_error(&format!("Could not open {name}"), &err);
                    }
                    session_op_done(app);
                },
            );
        }
        SessionOp::Close => app.bridge.run(
            TASK_SESSION,
            |engine| async move { engine.close_repository().await },
            |app, result| {
                if let Err(err) = result {
                    app.report_error("Could not close the repository", &err);
                }
                session_op_done(app);
            },
        ),
        SessionOp::Remove { id, name, then } => app.bridge.run(
            TASK_SESSION,
            move |engine| async move {
                engine.delete_repository(id).await?;
                if let Some(next) = then {
                    engine.open_repository(next).await?;
                }
                Ok(())
            },
            move |app, result| {
                match result {
                    Ok(()) => app.notify.success(format!("Removed {name} from GitYas")),
                    Err(err) => {
                        if app.repos_ui.queue.is_empty() {
                            app.store.open_tabs.active = app.store.active.clone();
                        }
                        app.report_error(&format!("Could not remove {name}"), &err);
                    }
                }
                session_op_done(app);
            },
        ),
    }
}

fn session_op_done(app: &mut GitYasApp) {
    if app.repos_ui.queue.is_empty() {
        app.repos_ui.opening = None;
        persist_tabs(app);
    } else {
        run_next(app);
    }
}

/// Writes the tab bar (`set_open_tabs`), one write at a time; changes made meanwhile are written afterwards.
fn persist_tabs(app: &mut GitYasApp) {
    if app.mock {
        return;
    }
    if app.bridge.is_running(TASK_PERSIST_TABS) || app.bridge.is_running(TASK_SESSION) {
        // A session operation persists when it finishes (`session_op_done`); a running write is followed by
        // another one.
        app.repos_ui.persist_again = true;
        return;
    }
    app.repos_ui.persist_again = false;
    let tabs = app.store.open_tabs.clone();
    app.bridge.run(
        TASK_PERSIST_TABS,
        move |engine| async move { engine.set_open_tabs(tabs).await },
        |app, result| {
            if let Err(err) = result {
                app.report_error("Could not save the open tabs", &err);
            }
            if std::mem::take(&mut app.repos_ui.persist_again) && !app.bridge.is_running(TASK_SESSION) {
                persist_tabs(app);
            }
        },
    );
}
