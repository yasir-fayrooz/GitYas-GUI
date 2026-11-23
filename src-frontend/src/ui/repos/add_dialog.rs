//! Add-repository dialog with the Open local / Clone tabs (reference UI §3).

use std::path::PathBuf;

use egui::{RichText, Ui};
use gityas_core::models::{RemoteOp, RemoteProgress};
use gityas_core::{GitYasError, Provider};
use gityas_engine::Engine;

use super::{AddTab, logic};
use crate::app::GitYasApp;
use crate::{dialogs, platform_ui, theme};

const TASK_ADD: &str = "repos.add";
const TASK_CLONE: &str = "repos.clone";

pub struct AddDialog {
    pub(super) tab: AddTab,
    pub(super) local: LocalForm,
    pub(super) clone: CloneForm,
}

#[derive(Default)]
pub(super) struct LocalForm {
    pub(super) folder: String,
    /// `None` = detect from the remotes.
    pub(super) provider: Option<Provider>,
    /// The user typed or chose something (show "Choose a folder" only then).
    pub(super) touched: bool,
    /// Field message from the engine ("Already added", "Not a git repository"), cleared when the folder changes.
    pub(super) field_error: Option<String>,
    /// Any other failure, shown above the buttons.
    pub(super) error: Option<String>,
}

pub(super) struct CloneForm {
    pub(super) url: String,
    pub(super) into: String,
    /// Optional folder name override (default: from the URL).
    pub(super) name: String,
    /// `None` = detect from the URL.
    pub(super) provider: Option<Provider>,
    pub(super) cloning: bool,
    /// Failure (or cancellation) of the last attempt, shown above the buttons.
    pub(super) error: Option<String>,
    /// `destination_error` of the last computed destination (a filesystem check, not repeated every frame).
    dest_check: Option<(PathBuf, Option<String>)>,
}

impl Default for CloneForm {
    fn default() -> Self {
        Self {
            url: String::new(),
            into: logic::home_dir().map(|h| h.display().to_string()).unwrap_or_default(),
            name: String::new(),
            provider: None,
            cloning: false,
            error: None,
            dest_check: None,
        }
    }
}

impl AddDialog {
    pub fn new(tab: AddTab) -> Self {
        Self {
            tab,
            local: LocalForm::default(),
            clone: CloneForm::default(),
        }
    }

    pub fn select_tab(&mut self, tab: AddTab) {
        if !self.clone.cloning {
            self.tab = tab;
        }
    }
}

/// What the dialog needs from the app while drawing (the dialog itself is borrowed mutably).
struct View<'a> {
    engine: &'a Engine,
    registered: Vec<PathBuf>,
    progress: Option<RemoteProgress>,
    connected: Vec<(Provider, bool)>,
    adding: bool,
}

impl View<'_> {
    fn has_token(&self, provider: Provider) -> bool {
        self.connected.iter().any(|(p, c)| *p == provider && *c)
    }
}

enum Outcome {
    AddLocal {
        path: PathBuf,
        provider: Option<Provider>,
    },
    Clone {
        url: String,
        dest: PathBuf,
        provider: Option<Provider>,
    },
    CancelClone,
    AddToken,
    Close,
}

pub fn show(ctx: &egui::Context, app: &mut GitYasApp) {
    let Some(mut dialog) = app.repos_ui.add.take() else {
        return;
    };
    let engine = app.bridge.engine().clone();
    let view = View {
        engine: &engine,
        registered: app.store.repositories.iter().map(|r| r.path.clone()).collect(),
        progress: app
            .store
            .progress
            .as_ref()
            .filter(|(repo, p)| repo.is_none() && p.op == RemoteOp::Clone)
            .map(|(_, p)| p.clone()),
        connected: app.store.auth.clone(),
        adding: app.bridge.is_running(TASK_ADD),
    };
    let modal = dialogs::modal(ctx, "add_repository", "Add repository", 480.0, |ui| dialog.ui(ui, &view));
    let mut outcome = modal.inner;
    if modal.closed && outcome.is_none() {
        // Esc / ✕ / backdrop: ignored while cloning (the clone has its own Cancel).
        outcome = (!dialog.clone.cloning).then_some(Outcome::Close);
    }
    match outcome {
        Some(Outcome::Close) => return,
        Some(Outcome::AddLocal { path, provider }) => {
            dialog.local.error = None;
            dialog.local.field_error = None;
            add_local(app, path, provider);
        }
        Some(Outcome::Clone { url, dest, provider }) => {
            dialog.clone.error = None;
            dialog.clone.cloning = true;
            clone(app, url, dest, provider);
        }
        Some(Outcome::CancelClone) => {
            // Cancels every running remote operation; the clone's completion then reports `Cancelled`.
            app.bridge.engine().cancel_remote_operation();
        }
        Some(Outcome::AddToken) => {
            app.repos_ui.add = Some(dialog);
            crate::ui::settings::open(app, crate::ui::settings::Page::Credentials);
            return;
        }
        None => {}
    }
    app.repos_ui.add = Some(dialog);
}

fn add_local(app: &mut GitYasApp, path: PathBuf, provider: Option<Provider>) {
    app.bridge.run(
        TASK_ADD,
        move |engine| async move { engine.add_repository(path, provider).await },
        |app, result| match result {
            Ok(repo) => {
                app.repos_ui.add = None;
                app.notify.success(format!("Added {}", repo.name));
                super::switch_to(app, repo.id);
            }
            Err(err) => match app.repos_ui.add.as_mut() {
                Some(dialog) => match logic::add_error_message(&err) {
                    Some(message) => dialog.local.field_error = Some(message),
                    None => dialog.local.error = Some(logic::error_text(&err)),
                },
                None => app.report_error("Could not add the repository", &err),
            },
        },
    );
}

fn clone(app: &mut GitYasApp, url: String, dest: PathBuf, provider: Option<Provider>) {
    let short = logic::short_repo_name(&url);
    app.bridge.run(
        TASK_CLONE,
        move |engine| async move { engine.clone_repository(url, dest, provider).await },
        move |app, result| {
            if app
                .store
                .progress
                .as_ref()
                .is_some_and(|(repo, p)| repo.is_none() && p.op == RemoteOp::Clone)
            {
                app.store.progress = None;
            }
            match result {
                Ok(repo) => {
                    app.repos_ui.add = None;
                    app.notify.success(format!("Cloned {short}"));
                    super::switch_to(app, repo.id);
                }
                Err(err) => match app.repos_ui.add.as_mut() {
                    Some(dialog) => {
                        dialog.clone.cloning = false;
                        dialog.clone.dest_check = None;
                        dialog.clone.error = Some(match err {
                            GitYasError::Cancelled => "Clone cancelled".into(),
                            other => logic::error_text(&other),
                        });
                    }
                    None => app.report_error("Could not clone the repository", &err),
                },
            }
        },
    );
}

impl AddDialog {
    fn ui(&mut self, ui: &mut Ui, view: &View<'_>) -> Option<Outcome> {
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.clone.cloning, |ui| {
                ui.selectable_value(&mut self.tab, AddTab::Local, "Open local");
                ui.selectable_value(&mut self.tab, AddTab::Clone, "Clone");
            });
        });
        ui.add_space(10.0);
        match self.tab {
            AddTab::Local => self.local_ui(ui, view),
            AddTab::Clone => self.clone_ui(ui, view),
        }
    }

    fn local_ui(&mut self, ui: &mut Ui, view: &View<'_>) -> Option<Outcome> {
        let p = theme::pal(ui);
        let form = &mut self.local;
        let mut outcome = None;
        let pre_check = logic::local_folder_error(&form.folder, &view.registered);
        egui::Grid::new("add_local").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Folder");
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(&mut form.folder)
                    .hint_text("~/src/project")
                    .desired_width(300.0);
                let resp = ui.add_enabled(!view.adding, edit);
                if resp.changed() {
                    form.touched = true;
                    form.field_error = None;
                    form.error = None;
                }
                if ui.add_enabled(!view.adding, egui::Button::new("Choose…")).clicked() {
                    let start = Some(logic::expand_home(&form.folder)).filter(|p| p.is_dir());
                    if let Some(path) = platform_ui::pick_folder("Choose a repository folder", start.as_deref()) {
                        form.folder = path.display().to_string();
                        form.touched = true;
                        form.field_error = None;
                        form.error = None;
                    }
                }
            });
            ui.end_row();
            let message = pre_check.clone().or_else(|| form.field_error.clone());
            if let Some(message) = message.filter(|_| form.touched) {
                ui.label("");
                ui.label(RichText::new(message).small().color(p.red));
                ui.end_row();
            }
            ui.label("Provider");
            provider_combo(ui, "local_provider", &mut form.provider, "Detect automatically".into());
            ui.end_row();
        });
        if let Some(error) = &form.error {
            ui.add_space(6.0);
            ui.add(egui::Label::new(RichText::new(error).color(p.red)).wrap());
        }
        let blocked = pre_check.or_else(|| form.field_error.clone());
        let reason = if view.adding {
            "Adding…".to_owned()
        } else {
            blocked.clone().unwrap_or_default()
        };
        let enabled = blocked.is_none() && !view.adding;
        let (cancel, ok) = dialogs::buttons(ui, "Add repository", false, enabled, &reason);
        if cancel {
            outcome = Some(Outcome::Close);
        } else if enabled && (ok || dialogs::enter_pressed(ui)) {
            outcome = Some(Outcome::AddLocal {
                path: logic::expand_home(&form.folder),
                provider: form.provider,
            });
        } else if dialogs::enter_pressed(ui) {
            form.touched = true;
        }
        outcome
    }

    fn clone_ui(&mut self, ui: &mut Ui, view: &View<'_>) -> Option<Outcome> {
        let p = theme::pal(ui);
        let form = &mut self.clone;
        let mut outcome = None;
        let busy = form.cloning;

        // Derived state of the fields.
        let url_problem = logic::url_error(&form.url);
        let into_problem = logic::into_error(&form.into);
        let name_problem = logic::name_error(&form.name);
        let base = logic::expand_home(&form.into);
        let derived = (!form.url.trim().is_empty() && url_problem.is_none() && into_problem.is_none())
            .then(|| view.engine.clone_destination(form.url.trim(), &base));
        let derived_name = derived
            .as_ref()
            .and_then(|d| d.as_ref().ok())
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        let dest: Option<Result<PathBuf, String>> = if form.name.trim().is_empty() {
            derived.map(|d| d.map_err(|_| "Can't tell the repository name from the URL; enter a name".to_owned()))
        } else if name_problem.is_none() && into_problem.is_none() && !form.url.trim().is_empty() {
            Some(Ok(base.join(form.name.trim())))
        } else {
            None
        };
        let dest_problem = match &dest {
            Some(Ok(path)) => {
                if form.dest_check.as_ref().is_none_or(|(checked, _)| checked != path) && !busy {
                    form.dest_check = Some((path.clone(), logic::destination_error(path)));
                }
                form.dest_check.as_ref().and_then(|(_, problem)| problem.clone())
            }
            Some(Err(message)) => Some(message.clone()),
            None => None,
        };
        let detected = view.engine.detect_provider(form.url.trim());
        let effective = form.provider.or(detected);

        egui::Grid::new("add_clone").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("URL");
            let edit = egui::TextEdit::singleline(&mut form.url)
                .hint_text("https://github.com/owner/repo.git")
                .desired_width(360.0);
            if ui.add_enabled(!busy, edit).changed() {
                form.error = None;
            }
            ui.end_row();
            if let Some(problem) = url_problem {
                error_row(ui, problem, p.red);
            }

            ui.label("Into");
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(&mut form.into).hint_text("~/src").desired_width(280.0);
                if ui.add_enabled(!busy, edit).changed() {
                    form.error = None;
                }
                if ui.add_enabled(!busy, egui::Button::new("Choose…")).clicked() {
                    let start = Some(logic::expand_home(&form.into)).filter(|p| p.is_dir());
                    if let Some(path) = platform_ui::pick_folder("Choose where to clone", start.as_deref()) {
                        form.into = path.display().to_string();
                        form.error = None;
                    }
                }
            });
            ui.end_row();
            if let Some(problem) = into_problem.filter(|_| !form.into.trim().is_empty()) {
                error_row(ui, problem, p.red);
            }

            ui.label("Name");
            let hint = derived_name
                .clone()
                .map_or_else(|| "(from URL)".to_owned(), |n| format!("{n}  (from URL)"));
            let edit = egui::TextEdit::singleline(&mut form.name).hint_text(hint).desired_width(360.0);
            if ui.add_enabled(!busy, edit).changed() {
                form.error = None;
            }
            ui.end_row();
            if let Some(problem) = name_problem {
                error_row(ui, problem, p.red);
            }

            ui.label("Provider");
            let detect_label = match detected {
                Some(provider) => format!("{} (detected)", provider.display_name()),
                None => "Detect automatically".to_owned(),
            };
            ui.add_enabled_ui(!busy, |ui| provider_combo(ui, "clone_provider", &mut form.provider, detect_label));
            ui.end_row();
        });

        // SSH remotes sign in with ssh-agent or ~/.ssh keys, never a token.
        let ssh = logic::classify_url(&form.url) == logic::UrlKind::Ssh;
        if let Some(provider) = effective.filter(|p| !ssh && !view.has_token(*p)) {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("No {} token saved —", provider.display_name()))
                        .small()
                        .color(p.text_muted),
                );
                if ui.link(RichText::new("Add token").small()).clicked() {
                    outcome = Some(Outcome::AddToken);
                }
            });
        }
        match (&dest, &dest_problem) {
            (_, Some(problem)) => {
                ui.label(RichText::new(problem).small().color(p.red));
            }
            (Some(Ok(path)), None) => {
                let home = logic::home_dir();
                ui.label(
                    RichText::new(format!("Will clone into {}", logic::display_path(path, home.as_deref())))
                        .small()
                        .color(p.text_muted),
                );
            }
            _ => {}
        }

        if busy {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                match &view.progress {
                    Some(progress) => {
                        let fraction = progress.percent.map_or(0.0, |pc| f32::from(pc) / 100.0);
                        let bar = egui::ProgressBar::new(fraction)
                            .desired_width(340.0)
                            .text(RichText::new(&progress.message).small())
                            .animate(progress.percent.is_none());
                        ui.add(bar);
                    }
                    None => {
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new("Connecting…").small().color(p.text_muted));
                    }
                }
                if ui.button("Cancel clone").clicked() {
                    outcome = Some(Outcome::CancelClone);
                }
            });
        }
        if let Some(error) = &form.error {
            ui.add_space(6.0);
            ui.add(egui::Label::new(RichText::new(error).color(p.red)).wrap());
        }

        let reason = if busy {
            Some("Cloning…")
        } else if form.url.trim().is_empty() {
            Some("Enter an https URL")
        } else {
            url_problem.or(into_problem).or(name_problem)
        };
        let reason = reason.map(str::to_owned).or_else(|| dest_problem.clone());
        let enabled = reason.is_none();
        let (cancel, ok) = dialogs::buttons(ui, "Clone", false, enabled, reason.as_deref().unwrap_or_default());
        if cancel {
            outcome = Some(if busy { Outcome::CancelClone } else { Outcome::Close });
        } else if enabled
            && (ok || dialogs::enter_pressed(ui))
            && let Some(Ok(dest)) = dest
        {
            outcome = Some(Outcome::Clone {
                url: form.url.trim().to_owned(),
                dest,
                provider: form.provider,
            });
        }
        outcome
    }
}

fn error_row(ui: &mut Ui, message: &str, color: egui::Color32) {
    ui.label("");
    ui.label(RichText::new(message).small().color(color));
    ui.end_row();
}

fn provider_combo(ui: &mut Ui, id: &str, provider: &mut Option<Provider>, detect_label: String) {
    let selected = provider.map_or_else(|| detect_label.clone(), |p| p.display_name().to_owned());
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(240.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(provider, None, detect_label);
            for p in Provider::ALL {
                ui.selectable_value(
                    provider,
                    Some(p),
                    format!("{}  {}", super::picker::provider_icon(p), p.display_name()),
                );
            }
        });
}
