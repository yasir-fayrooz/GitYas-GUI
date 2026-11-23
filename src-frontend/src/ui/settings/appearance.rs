//! Appearance page (§12, §15, §16): theme, icon bar position, default diff view, graph options, zoom.
//!
//! Each change goes through `GitYasApp::modify_settings` (an atomic read-modify-write in the engine, so two quick
//! changes, or a change racing a signature save, never overwrite each other); its completion copies the saved
//! appearance fields into `store.settings` and `GitYasApp` then applies the theme.
//!
//! Zoom is egui's zoom factor (not a GitYas setting). egui itself handles Cmd/Ctrl `+` / `=` / `−` / `0` at the end
//! of every pass (`Options::zoom_with_keyboard`, on by default), and eframe persists it with the egui memory.

use egui::{RichText, Ui};
use gityas_core::models::{DiffViewMode, Settings, Theme};

use super::model;
use crate::app::GitYasApp;
use crate::bridge::TaskKey;
use crate::theme;

pub const TASK_APPEARANCE: TaskKey = "settings.appearance";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Theme(Theme),
    DiffView(DiffViewMode),
    ShowRemoteBranches(bool),
    ShowTags(bool),
    ShowStashes(bool),
    MaxCommits(usize),
    /// Seconds; the Git page's background fetch interval (saved like the appearance settings).
    FetchInterval(u64),
}

/// Range of "Max commits shown".
pub const MAX_COMMITS_RANGE: std::ops::RangeInclusive<usize> = 1..=1_000_000;

impl Change {
    pub fn apply(self, settings: &mut Settings) {
        match self {
            Change::Theme(theme) => settings.theme = theme,
            Change::DiffView(mode) => settings.diff_view_mode = mode,
            Change::ShowRemoteBranches(on) => settings.graph.show_remote_branches = on,
            Change::ShowTags(on) => settings.graph.show_tags = on,
            Change::ShowStashes(on) => settings.graph.show_stashes = on,
            Change::MaxCommits(max) => settings.graph.max_commits = max,
            Change::FetchInterval(secs) => settings.fetch_interval_secs = secs,
        }
    }
}

fn theme_label(theme: Theme) -> &'static str {
    match theme {
        Theme::System => "System",
        Theme::Light => "Light",
        Theme::Dark => "Dark",
    }
}

fn diff_label(mode: DiffViewMode) -> &'static str {
    match mode {
        DiffViewMode::Hunk => "Hunk",
        DiffViewMode::Inline => "Inline",
        DiffViewMode::Split => "Split",
    }
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong().size(14.0));
}

pub(super) fn page(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let settings = app.store.settings.clone();
    let mut change = None;

    ui.horizontal(|ui| {
        ui.label(RichText::new("Theme").strong().size(14.0));
        if app.bridge.is_running(TASK_APPEARANCE) {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    ui.horizontal(|ui| {
        for theme in [Theme::System, Theme::Light, Theme::Dark] {
            if ui.radio(settings.theme == theme, theme_label(theme)).clicked() && settings.theme != theme {
                change = Some(Change::Theme(theme));
            }
        }
    });
    if settings.theme == Theme::System {
        ui.label(
            RichText::new("Follows the operating system's light / dark setting.")
                .small()
                .color(p.text_muted),
        );
    }

    section(ui, "Diff viewer");
    ui.horizontal(|ui| {
        ui.label("Default view");
        egui::ComboBox::from_id_salt("settings-diff-view")
            .selected_text(diff_label(settings.diff_view_mode))
            .show_ui(ui, |ui| {
                for mode in [DiffViewMode::Hunk, DiffViewMode::Inline, DiffViewMode::Split] {
                    if ui.selectable_label(settings.diff_view_mode == mode, diff_label(mode)).clicked() && settings.diff_view_mode != mode {
                        change = Some(Change::DiffView(mode));
                    }
                }
            });
    });

    section(ui, "Commit graph");
    let graph = &settings.graph;
    for (on, label, make) in [
        (
            graph.show_remote_branches,
            "Show remote branches",
            Change::ShowRemoteBranches as fn(bool) -> Change,
        ),
        (graph.show_tags, "Show tags", Change::ShowTags),
        (graph.show_stashes, "Show stashes", Change::ShowStashes),
    ] {
        let mut value = on;
        if ui.checkbox(&mut value, label).changed() {
            change = Some(make(value));
        }
    }
    ui.horizontal(|ui| {
        ui.label("Max commits shown");
        let draft = &mut app.settings_ui.max_commits;
        let mut value = draft.unwrap_or(graph.max_commits);
        let resp = ui
            .add(egui::DragValue::new(&mut value).range(MAX_COMMITS_RANGE).speed(20.0))
            .on_hover_text("The newest commits listed in the history; older ones are left out");
        // Saved once the drag / typing ends, not on every step.
        if resp.dragged() || resp.has_focus() {
            *draft = Some(value);
        } else if draft.take().is_some() && value != graph.max_commits {
            change = Some(Change::MaxCommits(value));
        }
    });

    section(ui, "Zoom");
    let ctx = ui.ctx().clone();
    ui.horizontal(|ui| {
        if ui.button("−").on_hover_text("Zoom out (Cmd+−)").clicked() {
            egui::gui_zoom::zoom_out(&ctx);
        }
        let reset = ui.button(model::zoom_label(ctx.zoom_factor())).on_hover_text("Reset zoom (Cmd+0)");
        if reset.clicked() {
            ctx.set_zoom_factor(1.0);
        }
        if ui.button("+").on_hover_text("Zoom in (Cmd++)").clicked() {
            egui::gui_zoom::zoom_in(&ctx);
        }
    });

    if let Some(change) = change {
        save(app, change);
    }
}

pub(super) fn save(app: &mut GitYasApp, change: Change) {
    app.modify_settings(
        TASK_APPEARANCE,
        move |settings| change.apply(settings),
        |app, result| {
            if let Err(err) = result {
                app.report_error("Could not save the settings", &err);
            }
        },
    );
}
