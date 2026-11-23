//! §11 Create branch / Rename branch / Create tag dialogs (400 px modals, live validation, Enter / Esc).

use egui::RichText;
use gityas_core::{GitYasError, Oid};

use super::actions::{TASK_CREATE_BRANCH, TASK_CREATE_TAG, TASK_RENAME_BRANCH, push_tag};
use super::model::ref_name_error;
use crate::app::GitYasApp;
use crate::dialogs::{buttons, enter_pressed, modal};
use crate::{theme, ui};

pub(super) struct CreateBranch {
    pub(super) name: String,
    /// Start commit; `None` = HEAD.
    pub(super) from: Option<Oid>,
    pub(super) from_label: String,
    pub(super) checkout: bool,
    /// Error from the engine (inline; cleared on edit).
    pub(super) error: Option<String>,
    focus: bool,
}

pub(super) struct RenameBranch {
    /// Full ref name of the branch being renamed.
    pub(super) refname: String,
    pub(super) old_name: String,
    pub(super) name: String,
    pub(super) error: Option<String>,
    focus: bool,
}

pub(super) struct CreateTag {
    pub(super) name: String,
    pub(super) on: Oid,
    pub(super) on_label: String,
    pub(super) annotated: bool,
    pub(super) message: String,
    pub(super) push: bool,
    /// Remote the tag is pushed to (`origin`, else the first remote); `None` when there are no remotes.
    pub(super) remote: Option<String>,
    pub(super) error: Option<String>,
    focus: bool,
}

/// "a1b2c3d Fix the graph" for a commit in the active history.
fn commit_label(app: &GitYasApp, id: &Oid) -> String {
    let summary = app
        .store
        .repo
        .as_ref()
        .and_then(|r| r.commit_index(id).and_then(|i| r.commits.commits.get(i)))
        .map(|c| c.summary.clone())
        .unwrap_or_default();
    format!("{} {summary}", id.short(7)).trim_end().to_owned()
}

/// `label` names the start point when it is a branch ("Create branch from x…"); otherwise the commit is described.
pub(super) fn open_create_branch(app: &mut GitYasApp, from: Option<Oid>, label: Option<String>) {
    let Some(repo) = app.store.repo.as_ref() else { return };
    let head = repo.head.as_ref();
    if from.is_none() && head.is_none_or(|h| h.is_unborn()) {
        app.notify.info("Commit something first: a branch needs a commit to start from");
        return;
    }
    let from_label = match (&label, &from) {
        (Some(label), _) => label.clone(),
        (None, Some(id)) => commit_label(app, id),
        (None, None) => format!("HEAD ({})", head.map(|h| h.label()).unwrap_or_default()),
    };
    app.history_ui.create_branch = Some(CreateBranch {
        name: String::new(),
        from,
        from_label,
        checkout: true,
        error: None,
        focus: true,
    });
}

pub(super) fn open_rename_branch(app: &mut GitYasApp, refname: String, name: String) {
    app.history_ui.rename_branch = Some(RenameBranch {
        refname,
        old_name: name.clone(),
        name,
        error: None,
        focus: true,
    });
}

pub(super) fn open_create_tag(app: &mut GitYasApp, on: Option<Oid>) {
    let Some(repo) = app.store.repo.as_ref() else { return };
    let Some(on) = on.or_else(|| repo.head_target().cloned()) else {
        app.notify.info("Commit something first: a tag needs a commit");
        return;
    };
    let remote = repo
        .remotes
        .iter()
        .find(|r| r.as_str() == "origin")
        .or_else(|| repo.remotes.first())
        .cloned();
    let on_label = commit_label(app, &on);
    app.history_ui.create_tag = Some(CreateTag {
        name: String::new(),
        on,
        on_label,
        annotated: true,
        message: String::new(),
        push: false,
        remote,
        error: None,
        focus: true,
    });
}

pub(super) fn show(ctx: &egui::Context, app: &mut GitYasApp) {
    if app.history_ui.create_branch.is_some() {
        show_create_branch(ctx, app);
    }
    if app.history_ui.rename_branch.is_some() {
        show_rename_branch(ctx, app);
    }
    if app.history_ui.create_tag.is_some() {
        show_create_tag(ctx, app);
    }
}

/// Live validation of the branch name: `Err(reason)` disables the button (reason shown when the name isn't empty).
fn branch_name_problem(app: &GitYasApp, name: &str) -> Result<(), &'static str> {
    if let Some(problem) = ref_name_error(name) {
        return Err(problem);
    }
    let exists = app
        .store
        .repo
        .as_ref()
        .is_some_and(|r| r.local_branches.iter().any(|b| b.name == name));
    if exists { Err("Already exists") } else { Ok(()) }
}

fn show_create_branch(ctx: &egui::Context, app: &mut GitYasApp) {
    let p = theme::palette(ctx);
    let running = app.bridge.is_running(TASK_CREATE_BRANCH);
    let problem = app
        .history_ui
        .create_branch
        .as_ref()
        .map(|d| branch_name_problem(app, d.name.trim()))
        .unwrap_or(Ok(()));
    let Some(dialog) = app.history_ui.create_branch.as_mut() else {
        return;
    };
    let outcome = modal(ctx, "create_branch", "Create branch", 400.0, |ui| {
        ui.label("Name");
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
        }
        if let Err(problem) = problem
            && !dialog.name.trim().is_empty()
        {
            ui.label(RichText::new(problem).color(p.red));
        }
        if let Some(error) = &dialog.error {
            ui.add(egui::Label::new(RichText::new(error).color(p.red)).wrap());
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("From").color(p.text_muted));
            ui.label(RichText::new(&dialog.from_label).monospace());
        });
        ui.checkbox(&mut dialog.checkout, "Check out after creating");
        let reason = if running { "Creating…" } else { problem.err().unwrap_or("") };
        let (cancel, ok) = buttons(ui, "Create branch", false, problem.is_ok() && !running, reason);
        (cancel, (ok || enter_pressed(ui)) && problem.is_ok() && !running)
    });
    let (cancel, ok) = outcome.inner;
    if cancel || outcome.closed {
        app.history_ui.create_branch = None;
    } else if ok {
        submit_branch(app);
    }
}

fn submit_branch(app: &mut GitYasApp) {
    let Some(dialog) = app.history_ui.create_branch.as_ref() else {
        return;
    };
    let name = dialog.name.trim().to_owned();
    let (from, checkout) = (dialog.from.clone(), dialog.checkout);
    let n = name.clone();
    app.bridge.run(
        TASK_CREATE_BRANCH,
        move |engine| async move { engine.create_branch(n, from, checkout).await },
        move |app, result| match result {
            Ok(_) => {
                app.history_ui.create_branch = None;
                let verb = if checkout { "Created and checked out" } else { "Created" };
                app.notify.success(format!("{verb} branch {name}"));
            }
            // Checkout refused because of local changes: the toast offers [Stash changes].
            Err(err @ GitYasError::Conflict(_)) => {
                app.history_ui.create_branch = None;
                app.report_error(&format!("Could not create branch {name}"), &err);
            }
            Err(err) => match app.history_ui.create_branch.as_mut() {
                Some(dialog) => dialog.error = Some(err.to_string()),
                None => app.report_error(&format!("Could not create branch {name}"), &err),
            },
        },
    );
}

fn show_rename_branch(ctx: &egui::Context, app: &mut GitYasApp) {
    let p = theme::palette(ctx);
    let running = app.bridge.is_running(TASK_RENAME_BRANCH);
    // An unchanged name just keeps the button disabled (no message).
    let (problem, unchanged) = app
        .history_ui
        .rename_branch
        .as_ref()
        .map(|d| {
            let name = d.name.trim();
            if name == d.old_name {
                (Ok(()), true)
            } else {
                (branch_name_problem(app, name), false)
            }
        })
        .unwrap_or((Ok(()), false));
    let Some(dialog) = app.history_ui.rename_branch.as_mut() else {
        return;
    };
    let can_submit = problem.is_ok() && !unchanged && !running;
    let outcome = modal(ctx, "rename_branch", "Rename branch", 400.0, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Branch").color(p.text_muted));
            ui.label(RichText::new(&dialog.old_name).monospace());
        });
        ui.add_space(4.0);
        ui.label("New name");
        let before = dialog.name.clone();
        let resp = ui.add(egui::TextEdit::singleline(&mut dialog.name).desired_width(f32::INFINITY));
        if dialog.focus {
            resp.request_focus();
            // Select the whole name so typing replaces it.
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), resp.id) {
                let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(dialog.name.chars().count()));
                state.cursor.set_char_range(Some(all));
                state.store(ui.ctx(), resp.id);
            }
            dialog.focus = false;
        }
        if dialog.name != before {
            dialog.error = None;
        }
        if let Err(problem) = problem
            && !dialog.name.trim().is_empty()
        {
            ui.label(RichText::new(problem).color(p.red));
        }
        if let Some(error) = &dialog.error {
            ui.add(egui::Label::new(RichText::new(error).color(p.red)).wrap());
        }
        let reason = if running { "Renaming…" } else { problem.err().unwrap_or("") };
        let (cancel, ok) = buttons(ui, "Rename", false, can_submit, reason);
        (cancel, (ok || enter_pressed(ui)) && can_submit)
    });
    let (cancel, ok) = outcome.inner;
    if cancel || outcome.closed {
        app.history_ui.rename_branch = None;
    } else if ok {
        submit_rename_branch(app);
    }
}

fn submit_rename_branch(app: &mut GitYasApp) {
    let Some(dialog) = app.history_ui.rename_branch.as_ref() else {
        return;
    };
    let (refname, old_name) = (dialog.refname.clone(), dialog.old_name.clone());
    let name = dialog.name.trim().to_owned();
    let n = name.clone();
    app.bridge.run(
        TASK_RENAME_BRANCH,
        move |engine| async move { engine.rename_branch(refname, n).await },
        move |app, result| match result {
            Ok(_) => {
                app.history_ui.rename_branch = None;
                app.notify.success(format!("Renamed branch {old_name} to {name}"));
            }
            Err(err) => match app.history_ui.rename_branch.as_mut() {
                Some(dialog) => dialog.error = Some(err.to_string()),
                None => app.report_error(&format!("Could not rename branch {old_name}"), &err),
            },
        },
    );
}

fn tag_problem(app: &GitYasApp, dialog: &CreateTag) -> Result<(), &'static str> {
    let name = dialog.name.trim();
    if let Some(problem) = ref_name_error(name) {
        return Err(problem);
    }
    if app.store.repo.as_ref().is_some_and(|r| r.commits.tag(name).is_some()) {
        return Err("Already exists");
    }
    if dialog.annotated && dialog.message.trim().is_empty() {
        return Err("Enter a message for the annotated tag");
    }
    Ok(())
}

fn show_create_tag(ctx: &egui::Context, app: &mut GitYasApp) {
    let p = theme::palette(ctx);
    let running = app.bridge.is_running(TASK_CREATE_TAG);
    let has_signature = app.store.signature().is_some();
    let problem = app.history_ui.create_tag.as_ref().map(|d| tag_problem(app, d)).unwrap_or(Ok(()));
    let mut set_signature = false;
    let Some(dialog) = app.history_ui.create_tag.as_mut() else { return };
    let outcome = modal(ctx, "create_tag", "Create tag", 400.0, |ui| {
        ui.label("Name");
        let before = (dialog.name.clone(), dialog.message.clone());
        let resp = ui.add(
            egui::TextEdit::singleline(&mut dialog.name)
                .hint_text("v1.0.0")
                .desired_width(f32::INFINITY),
        );
        if dialog.focus {
            resp.request_focus();
            dialog.focus = false;
        }
        if let Err(problem) = problem
            && !dialog.name.trim().is_empty()
        {
            ui.label(RichText::new(problem).color(p.red));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("On").color(p.text_muted));
            ui.label(RichText::new(&dialog.on_label).monospace());
        });
        ui.horizontal(|ui| {
            ui.radio_value(&mut dialog.annotated, false, "Lightweight");
            ui.radio_value(&mut dialog.annotated, true, "Annotated");
        });
        let mut message_focused = false;
        if dialog.annotated {
            ui.label("Message");
            let resp = ui.add(
                egui::TextEdit::multiline(&mut dialog.message)
                    .hint_text("Release notes")
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            message_focused = resp.has_focus();
            if !has_signature {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Annotated tags need your name and email.").color(p.orange));
                    set_signature = ui.link("Set signature…").clicked();
                });
            }
        }
        if let Some(remote) = &dialog.remote {
            ui.checkbox(&mut dialog.push, format!("Push to {remote}"));
        }
        if (dialog.name.clone(), dialog.message.clone()) != before {
            dialog.error = None;
        }
        if let Some(error) = &dialog.error {
            ui.add(egui::Label::new(RichText::new(error).color(p.red)).wrap());
        }
        let reason = if running { "Creating…" } else { problem.err().unwrap_or("") };
        let (cancel, ok) = buttons(ui, "Create tag", false, problem.is_ok() && !running, reason);
        let enter = enter_pressed(ui) && !message_focused;
        (cancel, (ok || enter) && problem.is_ok() && !running)
    });
    let (cancel, ok) = outcome.inner;
    if cancel || outcome.closed {
        app.history_ui.create_tag = None;
    } else if set_signature {
        request_signature_then_submit(app);
    } else if ok {
        submit_tag(app);
    }
}

/// Opens the signature dialog; once a valid signature is saved, the tag is created (if the dialog is still open).
fn request_signature_then_submit(app: &mut GitYasApp) {
    ui::settings::request_signature(
        app,
        Some(Box::new(|app: &mut GitYasApp| {
            if app.history_ui.create_tag.is_some() {
                submit_tag(app);
            }
        })),
    );
}

fn submit_tag(app: &mut GitYasApp) {
    let Some(dialog) = app.history_ui.create_tag.as_ref() else { return };
    if dialog.annotated && app.store.signature().is_none() {
        request_signature_then_submit(app);
        return;
    }
    let name = dialog.name.trim().to_owned();
    let on = dialog.on.clone();
    let message = dialog.annotated.then(|| dialog.message.clone());
    let push_to = dialog.remote.clone().filter(|_| dialog.push);
    let n = name.clone();
    app.bridge.run(
        TASK_CREATE_TAG,
        move |engine| async move { engine.create_tag(on, n, message).await },
        move |app, result| match result {
            Ok(()) => {
                app.history_ui.create_tag = None;
                app.notify.success(format!("Created tag {name}"));
                if let Some(remote) = push_to {
                    push_tag(app, remote, name);
                }
            }
            Err(GitYasError::SignatureRequired(_)) => request_signature_then_submit(app),
            Err(err) => match app.history_ui.create_tag.as_mut() {
                Some(dialog) => dialog.error = Some(err.to_string()),
                None => app.report_error(&format!("Could not create tag {name}"), &err),
            },
        },
    );
}
