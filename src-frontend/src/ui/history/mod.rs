//! History: the commit graph, refs sidebar, commit / stash details, create branch / tag dialogs and branch / tag /
//! stash deletes.
//!
//! Layout: [`sidebar`] (the drawer's Branches page), [`graph`] (centre), [`details`] (the Changes page while a commit /
//! stash is selected) and the dialogs. Screens render from the [`Store`](crate::store::Store) and collect
//! [`Intent`]s while drawing (no `&mut GitYasApp` inside the UI closures); [`run`] then turns each intent into an
//! engine command / dialog / selection change. UI-free logic lives in [`model`].

mod actions;
mod details;
mod dialogs;
mod graph_view;
mod menus;
pub(crate) mod model;
mod sidebar;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use egui::Ui;
use gityas_core::models::{CommitBody, Delta, ResetMode};
use gityas_core::{AppEvent, Oid};

use crate::app::{DiffRequest, GitYasApp, Selection};

/// A value fetched lazily from the engine.
#[derive(Debug, Clone)]
pub enum Loadable<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// Global state of this module (dialogs, popups, lazily loaded commit data).
#[derive(Default)]
pub struct HistoryUi {
    create_branch: Option<dialogs::CreateBranch>,
    rename_branch: Option<dialogs::RenameBranch>,
    create_tag: Option<dialogs::CreateTag>,
    /// Commit messages by id (`commit_body`); commits are immutable, so entries never go stale.
    bodies: HashMap<Oid, Loadable<CommitBody>>,
    /// Changed files by commit id (`commit_files`).
    files: HashMap<Oid, Loadable<Vec<Delta>>>,
    /// The commit message being edited in the details panel, if any.
    message_edit: Option<MessageEdit>,
    /// Whether the selected commit's message can be edited: `(HEAD, commit, answer)` for the last pair asked.
    rewordable: Option<(Oid, Oid, bool)>,
}

impl HistoryUi {
    #[cfg(test)]
    pub fn create_branch_open(&self) -> bool {
        self.create_branch.is_some()
    }
}

/// Draft of a commit's new message (click the message to edit it).
#[derive(Debug, Clone)]
pub struct MessageEdit {
    pub commit: Oid,
    /// The message as loaded, to tell whether anything changed.
    pub original: String,
    pub text: String,
    /// Focus the text field on the next frame (just opened).
    pub focus: bool,
}

/// Remote tag names (`remote_tag_names`, a network round trip): fetched on demand, never per frame.
#[derive(Debug, Clone, Default)]
pub enum RemoteTags {
    #[default]
    Unknown,
    Loading,
    Loaded(HashMap<String, Vec<String>>),
    Failed(String),
}

/// Per-tab state (graph search, sidebar filter, pending scroll, remote tags).
#[derive(Default)]
pub struct HistoryTab {
    /// Graph search (Cmd+F): highlights matches.
    pub search: String,
    pub search_open: bool,
    focus_search: bool,
    /// Sidebar filter.
    pub filter: String,
    /// Scroll the graph to this commit on the next frame (sidebar tag click, parent link).
    pub scroll_to: Option<Oid>,
    pub remote_tags: RemoteTags,
}

/// Something the user asked for while a screen was drawn; applied by [`run`] after drawing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Intent {
    Select(Selection),
    /// Select a commit and scroll the graph to it.
    Reveal(Oid),
    /// Check out a branch by full ref name (`label` names it in toasts).
    CheckoutBranch {
        refname: String,
        label: String,
    },
    CheckoutCommit(Oid),
    CreateBranch {
        from: Option<Oid>,
        label: Option<String>,
    },
    CreateTag(Option<Oid>),
    /// Open the rename dialog for a local branch.
    RenameBranch {
        refname: String,
        name: String,
    },
    DeleteBranch {
        refname: String,
        name: String,
    },
    DeleteRemoteBranch {
        refname: String,
        name: String,
    },
    Pull(String),
    Push(String),
    Merge(String),
    PushTag {
        remote: String,
        name: String,
    },
    DeleteTag(String),
    DeleteRemoteTag {
        remote: String,
        name: String,
    },
    LoadRemoteTags,
    ApplyStash(Oid),
    PopStash(Oid),
    DeleteStash(Oid),
    StashAll,
    OpenDiff(DiffRequest),
    /// Replace a commit's message (HEAD or an ancestor; the commits above it are rewritten).
    RewordCommit {
        commit: Oid,
        message: String,
    },
    /// Show only this branch (full ref name) and its ancestors in the graph; `None` shows every branch again.
    Solo(Option<String>),
    /// Move the checked-out branch to this commit (hard asks for confirmation first).
    Reset {
        commit: Oid,
        mode: ResetMode,
    },
}

/// Applies the intents collected while drawing.
pub(crate) fn run(app: &mut GitYasApp, intents: Vec<Intent>) {
    for intent in intents {
        actions::apply(app, intent);
    }
}

pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    match event {
        // A selected commit / stash that no longer exists (rewritten history, popped stash) falls back to the WIP.
        // A first page (`partial`) may just not reach it yet; the full history follows.
        AppEvent::CommitsUpdated { commits, .. } if !commits.partial => {
            let gone = match (app.tab().map(|t| &t.selection), app.store.repo.as_ref()) {
                (Some(Selection::Commit(id)), Some(repo)) => repo.commit_index(id).is_none(),
                _ => false,
            };
            if gone && let Some(tab) = app.tab_mut() {
                tab.selection = Selection::Wip;
            }
        }
        AppEvent::StashesUpdated { stashes, .. } => {
            let gone = match app.tab().map(|t| &t.selection) {
                Some(Selection::Stash(id)) => !stashes.iter().any(|s| &s.stash.id == id),
                _ => false,
            };
            if gone && let Some(tab) = app.tab_mut() {
                tab.selection = Selection::Wip;
            }
        }
        // Remote tag lists describe the remotes as they were; a new remote list invalidates them.
        AppEvent::RemotesUpdated { .. } => {
            if let Some(tab) = app.tab_mut()
                && matches!(tab.history.remote_tags, RemoteTags::Loaded(_) | RemoteTags::Failed(_))
            {
                tab.history.remote_tags = RemoteTags::Unknown;
            }
        }
        _ => {}
    }
}

pub fn show_dialogs(ctx: &egui::Context, app: &mut GitYasApp) {
    dialogs::show(ctx, app);
}

/// Branches page: LOCAL / REMOTE / TAGS / STASHES.
pub fn sidebar(ui: &mut Ui, app: &mut GitYasApp) {
    let intents = sidebar::show(ui, app);
    run(app, intents);
}

/// Centre: commit graph.
pub fn graph(ui: &mut Ui, app: &mut GitYasApp) {
    let intents = graph_view::show(ui, app);
    run(app, intents);
}

/// Changes page when a commit or stash is selected.
pub fn details(ui: &mut Ui, app: &mut GitYasApp) {
    let intents = details::show(ui, app);
    run(app, intents);
}

/// Opens the create-branch dialog from `from` (`None` = HEAD). Toolbar Branch, Cmd+B, context menus.
pub fn open_create_branch(app: &mut GitYasApp, from: Option<Oid>) {
    dialogs::open_create_branch(app, from, None);
}

/// Opens the create-tag dialog on `on` (`None` = HEAD).
pub fn open_create_tag(app: &mut GitYasApp, on: Option<Oid>) {
    dialogs::open_create_tag(app, on);
}
