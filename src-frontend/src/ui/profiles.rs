//! Profiles: the context bar's profile switcher, the New / Rename profile dialogs, and switching profile.
//!
//! A profile is its own set of repositories, settings, commit signature and git credentials
//! (`gityas_engine::ProfileStore`). Switching profile replaces the running engine with one opened on the other
//! profile's data and rebuilds the app state from it, like a fresh launch without the splash.

use std::sync::{Arc, Mutex, PoisonError};

use egui::{Align2, FontId, RichText, Sense, Ui, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;
use gityas_core::GitYasError;
use gityas_engine::{CarryOver, DEFAULT_PROFILE_ID, Engine, MAX_PROFILE_NAME, Profile, ProfileStore, Profiles};

use crate::app::{GitYasApp, Launch};
use crate::bridge::{Bridge, EguiEventSink, TaskKey};
use crate::dialogs::{self, Confirm};
use crate::theme;

pub const POPUP_W: f32 = 300.0;
const TASK_CREATE: TaskKey = "profile_create";
const NO_HOST: &str = "Profiles are not available in this mode";

/// What switching profile needs: the registry, the runtime to open engines on, and the slot holding the running
/// engine (shared with `main`, which shuts down whichever engine is current when the window closes).
#[derive(Clone)]
pub struct ProfileHost {
    pub store: ProfileStore,
    pub runtime: tokio::runtime::Handle,
    pub current: Arc<Mutex<Engine>>,
    /// Fill a missing commit signature from the global git config when a profile's engine starts. `false` in tests:
    /// they must not read this machine's git config.
    pub seed_signature: bool,
}

#[derive(Default)]
pub struct ProfilesUi {
    /// `None` with `--mock` and in tests: one Default profile, nothing to switch to.
    host: Option<ProfileHost>,
    list: Profiles,
    /// Switch to this profile at the start of the next frame (never mid-frame: it replaces the whole app state).
    pending_switch: Option<String>,
    dialog: Option<Dialog>,
}

impl ProfilesUi {
    pub fn new(host: Option<ProfileHost>) -> Self {
        let list = host.as_ref().map(|h| h.store.load()).unwrap_or_default();
        Self {
            host,
            list,
            pending_switch: None,
            dialog: None,
        }
    }

    /// [`ProfileHost::seed_signature`]; `false` without a host (`--mock`, tests).
    pub fn seeds_signature(&self) -> bool {
        self.host.as_ref().is_some_and(|h| h.seed_signature)
    }

    pub fn active_name(&self) -> &str {
        &self.list.active_profile().name
    }
}

enum Dialog {
    New(NewProfile),
    Rename { id: String, name: String, error: Option<String> },
}

struct NewProfile {
    name: String,
    fresh: bool,
    carry: CarryOver,
    switch_now: bool,
    error: Option<String>,
    focused: bool,
}

impl NewProfile {
    fn new() -> Self {
        Self {
            name: String::new(),
            fresh: false,
            carry: CarryOver::EVERYTHING,
            switch_now: true,
            error: None,
            focused: false,
        }
    }
}

/// The message of an input error, without the "Invalid input:" prefix.
fn message(err: &GitYasError) -> String {
    match err {
        GitYasError::InvalidInput(m) | GitYasError::AlreadyExists(m) => m.clone(),
        other => other.to_string(),
    }
}

/// Contents of the profile switcher's popup: New profile, then every profile (click to switch; hover to rename or
/// delete).
pub fn menu(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let enabled = app.profiles_ui.host.is_some();
    let mut new = false;
    crate::ui::trailing_row(
        ui,
        |ui| {
            new = ui
                .add_enabled(
                    enabled,
                    egui::Button::new(RichText::new(format!("{}  New profile…", icon::PLUS)).size(12.0)),
                )
                .on_disabled_hover_text(NO_HOST)
                .clicked();
        },
        |ui| {
            ui.label(RichText::new("PROFILES").size(10.5).color(p.text_faint));
        },
    );
    ui.add_space(4.0);

    let mut switch_to = None;
    let mut rename = None;
    let mut delete = None;
    let active = app.profiles_ui.list.active.clone();
    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for profile in &app.profiles_ui.list.profiles {
            let is_active = profile.id == active;
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), Sense::click());
            let label = if is_active {
                format!("{} profile (active)", profile.name)
            } else {
                format!("Switch to the {} profile", profile.name)
            };
            resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, enabled || is_active, is_active, &label));
            let hovered = ui.rect_contains_pointer(rect);
            let painter = ui.painter();
            if hovered {
                painter.rect_filled(rect, 6, p.bg_hover);
            }
            let (glyph, color) = if is_active {
                (icon::CHECK, p.green)
            } else {
                (icon::USER_CIRCLE, p.text_faint)
            };
            painter.text(
                rect.left_center() + egui::vec2(10.0, 0.0),
                Align2::LEFT_CENTER,
                glyph,
                FontId::proportional(14.0),
                color,
            );
            let buttons_w = if hovered && enabled { 56.0 } else { 0.0 };
            crate::ui::paint_truncated(
                painter,
                rect.left_center() + egui::vec2(32.0, 0.0),
                &profile.name,
                FontId::proportional(13.5),
                if is_active { p.text } else { p.text_muted },
                rect.width() - 40.0 - buttons_w,
            );
            if hovered && enabled {
                let size = egui::vec2(24.0, 24.0);
                let pencil = egui::Rect::from_center_size(rect.right_center() - egui::vec2(44.0, 0.0), size);
                let trash = egui::Rect::from_center_size(rect.right_center() - egui::vec2(16.0, 0.0), size);
                let rename_resp = ui
                    .put(
                        pencil,
                        egui::Button::new(RichText::new(icon::PENCIL_SIMPLE).size(14.0)).frame(false),
                    )
                    .on_hover_text(format!("Rename {}", profile.name));
                if rename_resp.clicked() {
                    rename = Some(profile.clone());
                }
                let deletable = profile.id != DEFAULT_PROFILE_ID && !is_active;
                let why = if profile.id == DEFAULT_PROFILE_ID {
                    "The Default profile cannot be deleted"
                } else {
                    "Switch to another profile to delete this one"
                };
                let delete_resp = ui
                    .put(
                        trash,
                        egui::Button::new(
                            RichText::new(icon::TRASH)
                                .size(14.0)
                                .color(if deletable { p.red } else { p.text_faint }),
                        )
                        .frame(false),
                    )
                    .on_hover_text(if deletable {
                        format!("Delete {}", profile.name)
                    } else {
                        why.to_owned()
                    });
                if delete_resp.clicked() && deletable {
                    delete = Some(profile.clone());
                }
            }
            if resp.clicked() && enabled && !is_active && rename.is_none() && delete.is_none() {
                switch_to = Some(profile.id.clone());
            }
        }
    });
    if !enabled {
        ui.add_space(4.0);
        ui.label(RichText::new(NO_HOST).size(11.5).color(p.text_faint));
    }

    if new {
        egui::Popup::close_all(ui.ctx());
        app.profiles_ui.dialog = Some(Dialog::New(NewProfile::new()));
    }
    if let Some(profile) = rename {
        egui::Popup::close_all(ui.ctx());
        app.profiles_ui.dialog = Some(Dialog::Rename {
            id: profile.id,
            name: profile.name,
            error: None,
        });
    }
    if let Some(profile) = delete {
        egui::Popup::close_all(ui.ctx());
        confirm_delete(app, profile);
    }
    if let Some(id) = switch_to {
        egui::Popup::close_all(ui.ctx());
        app.profiles_ui.pending_switch = Some(id);
    }
}

fn confirm_delete(app: &mut GitYasApp, profile: Profile) {
    let title = format!("Delete the {} profile?", profile.name);
    app.confirm(
        Confirm::new(
            title,
            "Its repository list, settings, commit signature and saved credentials are removed. The repositories on \
             disk are not touched.",
            "Delete profile",
            move |app| {
                let Some(host) = app.profiles_ui.host.clone() else { return };
                match host.store.remove(&profile.id) {
                    Ok(list) => {
                        app.profiles_ui.list = list;
                        app.notify.success(format!("Deleted the {} profile", profile.name));
                    }
                    Err(err) => app.notify.error("Could not delete the profile", &err),
                }
            },
        )
        .with_warning("This cannot be undone."),
    );
}

/// The New / Rename profile dialogs.
pub fn show_dialogs(ctx: &egui::Context, app: &mut GitYasApp) {
    let Some(mut dialog) = app.profiles_ui.dialog.take() else { return };
    let keep = match &mut dialog {
        Dialog::New(state) => new_dialog(ctx, app, state),
        Dialog::Rename { id, name, error } => rename_dialog(ctx, app, id, name, error),
    };
    if keep && app.profiles_ui.dialog.is_none() {
        app.profiles_ui.dialog = Some(dialog);
    }
}

fn name_field(ui: &mut Ui, name: &mut String, focus: bool) -> egui::Response {
    let resp = ui.add(
        egui::TextEdit::singleline(name)
            .hint_text("e.g. Work, Personal, Open source")
            .char_limit(MAX_PROFILE_NAME)
            .desired_width(f32::INFINITY),
    );
    if focus {
        resp.request_focus();
    }
    resp
}

/// Returns whether the dialog stays open.
fn new_dialog(ctx: &egui::Context, app: &mut GitYasApp, state: &mut NewProfile) -> bool {
    let p = theme::palette(ctx);
    let from = app.profiles_ui.active_name().to_owned();
    let repo_count = app.store.repositories.len();
    let busy = app.bridge.is_running(TASK_CREATE);
    let outcome = dialogs::modal(ctx, "new_profile", "New profile", 440.0, |ui| {
        ui.label(RichText::new("Name").strong());
        name_field(ui, &mut state.name, !state.focused);
        state.focused = true;
        ui.add_space(10.0);

        ui.label(RichText::new("Start from").strong());
        ui.radio_value(&mut state.fresh, false, format!("A copy of {from}"));
        ui.radio_value(&mut state.fresh, true, "Nothing (a fresh start)");
        ui.add_space(4.0);
        egui::Frame::new()
            .fill(p.bg_hover.gamma_multiply(0.5))
            .corner_radius(6)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                if state.fresh {
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "The new profile starts like a new install of GitYas: no repositories, default \
                                 settings, no commit signature and no saved credentials.",
                            )
                            .color(p.text_muted),
                        )
                        .wrap(),
                    );
                } else {
                    ui.label(RichText::new("Carry over").color(p.text_muted).size(12.0));
                    let repos = match repo_count {
                        1 => "Repositories (1)".to_owned(),
                        n => format!("Repositories ({n})"),
                    };
                    ui.checkbox(&mut state.carry.repositories, repos);
                    ui.checkbox(&mut state.carry.settings, "Settings and appearance");
                    ui.checkbox(&mut state.carry.signature, "Commit signature (name and email)");
                    ui.checkbox(&mut state.carry.credentials, "Credentials (access tokens)");
                }
            });
        ui.add_space(8.0);
        ui.checkbox(&mut state.switch_now, "Switch to it now");
        if let Some(error) = &state.error {
            ui.add_space(4.0);
            ui.label(RichText::new(error).color(p.red));
        }
        let ready = !state.name.trim().is_empty() && !busy;
        let reason = if busy {
            "Creating the profile…"
        } else {
            "Give the profile a name"
        };
        let (cancel, ok) = dialogs::buttons(ui, "Create profile", false, ready, reason);
        (cancel, ready && (ok || dialogs::enter_pressed(ui)))
    });
    let (cancel, ok) = outcome.inner;
    if cancel || outcome.closed {
        return false;
    }
    if ok {
        let carry = if state.fresh { CarryOver::default() } else { state.carry };
        match create(app, &state.name, carry, state.switch_now) {
            Ok(()) => return false,
            Err(err) => state.error = Some(message(&err)),
        }
    }
    true
}

/// Registers the profile, then copies what `carry` selects into it in the background and, with `switch_now`,
/// switches to it once that is done.
fn create(app: &mut GitYasApp, name: &str, carry: CarryOver, switch_now: bool) -> gityas_core::Result<()> {
    let Some(host) = app.profiles_ui.host.clone() else {
        return Err(GitYasError::Other(NO_HOST.into()));
    };
    let profile = host.store.add(name)?;
    app.profiles_ui.list = host.store.load();
    let store = host.store.clone();
    let id = profile.id.clone();
    app.bridge.run(
        TASK_CREATE,
        move |engine| async move { engine.copy_to_profile(&store, &id, carry).await },
        move |app, result| {
            if let Err(err) = result {
                app.report_error("Some settings could not be copied to the new profile", &err);
            }
            if switch_now {
                app.profiles_ui.pending_switch = Some(profile.id);
            } else {
                app.notify.success(format!("Created the {} profile", profile.name));
            }
        },
    );
    Ok(())
}

/// Returns whether the dialog stays open.
fn rename_dialog(ctx: &egui::Context, app: &mut GitYasApp, id: &str, name: &mut String, error: &mut Option<String>) -> bool {
    let p = theme::palette(ctx);
    let outcome = dialogs::modal(ctx, "rename_profile", "Rename profile", 400.0, |ui| {
        let resp = name_field(ui, name, false);
        if !resp.has_focus() && ui.memory(|m| m.focused().is_none()) {
            resp.request_focus();
        }
        if let Some(error) = error.as_ref() {
            ui.add_space(4.0);
            ui.label(RichText::new(error).color(p.red));
        }
        let ready = !name.trim().is_empty();
        let (cancel, ok) = dialogs::buttons(ui, "Rename", false, ready, "Give the profile a name");
        (cancel, ready && (ok || dialogs::enter_pressed(ui)))
    });
    let (cancel, ok) = outcome.inner;
    if cancel || outcome.closed {
        return false;
    }
    if ok {
        let Some(host) = app.profiles_ui.host.clone() else { return false };
        match host.store.rename(id, name) {
            Ok(list) => {
                app.profiles_ui.list = list;
                return false;
            }
            Err(err) => *error = Some(message(&err)),
        }
    }
    true
}

/// Runs a switch requested last frame. Call at the very start of a frame: it replaces the whole app state.
pub fn apply_pending_switch(app: &mut GitYasApp, ctx: &egui::Context) {
    let Some(id) = app.profiles_ui.pending_switch.take() else { return };
    let Some(host) = app.profiles_ui.host.clone() else { return };
    if id == app.profiles_ui.list.active {
        return;
    }
    let list = match host.store.set_active(&id) {
        Ok(list) => list,
        Err(err) => {
            app.notify.error("Could not switch profile", &err);
            return;
        }
    };
    let name = list.active_profile().name.clone();

    let (sink, events) = EguiEventSink::new();
    let sink = Arc::new(sink);
    sink.attach(ctx);
    let (engine, fatal) = match Engine::open_profile(&host.store, &id, sink.clone(), host.runtime.clone()) {
        Ok(engine) => (engine, None),
        Err(err) => {
            tracing::error!("cannot open profile {id}: {err}");
            match Engine::in_memory(sink, host.runtime.clone()) {
                Ok(engine) => (engine, Some(format!("Cannot open the {name} profile: {err}"))),
                Err(e) => {
                    app.notify.error("Could not switch profile", &e);
                    return;
                }
            }
        }
    };
    let old = std::mem::replace(&mut *host.current.lock().unwrap_or_else(PoisonError::into_inner), engine.clone());
    host.runtime.spawn(async move {
        if let Err(err) = old.shutdown().await {
            tracing::warn!("previous profile's engine shutdown: {err}");
        }
    });

    let bridge = Bridge::new(engine, host.runtime.clone(), ctx.clone(), events);
    let mut next = GitYasApp::new(
        ctx,
        Launch {
            bridge,
            mock: false,
            fatal,
            profiles: Some(host),
            switching_profile: true,
        },
    );
    next.saved_window = app.saved_window.take();
    next.notify.success(format!("Switched to the {name} profile"));
    *app = next;
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use gityas_platform::Paths;

    use super::*;
    use crate::testutil::{TestApp, temp_repo};

    /// An engine-backed app whose profiles live in a temp directory (the running engine stands in for Default).
    fn with_profiles() -> (TestApp, tempfile::TempDir) {
        let mut t = TestApp::engine();
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let paths = Paths::under(dir.path());
        paths.create_dirs().unwrap_or_else(|e| panic!("{e}"));
        let store = ProfileStore::with_service(paths, format!("com.gityas.GitYas.ui-test.{}", std::process::id()));
        let host = ProfileHost {
            store,
            runtime: t.handle(),
            current: Arc::new(Mutex::new(t.app().bridge.engine().clone())),
            seed_signature: false,
        };
        t.app().profiles_ui = ProfilesUi::new(Some(host));
        (t, dir)
    }

    fn switched_to(t: &mut TestApp, name: &str) {
        let name = name.to_owned();
        t.settle_until("profile switched", move |app| {
            app.profiles_ui.active_name() == name && app.store.started && app.profiles_ui.pending_switch.is_none()
        });
        t.settle();
    }

    #[test]
    fn a_copied_profile_keeps_the_repositories_and_a_fresh_one_starts_empty() {
        let (mut t, _dir) = with_profiles();
        let (repo_dir, _repo) = temp_repo();
        t.add_and_open(repo_dir.path());
        let carry = CarryOver {
            credentials: false,
            ..CarryOver::EVERYTHING
        };
        create(t.app(), "Work", carry, true).unwrap_or_else(|e| panic!("{e}"));
        switched_to(&mut t, "Work");
        assert_eq!(t.app().store.repositories.len(), 1);
        assert_eq!(t.app().profiles_ui.list.profiles.len(), 2);
        assert!(matches!(create(t.app(), "work", carry, false), Err(GitYasError::AlreadyExists(_))));

        create(t.app(), "Clean", CarryOver::default(), true).unwrap_or_else(|e| panic!("{e}"));
        switched_to(&mut t, "Clean");
        assert!(t.app().store.repositories.is_empty());
        assert!(t.app().store.settings.commit_signature.is_none());

        // Back to Work: its repositories are still there; Clean can now be deleted.
        let work = t.app().profiles_ui.list.profiles[1].id.clone();
        let clean = t.app().profiles_ui.list.profiles[2].id.clone();
        t.app().profiles_ui.pending_switch = Some(work);
        switched_to(&mut t, "Work");
        t.settle_until("repositories loaded", |app| app.store.repositories.len() == 1);
        let host = t.app().profiles_ui.host.clone().unwrap_or_else(|| panic!("no host"));
        host.store.remove(&clean).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.store.load().profiles.len(), 2);
    }

    #[test]
    fn messages_drop_the_error_kind_prefix() {
        assert_eq!(
            message(&GitYasError::InvalidInput("Give the profile a name".into())),
            "Give the profile a name"
        );
        assert_eq!(message(&GitYasError::Io("disk".into())), "I/O error: disk");
    }
}
