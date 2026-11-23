//! Engine commands behind the history screens' intents (checkout, branch / tag / stash operations) and their
//! feedback (toasts, confirmations). Store state is never updated here: the engine's refresh events do that.

use std::collections::HashMap;

use gityas_core::models::{ResetMode, StashApplyOutcome};
use gityas_core::{GitYasError, Oid};
use gityas_engine::CheckoutTarget;

use super::{Intent, RemoteTags, dialogs};
use crate::app::{GitYasApp, Selection, SidePage};
use crate::dialogs::Confirm;
use crate::ui;

pub const TASK_CHECKOUT: &str = "checkout";
pub const TASK_CREATE_BRANCH: &str = "create_branch";
pub const TASK_DELETE_BRANCH: &str = "delete_branch";
pub const TASK_RENAME_BRANCH: &str = "rename_branch";
pub const TASK_CREATE_TAG: &str = "create_tag";
/// Delete / push tags (local or remote).
pub const TASK_TAG: &str = "tag";
pub const TASK_REMOTE_TAGS: &str = "remote_tags";
pub const TASK_STASH: &str = "stash_op";
pub const TASK_COMMIT_DATA: &str = "commit_data";
pub const TASK_REWORD: &str = "reword_commit";
pub const TASK_RESET: &str = "reset";

pub(super) fn apply(app: &mut GitYasApp, intent: Intent) {
    match intent {
        // The selection's files / details are on the Changes page.
        Intent::Select(selection) => {
            if let Some(tab) = app.tab_mut() {
                tab.selection = selection;
            }
            app.layout.show(SidePage::Changes);
        }
        Intent::Reveal(id) => {
            if let Some(tab) = app.tab_mut() {
                tab.selection = Selection::Commit(id.clone());
                tab.history.scroll_to = Some(id);
            }
        }
        Intent::CheckoutBranch { refname, label } => checkout(app, CheckoutTarget::Branch(refname), label),
        Intent::CheckoutCommit(id) => {
            let label = id.short(7);
            checkout(app, CheckoutTarget::Commit(id), label);
        }
        Intent::CreateBranch { from, label } => dialogs::open_create_branch(app, from, label),
        Intent::CreateTag(on) => super::open_create_tag(app, on),
        Intent::RenameBranch { refname, name } => dialogs::open_rename_branch(app, refname, name),
        Intent::DeleteBranch { refname, name } => confirm_delete_branch(app, refname, name),
        Intent::DeleteRemoteBranch { refname, name } => confirm_delete_remote_branch(app, refname, name),
        Intent::Pull(refname) => ui::remote::pull(app, refname),
        Intent::Push(refname) => ui::remote::push(app, refname),
        Intent::Merge(refname) => ui::remote::merge_into_current(app, refname),
        Intent::PushTag { remote, name } => push_tag(app, remote, name),
        Intent::DeleteTag(name) => confirm_delete_tag(app, name),
        Intent::DeleteRemoteTag { remote, name } => confirm_delete_remote_tag(app, remote, name),
        Intent::LoadRemoteTags => load_remote_tags(app),
        Intent::ApplyStash(id) => stash_op(app, id, StashOp::Apply),
        Intent::PopStash(id) => stash_op(app, id, StashOp::Pop),
        Intent::DeleteStash(id) => confirm_delete_stash(app, id),
        Intent::StashAll => ui::worktree::open_stash_dialog(app, None),
        Intent::OpenDiff(request) => ui::diff::toggle(app, request),
        Intent::RewordCommit { commit, message } => reword(app, commit, message),
        Intent::Solo(refname) => solo(app, refname),
        Intent::Reset {
            commit,
            mode: ResetMode::Soft,
        } => reset(app, commit, ResetMode::Soft),
        Intent::Reset {
            commit,
            mode: ResetMode::Hard,
        } => confirm_hard_reset(app, commit),
    }
}

/// Solos a branch (or shows them all again). A selected commit the solo view hides falls back to the WIP.
fn solo(app: &mut GitYasApp, refname: Option<String>) {
    app.store.set_solo(refname);
    let hidden = match (app.tab().map(|t| &t.selection), app.store.repo.as_ref()) {
        (Some(Selection::Commit(id)), Some(repo)) => repo.commit_index(id).is_none(),
        _ => false,
    };
    if let Some(tab) = app.tab_mut() {
        if hidden {
            tab.selection = Selection::Wip;
        }
        if let Selection::Commit(id) = &tab.selection {
            tab.history.scroll_to = Some(id.clone());
        }
    }
}

/// "main" or "HEAD" (detached): what a reset moves, for labels.
fn reset_subject(app: &GitYasApp) -> String {
    app.store
        .repo
        .as_ref()
        .and_then(|r| r.head_branch())
        .map_or_else(|| "HEAD".to_owned(), |b| b.name.clone())
}

fn confirm_hard_reset(app: &mut GitYasApp, commit: Oid) {
    let body = format!("Reset {} to {}?", reset_subject(app), commit.short(7));
    app.confirm(
        Confirm::new("Hard reset", body, "Reset (discard changes)", move |app| {
            reset(app, commit, ResetMode::Hard)
        })
        .with_warning("Uncommitted changes and commits after it on this branch will be discarded"),
    );
}

fn reset(app: &mut GitYasApp, commit: Oid, mode: ResetMode) {
    let label = format!("{} to {}", reset_subject(app), commit.short(7));
    app.bridge.run(
        TASK_RESET,
        move |engine| async move { engine.reset_to_commit(commit, mode).await },
        move |app, result| match result {
            Ok(()) => app.notify.success(format!("Reset {label}")),
            Err(err) => app.report_error(&format!("Could not reset {label}"), &err),
        },
    );
}

/// Saves an edited commit message. On success the editor closes and the reworded commit (it has a new id) is
/// selected; on failure the draft stays open.
fn reword(app: &mut GitYasApp, commit: Oid, message: String) {
    let label = commit.short(7);
    let c = commit.clone();
    app.bridge.run(
        TASK_REWORD,
        move |engine| async move { engine.reword_commit(c, message).await },
        move |app, result| match result {
            Ok(new) => {
                if app.history_ui.message_edit.as_ref().is_some_and(|e| e.commit == commit) {
                    app.history_ui.message_edit = None;
                }
                if let Some(tab) = app.tab_mut()
                    && tab.selection == Selection::Commit(commit)
                {
                    tab.selection = Selection::Commit(new.clone());
                    tab.history.scroll_to = Some(new.clone());
                }
                app.notify.success(format!("Updated the message of {label} (now {})", new.short(7)));
            }
            Err(err) => app.report_error(&format!("Could not update the message of {label}"), &err),
        },
    );
}

/// Checks out a branch or commit. A refusal because of local changes is `GitYasError::Conflict`, for which
/// `report_error` offers `[Stash changes]`.
pub(super) fn checkout(app: &mut GitYasApp, target: CheckoutTarget, label: String) {
    app.bridge.run(
        TASK_CHECKOUT,
        move |engine| async move { engine.checkout(target).await },
        move |app, result| match result {
            Ok(()) => app.notify.success(format!("Checked out {label}")),
            Err(err) => app.report_error(&format!("Could not check out {label}"), &err),
        },
    );
}

fn confirm_delete_branch(app: &mut GitYasApp, refname: String, name: String) {
    let body = format!("Delete branch {name}?");
    app.confirm(Confirm::new("Delete branch", body, "Delete branch", move |app| {
        delete_branch(app, refname, name, false)
    }));
}

/// Deletes a local branch; if git refuses because it isn't merged, asks again with "Force delete".
fn delete_branch(app: &mut GitYasApp, refname: String, name: String, force: bool) {
    let r = refname.clone();
    app.bridge.run(
        TASK_DELETE_BRANCH,
        move |engine| async move { engine.delete_branch(r, force).await },
        move |app, result| match result {
            Ok(()) => app.notify.success(format!("Deleted branch {name}")),
            Err(GitYasError::NotMerged(_)) if !force => {
                let body = format!("Delete branch {name}?");
                app.confirm(
                    Confirm::new("Delete branch", body, "Force delete", move |app| {
                        delete_branch(app, refname, name, true)
                    })
                    .with_warning("It isn't merged; its commits will be lost"),
                );
            }
            Err(err) => app.report_error(&format!("Could not delete branch {name}"), &err),
        },
    );
}

fn confirm_delete_remote_branch(app: &mut GitYasApp, refname: String, name: String) {
    let body = format!("Delete {name} on the remote? Others may be using it.");
    app.confirm(Confirm::new("Delete remote branch", body, "Delete from remote", move |app| {
        app.bridge.run(
            TASK_DELETE_BRANCH,
            move |engine| async move { engine.delete_branch(refname, false).await },
            move |app, result| match result {
                Ok(()) => app.notify.success(format!("Deleted {name} on the remote")),
                Err(err) => app.report_error(&format!("Could not delete {name} on the remote"), &err),
            },
        );
    }));
}

fn confirm_delete_tag(app: &mut GitYasApp, name: String) {
    let body = format!("Delete tag {name}?");
    app.confirm(Confirm::new("Delete tag", body, "Delete tag", move |app| {
        app.bridge.run(
            TASK_TAG,
            {
                let name = name.clone();
                move |engine| async move { engine.delete_tag(name).await }
            },
            move |app, result| match result {
                Ok(()) => app.notify.success(format!("Deleted tag {name}")),
                Err(err) => app.report_error(&format!("Could not delete tag {name}"), &err),
            },
        );
    }));
}

fn confirm_delete_remote_tag(app: &mut GitYasApp, remote: String, name: String) {
    let body = format!("Delete tag {name} on {remote}? Others may be using it.");
    app.confirm(Confirm::new(
        "Delete remote tag",
        body,
        format!("Delete from {remote}"),
        move |app| {
            let (r, n) = (remote.clone(), name.clone());
            app.bridge.run(
                TASK_TAG,
                move |engine| async move { engine.delete_remote_tag(r, n).await },
                move |app, result| {
                    match result {
                        Ok(()) => app.notify.success(format!("Deleted tag {name} on {remote}")),
                        Err(err) => app.report_error(&format!("Could not delete tag {name} on {remote}"), &err),
                    }
                    load_remote_tags(app);
                },
            );
        },
    ));
}

pub(super) fn push_tag(app: &mut GitYasApp, remote: String, name: String) {
    let (r, n) = (remote.clone(), name.clone());
    app.bridge.run(
        TASK_TAG,
        move |engine| async move { engine.push_tag(r, n).await },
        move |app, result| {
            match result {
                Ok(()) => app.notify.success(format!("Pushed tag {name} to {remote}")),
                Err(err) => app.report_error(&format!("Could not push tag {name} to {remote}"), &err),
            }
            // Only refresh the "on remote" markers if they were shown already (a network round trip).
            if app.tab().is_some_and(|t| !matches!(t.history.remote_tags, RemoteTags::Unknown)) {
                load_remote_tags(app);
            }
        },
    );
}

/// Fetches every remote's tag names (network). Results for a repository that is no longer active are dropped.
pub(super) fn load_remote_tags(app: &mut GitYasApp) {
    let Some(repo) = app.store.repo.as_ref() else { return };
    let remotes = repo.remotes.clone();
    let repo_id = repo.id.clone();
    let Some(tab) = app.tab_mut() else { return };
    if remotes.is_empty() {
        tab.history.remote_tags = RemoteTags::Loaded(HashMap::new());
        return;
    }
    tab.history.remote_tags = RemoteTags::Loading;
    app.bridge.run(
        TASK_REMOTE_TAGS,
        move |engine| async move {
            let mut tags = HashMap::new();
            for remote in remotes {
                let names = engine.remote_tag_names(remote.clone()).await?;
                tags.insert(remote, names);
            }
            Ok(tags)
        },
        move |app, result| {
            let active = app.store.active.as_deref() == Some(repo_id.as_str());
            let Some(tab) = app.tabs.get_mut(&repo_id) else { return };
            tab.history.remote_tags = match result {
                _ if !active => RemoteTags::Unknown,
                Ok(tags) => RemoteTags::Loaded(tags),
                Err(err) => RemoteTags::Failed(err.to_string()),
            };
        },
    );
}

#[derive(Clone, Copy)]
enum StashOp {
    Apply,
    Pop,
}

/// Current `stash@{n}` index and label of the stash with this commit id (indices shift as stashes come and go).
fn stash_index(app: &GitYasApp, id: &Oid) -> Option<(usize, String)> {
    let stash = app.store.repo.as_ref()?.stashes.iter().find(|s| &s.stash.id == id)?;
    Some((stash.stash.index, stash.stash.revspec()))
}

fn stash_op(app: &mut GitYasApp, id: Oid, op: StashOp) {
    let Some((index, label)) = stash_index(app, &id) else {
        app.notify.info("That stash no longer exists");
        return;
    };
    app.bridge.run(
        TASK_STASH,
        move |engine| async move {
            match op {
                StashOp::Apply => engine.apply_stash(index).await,
                StashOp::Pop => engine.pop_stash(index).await,
            }
        },
        move |app, result| match (op, result) {
            (StashOp::Apply, Ok(StashApplyOutcome::Applied)) => app.notify.success(format!("Applied {label}")),
            (StashOp::Pop, Ok(StashApplyOutcome::Applied)) => app.notify.success(format!("Popped {label}")),
            (op, Ok(StashApplyOutcome::Conflicts { paths })) => {
                let kept = matches!(op, StashOp::Pop).then_some(" The stash was kept.").unwrap_or_default();
                app.notify.warning(
                    format!("{label} applied with conflicts"),
                    Some(format!(
                        "{} conflicted file(s); resolve them in the working tree.{kept}",
                        paths.len()
                    )),
                );
            }
            (StashOp::Apply, Err(err)) => app.report_error(&format!("Could not apply {label}"), &err),
            (StashOp::Pop, Err(err)) => app.report_error(&format!("Could not pop {label}"), &err),
        },
    );
}

fn confirm_delete_stash(app: &mut GitYasApp, id: Oid) {
    let Some(repo) = app.store.repo.as_ref() else { return };
    let Some(stash) = repo.stashes.iter().find(|s| s.stash.id == id) else {
        return;
    };
    let body = format!("Delete {} '{}'?", stash.stash.revspec(), stash.stash.message);
    app.confirm(Confirm::new("Delete stash", body, "Delete stash", move |app| {
        let Some((index, label)) = stash_index(app, &id) else { return };
        app.bridge.run(
            TASK_STASH,
            move |engine| async move { engine.delete_stash(index).await },
            move |app, result| match result {
                Ok(()) => app.notify.success(format!("Deleted {label}")),
                Err(err) => app.report_error(&format!("Could not delete {label}"), &err),
            },
        );
    }));
}

/// Starts loading a commit's message and file list (once per commit; cached in `HistoryUi`).
pub(super) fn ensure_commit_data(app: &mut GitYasApp, id: &Oid) {
    if app.mock {
        return;
    }
    if !app.history_ui.bodies.contains_key(id) {
        trim_cache(&mut app.history_ui.bodies);
        app.history_ui.bodies.insert(id.clone(), super::Loadable::Loading);
        let (c, key) = (id.clone(), id.clone());
        app.bridge.run(
            TASK_COMMIT_DATA,
            move |engine| async move { engine.commit_body(c).await },
            move |app, result| {
                let value = match result {
                    Ok(body) => super::Loadable::Ready(body),
                    Err(err) => super::Loadable::Failed(err.to_string()),
                };
                app.history_ui.bodies.insert(key, value);
            },
        );
    }
    if !app.history_ui.files.contains_key(id) {
        trim_cache(&mut app.history_ui.files);
        app.history_ui.files.insert(id.clone(), super::Loadable::Loading);
        let (c, key) = (id.clone(), id.clone());
        app.bridge.run(
            TASK_COMMIT_DATA,
            move |engine| async move { engine.commit_files(c).await },
            move |app, result| {
                let value = match result {
                    Ok(files) => super::Loadable::Ready(files),
                    Err(err) => super::Loadable::Failed(err.to_string()),
                };
                app.history_ui.files.insert(key, value);
            },
        );
    }
}

/// Keeps the per-commit caches bounded (finished entries are dropped wholesale; in-flight ones stay).
fn trim_cache<T>(cache: &mut HashMap<Oid, super::Loadable<T>>) {
    const MAX: usize = 256;
    if cache.len() >= MAX {
        cache.retain(|_, v| matches!(v, super::Loadable::Loading));
    }
}
