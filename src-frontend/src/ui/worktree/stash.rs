//! Stash dialog (§11: message, include untracked) and toolbar Pop (`stash@{0}`).

use egui::RichText;
use gityas_core::models::{Delta, StashApplyOutcome};
use gityas_core::{GitYasError, RepoId};

use super::model::{self, files_word};
use super::{TASK_POP, TASK_STASH, files};
use crate::app::GitYasApp;
use crate::{dialogs, theme, ui};

pub(super) struct StashDialog {
    pub(super) repo: RepoId,
    /// `None` = all changes (`stash_all`), else these files (`stash_files`).
    pub(super) files: Option<Vec<Delta>>,
    pub(super) message: String,
    pub(super) include_untracked: bool,
    focus_requested: bool,
}

pub(super) fn open(app: &mut GitYasApp, files: Option<Vec<Delta>>) {
    let Some(repo) = app.store.active.clone() else { return };
    app.worktree_ui.stash = Some(StashDialog {
        repo,
        files,
        message: String::new(),
        include_untracked: true,
        focus_requested: false,
    });
}

fn title(files: Option<&[Delta]>) -> String {
    match files {
        None => "Stash changes".into(),
        Some(f) => format!("Stash {} {}", f.len(), files_word(f.len())),
    }
}

pub(super) fn show(ctx: &egui::Context, app: &mut GitYasApp) {
    let Some(dialog) = app.worktree_ui.stash.as_mut() else { return };
    let p = theme::palette(ctx);
    let running = app.bridge.is_running(TASK_STASH);
    let changes: Vec<&Delta> = match (&dialog.files, app.store.repo.as_ref()) {
        (Some(f), _) => f.iter().collect(),
        (None, Some(repo)) => files::all_changes(&repo.files),
        (None, None) => Vec::new(),
    };
    let nothing = model::nothing_to_stash(&changes, dialog.include_untracked);
    let outcome = dialogs::modal(ctx, "stash_dialog", &title(dialog.files.as_deref()), 400.0, |ui| {
        ui.label("Message");
        let edit = ui.add(
            egui::TextEdit::singleline(&mut dialog.message)
                .hint_text("Optional (default: WIP on the current branch)")
                .desired_width(f32::INFINITY),
        );
        if !dialog.focus_requested {
            edit.request_focus();
            dialog.focus_requested = true;
        }
        ui.add_space(4.0);
        ui.checkbox(&mut dialog.include_untracked, "Include untracked files");
        if nothing {
            ui.label(RichText::new("There are no changes to stash.").small().color(p.text_muted));
        }
        let enabled = !running && !nothing;
        let reason = if running { "Stashing…" } else { "Nothing to stash" };
        let (cancel, ok) = dialogs::buttons(ui, "Stash", false, enabled, reason);
        (cancel, enabled && (ok || dialogs::enter_pressed(ui)))
    });
    let (cancel, ok) = outcome.inner;
    if cancel || outcome.closed {
        app.worktree_ui.stash = None;
    } else if ok {
        request(app);
    }
}

/// Stashes with the dialog's settings, asking for a signature first (stashes are commits).
fn request(app: &mut GitYasApp) {
    if app.store.signature().is_none() {
        app.worktree_ui.awaiting_signature = true;
        ui::settings::request_signature(
            app,
            Some(Box::new(|app: &mut GitYasApp| {
                app.worktree_ui.awaiting_signature = false;
                run(app);
            })),
        );
        return;
    }
    run(app);
}

fn run(app: &mut GitYasApp) {
    let Some(dialog) = app.worktree_ui.stash.as_ref() else { return };
    let message = model::stash_message_arg(&dialog.message);
    let include_untracked = dialog.include_untracked;
    let files = dialog.files.clone();
    let done_title = title(files.as_deref()).replacen("Stash", "Stashed", 1);
    app.bridge.run(
        TASK_STASH,
        move |e| async move {
            match files {
                None => e.stash_all(message, include_untracked).await,
                Some(files) => e.stash_files(files, message, include_untracked).await,
            }
        },
        move |app, result| match result {
            Ok(()) => {
                app.worktree_ui.stash = None;
                if let Some(tab) = app.tab_mut() {
                    tab.worktree.selection.clear();
                }
                app.notify.success(done_title);
            }
            // "there are no local changes to stash" and other user-level refusals: not an error.
            Err(GitYasError::InvalidInput(message)) => {
                app.worktree_ui.stash = None;
                app.notify.info(capitalize(&message));
            }
            Err(err) => app.report_error("Could not stash changes", &err),
        },
    );
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Toolbar Pop: applies and drops `stash@{0}`; on conflicts the stash is kept (warning).
pub(super) fn pop_latest(app: &mut GitYasApp) {
    let Some(repo) = app.store.repo.as_ref() else { return };
    let Some(latest) = repo.stashes.iter().find(|s| s.stash.index == 0) else {
        app.notify.info("No stashes to pop");
        return;
    };
    let label = format!("{} '{}'", latest.stash.revspec(), latest.stash.message);
    app.bridge.run(
        TASK_POP,
        |e| async move { e.pop_stash(0).await },
        move |app, result| match result {
            Ok(StashApplyOutcome::Applied) => app.notify.success(format!("Popped {label}")),
            Ok(StashApplyOutcome::Conflicts { paths }) => app.notify.warning(
                "Stash applied with conflicts",
                Some(format!(
                    "{label} was kept. Resolve {} conflicted {}, then delete the stash.",
                    paths.len(),
                    files_word(paths.len())
                )),
            ),
            Err(err) => app.report_error("Could not pop the stash", &err),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_capitalize() {
        assert_eq!(title(None), "Stash changes");
        let one = [Delta::new("a", gityas_core::models::DeltaChange::Modified)];
        assert_eq!(title(Some(&one)), "Stash 1 file");
        assert_eq!(capitalize("there are no local changes"), "There are no local changes");
        assert_eq!(capitalize(""), "");
    }
}
