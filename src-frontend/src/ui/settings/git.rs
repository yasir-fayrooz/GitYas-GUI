//! Git page: the background fetch interval and speed mode's shortcuts (GitYas settings), and on Windows
//! `core.autocrlf` in the user's global git config.
//!
//! `core.autocrlf` is not a GitYas setting: the value is read from git config when the page is first shown and
//! written straight to `~/.gitconfig`, so it also applies to the `git` CLI and other tools.

use egui::{RichText, Ui};
use gityas_core::models::{AutoCrlf, MIN_FETCH_INTERVAL_SECS};

use super::appearance::{self, Change};
use crate::app::GitYasApp;
use crate::bridge::TaskKey;
use crate::theme;

pub const TASK_AUTOCRLF: TaskKey = "settings.autocrlf";

/// Longest "Fetch every" (one day).
const MAX_FETCH_INTERVAL_SECS: u64 = 24 * 60 * 60;

#[cfg_attr(not(windows), allow(dead_code))]
fn label(value: AutoCrlf) -> &'static str {
    match value {
        AutoCrlf::True => "true — CRLF in the working tree, LF in commits",
        AutoCrlf::False => "false — no conversion",
        AutoCrlf::Input => "input — convert CRLF to LF on commit only",
    }
}

pub(super) fn page(ui: &mut Ui, app: &mut GitYasApp) {
    fetch_interval(ui, app);
    ui.add_space(12.0);
    crate::ui::speed::keys::settings_section(ui, app);
    #[cfg(windows)]
    {
        ui.add_space(12.0);
        line_endings(ui, app);
    }
}

fn fetch_interval(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Background fetch").strong().size(14.0));
        if app.bridge.is_running(appearance::TASK_APPEARANCE) {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    let saved = app.store.settings.fetch_interval_secs.max(MIN_FETCH_INTERVAL_SECS);
    let mut change = None;
    ui.horizontal(|ui| {
        ui.label("Fetch every");
        let draft = &mut app.settings_ui.fetch_interval;
        let mut value = draft.unwrap_or(saved);
        let resp = ui
            .add(
                egui::DragValue::new(&mut value)
                    .range(MIN_FETCH_INTERVAL_SECS..=MAX_FETCH_INTERVAL_SECS)
                    .speed(1.0)
                    .suffix(" s"),
            )
            .on_hover_text(format!("At least {MIN_FETCH_INTERVAL_SECS} seconds"));
        // Saved once the drag / typing ends, not on every step.
        if resp.dragged() || resp.has_focus() {
            *draft = Some(value);
        } else if draft.take().is_some() && value != saved {
            change = Some(Change::FetchInterval(value));
        }
    });
    ui.label(
        RichText::new(format!(
            "Fetches every remote of the open repository in the background. Minimum {MIN_FETCH_INTERVAL_SECS} seconds."
        ))
        .small()
        .color(p.text_muted),
    );
    if let Some(change) = change {
        appearance::save(app, change);
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn line_endings(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let busy = app.bridge.is_running(TASK_AUTOCRLF);
    if app.settings_ui.autocrlf.is_none() && !busy {
        load(app);
    }

    ui.horizontal(|ui| {
        ui.label(RichText::new("Line endings").strong().size(14.0));
        if busy {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    ui.horizontal(|ui| {
        ui.label("AutoCRLF");
        let current = app.settings_ui.autocrlf;
        let mut picked = None;
        ui.add_enabled_ui(current.is_some() && !busy, |ui| {
            egui::ComboBox::from_id_salt("settings-autocrlf")
                .selected_text(current.map_or("…", AutoCrlf::as_str))
                .show_ui(ui, |ui| {
                    for value in AutoCrlf::ALL {
                        if ui.selectable_label(current == Some(value), label(value)).clicked() && current != Some(value) {
                            picked = Some(value);
                        }
                    }
                });
        });
        if let Some(value) = picked {
            save(app, value);
        }
    });
    ui.label(
        RichText::new("Sets core.autocrlf in your global git config (~/.gitconfig).")
            .small()
            .color(p.text_muted),
    );
}

#[cfg_attr(not(windows), allow(dead_code))]
fn load(app: &mut GitYasApp) {
    app.bridge.run(
        TASK_AUTOCRLF,
        |engine| async move { engine.autocrlf().await },
        |app, result| match result {
            Ok(value) => app.settings_ui.autocrlf = Some(value),
            Err(err) => {
                // Shown as git's default so the page stays usable; saving writes the global file anyway.
                app.settings_ui.autocrlf = Some(AutoCrlf::False);
                app.report_error("Could not read the git config", &err);
            }
        },
    );
}

#[cfg_attr(not(windows), allow(dead_code))]
fn save(app: &mut GitYasApp, value: AutoCrlf) {
    app.bridge.run(
        TASK_AUTOCRLF,
        move |engine| async move { engine.set_global_autocrlf(value).await },
        move |app, result| match result {
            Ok(()) => app.settings_ui.autocrlf = Some(value),
            Err(err) => app.report_error("Could not save core.autocrlf", &err),
        },
    );
}
