//! WP16 — remote ops UI: fetch / pull / push with progress and errors, merge, abort merge, push-without-upstream
//! dialog, the banner (merge in progress, warnings) and the status-bar progress (reference UI §1 banner, §10, §11,
//! §13 abort merge).
//!
//! Texts and decisions are pure functions in [`text`] (unit-tested); this file wires them to the engine and egui.
//!
//! **`store.progress` exception:** the store is otherwise changed only by events, but `RemoteProgress` has no
//! "finished" event, so the completion of each remote command clears `store.progress` (transient UI state). The
//! status bar additionally shows progress only while one of this module's remote tasks is running, so a late
//! progress event can never leave a stale "Fetching…" behind.

#[cfg(test)]
mod tests;
mod text;

use std::path::PathBuf;

use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use gityas_core::models::{FetchSummary, MergeOutcome, RemoteOp, shorten_refname};
use gityas_core::{AppEvent, GitYasError};

use crate::app::GitYasApp;
use crate::dialogs::{self, Confirm};
use crate::notify::{Level, ToastAction};
use crate::theme;
use crate::ui::{merge, settings};

pub use text::PushPlan;
use text::{BannerRow, MergeReport};

/// [`Bridge`](crate::bridge::Bridge) task keys, so the toolbar can show a spinner per operation.
pub const TASK_FETCH: &str = "fetch";
pub const TASK_PULL: &str = "pull";
pub const TASK_PUSH: &str = "push";
pub const TASK_MERGE: &str = "merge";
pub const TASK_ABORT_MERGE: &str = "abort_merge";

/// Remote tasks whose progress the status bar shows.
const REMOTE_TASKS: [&str; 3] = [TASK_FETCH, TASK_PULL, TASK_PUSH];

/// How long the status bar shows "Up to date" after a fetch that found nothing.
const FLASH_SECONDS: f64 = 3.0;

/// Global state (push dialog, status-bar labels).
#[derive(Default)]
pub struct RemoteUi {
    push_dialog: Option<PushDialog>,
    /// What the running remote operation works on, per op ("origin", "main"), for the status-bar label.
    targets: Vec<(RemoteOp, String)>,
    /// Short status-bar message ("Up to date") and when it expires (set on first draw).
    flash: Option<(String, Option<f64>)>,
}

/// §11 "Push branch without upstream" (only when there are several remotes).
#[derive(Debug, Clone)]
struct PushDialog {
    refname: String,
    local: String,
    remotes: Vec<String>,
    remote: String,
    name: String,
    /// Inline error from the engine (e.g. "already exists on the remote").
    error: Option<String>,
    /// Focus the name field on the first frame (§11: text inputs autofocus).
    autofocus: bool,
}

impl RemoteUi {
    fn set_target(&mut self, op: RemoteOp, target: String) {
        self.targets.retain(|(o, _)| *o != op);
        self.targets.push((op, target));
    }

    fn target(&self, op: RemoteOp) -> &str {
        self.targets.iter().find(|(o, _)| *o == op).map_or("", |(_, t)| t.as_str())
    }

    fn flash(&mut self, message: impl Into<String>) {
        self.flash = Some((message.into(), None));
    }
}

pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    match event {
        AppEvent::SelectedRepositoryUpdated(_) => {
            app.remote_ui.push_dialog = None;
            app.remote_ui.flash = None;
        }
        AppEvent::LocalBranchesUpdated { branches, .. } => {
            // The branch the dialog was opened for is gone, or got an upstream elsewhere: the dialog is stale.
            if let Some(dialog) = &app.remote_ui.push_dialog
                && !app.bridge.is_running(TASK_PUSH)
                && !branches.iter().any(|b| b.refname == dialog.refname && !b.has_upstream())
            {
                app.remote_ui.push_dialog = None;
            }
        }
        _ => {}
    }
}

pub fn show_dialogs(ctx: &egui::Context, app: &mut GitYasApp) {
    push_dialog(ctx, app);
}

// ---- Commands ----------------------------------------------------------------------------------------------------

/// Toolbar Fetch / Cmd+Shift+F: fetch all remotes.
pub fn fetch_all(app: &mut GitYasApp) {
    if app.bridge.is_running(TASK_FETCH) {
        return;
    }
    let remotes = app.store.repo.as_ref().map(|r| r.remotes.clone()).unwrap_or_default();
    app.remote_ui.set_target(RemoteOp::Fetch, text::fetch_target(&remotes));
    app.remote_ui.flash = None;
    app.bridge.run(
        TASK_FETCH,
        |engine| async move { engine.fetch(None).await },
        |app, result| {
            finish_remote_op(app);
            match result {
                Ok(summary) => report_fetch(app, &summary),
                Err(err) => report_remote_error(app, "Could not fetch", &err),
            }
        },
    );
}

fn report_fetch(app: &mut GitYasApp, summary: &FetchSummary) {
    match text::fetch_message(summary) {
        Some(message) => app.notify.success(message),
        None if summary.failed.is_empty() => app.remote_ui.flash("Up to date"),
        None => {}
    }
    for (remote, err) in &summary.failed {
        report_remote_error(app, &format!("Could not fetch {remote}"), err);
    }
}

/// Toolbar Pull / Cmd+Shift+L: pull the current branch.
pub fn pull_current(app: &mut GitYasApp) {
    match current_branch(app) {
        Ok(refname) => pull(app, refname),
        Err(reason) => app.notify.info(reason),
    }
}

/// Toolbar Push / Cmd+Shift+P: push the current branch (asks for a remote when it has no upstream and there are
/// several remotes).
pub fn push_current(app: &mut GitYasApp) {
    match current_branch(app) {
        Ok(refname) => push(app, refname),
        Err(reason) => app.notify.info(reason),
    }
}

/// Full ref name of the checked-out branch, or why there is none to pull / push.
fn current_branch(app: &GitYasApp) -> Result<String, &'static str> {
    let repo = app.store.repo.as_ref().ok_or("Open a repository first")?;
    match &repo.head {
        None => Err("The repository is still loading"),
        Some(head) if head.is_detached() => Err("HEAD is detached; check out a branch first"),
        Some(head) if head.is_unborn() => Err("No commits yet"),
        Some(head) => head
            .refname()
            .map(str::to_owned)
            .ok_or("HEAD is detached; check out a branch first"),
    }
}

/// Pull a local branch by full ref name (sidebar menu, HEAD only).
pub fn pull(app: &mut GitYasApp, refname: String) {
    if app.bridge.is_running(TASK_PULL) {
        return;
    }
    let local = shorten_refname(&refname).to_owned();
    let branch = app
        .store
        .repo
        .as_ref()
        .and_then(|r| r.local_branches.iter().find(|b| b.refname == refname));
    let upstream = branch.and_then(|b| b.upstream.clone());
    if branch.is_some() && upstream.is_none() {
        let push_ref = refname.clone();
        app.notify.push(
            Level::Warning,
            format!("{local} has no upstream branch"),
            Some("Push it first to publish it and set an upstream.".into()),
            Some(ToastAction::new("Push", move |app| push(app, push_ref))),
        );
        return;
    }
    let upstream = upstream.unwrap_or_else(|| "its upstream".into());
    app.remote_ui.set_target(RemoteOp::Pull, local.clone());
    let retry = refname.clone();
    app.bridge.run(
        TASK_PULL,
        move |engine| async move { engine.pull(refname).await },
        move |app, result| {
            finish_remote_op(app);
            match result {
                Ok(outcome) => report_merge(app, &outcome.merge, &upstream, &local),
                Err(GitYasError::SignatureRequired(_)) => {
                    settings::request_signature(app, Some(Box::new(move |app: &mut GitYasApp| pull(app, retry))));
                }
                Err(err) => report_remote_error(app, &format!("Could not pull {local}"), &err),
            }
        },
    );
}

/// Push a local branch by full ref name (sidebar menu).
pub fn push(app: &mut GitYasApp, refname: String) {
    if app.bridge.is_running(TASK_PUSH) {
        return;
    }
    let local = shorten_refname(&refname).to_owned();
    let (has_upstream, remotes, upstream, behind) = match app.store.repo.as_ref() {
        Some(repo) => {
            let branch = repo.local_branches.iter().find(|b| b.refname == refname);
            let has_upstream = (repo.loaded.local_branches && repo.loaded.remotes).then(|| branch.is_some_and(|b| b.has_upstream()));
            let behind = branch.filter(|b| b.has_upstream() && !b.upstream_gone).map_or(0, |b| b.behind);
            (has_upstream, repo.remotes.clone(), branch.and_then(|b| b.upstream.clone()), behind)
        }
        None => (None, Vec::new(), None, 0),
    };
    match text::push_plan(has_upstream, &remotes) {
        PushPlan::NoRemote => {
            app.notify.push(
                Level::Warning,
                "This repository has no remotes",
                Some("Add a remote with git, then push again.".into()),
                None,
            );
        }
        PushPlan::Dialog { default } => {
            app.remote_ui.push_dialog = Some(PushDialog {
                refname,
                name: local.clone(),
                local,
                remotes,
                remote: default,
                error: None,
                autofocus: true,
            });
        }
        PushPlan::Push => {
            app.remote_ui.set_target(RemoteOp::Push, local.clone());
            // Name used in the rejection toast: the upstream, or where the engine pushes a new branch.
            let default_remote = remotes
                .iter()
                .find(|r| r.as_str() == "origin")
                .or(remotes.first())
                .cloned()
                .unwrap_or_else(|| "origin".into());
            let upstream = upstream.unwrap_or_else(|| format!("{default_remote}/{local}"));
            if behind > 0 {
                // The upstream has commits this branch doesn't (e.g. after a reset): a normal push would be
                // rejected, so offer to overwrite the remote branch instead.
                confirm_force_push(app, refname, local, upstream, behind);
            } else {
                run_push(app, refname, local, upstream, false);
            }
        }
    }
}

fn confirm_force_push(app: &mut GitYasApp, refname: String, local: String, upstream: String, behind: usize) {
    let body = text::force_push_body(&local, &upstream, behind);
    let warning = text::force_push_warning(&upstream, behind);
    app.confirm(
        Confirm::new("Force push", body, "Force push", move |app| {
            run_push(app, refname, local, upstream, true)
        })
        .with_warning(warning),
    );
}

fn run_push(app: &mut GitYasApp, refname: String, local: String, upstream: String, force: bool) {
    app.remote_ui.set_target(RemoteOp::Push, local.clone());
    let pull_ref = refname.clone();
    app.bridge.run(
        TASK_PUSH,
        move |engine| async move { engine.push(refname, force).await },
        move |app, result| {
            finish_remote_op(app);
            match result {
                Ok(outcome) => {
                    let remote_branch = shorten_refname(&outcome.remote_ref).to_owned();
                    let message = text::push_message(&local, &outcome.remote, &remote_branch, outcome.upstream_set);
                    app.notify.success(if force {
                        message.replacen("Pushed", "Force pushed", 1)
                    } else {
                        message
                    });
                }
                Err(err) => report_push_error(app, &local, &upstream, pull_ref, &err),
            }
        },
    );
}

fn report_push_error(app: &mut GitYasApp, local: &str, upstream: &str, refname: String, err: &GitYasError) {
    if matches!(err, GitYasError::PushRejected(_)) {
        app.notify.error_with_action(
            text::rejected_title(upstream),
            err,
            ToastAction::new("Pull", move |app| pull(app, refname)),
        );
    } else {
        report_remote_error(app, &format!("Could not push {local}"), err);
    }
}

/// Merge `source` (full ref name, local or remote branch) into the current branch (sidebar menu).
pub fn merge_into_current(app: &mut GitYasApp, source: String) {
    if app.bridge.is_running(TASK_MERGE) {
        return;
    }
    let target = match current_branch(app) {
        Ok(target) => target,
        Err(reason) => {
            app.notify.info(reason);
            return;
        }
    };
    let source_name = shorten_refname(&source).to_owned();
    let target_name = shorten_refname(&target).to_owned();
    let retry = source.clone();
    app.bridge.run(
        TASK_MERGE,
        move |engine| async move { engine.merge(source, target).await },
        move |app, result| match result {
            Ok(outcome) => report_merge(app, &outcome, &source_name, &target_name),
            Err(GitYasError::SignatureRequired(_)) => {
                settings::request_signature(app, Some(Box::new(move |app: &mut GitYasApp| merge_into_current(app, retry))));
            }
            Err(err) => app.report_error(&format!("Could not merge {source_name} into {target_name}"), &err),
        },
    );
}

/// Toast for a merge / pull outcome; conflicts also show the banner and WP12's conflicted section (both driven by
/// the `RepoStatusUpdated` / `GitFilesChanged` events the engine emits).
fn report_merge(app: &mut GitYasApp, outcome: &MergeOutcome, source: &str, target: &str) {
    match text::merge_report(outcome, source, target) {
        MergeReport::Done { title, info: true } => app.notify.info(title),
        MergeReport::Done { title, info: false } => app.notify.success(title),
        MergeReport::Conflicts { title, first } => {
            let action = first.map(|path: PathBuf| ToastAction::new("Resolve", move |app| merge::open(app, path)));
            app.notify.push(
                Level::Warning,
                title,
                Some("Resolve them in the conflicted files list, then commit; or abort the merge.".into()),
                action,
            );
        }
    }
}

/// Banner `[Abort merge]` (confirmed, §13).
pub fn abort_merge(app: &mut GitYasApp) {
    app.confirm(Confirm::new(
        "Abort merge",
        "Abort the merge? Files changed by the merge are restored; your other changes are kept.",
        "Abort merge",
        |app| {
            app.bridge.run(
                TASK_ABORT_MERGE,
                |engine| async move { engine.abort_merge().await },
                |app, result| match result {
                    Ok(outcome) => app.notify.push(Level::Success, "Merge aborted", text::abort_detail(&outcome), None),
                    Err(err) => app.report_error("Could not abort the merge", &err),
                },
            );
        },
    ));
}

/// Clears the transient progress of a finished remote operation (see the module docs).
fn finish_remote_op(app: &mut GitYasApp) {
    if app.store.progress.as_ref().is_some_and(|(_, p)| p.op != RemoteOp::Clone) {
        app.store.progress = None;
    }
}

/// Error toast for a remote operation; authentication failures get a provider-specific title ("GitHub rejected the
/// token") and `[Open credentials]` (from `report_error`). `Cancelled` is silent; the status bar says "Cancelled".
fn report_remote_error(app: &mut GitYasApp, fallback: &str, err: &GitYasError) {
    if matches!(err, GitYasError::Cancelled) {
        app.remote_ui.flash("Cancelled");
        return;
    }
    let provider = app.store.active_repository().and_then(|r| r.provider);
    let connected = provider.is_some_and(|p| app.store.is_connected(p));
    let title = text::auth_title(err, provider, connected, fallback);
    app.report_error(&title, err);
}

// ---- Push dialog ---------------------------------------------------------------------------------------------------

fn push_dialog(ctx: &egui::Context, app: &mut GitYasApp) {
    let Some(mut dialog) = app.remote_ui.push_dialog.take() else {
        return;
    };
    let running = app.bridge.is_running(TASK_PUSH);
    let p = theme::palette(ctx);
    let title = format!("Push {}", dialog.local);
    let outcome = dialogs::modal(ctx, "remote_push_dialog", &title, 400.0, |ui| {
        ui.label(RichText::new(format!("{} has no upstream branch yet. Choose where to publish it.", dialog.local)).color(p.text_muted));
        ui.add_space(8.0);
        egui::Grid::new("push_dialog_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Remote");
                egui::ComboBox::from_id_salt("push_dialog_remote")
                    .selected_text(dialog.remote.as_str())
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        for remote in &dialog.remotes {
                            ui.selectable_value(&mut dialog.remote, remote.clone(), remote.as_str());
                        }
                    });
                ui.end_row();
                ui.label("Remote branch");
                let field = ui.add(
                    egui::TextEdit::singleline(&mut dialog.name)
                        .desired_width(240.0)
                        .hint_text("branch name"),
                );
                if field.changed() {
                    dialog.error = None;
                }
                if std::mem::take(&mut dialog.autofocus) {
                    field.request_focus();
                }
                ui.end_row();
            });
        let invalid = text::validate_remote_branch_name(&dialog.name).err();
        if let Some(message) = dialog.error.as_deref().or(invalid.filter(|_| !dialog.name.is_empty())) {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(message).color(p.red)).wrap());
        }
        let enabled = invalid.is_none() && !running;
        let reason = if running { "Pushing…" } else { invalid.unwrap_or("") };
        let (cancel, ok) = dialogs::buttons(ui, "Push", false, enabled, reason);
        (cancel, enabled && (ok || dialogs::enter_pressed(ui)))
    });
    let (cancel, ok) = outcome.inner;
    if (cancel || outcome.closed) && !running {
        return;
    }
    if ok {
        start_publish(app, &dialog);
    }
    app.remote_ui.push_dialog = Some(dialog);
}

fn start_publish(app: &mut GitYasApp, dialog: &PushDialog) {
    let (refname, remote, name) = (dialog.refname.clone(), dialog.remote.clone(), dialog.name.trim().to_owned());
    let local = dialog.local.clone();
    app.remote_ui.set_target(RemoteOp::Push, local.clone());
    let (msg_remote, msg_name) = (remote.clone(), name.clone());
    app.bridge.run(
        TASK_PUSH,
        move |engine| async move { engine.publish_branch(refname, remote, name).await },
        move |app, result| {
            finish_remote_op(app);
            match result {
                Ok(_) => {
                    app.remote_ui.push_dialog = None;
                    app.notify.success(text::push_message(&local, &msg_remote, &msg_name, true));
                }
                // Validation-type failures stay in the dialog (inline), everything else is a toast.
                Err(GitYasError::InvalidInput(message) | GitYasError::AlreadyExists(message)) => {
                    if let Some(dialog) = app.remote_ui.push_dialog.as_mut() {
                        dialog.error = Some(message);
                    }
                }
                Err(err) => {
                    app.remote_ui.push_dialog = None;
                    let upstream = format!("{msg_remote}/{msg_name}");
                    let refname = format!("refs/heads/{local}");
                    report_push_error(app, &local, &upstream, refname, &err);
                }
            }
        },
    );
}

// ---- Banner and status bar ---------------------------------------------------------------------------------------

/// Full-width banner under the toolbar (merge / rebase in progress, `app.warnings`). Draws nothing when not needed.
pub fn banner(ui: &mut Ui, app: &mut GitYasApp) {
    let repo = app.store.repo.as_ref().filter(|r| r.loaded.status);
    let head = repo.and_then(|r| r.head.as_ref()).and_then(|h| h.branch_name()).map(str::to_owned);
    let rows = text::banner_rows(repo.map(|r| &r.status), head.as_deref(), repo.map(|r| &r.files), &app.warnings);
    if rows.is_empty() {
        return;
    }
    let p = theme::pal(ui);
    const ROW: f32 = 28.0;
    let mut dismiss = None;
    let mut abort = false;
    let mut credentials = false;
    let aborting = app.bridge.is_running(TASK_ABORT_MERGE);
    egui::Panel::top("remote_banner")
        .exact_size(ROW * rows.len() as f32)
        .frame(
            egui::Frame::new()
                .fill(p.banner_bg)
                .inner_margin(egui::Margin::symmetric(12, 0))
                .stroke(egui::Stroke::new(1.0, p.border)),
        )
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for row in &rows {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), ROW),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| match row {
                        BannerRow::Merge(message) => crate::ui::trailing_row(
                            ui,
                            |ui| {
                                let resp = crate::ui::solid_button(ui, "Abort merge", p.red, !aborting);
                                if resp.on_hover_text("Abort the merge and restore the files it changed").clicked() {
                                    abort = true;
                                }
                            },
                            |ui| {
                                ui.label(RichText::new(icon::WARNING).color(p.orange));
                                ui.add(egui::Label::new(RichText::new(message).color(p.text)).truncate())
                                    .on_hover_text(message);
                            },
                        ),
                        BannerRow::Operation(message) => {
                            ui.label(RichText::new(icon::INFO).color(p.accent));
                            ui.add(egui::Label::new(RichText::new(message).color(p.text)).truncate())
                                .on_hover_text(message);
                        }
                        BannerRow::Warning {
                            index,
                            text,
                            credentials: offer,
                        } => crate::ui::trailing_row(
                            ui,
                            |ui| {
                                let close = crate::ui::icon_button(ui, icon::X, "Dismiss warning");
                                close.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Dismiss warning"));
                                if close.clicked() {
                                    dismiss = Some(*index);
                                }
                                if *offer && ui.button("Open credentials").clicked() {
                                    credentials = true;
                                }
                            },
                            |ui| {
                                ui.label(RichText::new(icon::WARNING).color(p.orange));
                                ui.add(egui::Label::new(RichText::new(text).color(p.text)).truncate())
                                    .on_hover_text(text);
                            },
                        ),
                    },
                );
            }
        });
    if let Some(index) = dismiss
        && index < app.warnings.len()
    {
        app.warnings.remove(index);
    }
    if abort {
        abort_merge(app);
    }
    if credentials {
        settings::open(app, settings::Page::Credentials);
    }
}

/// Right side of the status bar: running remote operation with progress and `[Cancel]` (laid out right to left).
pub fn status_progress(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let running: Vec<RemoteOp> = REMOTE_TASKS
        .iter()
        .zip([RemoteOp::Fetch, RemoteOp::Pull, RemoteOp::Push])
        .filter(|(key, _)| app.bridge.is_running(key))
        .map(|(_, op)| op)
        .collect();
    if let Some(&first) = running.first() {
        // The latest progress report, if it belongs to a running operation; else the first running one.
        let progress = app.store.progress.as_ref().map(|(_, p)| p).filter(|pr| running.contains(&pr.op));
        let op = progress.map_or(first, |pr| pr.op);
        let label = text::progress_label(op, app.remote_ui.target(op), progress.and_then(|pr| pr.percent));
        if ui
            .small_button("Cancel")
            .on_hover_text("Cancel the running remote operation")
            .clicked()
        {
            let signalled = app.bridge.engine().cancel_remote_operation();
            tracing::debug!(signalled, "cancel remote operation");
        }
        let resp = ui.label(RichText::new(label).small().color(p.text_muted));
        if let Some(pr) = progress.filter(|pr| !pr.message.is_empty()) {
            resp.on_hover_text(&pr.message);
        }
        ui.add(egui::Spinner::new().size(12.0).color(p.accent));
        return;
    }
    let now = ui.input(|i| i.time);
    if let Some((message, expires)) = app.remote_ui.flash.as_mut() {
        let until = *expires.get_or_insert(now + FLASH_SECONDS);
        if now < until {
            ui.label(RichText::new(format!("{}  {message}", icon::CHECK)).small().color(p.text_muted));
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64((until - now).max(0.05)));
        } else {
            app.remote_ui.flash = None;
        }
    }
}
