//! The egui application: owns the [`Bridge`] to the engine, the engine-derived [`Store`], per-tab UI state and
//! one UI-state struct per screen module. Screen state belongs in each module's own structs
//! (`ui::<module>::*Ui` / `*Tab`), not here.
//!
//! Startup: with an updater compiled in, the splash (`ui::splash`) covers the window and the engine starts only
//! after it (an update may restart the app first).
//!
//! Frame order: drain engine events → run command completions → global shortcuts → chrome (tab bar, context bar,
//! banner, status bar) → Changes panel and drawer → centre → dialogs → toasts.

use std::collections::HashMap;
use std::path::PathBuf;

use gityas_core::diff::DiffTarget;
use gityas_core::models::{Delta, FileState, Settings, Theme};
use gityas_core::{AppEvent, GitYasError, Oid, RepoId};

use crate::bridge::{Bridge, TaskKey};
use crate::dialogs::{self, Confirm};
use crate::notify::{Notifications, ToastAction};
use crate::store::Store;
use crate::{mock, theme, ui, updater};

/// eframe's storage key for the window geometry it saves (`NativeOptions::persist_window`).
pub const WINDOW_KEY: &str = "window";

/// Smallest size of the main window (speed mode lifts it while the window is the compact panel).
pub const MIN_WINDOW_SIZE: egui::Vec2 = egui::vec2(960.0, 600.0);

/// A UI action to run later on the UI thread (e.g. "commit once a signature is saved", toast actions).
pub type Deferred = Box<dyn FnOnce(&mut GitYasApp)>;

/// What the right panel shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Selection {
    /// Working tree (the WIP row): staged / unstaged files and the commit box.
    #[default]
    Wip,
    /// A commit's details.
    Commit(Oid),
    /// A stash's details; by stash commit id, since `stash@{n}` indices shift.
    Stash(Oid),
}

/// A file diff to show in the centre. Working-tree targets are re-fetched when the files change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRequest {
    pub target: DiffTarget,
    pub file: Delta,
}

/// What the centre area shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Center {
    /// Commit graph.
    #[default]
    Graph,
    /// Diff viewer; Esc / ✕ returns to the graph.
    Diff(DiffRequest),
    /// Merge-conflict editor for this repository-relative path.
    Merge(PathBuf),
    /// Full-screen history / blame of the file of this diff (the one it was opened from; ✕ / Esc returns to it).
    File(DiffRequest),
}

impl Center {
    /// Views that take the whole window: the side panels are hidden while they are shown.
    pub fn is_full_screen(&self) -> bool {
        matches!(self, Center::Merge(_) | Center::File(_))
    }
}

/// UI-only state of one repository tab (D2: data is re-fetched on switch; this is what survives it).
#[derive(Default)]
pub struct TabState {
    pub selection: Selection,
    pub center: Center,
    pub history: ui::history::HistoryTab,
    pub worktree: ui::worktree::WorktreeTab,
    pub diff: ui::diff::DiffTab,
    pub file: ui::diff::file_tools::FileTools,
    pub merge: ui::merge::MergeTab,
}

/// Pages of the main window's side panels. Changes is pinned on the left and Branches on the right; Settings opens
/// in the drawer that slides out on the far right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SidePage {
    /// Working-tree changes and the commit box (WIP selected), or the selected commit / stash's details.
    #[default]
    Changes,
    /// LOCAL / REMOTE / TAGS / STASHES.
    Branches,
    Settings,
}

/// Side panel state: the pinned Changes and Branches panels and the right-hand drawer.
pub struct LayoutState {
    /// The drawer's page; `None` while it is closed. Only ever `Settings`.
    pub drawer: Option<SidePage>,
    /// Whether the pinned panels (Changes, Branches) are shown; the merge editor hides them while it is the centre
    /// view.
    pub show_changes: bool,
    /// Working-tree files last seen (per repository), to tell a real change from a re-sent state.
    seen_files: Option<(RepoId, FileState)>,
}

impl Default for LayoutState {
    fn default() -> Self {
        Self {
            drawer: None,
            show_changes: true,
            seen_files: None,
        }
    }
}

/// Settings saves in flight ([`GitYasApp::modify_settings`]). The engine serialises the writes, but their completions
/// can reach the UI in another order, so an older save's result may be copied into the store after a newer one's.
/// When saves overlapped, the last one to finish re-reads the stored settings, which come after every write.
#[derive(Debug, Default)]
pub struct SettingsSaves {
    in_flight: u32,
    overlapped: bool,
}

impl SettingsSaves {
    fn begin(&mut self) {
        self.in_flight += 1;
        self.overlapped |= self.in_flight > 1;
    }

    /// One save finished. `true`: it was the last of saves that overlapped, so the store may hold an older result.
    fn finish(&mut self) -> bool {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.in_flight == 0 && std::mem::take(&mut self.overlapped)
    }

    fn idle(&self) -> bool {
        self.in_flight == 0
    }
}

impl LayoutState {
    /// Hides the pinned panels while a full-screen view (the merge editor, file history) is the centre view and
    /// brings them back once it is left (closed, saved, resolved elsewhere, another repository tab). The drawer is
    /// untouched.
    pub fn follow_center(&mut self, full_screen: bool) {
        self.show_changes = !full_screen;
    }

    /// Shows `page`: opens the drawer on it (Changes and Branches are always shown, except in the merge editor).
    pub fn show(&mut self, page: SidePage) {
        if page == SidePage::Settings {
            self.drawer = Some(page);
        }
    }

    /// Opens the drawer on `page`, or closes it when it already shows `page`.
    pub fn toggle(&mut self, page: SidePage) {
        if self.drawer == Some(page) {
            self.drawer = None;
        } else {
            self.show(page);
        }
    }

    /// Whether `page` is on screen this frame.
    pub fn showing(&self, page: SidePage) -> bool {
        match page {
            SidePage::Changes | SidePage::Branches => self.show_changes,
            SidePage::Settings => self.drawer == Some(page),
        }
    }

    /// Records the working tree of `repo`; true when it changed since the last state seen for the same repository
    /// (the first state after opening or switching tabs is not a change) and has changes to review.
    pub fn files_changed(&mut self, repo: &RepoId, files: &FileState) -> bool {
        let changed = self.seen_files.as_ref().is_some_and(|(seen, old)| seen == repo && old != files);
        self.seen_files = Some((repo.clone(), files.clone()));
        changed && !(files.staged.is_empty() && files.unstaged.is_empty())
    }
}

pub struct GitYasApp {
    pub bridge: Bridge,
    pub store: Store,
    pub notify: Notifications,
    /// The open confirmation; see [`GitYasApp::confirm`].
    pub confirm: Option<Confirm>,
    /// Per-repository UI state, keyed by repository id (kept for closed tabs too, cheap).
    pub tabs: HashMap<RepoId, TabState>,
    pub layout: LayoutState,
    /// Non-fatal engine warnings not yet dismissed (the remote banner draws them).
    pub warnings: Vec<String>,
    /// Running with `--mock`: static data, no repository on disk; commands fail with `NoActiveRepository`.
    pub mock: bool,
    /// Update check at startup; `Some` until it finishes.
    pub splash: Option<ui::splash::Splash>,
    applied_theme: Option<Theme>,
    /// Window geometry as eframe last saved it while the window was on screen (see [`eframe::App::save`]).
    pub saved_window: Option<String>,
    /// Files dragged out of the window to other apps (started after the frame, where the window handle is).
    pub file_drag: crate::platform_ui::FileDrag,

    // One struct per screen module (global UI state: dialogs, popups, caches).
    pub repos_ui: ui::repos::ReposUi,
    pub history_ui: ui::history::HistoryUi,
    pub worktree_ui: ui::worktree::WorktreeUi,
    pub settings_ui: ui::settings::SettingsUi,
    pub remote_ui: ui::remote::RemoteUi,
    pub changelog_ui: ui::changelog::ChangelogUi,
    pub profiles_ui: ui::profiles::ProfilesUi,
    pub speed: ui::speed::SpeedUi,
    pub settings_saves: SettingsSaves,
}

/// How the app was launched.
pub struct Launch {
    pub bridge: Bridge,
    pub mock: bool,
    /// Engine construction failed (e.g. data directory unwritable); the app shows this instead of the UI and the
    /// bridge runs an in-memory engine.
    pub fatal: Option<String>,
    /// Profiles and how to switch between them (`None`: `--mock` and tests).
    pub profiles: Option<ui::profiles::ProfileHost>,
    /// Built by a profile switch: no update splash, the engine starts right away.
    pub switching_profile: bool,
}

impl GitYasApp {
    pub fn new(ctx: &egui::Context, launch: Launch) -> Self {
        theme::install(ctx);
        // Real launches only (tests and `--mock` have no profiles): tests must not take system-wide shortcuts.
        let global_shortcut = if launch.profiles.is_some() {
            ui::speed::keys::GlobalShortcut::install(ctx)
        } else {
            ui::speed::keys::GlobalShortcut::default()
        };
        let mut app = Self {
            bridge: launch.bridge,
            store: Store::default(),
            notify: Notifications::default(),
            confirm: None,
            tabs: HashMap::new(),
            layout: LayoutState::default(),
            warnings: Vec::new(),
            mock: launch.mock,
            splash: None,
            applied_theme: None,
            saved_window: None,
            file_drag: Default::default(),
            repos_ui: Default::default(),
            history_ui: Default::default(),
            worktree_ui: Default::default(),
            settings_ui: Default::default(),
            remote_ui: Default::default(),
            changelog_ui: Default::default(),
            profiles_ui: ui::profiles::ProfilesUi::new(launch.profiles),
            speed: ui::speed::SpeedUi::new(global_shortcut),
            settings_saves: SettingsSaves::default(),
        };
        if let Some(fatal) = launch.fatal {
            app.store.fatal = Some(fatal);
            app.store.started = true;
        } else if app.mock {
            mock::install(&mut app);
        } else if launch.switching_profile {
            app.start_engine();
        } else if let Some(splash) = ui::splash::Splash::start(ctx) {
            app.splash = Some(splash);
        } else {
            app.start_engine();
        }
        app
    }

    /// `Engine::start` (emits repositories, signature, auth, reopens the last tab), then loads open tabs, settings
    /// and auth status that are not pushed as events. With no signature set, real launches take it from the global
    /// git config in between ([`ui::profiles::ProfileHost::seed_signature`]).
    fn start_engine(&mut self) {
        let seed_signature = self.profiles_ui.seeds_signature();
        self.bridge.run(
            "start",
            move |engine| async move {
                engine.start().await?;
                if seed_signature && let Err(err) = engine.seed_signature_from_git_config().await {
                    tracing::warn!("commit signature not read from the git config: {err}");
                }
                let tabs = engine.open_tabs().await?;
                let settings = engine.settings().await?;
                let auth = engine.auth_status().await?;
                Ok((tabs, settings, auth))
            },
            |app, result| match result {
                Ok((tabs, settings, auth)) => {
                    app.store.open_tabs = tabs;
                    app.store.set_settings(settings);
                    app.store.auth = auth;
                    app.store.started = true;
                    ui::changelog::after_start(app);
                }
                Err(err) => {
                    app.store.fatal = Some(format!("GitYas could not start: {err}"));
                    app.store.started = true;
                }
            },
        );
    }

    /// Per-tab state of the active repository (created on first use).
    pub fn tab_mut(&mut self) -> Option<&mut TabState> {
        let id = self.store.active.clone()?;
        Some(self.tabs.entry(id).or_default())
    }

    pub fn tab(&self) -> Option<&TabState> {
        self.store.active.as_ref().and_then(|id| self.tabs.get(id))
    }

    /// Asks for confirmation before a destructive action. Replaces any open confirmation.
    pub fn confirm(&mut self, confirm: Confirm) {
        self.confirm = Some(confirm);
    }

    /// Error toast with the action the error suggests: auth failures offer `[Open credentials]`, refused
    /// checkouts / merges (`Conflict`) offer `[Stash changes]`. `Cancelled` is silent; a stale hunk is an info toast
    /// (the diff viewer re-fetches).
    pub fn report_error(&mut self, title: &str, err: &GitYasError) {
        match err {
            GitYasError::Cancelled => {}
            GitYasError::StaleHunk => self.notify.info("File changed since it was shown; refreshed"),
            GitYasError::Auth(_) => self.notify.error_with_action(
                title,
                err,
                ToastAction::new("Open credentials", |app| ui::settings::open(app, ui::settings::Page::Credentials)),
            ),
            GitYasError::Conflict(_) => self.notify.error_with_action(
                title,
                err,
                ToastAction::new("Stash changes", |app| ui::worktree::open_stash_dialog(app, None)),
            ),
            _ => self.notify.error(title, err),
        }
    }

    /// Changes stored settings (theme, diff view mode, graph options, ...) with an atomic read-modify-write in the
    /// engine (`Engine::modify_settings`), so concurrent writers never undo each other. There is no settings event:
    /// on success the saved appearance fields are copied into `store.settings` (the signature arrives as
    /// `CommitsSignatureUpdated`), then `done` runs. When saves overlapped, the last to finish also re-reads the
    /// stored settings ([`SettingsSaves`]).
    pub fn modify_settings(
        &mut self,
        key: TaskKey,
        change: impl FnOnce(&mut Settings) + Send + 'static,
        done: impl FnOnce(&mut GitYasApp, gityas_core::Result<Settings>) + Send + 'static,
    ) {
        self.settings_saves.begin();
        self.bridge.run(
            key,
            move |engine| async move { engine.modify_settings(change).await },
            move |app, result| {
                if let Ok(saved) = &result {
                    app.copy_saved_settings(saved);
                }
                if app.settings_saves.finish() {
                    app.reload_saved_settings();
                }
                done(app, result);
            },
        );
    }

    /// The fields [`Self::modify_settings`] keeps in sync with the engine.
    fn copy_saved_settings(&mut self, saved: &Settings) {
        let mut s = self.store.settings.clone();
        s.theme = saved.theme;
        s.diff_view_mode = saved.diff_view_mode;
        s.graph = saved.graph.clone();
        s.fetch_interval_secs = saved.fetch_interval_secs;
        s.speed = saved.speed.clone();
        s.swap_sidebars = saved.swap_sidebars;
        self.store.set_settings(s);
    }

    /// Re-reads the stored settings after overlapping saves. A save started meanwhile does its own re-read when it
    /// finishes, so this result is only used while none is running.
    fn reload_saved_settings(&mut self) {
        self.bridge.run(
            "settings_reload",
            |engine| async move { engine.settings().await },
            |app, result| match result {
                Ok(saved) if app.settings_saves.idle() => app.copy_saved_settings(&saved),
                Ok(_) => {}
                Err(err) => tracing::warn!("cannot re-read the settings: {err}"),
            },
        );
    }

    /// Applies engine events (each screen module reacts via `ui::on_event`), then runs command completions.
    ///
    /// Completions are collected *before* the events are drained: a command emits its refresh events before it
    /// returns, so every event a collected completion depends on (e.g. `CommitsSignatureUpdated` for "retry the
    /// tag once the signature is saved") is already in the channel and is applied first. Draining events first
    /// would let a completion that lands in between run against a store that misses its own events.
    pub fn pump(&mut self) {
        let completions = self.bridge.drain_completions();
        for event in self.bridge.drain_events() {
            self.handle_event(event);
        }
        for completion in completions {
            completion(self);
        }
    }

    pub fn handle_event(&mut self, event: AppEvent) {
        if let AppEvent::Warning(message) = &event {
            self.notify.warning("Warning", Some(message.clone()));
            self.warnings.push(message.clone());
        }
        let seen = event.clone();
        if self.store.apply(event) {
            ui::on_event(self, &seen);
        }
    }

    fn sync_theme(&mut self, ctx: &egui::Context) {
        let wanted = self.store.settings.theme;
        if self.applied_theme != Some(wanted) {
            theme::set_preference(ctx, wanted);
            self.applied_theme = Some(wanted);
        }
    }

    /// The startup update check finished: start the engine, or quit so the updated version can start.
    fn finish_update_check(&mut self, ctx: &egui::Context, outcome: updater::Outcome) {
        match outcome {
            updater::Outcome::Relaunch(path) => {
                updater::request_relaunch(path);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            updater::Outcome::Available(version) => self.notify.push(
                crate::notify::Level::Info,
                format!("GitYas {version} is available"),
                Some("This installation cannot update itself; install the new version from the GitYas releases page on GitHub.".into()),
                None,
            ),
            updater::Outcome::UpToDate | updater::Outcome::Failed(_) | updater::Outcome::Skipped => {}
        }
        self.start_engine();
    }

    /// One frame. `ui` is the whole window.
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        ui::profiles::apply_pending_switch(self, ui.ctx());
        if let Some(splash) = self.splash.as_mut() {
            match ui::splash::show(ui, splash) {
                None => return,
                Some(outcome) => {
                    self.splash = None;
                    self.finish_update_check(ui.ctx(), outcome);
                }
            }
        }
        self.pump();
        self.sync_theme(ui.ctx());
        ui::speed::poll(ui.ctx(), self);
        // Global shortcuts are off while any modal (confirmation, settings, a dialog) is open; the modal's own keys
        // (Enter / Esc) still work. `top_modal_layer` is last frame's, which is what the user is looking at.
        let modal_open = ui.ctx().memory(|m| m.top_modal_layer().is_some());
        if self.speed.active {
            // Speed mode: the window is the compact keyboard panel; it handles its own keys.
            if self.store.started && self.store.fatal.is_none() {
                ui::speed::frame(ui, self, modal_open);
                let ctx = ui.ctx().clone();
                ui::show_dialogs(&ctx, self);
                dialogs::show_confirm(&ctx, self);
                if self.speed.expanded() {
                    crate::notify::show_toasts(ui, self);
                }
                ui::pointer_cursor(&ctx);
                return;
            }
            ui::speed::exit(ui.ctx(), self);
        }
        if self.confirm.is_none() && !modal_open {
            ui::shortcuts(ui.ctx(), self);
        }

        let details = ui::side::details_guard(ui.ctx(), self, modal_open);
        let mut center = egui::Rect::NOTHING;
        ui::repos::tab_bar(ui, self);
        if self.store.started && self.store.fatal.is_none() {
            ui::context_bar::show(ui, self);
            ui::remote::banner(ui, self);
        }
        ui::status_bar::show(ui, self);

        if !self.store.started || self.store.fatal.is_some() {
            ui::loading_frame(ui, self);
        } else {
            let full_screen = self.tab().is_some_and(|t| t.center.is_full_screen());
            self.layout.follow_center(full_screen);
            ui::settings::sync(ui.ctx(), self);
            ui::side::panels(ui, self);
            center = ui.available_rect_before_wrap();
            if self.store.repo.is_none() {
                ui::repos::welcome(ui, self);
            } else {
                match self.tab().map(|t| t.center.clone()).unwrap_or_default() {
                    Center::Graph => ui::history::graph(ui, self),
                    Center::Diff(request) => ui::diff::view(ui, self, &request),
                    Center::Merge(path) => ui::merge::view(ui, self, &path),
                    Center::File(origin) => ui::diff::file_tools::view(ui, self, &origin),
                }
            }
        }

        let ctx = ui.ctx().clone();
        ui::show_dialogs(&ctx, self);
        dialogs::show_confirm(&ctx, self);
        crate::notify::show_toasts(ui, self);
        ui::side::dismiss_details(&ctx, self, details, center);
        ui::pointer_cursor(&ctx);
    }
}

/// Whether eframe's saved window geometry (RON) is a window on screen: not minimised, which Windows reports as a
/// window of a few pixels at -32000, -32000.
fn window_on_screen(window: &str) -> bool {
    let number_after = |key: &str| -> Option<f32> {
        let rest = &window[window.find(key)? + key.len()..];
        rest[..rest.find([',', ')'])?].trim().parse().ok()
    };
    let width = number_after("inner_size_points:Some((x:");
    let x = number_after("outer_position_pixels:Some((x:");
    width.is_some_and(|w| w >= 200.0) && x.is_none_or(|x| x > -30_000.0)
}

impl eframe::App for GitYasApp {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        let p = if visuals.dark_mode { &theme::DARK } else { &theme::LIGHT };
        p.bg_app.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.frame(ui);
        if let Err(err) = self.file_drag.start(frame) {
            tracing::warn!("could not drag files out: {err}");
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.file_drag.fix_input(ctx, raw_input);
    }

    /// eframe has just stored the window geometry. Minimised (closed from the taskbar, or the periodic save), that is
    /// a tiny window off screen, which the next launch would open as: the last geometry saved on screen is kept
    /// instead.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        match storage.get_string(WINDOW_KEY) {
            // In speed mode the window is the compact panel: keep the geometry from before it.
            Some(window) if window_on_screen(&window) && !self.speed.active => self.saved_window = Some(window),
            _ => {
                if let Some(window) = &self.saved_window {
                    storage.set_string(WINDOW_KEY, window.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::kittest::Queryable;
    use gityas_core::GitYasError;

    use super::*;
    use crate::testutil::{TestApp, temp_repo, write_file};

    #[test]
    fn overlapping_settings_saves_reload_once_the_last_finishes() {
        let mut saves = SettingsSaves::default();
        saves.begin();
        assert!(!saves.finish(), "a lone save's own result is current");
        assert!(saves.idle());

        saves.begin();
        saves.begin();
        assert!(!saves.finish(), "another save is still running");
        assert!(!saves.idle());
        saves.begin();
        assert!(!saves.finish());
        assert!(saves.finish(), "the last of the overlapping saves");
        assert!(saves.idle());

        saves.begin();
        assert!(!saves.finish(), "the next lone save starts afresh");
        assert!(!saves.finish(), "an extra finish never underflows");
    }

    #[test]
    fn a_minimised_window_is_not_a_window_on_screen() {
        let shown = "(inner_position_pixels:Some((x:214.0,y:248.0)),outer_position_pixels:Some((x:206.0,y:217.0)),                     fullscreen:false,maximized:false,inner_size_points:Some((x:1377.0,y:713.0)))";
        let minimised = "(inner_position_pixels:Some((x:-32000.0,y:-32000.0)),outer_position_pixels:Some((x:-32000.0,                         y:-32000.0)),fullscreen:false,maximized:false,inner_size_points:Some((x:160.0,y:28.0)))";
        assert!(window_on_screen(shown));
        assert!(!window_on_screen(minimised));
        assert!(!window_on_screen("garbage"));
    }

    #[test]
    fn mock_app_shows_the_mock_repository() {
        let mut t = TestApp::mock();
        t.harness.step();
        let app = t.app();
        assert!(app.store.started && app.store.fatal.is_none());
        let repo = app.store.repo.as_ref().unwrap();
        assert!(repo.is_ready() && repo.has_wip() && repo.graph.wip.is_some());
        assert_eq!(repo.commits.len(), 135);
        t.harness.get_by_label("MOCK DATA");
    }

    #[test]
    #[ignore = "needs a GPU"]
    fn snapshot_main_window_dark_and_light() {
        let mut t = TestApp::mock_gpu();
        t.harness.step();
        t.snapshot("main_window");
        t.app().store.settings.theme = Theme::Light;
        t.harness.step();
        t.harness.step();
        t.snapshot("main_window_light");
    }

    #[test]
    fn engine_start_without_repositories_shows_welcome() {
        let mut t = TestApp::engine();
        let app = t.app();
        assert!(app.store.fatal.is_none());
        assert!(app.store.repo.is_none() && app.store.repositories.is_empty());
        assert_eq!(app.store.auth.len(), gityas_core::Provider::ALL.len());
    }

    #[test]
    fn opening_a_real_repository_loads_its_state_and_follows_the_working_tree() {
        let mut t = TestApp::engine();
        let (dir, repo) = temp_repo();
        let registered = t.add_and_open(dir.path());
        let data = t.app().store.repo.as_ref().unwrap();
        assert_eq!(data.id, registered.id);
        assert_eq!(data.commits.len(), 1);
        assert!(data.head.as_ref().is_some_and(|h| h.branch_name() == Some("main")));
        assert!(!data.has_wip());
        assert_eq!(t.app().store.open_tabs.active.as_deref(), Some(registered.id.as_str()));

        // A command's refresh reaches the store through events (no optimistic update).
        write_file(&repo, "new.txt", "x\n");
        let engine = t.app().bridge.engine().clone();
        t.block_on(engine.refresh(gityas_engine::Refresh::FILES)).unwrap();
        t.settle_until("WIP appears", |app| {
            app.store.repo.as_ref().is_some_and(|r| r.has_wip() && r.graph.wip.is_some())
        });
    }

    #[test]
    fn global_shortcuts_are_off_while_a_modal_is_open() {
        let mut t = TestApp::engine();
        let (dir, _repo) = temp_repo();
        t.add_and_open(dir.path());
        ui::repos::open_add_dialog(t.app(), ui::repos::AddTab::Local);
        t.harness.step();
        t.harness.step();
        t.harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::W);
        t.settle();
        assert_eq!(
            t.app().store.open_tabs.open.len(),
            1,
            "Cmd+W must not close the tab behind the add-repository dialog"
        );
        t.harness.key_press(egui::Key::Escape);
        t.harness.step();
        t.harness.step();
        t.harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::W);
        t.settle_until("tab closed", |app| app.store.open_tabs.open.is_empty());
    }

    #[test]
    fn report_error_adds_the_suggested_action() {
        let mut t = TestApp::mock();
        let app = t.app();
        app.report_error("Could not push", &GitYasError::Auth("rejected".into()));
        app.report_error("Could not check out", &GitYasError::Conflict(vec!["a.txt".into()]));
        app.report_error("ignored", &GitYasError::Cancelled);
        let actions: Vec<Option<&str>> = app.notify.toasts().map(|t| t.action.as_ref().map(|a| a.label.as_str())).collect();
        assert_eq!(actions, [Some("Open credentials"), Some("Stash changes")]);
    }

    #[test]
    fn completions_run_on_the_ui_thread_and_release_their_key() {
        let mut t = TestApp::mock();
        t.app().bridge.run(
            "probe",
            |engine| async move { engine.repositories().await },
            |app, result| app.warnings.push(format!("{}", result.map(|r| r.len()).unwrap_or(99))),
        );
        assert!(t.app().bridge.is_running("probe"));
        t.settle();
        assert!(!t.app().bridge.is_running("probe"));
        assert_eq!(t.app().warnings, ["0"]);
    }

    #[test]
    fn confirm_runs_its_action_only_when_confirmed() {
        let mut t = TestApp::mock();
        t.app()
            .confirm(Confirm::new("Delete tag v1.0?", "Delete tag v1.0?", "Delete tag", |app| {
                app.warnings.push("deleted".into())
            }));
        t.harness.step();
        t.harness.get_by_label("Cancel").click();
        t.harness.step();
        assert!(t.app().confirm.is_none() && t.app().warnings.is_empty());

        t.app().confirm(Confirm::new("Delete tag v1.0?", "body", "Delete tag", |app| {
            app.warnings.push("deleted".into())
        }));
        t.harness.step();
        t.harness.get_by_label("Delete tag").click();
        t.harness.step();
        t.harness.step();
        assert_eq!(t.app().warnings, ["deleted"]);
    }
}
