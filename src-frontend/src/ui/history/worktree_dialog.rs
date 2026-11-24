//! Add worktree dialog (`git worktree add`): a new or existing branch, and the folder to check it out into.

use std::path::{Path, PathBuf};

use egui::RichText;
use gityas_core::models::WorktreeBranch;

use super::actions::{TASK_WORKTREE, open_worktree};
use super::dialogs::branch_name_problem;
use crate::app::GitYasApp;
use crate::dialogs::{buttons, enter_pressed, modal};
use crate::store::RepoData;
use crate::{platform_ui, theme};

pub(super) struct AddWorktree {
    /// Check out a new branch (`name`) or an existing one (`existing`).
    pub(super) new_branch: bool,
    pub(super) name: String,
    /// Full ref name of the chosen existing branch.
    pub(super) existing: Option<String>,
    pub(super) folder: String,
    /// The folder was typed or chosen: stop deriving it from the branch name.
    folder_edited: bool,
    /// Where suggested folders go (next to the main worktree) and their prefix (its folder name).
    base: Option<PathBuf>,
    prefix: String,
    pub(super) open_after: bool,
    pub(super) error: Option<String>,
    focus: bool,
}

impl AddWorktree {
    /// `<next to the main worktree>/<main folder>-<branch>` for the chosen branch, if there is one.
    fn suggested_folder(&self) -> Option<String> {
        let branch = if self.new_branch {
            self.name.trim()
        } else {
            self.existing.as_deref()?.strip_prefix("refs/heads/")?
        };
        if branch.is_empty() {
            return None;
        }
        let base = self.base.as_ref()?;
        Some(base.join(format!("{}-{}", self.prefix, folder_slug(branch))).display().to_string())
    }

    fn update_folder(&mut self) {
        if !self.folder_edited {
            self.folder = self.suggested_folder().unwrap_or_default();
        }
    }
}

/// A branch name as a folder name: `feature/x y` → `feature-x-y`.
pub(super) fn folder_slug(branch: &str) -> String {
    let slug: String = branch
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut out = String::with_capacity(slug.len());
    for c in slug.chars() {
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    out.trim_matches(['-', '.']).to_owned()
}

/// Local branches (full ref names) not checked out in any worktree: the ones git lets a new worktree check out.
fn free_branches(repo: &RepoData) -> Vec<(String, String)> {
    repo.local_branches
        .iter()
        .filter(|b| !repo.worktrees.iter().any(|w| w.branch.as_deref() == Some(b.name.as_str())))
        .map(|b| (b.refname.clone(), b.name.clone()))
        .collect()
}

/// Opens the dialog; `existing` preselects a local branch (full ref name).
pub(super) fn open(app: &mut GitYasApp, existing: Option<String>) {
    let Some(repo) = app.store.repo.as_ref() else { return };
    if repo.head.as_ref().is_none_or(|h| h.is_unborn()) {
        app.notify.info("Commit something first: a worktree needs a commit to check out");
        return;
    }
    let main = repo
        .worktrees
        .iter()
        .find(|w| w.is_main())
        .map(|w| w.path.clone())
        .or_else(|| app.store.repository(&repo.id).map(|r| r.path.clone()));
    let base = main.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    let prefix = main
        .as_deref()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worktree".to_owned());
    let mut dialog = AddWorktree {
        new_branch: existing.is_none(),
        name: String::new(),
        existing,
        folder: String::new(),
        folder_edited: false,
        base,
        prefix,
        open_after: true,
        error: None,
        focus: true,
    };
    dialog.update_folder();
    app.history_ui.add_worktree = Some(dialog);
}

/// Why the folder can't be used, if it can't.
fn folder_problem(folder: &str) -> Result<PathBuf, &'static str> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Err("Choose a folder");
    }
    let path = PathBuf::from(folder);
    if !path.is_absolute() {
        return Err("Use a full path");
    }
    if path.is_file() {
        return Err("That is a file");
    }
    if path.is_dir() && std::fs::read_dir(&path).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err("The folder isn't empty");
    }
    Ok(path)
}

pub(super) fn show(ctx: &egui::Context, app: &mut GitYasApp) {
    let p = theme::palette(ctx);
    let running = app.bridge.is_running(TASK_WORKTREE);
    let (branches, name_problem) = match (app.store.repo.as_ref(), app.history_ui.add_worktree.as_ref()) {
        (Some(repo), Some(d)) => (
            free_branches(repo),
            if d.new_branch {
                branch_name_problem(app, d.name.trim())
            } else {
                Ok(())
            },
        ),
        _ => return,
    };
    let Some(dialog) = app.history_ui.add_worktree.as_mut() else {
        return;
    };
    let branch_problem = if dialog.new_branch {
        name_problem
    } else if dialog
        .existing
        .as_ref()
        .is_none_or(|r| !branches.iter().any(|(refname, _)| refname == r))
    {
        Err("Choose a branch")
    } else {
        Ok(())
    };
    let folder = folder_problem(&dialog.folder);
    let problem = branch_problem.and(folder.as_ref().map(drop).map_err(|e| *e));
    let can_submit = problem.is_ok() && !running;
    let outcome = modal(ctx, "add_worktree", "Add worktree", 460.0, |ui| {
        ui.label(RichText::new("Check out a branch in its own folder, to work on it without switching branches here.").color(p.text_muted));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let before = dialog.new_branch;
            ui.radio_value(&mut dialog.new_branch, true, "New branch");
            ui.radio_value(&mut dialog.new_branch, false, "Existing branch");
            if dialog.new_branch != before {
                dialog.error = None;
                dialog.focus = dialog.new_branch;
                dialog.update_folder();
            }
        });
        ui.add_space(4.0);
        if dialog.new_branch {
            let before = dialog.name.clone();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut dialog.name)
                    .hint_text("feature/my-branch")
                    .desired_width(f32::INFINITY),
            );
            if dialog.focus {
                resp.request_focus();
                dialog.focus = false;
            }
            if dialog.name != before {
                dialog.error = None;
                dialog.update_folder();
            }
            if let Err(problem) = name_problem
                && !dialog.name.trim().is_empty()
            {
                ui.label(RichText::new(problem).color(p.red));
            }
            ui.label(RichText::new("Starts at HEAD").color(p.text_faint));
        } else if branches.is_empty() {
            ui.label(RichText::new("Every local branch is already checked out in a worktree").color(p.text_faint));
        } else {
            let selected = dialog
                .existing
                .as_ref()
                .and_then(|r| branches.iter().find(|(refname, _)| refname == r))
                .map_or("Choose a branch", |(_, name)| name.as_str());
            let before = dialog.existing.clone();
            egui::ComboBox::from_id_salt("add_worktree_branch")
                .selected_text(selected)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for (refname, name) in &branches {
                        ui.selectable_value(&mut dialog.existing, Some(refname.clone()), name);
                    }
                });
            if dialog.existing != before {
                dialog.error = None;
                dialog.update_folder();
            }
        }
        ui.add_space(8.0);
        ui.label("Folder");
        ui.horizontal(|ui| {
            let choose = egui::Button::new("Choose…");
            let width = ui.available_width() - 80.0;
            let resp = ui.add(
                egui::TextEdit::singleline(&mut dialog.folder)
                    .hint_text("/path/to/new-folder")
                    .desired_width(width),
            );
            if resp.changed() {
                dialog.folder_edited = true;
                dialog.error = None;
            }
            if ui
                .add_enabled(!running, choose)
                .on_hover_text("Choose the folder to put it in")
                .clicked()
            {
                let current = PathBuf::from(dialog.folder.trim());
                let start = current.parent().map(Path::to_path_buf).or_else(|| dialog.base.clone());
                if let Some(parent) = platform_ui::pick_folder("Choose where to put the worktree", start.as_deref()) {
                    // The picked folder holds the worktree; keep the suggested name unless it is empty itself.
                    let name = current.file_name().map(|n| n.to_owned());
                    let empty = std::fs::read_dir(&parent).map(|mut d| d.next().is_none()).unwrap_or(false);
                    dialog.folder = match name {
                        Some(name) if !empty => parent.join(name),
                        _ => parent,
                    }
                    .display()
                    .to_string();
                    dialog.folder_edited = true;
                    dialog.error = None;
                }
            }
        });
        if let Err(problem) = folder
            && !dialog.folder.trim().is_empty()
        {
            ui.label(RichText::new(problem).color(p.red));
        }
        if let Some(error) = &dialog.error {
            ui.add(egui::Label::new(RichText::new(error).color(p.red)).wrap());
        }
        ui.add_space(6.0);
        ui.checkbox(&mut dialog.open_after, "Open it in a new tab");
        let reason = if running { "Adding…" } else { problem.err().unwrap_or("") };
        let (cancel, ok) = buttons(ui, "Add worktree", false, can_submit, reason);
        (cancel, (ok || enter_pressed(ui)) && can_submit)
    });
    let (cancel, ok) = outcome.inner;
    if cancel || outcome.closed {
        app.history_ui.add_worktree = None;
    } else if ok {
        submit(app);
    }
}

fn submit(app: &mut GitYasApp) {
    let Some(dialog) = app.history_ui.add_worktree.as_ref() else {
        return;
    };
    let Ok(path) = folder_problem(&dialog.folder) else { return };
    let branch = if dialog.new_branch {
        WorktreeBranch::New {
            name: dialog.name.trim().to_owned(),
            from: None,
        }
    } else {
        let Some(refname) = dialog.existing.clone() else { return };
        WorktreeBranch::Existing { refname }
    };
    let open_after = dialog.open_after;
    app.bridge.run(
        TASK_WORKTREE,
        move |engine| async move { engine.add_worktree(path, branch).await },
        move |app, result| match result {
            Ok(worktree) => {
                app.history_ui.add_worktree = None;
                app.notify.success(format!("Added worktree {}", worktree.folder_name()));
                if open_after {
                    open_worktree(app, worktree.path);
                }
            }
            Err(err) => match app.history_ui.add_worktree.as_mut() {
                Some(dialog) => dialog.error = Some(err.to_string()),
                None => app.report_error("Could not add the worktree", &err),
            },
        },
    );
}

#[cfg(test)]
mod tests {
    use super::folder_slug;

    #[test]
    fn slugs_are_folder_safe() {
        assert_eq!(folder_slug("feature/x y"), "feature-x-y");
        assert_eq!(folder_slug("fix//double--dash"), "fix-double-dash");
        assert_eq!(folder_slug("v1.2"), "v1.2");
        assert_eq!(folder_slug("/lead/"), "lead");
    }
}
