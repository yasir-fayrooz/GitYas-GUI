//! Commit box (§6): summary with a 72-character counter, description, "Commit changes to N files" / "Commit merge",
//! Cmd+Enter, signature check before committing (§12 signature dialog via WP15).

use egui::{Id, Key, RichText, Ui};
use egui_phosphor::regular as icon;

use super::TASK_COMMIT;
use super::model::{self, CommitContext, CounterTone};
use crate::app::GitYasApp;
use crate::{theme, ui};

pub(super) fn show(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let Some(id) = app.store.active.clone() else { return };
    let Some(repo) = app.store.repo.as_ref() else { return };
    let committing = app.bridge.is_running(TASK_COMMIT);
    let has_signature = app.store.signature().is_some();
    let awaiting_signature = app.worktree_ui.awaiting_signature;
    let tab = &mut app.tabs.entry(id).or_default().worktree;

    let summary_id = Id::new("wip_commit_summary");
    let description_id = Id::new("wip_commit_description");

    // Cmd+Enter in either field commits; consumed before the multi-line field would see the Enter.
    let focused = ui.memory(|m| m.has_focus(summary_id) || m.has_focus(description_id));
    let mut commit = focused && ui.input_mut(|i| i.consume_shortcut(&ui::cmd(Key::Enter)));

    let branch = repo.head.as_ref().map_or_else(|| "…".to_string(), |h| h.label());
    let ctx = CommitContext::new(&repo.files, &repo.status, &tab.summary, committing);
    let blocker = ctx.blocker();
    let label = ctx.button_label();
    egui::Frame::new()
        .fill(p.bg_graph)
        .stroke(egui::Stroke::new(1.0, p.border))
        .corner_radius(10)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}  Commit", icon::GIT_COMMIT)).strong());
                ui.add(egui::Label::new(RichText::new(format!("to {}  {branch}", icon::GIT_BRANCH)).color(p.text_muted)).truncate());
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let (remaining, tone) = model::summary_counter(&tab.summary);
                let color = match tone {
                    CounterTone::Normal => p.text_faint,
                    CounterTone::Warn => p.orange,
                    CounterTone::Over => p.red,
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(remaining.to_string()).small().color(color))
                        .on_hover_text("Characters left of the recommended 72");
                    ui.add(
                        egui::TextEdit::singleline(&mut tab.summary)
                            .id(summary_id)
                            .hint_text("Summary")
                            .font(egui::FontId::proportional(14.0))
                            .margin(egui::vec2(8.0, 6.0))
                            .desired_width(f32::INFINITY),
                    );
                });
            });
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::multiline(&mut tab.description)
                    .id(description_id)
                    .hint_text("Description")
                    .margin(egui::vec2(8.0, 6.0))
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(8.0);

            let (fill, text_color) = if blocker.is_none() {
                (p.accent, ui::text_on(p.accent))
            } else {
                (p.bg_hover, p.text_faint)
            };
            let button = egui::Button::new(RichText::new(format!("{}  {label}", icon::CHECK)).color(text_color).strong())
                .fill(fill)
                .corner_radius(8)
                .min_size(egui::vec2(ui.available_width(), 36.0));
            let resp = ui.add_enabled(blocker.is_none(), button);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, blocker.is_none(), &label));
            let resp = match &blocker {
                Some(reason) => resp.on_disabled_hover_text(reason),
                None => resp.on_hover_text("Cmd+Enter"),
            };
            commit |= resp.clicked();
            if !has_signature {
                let hint = if awaiting_signature {
                    "Waiting for your commit signature…"
                } else {
                    "No commit signature set yet; you will be asked for one."
                };
                ui.label(RichText::new(hint).small().color(p.text_muted));
            }
        });

    if commit {
        match blocker {
            None => request(app, false),
            Some(reason) => app.notify.info(reason),
        }
    }
}

/// Commits, asking for a signature first if none is set (then commits once it is saved). `push_after` pushes the
/// current branch once the commit succeeded (speed mode's commit & push).
pub(super) fn request(app: &mut GitYasApp, push_after: bool) {
    if app.store.signature().is_none() {
        app.worktree_ui.awaiting_signature = true;
        ui::settings::request_signature(
            app,
            Some(Box::new(move |app: &mut GitYasApp| {
                app.worktree_ui.awaiting_signature = false;
                run(app, push_after);
            })),
        );
        return;
    }
    run(app, push_after);
}

/// Starts the commit with the active tab's draft. On success the draft is cleared (if unchanged meanwhile) and
/// "Committed a1b2c3d" is shown; events bring the new history and file state.
fn run(app: &mut GitYasApp, push_after: bool) {
    let Some(id) = app.store.active.clone() else { return };
    let Some(repo) = app.store.repo.as_ref() else { return };
    let committing = app.bridge.is_running(TASK_COMMIT);
    let tab = &app.tabs.entry(id.clone()).or_default().worktree;
    // Re-checked here: the retry after the signature dialog runs later, on whatever state is current then.
    if let Some(reason) = CommitContext::new(&repo.files, &repo.status, &tab.summary, committing).blocker() {
        app.notify.info(reason);
        return;
    }
    let summary = tab.summary.clone();
    let description = tab.description.clone();
    let message = summary.trim().to_string();
    let description_arg = model::description_arg(&description);
    app.bridge.run(
        TASK_COMMIT,
        move |e| async move { e.commit_staged_files(message, description_arg).await },
        move |app, result| match result {
            Ok(oid) => {
                if let Some(tab) = app.tabs.get_mut(&id) {
                    let wt = &mut tab.worktree;
                    if wt.summary == summary && wt.description == description {
                        wt.summary.clear();
                        wt.description.clear();
                    }
                    wt.selection.clear();
                }
                app.notify.success(format!("Committed {}", oid.short(7)));
                if push_after {
                    ui::remote::push_current(app);
                }
            }
            Err(err) => app.report_error("Could not commit", &err),
        },
    );
}
