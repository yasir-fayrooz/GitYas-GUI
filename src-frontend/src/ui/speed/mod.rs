//! Speed Git mode: the main window turns into a small, frameless, always-on-top panel in the bottom-right corner of
//! the screen, driven from the keyboard. Leaving it puts the window back where it was.
//!
//! * The **Speed mode** button (beside Stash / Pop) enters it collapsed: a one-line bar showing the branch and counts.
//! * The summon shortcut (default Alt+Shift+G / ⌥⇧G, configurable on the Git settings page) works from any app: it
//!   enters speed mode or opens the panel, and focuses it; pressed again while the panel has the focus, it collapses
//!   it.
//! * In the panel: ↑ ↓ move, Enter / → open, Esc / ← back (from the menu: collapse), and one key per action (C
//!   changes, M commit, P push, …; configurable too).
//!
//! Screens: the menu, the changes list (stage / unstage / discard one or all), a file viewer (the working-tree diff
//! of one file), the commit box and the branch list. Stash, tag and new branch reuse the main window's dialogs,
//! which already work from the keyboard (Enter / Esc). Everything calls the same functions as the main window, so
//! the engine's events update both.

mod desktop;
pub mod keys;

use std::sync::Arc;

use egui::{Align2, Event, FontId, Key, Pos2, Rect, RichText, Sense, Ui, Vec2, ViewportCommand, vec2};
use egui_phosphor::regular as icon;
use gityas_core::AppEvent;
use gityas_core::diff::{DiffTarget, FileDiff, LineOrigin};
use gityas_core::models::Delta;

use crate::app::{GitYasApp, MIN_WINDOW_SIZE};
use crate::bridge::TaskKey;
use crate::theme;
use crate::ui::worktree::model::{self, Section};
use crate::ui::{self, history, remote, worktree};

use self::keys::{Binding, Combo};

const TASK_DIFF: TaskKey = "speed_diff";
const WIDTH: f32 = 460.0;
const EXPANDED_HEIGHT: f32 = 540.0;
/// The collapsed bar's height until its content was measured (header plus two rows of action keys).
const COLLAPSED_HEIGHT: f32 = 96.0;
/// Speed mode's toasts: success / info / warning, and errors (which elsewhere stay until dismissed).
const TOAST_SECS: f64 = 2.5;
const ERROR_TOAST_SECS: f64 = 6.0;
/// Gap between the panel and the screen's right edge; the bottom also leaves room for a taskbar / dock.
const SCREEN_MARGIN: f32 = 16.0;
const TASKBAR_ROOM: f32 = 56.0;
const ROW_HEIGHT: f32 = 26.0;
/// How long a new toast's title shows in the collapsed bar.
const FLASH_SECS: f64 = 2.5;
const SUMMARY_ID: &str = "speed_commit_summary";
const PROMPT_ID: &str = "speed_prompt";
const TASK_PROMPT: TaskKey = "speed_prompt";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Home,
    Changes,
    Viewer,
    Commit,
    Branches,
    /// A one-field form inside the panel (the main window's dialogs are too big for it).
    Prompt(Prompt),
}

/// What the prompt screen asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    /// A lightweight tag on HEAD.
    Tag,
    /// A branch from HEAD, checked out.
    Branch,
    /// Stash all changes (untracked included), with an optional message.
    Stash,
}

impl Prompt {
    fn title(self) -> &'static str {
        match self {
            Prompt::Tag => "Tag HEAD",
            Prompt::Branch => "New branch",
            Prompt::Stash => "Stash changes",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Prompt::Tag => "Tag name, e.g. v1.0.0",
            Prompt::Branch => "Branch name",
            Prompt::Stash => "Message (optional)",
        }
    }
}

/// Window geometry from before speed mode.
struct Restore {
    position: Option<Pos2>,
    size: Vec2,
    maximized: bool,
    fullscreen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Header,
    Add,
    Del,
    Context,
    Note,
}

struct Viewer {
    file: Delta,
    staged: bool,
    /// `None` while loading; `Err` holds the message.
    lines: Option<Result<Vec<(LineKind, String)>, String>>,
    scroll: f32,
    /// Scroll offset set by a key this frame (applied to the scroll area).
    scroll_to: Option<f32>,
    max_scroll: f32,
}

/// State of speed mode (`GitYasApp::speed`).
#[derive(Default)]
pub struct SpeedUi {
    pub active: bool,
    collapsed: bool,
    screen: Screen,
    back: Vec<Screen>,
    home: usize,
    changes: usize,
    branches: usize,
    viewer: Option<Viewer>,
    restore: Option<Restore>,
    /// The app the summon shortcut took the focus from; it gets it back when the panel is tucked away.
    previous: Option<desktop::PreviousApp>,
    /// Measured height of the collapsed bar.
    bar_height: Option<f32>,
    /// Window size last asked for (the window is re-placed when it changes).
    placed: Option<Vec2>,
    scroll_to_cursor: bool,
    focus_summary: bool,
    /// The prompt screen's text and whether its field still needs the focus.
    prompt_text: String,
    focus_prompt: bool,
    /// Newest toast seen, and the title shown in the collapsed bar since `time`.
    seen_toast: Option<u64>,
    flash: Option<(String, f64)>,
    // Kept across entering / leaving speed mode:
    /// The system-wide summon shortcut.
    pub global: keys::GlobalShortcut,
    /// The settings page is waiting for a key press for this binding.
    recording: Option<Binding>,
    /// Why the last key pressed for a binding was refused.
    key_error: Option<(Binding, String)>,
}

impl SpeedUi {
    pub fn new(global: keys::GlobalShortcut) -> Self {
        Self { global, ..Self::default() }
    }

    /// Speed mode with the panel open (not collapsed to the bar).
    pub fn expanded(&self) -> bool {
        self.active && !self.collapsed
    }

    #[cfg(test)]
    pub fn screen(&self) -> Screen {
        self.screen
    }

    #[cfg(test)]
    pub fn collapsed(&self) -> bool {
        self.collapsed
    }
}

// ---- Entering / leaving -----------------------------------------------------------------------------------------

/// Replaces the panel state, keeping what outlives speed mode (the system-wide shortcut, the settings editor).
fn reset(app: &mut GitYasApp, next: SpeedUi) {
    let old = std::mem::take(&mut app.speed);
    app.speed = SpeedUi {
        global: old.global,
        recording: old.recording,
        key_error: old.key_error,
        ..next
    };
}

/// Turns the window into the speed panel (bottom-right, always on top, no title bar), collapsed to its bar.
pub fn enter(ctx: &egui::Context, app: &mut GitYasApp) {
    if app.speed.active {
        return;
    }
    let viewport = ctx.input(|i| i.viewport().clone());
    let restore = Restore {
        position: viewport.outer_rect.map(|r| r.min),
        size: viewport.inner_rect.map_or(vec2(1440.0, 900.0), |r| r.size()),
        maximized: viewport.maximized == Some(true),
        fullscreen: viewport.fullscreen == Some(true),
    };
    if restore.fullscreen {
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
    }
    if restore.maximized {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
    }
    let seen_toast = newest_toast(app);
    reset(
        app,
        SpeedUi {
            active: true,
            collapsed: true,
            restore: Some(restore),
            seen_toast,
            ..SpeedUi::default()
        },
    );
    ctx.send_viewport_cmd(ViewportCommand::Decorations(false));
    ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(vec2(WIDTH, 40.0)));
    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
    ctx.request_repaint();
}

/// Puts the window back as it was before speed mode.
pub fn exit(ctx: &egui::Context, app: &mut GitYasApp) {
    if !app.speed.active {
        return;
    }
    let restore = app.speed.restore.take();
    ctx.memory_mut(|m| m.surrender_focus(egui::Id::new(SUMMARY_ID)));
    reset(app, SpeedUi::default());
    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
    ctx.send_viewport_cmd(ViewportCommand::Decorations(true));
    ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(MIN_WINDOW_SIZE));
    if let Some(restore) = restore {
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(restore.size.max(MIN_WINDOW_SIZE)));
        if let Some(position) = restore.position {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(position));
        }
        if restore.maximized {
            ctx.send_viewport_cmd(ViewportCommand::Maximized(true));
        }
        if restore.fullscreen {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
        }
    } else {
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(1440.0, 900.0)));
    }
    ctx.send_viewport_cmd(ViewportCommand::Focus);
    ctx.request_repaint();
}

/// The summon shortcut: enters speed mode (panel open) or opens the panel, and brings it to the front with the
/// focus; when the open panel already has the focus, collapses it.
pub fn summon(ctx: &egui::Context, app: &mut GitYasApp) {
    let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
    if app.speed.active && !app.speed.collapsed && focused {
        tuck_away(app);
        return;
    }
    // Remember where the user was typing, to hand the focus back afterwards.
    if let Some(previous) = desktop::capture() {
        app.speed.previous = Some(previous);
    }
    if !app.speed.active {
        enter(ctx, app);
    }
    app.speed.collapsed = false;
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
    ctx.request_repaint();
}

/// Collapses the panel to its bar and gives the focus back to the app the summon shortcut took it from.
fn tuck_away(app: &mut GitYasApp) {
    app.speed.collapsed = true;
    if let Some(previous) = app.speed.previous.take() {
        desktop::restore(previous);
    }
}

/// Every frame, in or out of speed mode: keeps the system-wide shortcut registered as the settings say, and answers
/// it (also the in-window match when it is not registered system-wide).
pub fn poll(ctx: &egui::Context, app: &mut GitYasApp) {
    if !app.store.started {
        return;
    }
    app.speed.global.sync(&app.store.settings.speed);
    let mut pressed = keys::take_pressed();
    let in_window = !app.speed.global.active() && app.speed.recording.is_none();
    if in_window && let Some(summon) = Combo::parse(&app.store.settings.speed.summon) {
        pressed |= ctx.input_mut(|i| {
            let at = i.events.iter().position(
                |e| matches!(e, Event::Key { key, pressed: true, modifiers, .. } if Combo::from_event(*key, *modifiers) == summon),
            );
            at.map(|at| i.events.remove(at)).is_some()
        });
    }
    if pressed {
        summon(ctx, app);
    }
}

/// Sizes the window for the panel's state. The first time it goes to the bottom-right corner of the screen's work
/// area (above the taskbar / dock); after that its bottom-right corner stays where it is (the user may have dragged
/// it), pulled back inside the work area if it would end up under the taskbar / dock or off screen.
fn place_window(ctx: &egui::Context, app: &mut GitYasApp) {
    let bar = app.speed.bar_height.unwrap_or(COLLAPSED_HEIGHT);
    let size = vec2(WIDTH, if app.speed.collapsed { bar } else { EXPANDED_HEIGHT });
    if app.speed.placed == Some(size) {
        return;
    }
    let first = app.speed.placed.is_none();
    app.speed.placed = Some(size);
    let (monitor, outer, ppp) = ctx.input(|i| {
        let v = i.viewport();
        (v.monitor_size, v.outer_rect, v.native_pixels_per_point.unwrap_or(1.0))
    });
    let near = outer.map_or(Pos2::ZERO, |r| r.center());
    let work = desktop::work_area(near, ppp).unwrap_or_else(|| {
        let monitor = monitor.unwrap_or(vec2(1920.0, 1080.0));
        Rect::from_min_max(Pos2::ZERO, Pos2::new(monitor.x, monitor.y - TASKBAR_ROOM))
    });
    let corner = match outer {
        Some(outer) if !first => outer.right_bottom(),
        _ => work.right_bottom() - vec2(SCREEN_MARGIN, SCREEN_MARGIN),
    };
    let min = corner_bounds(work, size);
    let corner = corner.clamp(min.min, min.max);
    ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
    ctx.send_viewport_cmd(ViewportCommand::OuterPosition(corner - size));
}

/// Where the window's bottom-right corner may go so a window of `size` stays inside `work` (when it fits).
fn corner_bounds(work: Rect, size: Vec2) -> Rect {
    let min = work.min + size;
    Rect::from_min_max(min, work.max.max(min))
}

// ---- Navigation -------------------------------------------------------------------------------------------------

fn go(ctx: &egui::Context, app: &mut GitYasApp, screen: Screen) {
    let s = &mut app.speed;
    s.collapsed = false;
    if s.screen != screen {
        s.back.push(s.screen);
        s.screen = screen;
    }
    s.scroll_to_cursor = true;
    match screen {
        Screen::Commit => s.focus_summary = true,
        Screen::Prompt(_) => {
            s.prompt_text.clear();
            s.focus_prompt = true;
        }
        _ => release_text_focus(ctx),
    }
}

fn release_text_focus(ctx: &egui::Context) {
    ctx.memory_mut(|m| {
        m.surrender_focus(egui::Id::new(SUMMARY_ID));
        m.surrender_focus(egui::Id::new(PROMPT_ID));
    });
}

/// One screen back; from the menu, collapses the panel.
fn back(ctx: &egui::Context, app: &mut GitYasApp) {
    release_text_focus(ctx);
    let s = &mut app.speed;
    match s.back.pop() {
        Some(screen) => s.screen = screen,
        None if s.screen != Screen::Home => s.screen = Screen::Home,
        None => {
            s.scroll_to_cursor = true;
            tuck_away(app);
            return;
        }
    }
    s.scroll_to_cursor = true;
}

fn cursor(app: &GitYasApp) -> usize {
    match app.speed.screen {
        Screen::Home => app.speed.home,
        Screen::Changes => app.speed.changes,
        Screen::Branches => app.speed.branches,
        Screen::Viewer | Screen::Commit | Screen::Prompt(_) => 0,
    }
}

fn set_cursor(app: &mut GitYasApp, value: usize) {
    let s = &mut app.speed;
    match s.screen {
        Screen::Home => s.home = value,
        Screen::Changes => s.changes = value,
        Screen::Branches => s.branches = value,
        Screen::Viewer | Screen::Commit | Screen::Prompt(_) => {}
    }
    s.scroll_to_cursor = true;
}

/// Moves the cursor of a list of `len` rows by `delta`, clamped to the list.
fn move_cursor(app: &mut GitYasApp, delta: isize, len: usize) {
    let current = cursor(app).min(len.saturating_sub(1));
    let next = current.saturating_add_signed(delta).min(len.saturating_sub(1));
    set_cursor(app, next);
}

// ---- Keys -------------------------------------------------------------------------------------------------------

fn handle_keys(ctx: &egui::Context, app: &mut GitYasApp) {
    let typing = matches!(app.speed.screen, Screen::Commit | Screen::Prompt(_)) && !app.speed.collapsed;
    let events = ctx.input(|i| i.events.clone());
    if !typing {
        // The panel's keys are not egui's focus navigation.
        ctx.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Key { .. })));
    }
    for event in events {
        let Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            continue;
        };
        if typing {
            match app.speed.screen {
                Screen::Prompt(prompt) => prompt_key(ctx, app, prompt, key, modifiers),
                _ => commit_key(ctx, app, key, modifiers),
            }
        } else {
            key_press(ctx, app, key, modifiers);
        }
        if !app.speed.active {
            return;
        }
    }
}

/// Keys of the commit screen (its summary field takes the rest).
fn commit_key(ctx: &egui::Context, app: &mut GitYasApp, key: Key, modifiers: egui::Modifiers) {
    match key {
        Key::Escape => back(ctx, app),
        Key::Enter => {
            worktree::commit(app, modifiers.command);
            if app.bridge.is_running(worktree::TASK_COMMIT) {
                back(ctx, app);
            }
        }
        _ => {}
    }
}

fn key_press(ctx: &egui::Context, app: &mut GitYasApp, key: Key, modifiers: egui::Modifiers) {
    if !(modifiers.is_none() || modifiers.shift_only() && key == Key::Tab) {
        return;
    }
    if app.speed.collapsed {
        // The bar: ↑ / Enter open the panel; action keys still work.
        if matches!(key, Key::ArrowUp | Key::Enter | Key::Space) {
            app.speed.collapsed = false;
            return;
        }
    } else if matches!(key, Key::Escape | Key::Backspace | Key::ArrowLeft) {
        back(ctx, app);
        return;
    }
    let screen = if app.speed.collapsed { None } else { Some(app.speed.screen) };
    let handled = match screen {
        Some(Screen::Home) => home_key(ctx, app, key),
        Some(Screen::Changes) => changes_key(ctx, app, key),
        Some(Screen::Viewer) => viewer_key(app, key, modifiers),
        Some(Screen::Branches) => branches_key(app, key),
        Some(Screen::Commit | Screen::Prompt(_)) | None => false,
    };
    let pressed = Combo::from_event(key, modifiers);
    let speed = &app.store.settings.speed;
    let item = HOME
        .iter()
        .find(|m| Combo::parse(keys::get(speed, Binding::Action(m.item))) == Some(pressed))
        .map(|m| m.item);
    if !handled && let Some(item) = item {
        run_item(ctx, app, item);
    }
}

// ---- Menu -------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Changes,
    Commit,
    Push,
    Pull,
    Fetch,
    Branches,
    NewBranch,
    Stash,
    Pop,
    Tag,
    Exit,
}

pub struct MenuItem {
    pub item: Item,
    glyph: &'static str,
    pub label: &'static str,
}

const fn menu(item: Item, glyph: &'static str, label: &'static str) -> MenuItem {
    MenuItem { item, glyph, label }
}

/// The menu, in order. Each action's key (`SpeedSettings`) works on every screen except while typing.
pub const HOME: [MenuItem; 11] = [
    menu(Item::Changes, icon::FILES, "Stage / unstage changes"),
    menu(Item::Commit, icon::CHECK_CIRCLE, "Commit"),
    menu(Item::Push, icon::ARROW_LINE_UP, "Push"),
    menu(Item::Pull, icon::ARROW_LINE_DOWN, "Pull"),
    menu(Item::Fetch, icon::ARROWS_CLOCKWISE, "Fetch"),
    menu(Item::Branches, icon::GIT_BRANCH, "Switch branch"),
    menu(Item::NewBranch, icon::PLUS, "New branch"),
    menu(Item::Stash, icon::TRAY_ARROW_DOWN, "Stash changes"),
    menu(Item::Pop, icon::TRAY_ARROW_UP, "Pop latest stash"),
    menu(Item::Tag, icon::TAG, "Tag HEAD"),
    menu(Item::Exit, icon::SIGN_OUT, "Exit speed mode"),
];

pub fn item_label(item: Item) -> &'static str {
    HOME.iter().find(|m| m.item == item).map_or("", |m| m.label)
}

fn run_item(ctx: &egui::Context, app: &mut GitYasApp, item: Item) {
    if item == Item::Exit {
        exit(ctx, app);
        return;
    }
    if app.store.repo.is_none() {
        app.notify.info("Open a repository first");
        return;
    }
    match item {
        Item::Changes => go(ctx, app, Screen::Changes),
        Item::Commit => go(ctx, app, Screen::Commit),
        Item::Branches => go(ctx, app, Screen::Branches),
        Item::Push => remote::push_current(app),
        Item::Pull => remote::pull_current(app),
        Item::Fetch => remote::fetch_all(app),
        Item::NewBranch => go(ctx, app, Screen::Prompt(Prompt::Branch)),
        Item::Stash => go(ctx, app, Screen::Prompt(Prompt::Stash)),
        Item::Pop => worktree::pop_latest(app),
        Item::Tag => go(ctx, app, Screen::Prompt(Prompt::Tag)),
        Item::Exit => {}
    }
}

fn home_key(ctx: &egui::Context, app: &mut GitYasApp, key: Key) -> bool {
    match key {
        Key::ArrowUp => move_cursor(app, -1, HOME.len()),
        Key::ArrowDown => move_cursor(app, 1, HOME.len()),
        Key::Enter | Key::Space | Key::ArrowRight => {
            let item = HOME[app.speed.home.min(HOME.len() - 1)].item;
            run_item(ctx, app, item);
        }
        _ => return false,
    }
    true
}

// ---- Changes ----------------------------------------------------------------------------------------------------

/// The changes as listed: conflicted, unstaged, then staged.
fn change_rows(app: &GitYasApp) -> Vec<(Section, Delta)> {
    let Some(repo) = app.store.repo.as_ref() else { return Vec::new() };
    [Section::Conflicted, Section::Unstaged, Section::Staged]
        .into_iter()
        .flat_map(|section| {
            model::section_files(&repo.files, section)
                .into_iter()
                .map(move |d| (section, d.clone()))
        })
        .collect()
}

fn toggle_stage(app: &mut GitYasApp, section: Section, file: Delta) {
    if app.bridge.is_running(worktree::TASK_FILES) {
        return;
    }
    match section {
        Section::Unstaged => worktree::stage(app, vec![file]),
        Section::Staged => worktree::unstage(app, vec![file]),
        Section::Conflicted => app.notify.info("Resolve conflicts in the main window (exit speed mode first)"),
    }
}

fn section_all(app: &GitYasApp, section: Section) -> Vec<Delta> {
    app.store
        .repo
        .as_ref()
        .map(|r| model::section_files(&r.files, section).into_iter().cloned().collect())
        .unwrap_or_default()
}

fn changes_key(ctx: &egui::Context, app: &mut GitYasApp, key: Key) -> bool {
    let rows = change_rows(app);
    let current = rows.get(app.speed.changes.min(rows.len().saturating_sub(1))).cloned();
    match key {
        Key::ArrowUp => move_cursor(app, -1, rows.len()),
        Key::ArrowDown => move_cursor(app, 1, rows.len()),
        Key::Space | Key::Enter => {
            if let Some((section, file)) = current {
                toggle_stage(app, section, file);
            }
        }
        Key::ArrowRight | Key::V => {
            if let Some((section, file)) = current {
                open_viewer(ctx, app, file, section.staged());
            }
        }
        Key::A => {
            let files = section_all(app, Section::Unstaged);
            if !files.is_empty() && !app.bridge.is_running(worktree::TASK_FILES) {
                worktree::stage(app, files);
            }
        }
        Key::U => {
            let files = section_all(app, Section::Staged);
            if !files.is_empty() && !app.bridge.is_running(worktree::TASK_FILES) {
                worktree::unstage(app, files);
            }
        }
        Key::D => match current {
            Some((Section::Unstaged, file)) => worktree::discard(app, vec![file]),
            Some(_) => app.notify.info("Only unstaged changes can be discarded; unstage it first"),
            None => {}
        },
        _ => return false,
    }
    true
}

// ---- Viewer -----------------------------------------------------------------------------------------------------

fn open_viewer(ctx: &egui::Context, app: &mut GitYasApp, file: Delta, staged: bool) {
    app.speed.viewer = Some(Viewer {
        file,
        staged,
        lines: None,
        scroll: 0.0,
        scroll_to: Some(0.0),
        max_scroll: 0.0,
    });
    load_diff(app);
    go(ctx, app, Screen::Viewer);
}

fn load_diff(app: &mut GitYasApp) {
    let Some(viewer) = app.speed.viewer.as_ref() else { return };
    let file = viewer.file.clone();
    let staged = viewer.staged;
    let path = file.path.clone();
    app.bridge.run(
        TASK_DIFF,
        move |engine| async move { engine.file_diff(DiffTarget::WorkingTree { staged }, file).await },
        move |app, result| {
            if let Some(viewer) = app.speed.viewer.as_mut().filter(|v| v.file.path == path && v.staged == staged) {
                viewer.lines = Some(result.map(|d| diff_lines(&Arc::new(d))).map_err(|e| e.to_string()));
            }
        },
    );
}

/// The diff as display lines: hunk headers, then `+` / `-` / context lines.
fn diff_lines(diff: &FileDiff) -> Vec<(LineKind, String)> {
    if let Some(message) = diff.kind.placeholder_message() {
        return vec![(LineKind::Note, message)];
    }
    if diff.hunks.is_empty() {
        return vec![(LineKind::Note, "No changes to show".into())];
    }
    let mut lines = Vec::new();
    for hunk in &diff.hunks {
        lines.push((LineKind::Header, hunk.header.trim_end().to_owned()));
        for line in &hunk.lines {
            let (kind, prefix) = match line.origin {
                LineOrigin::Add => (LineKind::Add, '+'),
                LineOrigin::Del => (LineKind::Del, '-'),
                LineOrigin::Context => (LineKind::Context, ' '),
                LineOrigin::NoNewlineAtEof => (LineKind::Note, '\\'),
            };
            // Tabs as spaces: egui draws a tab as one narrow glyph.
            lines.push((kind, format!("{prefix} {}", line.text.replace('\t', "    "))));
        }
    }
    lines
}

/// After the files changed: the viewed file may have moved between unstaged and staged; follow it and re-fetch.
fn refresh_viewer(app: &mut GitYasApp) {
    let Some(viewer) = app.speed.viewer.as_ref() else { return };
    let path = viewer.file.path.clone();
    let rows = change_rows(app);
    let same = rows.iter().find(|(s, d)| d.path == path && s.staged() == viewer.staged);
    let found = same.or_else(|| rows.iter().find(|(_, d)| d.path == path)).cloned();
    if let (Some((section, file)), Some(viewer)) = (found, app.speed.viewer.as_mut()) {
        viewer.staged = section.staged();
        viewer.file = file;
        load_diff(app);
    }
}

fn viewer_key(app: &mut GitYasApp, key: Key, modifiers: egui::Modifiers) -> bool {
    let rows = change_rows(app);
    let Some(viewer) = app.speed.viewer.as_mut() else { return false };
    let page = EXPANDED_HEIGHT - 160.0;
    let scroll = |viewer: &mut Viewer, by: f32| {
        let to = (viewer.scroll + by).clamp(0.0, viewer.max_scroll.max(0.0));
        viewer.scroll = to;
        viewer.scroll_to = Some(to);
    };
    match key {
        Key::ArrowUp => scroll(viewer, -ROW_HEIGHT * 2.0),
        Key::ArrowDown => scroll(viewer, ROW_HEIGHT * 2.0),
        Key::PageUp => scroll(viewer, -page),
        Key::PageDown => scroll(viewer, page),
        Key::Home => scroll(viewer, f32::NEG_INFINITY),
        Key::End => scroll(viewer, f32::INFINITY),
        Key::Space | Key::Enter => {
            let section = if viewer.staged { Section::Staged } else { Section::Unstaged };
            let file = viewer.file.clone();
            toggle_stage(app, section, file);
        }
        Key::Tab => {
            // Next / previous file of the changes list.
            let at = rows
                .iter()
                .position(|(s, d)| d.path == viewer.file.path && s.staged() == viewer.staged)
                .unwrap_or(app.speed.changes);
            let next = if modifiers.shift {
                at.checked_sub(1)
            } else {
                Some(at + 1).filter(|n| *n < rows.len())
            };
            if let Some((section, file)) = next.and_then(|n| rows.get(n)).cloned() {
                app.speed.changes = next.unwrap_or_default();
                app.speed.viewer = Some(Viewer {
                    file,
                    staged: section.staged(),
                    lines: None,
                    scroll: 0.0,
                    scroll_to: Some(0.0),
                    max_scroll: 0.0,
                });
                load_diff(app);
            }
        }
        _ => return false,
    }
    true
}

// ---- Branches ---------------------------------------------------------------------------------------------------

fn branches_key(app: &mut GitYasApp, key: Key) -> bool {
    let branches: Vec<_> = app.store.repo.as_ref().map(|r| r.local_branches.clone()).unwrap_or_default();
    match key {
        Key::ArrowUp => move_cursor(app, -1, branches.len()),
        Key::ArrowDown => move_cursor(app, 1, branches.len()),
        Key::Enter | Key::Space | Key::ArrowRight => {
            if let Some(branch) = branches.get(app.speed.branches) {
                if branch.is_head {
                    app.notify.info(format!("Already on {}", branch.name));
                } else {
                    history::run(
                        app,
                        vec![history::Intent::CheckoutBranch {
                            refname: branch.refname.clone(),
                            label: branch.name.clone(),
                        }],
                    );
                }
            }
        }
        _ => return false,
    }
    true
}

// ---- Prompts (tag / new branch / stash) ---------------------------------------------------------------------

/// Why the prompt's text cannot be submitted yet; `None` when it can.
fn prompt_problem(app: &GitYasApp, prompt: Prompt, text: &str) -> Option<&'static str> {
    let repo = app.store.repo.as_ref()?;
    let name = text.trim();
    match prompt {
        Prompt::Stash if !repo.has_wip() => Some("Nothing to stash"),
        Prompt::Stash => None,
        Prompt::Tag | Prompt::Branch => {
            if let Some(problem) = history::model::ref_name_error(name) {
                return Some(problem);
            }
            let taken = match prompt {
                Prompt::Tag => repo.commits.tag(name).is_some(),
                _ => repo.local_branches.iter().any(|b| b.name == name),
            };
            if taken {
                return Some("Already exists");
            }
            (prompt == Prompt::Tag && repo.head_target().is_none()).then_some("No commits to tag yet")
        }
    }
}

/// Keys of the prompt screen (its field takes the rest): Enter submits, Ctrl / Cmd+Enter also pushes a tag.
fn prompt_key(ctx: &egui::Context, app: &mut GitYasApp, prompt: Prompt, key: Key, modifiers: egui::Modifiers) {
    match key {
        Key::Escape => back(ctx, app),
        Key::Enter => {
            let text = app.speed.prompt_text.clone();
            if let Some(problem) = prompt_problem(app, prompt, &text) {
                app.notify.info(problem);
                app.speed.focus_prompt = true;
                return;
            }
            if app.bridge.is_running(TASK_PROMPT) {
                return;
            }
            submit_prompt(app, prompt, text, modifiers.command);
            back(ctx, app);
        }
        _ => {}
    }
}

fn submit_prompt(app: &mut GitYasApp, prompt: Prompt, text: String, push: bool) {
    let name = text.trim().to_owned();
    match prompt {
        Prompt::Tag => {
            let Some(head) = app.store.repo.as_ref().and_then(|r| r.head_target()).cloned() else {
                return;
            };
            let remote = app
                .store
                .repo
                .as_ref()
                .and_then(|r| r.remotes.iter().find(|n| *n == "origin").or_else(|| r.remotes.first()).cloned());
            let n = name.clone();
            app.bridge.run(
                TASK_PROMPT,
                move |engine| async move { engine.create_tag(head, n, None).await },
                move |app, result| match result {
                    Ok(()) => {
                        app.notify.success(format!("Created tag {name}"));
                        if let (true, Some(remote)) = (push, remote) {
                            history::run(app, vec![history::Intent::PushTag { remote, name }]);
                        }
                    }
                    Err(err) => app.report_error(&format!("Could not create tag {name}"), &err),
                },
            );
        }
        Prompt::Branch => {
            let n = name.clone();
            app.bridge.run(
                TASK_PROMPT,
                move |engine| async move { engine.create_branch(n, None, true).await },
                move |app, result| match result {
                    Ok(_) => app.notify.success(format!("Created and switched to {name}")),
                    Err(err) => app.report_error(&format!("Could not create branch {name}"), &err),
                },
            );
        }
        Prompt::Stash => stash_all(app, model::stash_message_arg(&text)),
    }
}

/// Stashes everything (untracked files too), asking for a signature first if there is none (stashes are commits).
fn stash_all(app: &mut GitYasApp, message: Option<String>) {
    if app.store.signature().is_none() {
        ui::settings::request_signature(app, Some(Box::new(move |app: &mut GitYasApp| stash_all(app, message))));
        return;
    }
    app.bridge.run(
        TASK_PROMPT,
        move |engine| async move { engine.stash_all(message, true).await },
        |app, result| match result {
            Ok(()) => app.notify.success("Stashed all changes"),
            Err(gityas_core::GitYasError::InvalidInput(message)) => app.notify.info(message),
            Err(err) => app.report_error("Could not stash changes", &err),
        },
    );
}

fn prompt_screen(ui: &mut Ui, app: &mut GitYasApp, prompt: Prompt) {
    let p = theme::pal(ui);
    title(ui, prompt.title());
    let repo = app.store.repo.as_ref();
    let detail = match prompt {
        Prompt::Tag => repo
            .and_then(|r| r.head_target())
            .map(|id| format!("Lightweight tag on {}", id.short(7))),
        Prompt::Branch => repo
            .and_then(|r| r.head.as_ref())
            .map(|h| format!("From {} and switch to it", h.label())),
        Prompt::Stash => repo.map(|r| {
            let n = r.files.unstaged.len() + r.files.staged.len();
            format!("{n} changed {} (untracked files included)", model::files_word(n))
        }),
    };
    if let Some(detail) = detail {
        ui.label(RichText::new(detail).size(12.0).color(p.text_muted));
    }
    ui.add_space(6.0);
    let focus = std::mem::take(&mut app.speed.focus_prompt);
    let resp = ui.add(
        egui::TextEdit::singleline(&mut app.speed.prompt_text)
            .id(egui::Id::new(PROMPT_ID))
            .hint_text(prompt.hint())
            .desired_width(f32::INFINITY),
    );
    if focus {
        resp.request_focus();
    }
    let text = app.speed.prompt_text.clone();
    if let Some(problem) = prompt_problem(app, prompt, &text)
        && (prompt == Prompt::Stash || !text.trim().is_empty())
    {
        ui.label(RichText::new(problem).size(12.0).color(p.orange));
    }
    if app.bridge.is_running(TASK_PROMPT) {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.label(RichText::new("Working…").color(p.text_muted));
        });
    }
}

// ---- Events -----------------------------------------------------------------------------------------------------

pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    if !app.speed.active {
        return;
    }
    match event {
        AppEvent::GitFilesChanged { .. } => refresh_viewer(app),
        AppEvent::SelectedRepositoryUpdated(_) => {
            app.speed.viewer = None;
            if app.speed.screen == Screen::Viewer {
                app.speed.screen = Screen::Changes;
            }
        }
        _ => {}
    }
}

fn newest_toast(app: &GitYasApp) -> Option<u64> {
    app.notify.newest().map(|(id, _)| id)
}

// ---- Drawing ----------------------------------------------------------------------------------------------------

/// One frame of speed mode. `modal_open`: a dialog has the keys.
pub fn frame(ui: &mut Ui, app: &mut GitYasApp, modal_open: bool) {
    let ctx = ui.ctx().clone();
    if !modal_open && app.confirm.is_none() {
        handle_keys(&ctx, app);
    }
    if !app.speed.active {
        return;
    }
    // A dialog needs the open panel.
    if modal_open || app.confirm.is_some() {
        app.speed.collapsed = false;
    }
    place_window(&ctx, app);
    let now = ctx.input(|i| i.time);
    app.notify.cap_lifetimes(TOAST_SECS, ERROR_TOAST_SECS);
    if app.speed.collapsed {
        // Toasts are only drawn on the open panel; keep them ageing so old ones do not pile up behind the bar.
        let dt = f64::from(ctx.input(|i| i.stable_dt)).min(0.1);
        app.notify.tick(dt, None);
    }
    let newest = newest_toast(app);
    if newest != app.speed.seen_toast {
        app.speed.seen_toast = newest;
        app.speed.flash = app.notify.newest().map(|(_, title)| (title.to_owned(), now));
    }

    let p = theme::pal(ui);
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(p.bg_app)
                .stroke(egui::Stroke::new(1.0, p.border))
                .inner_margin(egui::Margin::symmetric(10, 8)),
        )
        .show(ui, |ui| {
            header(ui, app, now);
            if !app.speed.active {
                return;
            }
            if app.speed.collapsed {
                // The menu's hints (move, run, collapse, hide / show, exit) stay in view on the bar.
                ui.add_space(2.0);
                let keys_height = hints(ui, app, Screen::Home);
                // Fit the window to the bar (custom keys may wrap to another row). Only the content's own height
                // counts, never the panel's (which follows the window): measuring that would grow the window
                // every frame.
                let height = bar_height(keys_height);
                if app.speed.bar_height.is_none_or(|h| (h - height).abs() > 1.0) {
                    app.speed.bar_height = Some(height);
                }
                return;
            }
            ui.add_space(4.0);
            ui.separator();
            egui::Panel::bottom("speed_hints")
                .frame(egui::Frame::new().inner_margin(egui::Margin {
                    top: 6,
                    ..Default::default()
                }))
                .show_separator_line(true)
                .resizable(false)
                .show(ui, |ui| {
                    hints(ui, app, app.speed.screen);
                });
            egui::CentralPanel::default()
                .frame(egui::Frame::new())
                .show(ui, |ui| match app.speed.screen {
                    Screen::Home => home(ui, app),
                    Screen::Changes => changes(ui, app),
                    Screen::Viewer => viewer(ui, app),
                    Screen::Commit => commit(ui, app),
                    Screen::Prompt(prompt) => prompt_screen(ui, app, prompt),
                    Screen::Branches => branches(ui, app),
                });
        });
    app.speed.scroll_to_cursor = false;
    // Keep the flash timing live.
    if app.speed.flash.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

/// Header row (the window's drag handle): repository, branch with ahead / behind, change counts, then collapse and
/// exit. Collapsed, it is the whole panel and shows the newest toast for a few seconds.
fn header(ui: &mut Ui, app: &mut GitYasApp, now: f64) {
    let p = theme::pal(ui);
    let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 34.0));
    let drag = ui.interact(rect, ui.id().with("speed_drag"), Sense::click_and_drag());
    if drag.drag_started() {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if drag.double_clicked() {
        app.speed.collapsed = !app.speed.collapsed;
    }
    let repo = app.store.repo.as_ref();
    let name = app
        .store
        .repositories
        .iter()
        .find(|r| app.store.active.as_ref() == Some(&r.id))
        .map_or("No repository", |r| r.name.as_str())
        .to_owned();
    let branch = repo.and_then(|r| r.head.as_ref()).map(|h| h.label());
    let (ahead, behind) = repo.and_then(|r| r.head_branch()).map_or((0, 0), |b| (b.ahead, b.behind));
    let (unstaged, staged) = repo.map_or((0, 0), |r| (r.files.unstaged.len(), r.files.staged.len()));
    let busy = [
        remote::TASK_PUSH,
        remote::TASK_PULL,
        remote::TASK_FETCH,
        worktree::TASK_COMMIT,
        worktree::TASK_FILES,
        worktree::TASK_STASH,
        worktree::TASK_POP,
    ]
    .iter()
    .any(|k| app.bridge.is_running(k));
    let flash = app
        .speed
        .flash
        .as_ref()
        .filter(|(_, at)| now - at < FLASH_SECS)
        .map(|(t, _)| t.clone());
    if flash.is_none() {
        app.speed.flash = None;
    }
    let collapsed = app.speed.collapsed;
    let summon_key = keys::label(&app.store.settings.speed.summon);
    let exit_key = keys::label(&app.store.settings.speed.exit);
    let mut toggle = false;
    let mut leave = false;
    ui.allocate_ui_with_layout(rect.size(), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(rect.size());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            leave = ui::icon_button(ui, icon::X, &format!("Exit speed mode ({exit_key})")).clicked();
            let (glyph, tip) = if collapsed {
                (icon::CARET_UP, format!("Open ({summon_key} or ↑)"))
            } else {
                (icon::CARET_DOWN, format!("Collapse ({summon_key} or Esc on the menu)"))
            };
            toggle = ui::icon_button(ui, glyph, &tip).clicked();
            if busy {
                ui.add(egui::Spinner::new().size(14.0).color(p.accent));
            }
            // The counts sit beside the buttons, laid out before the names: a long branch name truncates instead
            // of pushing them out of view.
            let mut counts = Vec::new();
            if ahead > 0 {
                counts.push(format!("↑{ahead}"));
            }
            if behind > 0 {
                counts.push(format!("↓{behind}"));
            }
            counts.push(format!("{unstaged}·{staged}"));
            ui.label(RichText::new(counts.join(" ")).size(11.5).color(p.text_muted))
                .on_hover_text(format!("{unstaged} unstaged · {staged} staged"));
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(icon::LIGHTNING).size(16.0).color(p.accent));
                if let Some(flash) = flash.filter(|_| collapsed) {
                    ui.add(egui::Label::new(RichText::new(flash).color(p.text)).truncate());
                    return;
                }
                // The repository name gets at most 40% of the room; the branch takes the rest. Both truncate.
                let height = ui.available_height();
                let name_text = RichText::new(name).strong();
                let name_width = egui::WidgetText::from(name_text.clone())
                    .into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body)
                    .size()
                    .x;
                let room = ui.available_width();
                let cap = if branch.is_some() { room * 0.4 } else { room };
                ui.allocate_ui(vec2(name_width.min(cap), height), |ui| {
                    ui.add(egui::Label::new(name_text).truncate());
                });
                if let Some(branch) = branch {
                    ui.add(egui::Label::new(RichText::new(format!("{} {branch}", icon::GIT_BRANCH)).color(p.accent)).truncate())
                        .on_hover_text(branch.as_str());
                }
            });
        });
    });
    if toggle {
        app.speed.collapsed = !app.speed.collapsed;
    }
    if leave {
        exit(ui.ctx(), app);
    }
}

/// Window height of the collapsed bar whose hint rows are `keys_height` tall: frame margins (8 + 8), the header
/// (34), the gap above the keys (2) and a little air; clamped so a bad measurement can never run away.
pub fn bar_height(keys_height: f32) -> f32 {
    (8.0 + 34.0 + 2.0 + keys_height + 8.0 + 4.0).ceil().clamp(52.0, 200.0)
}

/// Hotkey hints of `screen` (action keys as configured). Returns their height.
fn hints(ui: &mut Ui, app: &GitYasApp, screen: Screen) -> f32 {
    let speed = &app.store.settings.speed;
    let key = |item: Item| keys::label(keys::get(speed, Binding::Action(item)));
    let fixed = |pairs: &[(&str, &'static str)]| pairs.iter().map(|(k, w)| ((*k).to_owned(), *w)).collect::<Vec<_>>();
    let summon = keys::label(&speed.summon);
    let keys: Vec<(String, &str)> = match screen {
        Screen::Home => {
            let mut v = fixed(&[("↑↓", "move"), ("Enter", "run"), ("Esc", "collapse")]);
            v.push((summon, "hide / show"));
            v.push((key(Item::Exit), "exit"));
            v
        }
        Screen::Changes => {
            let mut v = fixed(&[
                ("Space", "stage/unstage"),
                ("A", "stage all"),
                ("U", "unstage all"),
                ("→", "view"),
                ("D", "discard"),
            ]);
            v.push((key(Item::Commit), "commit"));
            v.push(("Esc".into(), "back"));
            v
        }
        Screen::Viewer => fixed(&[
            ("↑↓ PgUp PgDn", "scroll"),
            ("Space", "stage/unstage"),
            ("Tab", "next file"),
            ("Esc", "back"),
        ]),
        Screen::Commit => fixed(&[("Enter", "commit"), ("Ctrl+Enter", "commit & push"), ("Esc", "back")]),
        Screen::Prompt(Prompt::Tag) => fixed(&[("Enter", "create"), ("Ctrl+Enter", "create & push"), ("Esc", "back")]),
        Screen::Prompt(Prompt::Branch) => fixed(&[("Enter", "create & switch"), ("Esc", "back")]),
        Screen::Prompt(Prompt::Stash) => fixed(&[("Enter", "stash"), ("Esc", "back")]),
        Screen::Branches => {
            let mut v = fixed(&[("↑↓", "move"), ("Enter", "check out")]);
            v.push((key(Item::NewBranch), "new branch"));
            v.push(("Esc".into(), "back"));
            v
        }
    };
    let p = theme::pal(ui);
    let rows = ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
        for (key, what) in keys {
            ui.label(RichText::new(key).monospace().size(11.0).color(p.text).background_color(p.bg_input));
            ui.label(RichText::new(what).size(11.0).color(p.text_muted));
            ui.add_space(6.0);
        }
    });
    rows.response.rect.height()
}

fn title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(13.0).strong());
    ui.add_space(4.0);
}

/// A selectable list row; `paint` draws its content into the row's rect. Returns the response.
fn list_row(ui: &mut Ui, selected: bool, scroll: bool, paint: impl FnOnce(&egui::Painter, Rect)) -> egui::Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 6, p.bg_selected);
        ui.painter().rect_filled(
            Rect::from_min_size(rect.min + vec2(0.0, 5.0), vec2(3.0, rect.height() - 10.0)),
            2,
            p.accent,
        );
        if scroll {
            resp.scroll_to_me(None);
        }
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 6, p.bg_hover);
    }
    paint(ui.painter(), rect.shrink2(vec2(10.0, 0.0)));
    resp
}

fn key_badge(painter: &egui::Painter, p: &theme::Palette, left_center: Pos2, key: &str) -> f32 {
    let galley = painter.layout_no_wrap(key.to_owned(), FontId::monospace(11.0), p.text);
    let size = galley.size() + vec2(10.0, 4.0);
    let rect = Rect::from_min_size(left_center - vec2(0.0, size.y / 2.0), size);
    painter.rect_filled(rect, 4, p.bg_input);
    painter.galley(rect.center() - galley.size() / 2.0, galley, p.text);
    size.x
}

fn home(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    if app.store.repo.is_none() {
        ui.label(RichText::new("Open a repository in GitYas first.").color(p.text_muted));
        ui.add_space(6.0);
    }
    let repo = app.store.repo.as_ref();
    let (ahead, behind) = repo.and_then(|r| r.head_branch()).map_or((0, 0), |b| (b.ahead, b.behind));
    let changes = repo.map_or(0, |r| r.files.unstaged.len() + r.files.staged.len());
    let staged = repo.map_or(0, |r| r.files.staged.len());
    let stashes = repo.map_or(0, |r| r.stashes.len());
    let mut run = None;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for (i, entry) in HOME.iter().enumerate() {
            let detail = match entry.item {
                Item::Changes => (changes > 0).then(|| format!("{changes}")),
                Item::Commit => (staged > 0).then(|| format!("{staged} staged")),
                Item::Push => (ahead > 0).then(|| format!("↑{ahead}")),
                Item::Pull => (behind > 0).then(|| format!("↓{behind}")),
                Item::Pop => (stashes > 0).then(|| format!("{stashes}")),
                _ => None,
            };
            let key = keys::label(keys::get(&app.store.settings.speed, Binding::Action(entry.item)));
            let resp = list_row(ui, app.speed.home == i, app.speed.scroll_to_cursor, |painter, rect| {
                let mut x = rect.left();
                x += key_badge(painter, p, Pos2::new(x, rect.center().y), &key) + 10.0;
                painter.text(
                    Pos2::new(x, rect.center().y),
                    Align2::LEFT_CENTER,
                    entry.glyph,
                    FontId::proportional(15.0),
                    p.accent,
                );
                painter.text(
                    Pos2::new(x + 24.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    entry.label,
                    FontId::proportional(13.0),
                    p.text,
                );
                if let Some(detail) = detail {
                    painter.text(
                        rect.right_center(),
                        Align2::RIGHT_CENTER,
                        detail,
                        FontId::proportional(12.0),
                        p.text_muted,
                    );
                }
            });
            if resp.clicked() {
                app.speed.home = i;
                run = Some(entry.item);
            }
        }
    });
    if let Some(item) = run {
        let ctx = ui.ctx().clone();
        run_item(&ctx, app, item);
    }
}

fn changes(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let rows = change_rows(app);
    if rows.is_empty() {
        title(ui, "Changes");
        ui.label(RichText::new("Nothing to commit: the working tree is clean.").color(p.text_muted));
        return;
    }
    app.speed.changes = app.speed.changes.min(rows.len() - 1);
    let busy = app.bridge.is_running(worktree::TASK_FILES);
    let mut clicked = None;
    let mut open = None;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let mut section = None;
        for (i, (s, file)) in rows.iter().enumerate() {
            if section != Some(*s) {
                section = Some(*s);
                let n = rows.iter().filter(|(x, _)| x == s).count();
                let label = match s {
                    Section::Conflicted => "CONFLICTED",
                    Section::Unstaged => "UNSTAGED",
                    Section::Staged => "STAGED",
                };
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{label}  {n}")).size(11.0).strong().color(p.text_muted));
                    if busy {
                        ui.add(egui::Spinner::new().size(10.0));
                    }
                });
            }
            let (glyph, color) = ui::change_icon(p, file.change);
            let name = file
                .path
                .file_name()
                .map_or_else(|| file.path.to_string_lossy(), |n| n.to_string_lossy());
            let dir = file.path.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
            let resp = list_row(ui, app.speed.changes == i, app.speed.scroll_to_cursor, |painter, rect| {
                painter.text(
                    Pos2::new(rect.left() + 6.0, rect.center().y),
                    Align2::CENTER_CENTER,
                    glyph,
                    FontId::proportional(13.0),
                    color,
                );
                let x = rect.left() + 20.0;
                let name_rect = ui::paint_truncated(
                    painter,
                    Pos2::new(x, rect.center().y),
                    &name,
                    FontId::proportional(13.0),
                    p.text,
                    rect.width() - 30.0,
                );
                if !dir.is_empty() {
                    ui::paint_truncated(
                        painter,
                        Pos2::new(name_rect.right() + 8.0, rect.center().y),
                        &dir,
                        FontId::proportional(11.5),
                        p.text_faint,
                        (rect.right() - name_rect.right() - 8.0).max(0.0),
                    );
                }
            });
            if resp.double_clicked() {
                open = Some((file.clone(), s.staged()));
            } else if resp.clicked() {
                clicked = Some(i);
            }
        }
    });
    if let Some(i) = clicked {
        app.speed.changes = i;
    }
    if let Some((file, staged)) = open {
        let ctx = ui.ctx().clone();
        open_viewer(&ctx, app, file, staged);
    }
}

fn viewer(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let Some(viewer) = app.speed.viewer.as_mut() else {
        app.speed.screen = Screen::Changes;
        return;
    };
    ui.horizontal(|ui| {
        let (glyph, color) = ui::change_icon(p, viewer.file.change);
        ui.label(RichText::new(glyph).color(color));
        ui.add(egui::Label::new(RichText::new(viewer.file.path.to_string_lossy()).strong()).truncate());
    });
    let state = if viewer.staged { "Staged" } else { "Unstaged" };
    ui.label(RichText::new(state).size(11.0).color(p.text_muted));
    ui.add_space(4.0);
    let lines = match &viewer.lines {
        None => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(RichText::new("Loading…").color(p.text_muted));
            });
            return;
        }
        Some(Err(err)) => {
            ui.label(RichText::new(err).color(p.red));
            return;
        }
        Some(Ok(lines)) => lines,
    };
    let font = FontId::monospace(12.0);
    let row_height = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    let mut area = egui::ScrollArea::both().id_salt("speed_viewer").auto_shrink(false);
    if let Some(to) = viewer.scroll_to.take() {
        area = area.vertical_scroll_offset(to);
    }
    let out = area.show_rows(ui, row_height, lines.len(), |ui, range| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        for (kind, text) in &lines[range] {
            let (color, bg) = match kind {
                LineKind::Header => (p.accent, Some(p.bg_input)),
                LineKind::Add => (p.green, Some(p.green.gamma_multiply(0.12))),
                LineKind::Del => (p.red, Some(p.red.gamma_multiply(0.12))),
                LineKind::Context => (p.text_muted, None),
                LineKind::Note => (p.text_faint, None),
            };
            let mut rich = RichText::new(text).font(font.clone()).color(color);
            if let Some(bg) = bg {
                rich = rich.background_color(bg);
            }
            ui.label(rich);
        }
    });
    viewer.scroll = out.state.offset.y;
    viewer.max_scroll = (out.content_size.y - out.inner_rect.height()).max(0.0);
}

fn commit(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let Some(id) = app.store.active.clone() else { return };
    let Some(repo) = app.store.repo.as_ref() else { return };
    let staged: Vec<Delta> = repo.files.staged.clone();
    let branch = repo.head.as_ref().map_or_else(|| "…".to_string(), |h| h.label());
    let committing = app.bridge.is_running(worktree::TASK_COMMIT);
    title(ui, "Commit");
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} staged {} on", staged.len(), model::files_word(staged.len())))
                .size(12.0)
                .color(p.text_muted),
        );
        ui.label(RichText::new(format!("{} {branch}", icon::GIT_BRANCH)).size(12.0).color(p.accent));
    });
    ui.add_space(6.0);
    let focus = std::mem::take(&mut app.speed.focus_summary);
    let summary = &mut app.tabs.entry(id).or_default().worktree.summary;
    let resp = ui.add_enabled(
        !committing,
        egui::TextEdit::singleline(summary)
            .id(egui::Id::new(SUMMARY_ID))
            .hint_text("Summary (required)")
            .desired_width(f32::INFINITY),
    );
    if focus {
        resp.request_focus();
    }
    if committing {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.label(RichText::new("Committing…").color(p.text_muted));
        });
    }
    ui.add_space(8.0);
    if staged.is_empty() {
        ui.label(
            RichText::new("Nothing is staged. Esc, then C to stage changes.")
                .size(12.0)
                .color(p.orange),
        );
        return;
    }
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        for file in &staged {
            let (glyph, color) = ui::change_icon(p, file.change);
            ui.horizontal(|ui| {
                ui.label(RichText::new(glyph).color(color));
                ui.add(egui::Label::new(RichText::new(file.path.to_string_lossy()).size(12.0)).truncate());
            });
        }
    });
}

fn branches(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let Some(repo) = app.store.repo.as_ref() else { return };
    let list = repo.local_branches.clone();
    title(ui, "Switch branch");
    if list.is_empty() {
        ui.label(RichText::new("No local branches yet.").color(p.text_muted));
        return;
    }
    app.speed.branches = app.speed.branches.min(list.len() - 1);
    let checking_out = app.bridge.is_running("checkout");
    let mut clicked = None;
    let mut checkout = false;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for (i, branch) in list.iter().enumerate() {
            let mut detail = Vec::new();
            if branch.ahead > 0 {
                detail.push(format!("↑{}", branch.ahead));
            }
            if branch.behind > 0 {
                detail.push(format!("↓{}", branch.behind));
            }
            if let Some(upstream) = &branch.upstream {
                detail.push(upstream.clone());
            }
            let resp = list_row(ui, app.speed.branches == i, app.speed.scroll_to_cursor, |painter, rect| {
                let (glyph, color) = if branch.is_head {
                    (icon::CHECK, p.green)
                } else {
                    (icon::GIT_BRANCH, p.text_muted)
                };
                painter.text(
                    Pos2::new(rect.left() + 6.0, rect.center().y),
                    Align2::CENTER_CENTER,
                    glyph,
                    FontId::proportional(13.0),
                    color,
                );
                let detail_rect = painter.text(
                    rect.right_center(),
                    Align2::RIGHT_CENTER,
                    detail.join("  "),
                    FontId::proportional(11.5),
                    p.text_faint,
                );
                ui::paint_truncated(
                    painter,
                    Pos2::new(rect.left() + 20.0, rect.center().y),
                    &branch.name,
                    FontId::proportional(13.0),
                    if branch.is_head { p.text } else { p.text_muted },
                    (detail_rect.left() - rect.left() - 30.0).max(0.0),
                );
            });
            if resp.double_clicked() {
                clicked = Some(i);
                checkout = true;
            } else if resp.clicked() {
                clicked = Some(i);
            }
        }
    });
    if checking_out {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.label(RichText::new("Checking out…").color(p.text_muted));
        });
    }
    if let Some(i) = clicked {
        app.speed.branches = i;
        if checkout {
            branches_key(app, Key::Enter);
        }
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::testutil::{TestApp, temp_repo, write_file};

    fn press(t: &mut TestApp, key: Key) {
        t.harness.key_press(key);
        t.harness.step();
    }

    #[test]
    fn keyboard_stages_and_commits() {
        let mut t = TestApp::engine();
        let (dir, repo) = temp_repo();
        t.add_and_open(dir.path());
        write_file(&repo, "a.txt", "a\n");
        write_file(&repo, "b.txt", "b\n");
        let engine = t.app().bridge.engine().clone();
        t.block_on(engine.update_commit_signature(gityas_core::models::CommitSignature::new("Ada Lovelace", "ada@example.com")))
            .unwrap();
        t.block_on(engine.refresh(gityas_engine::Refresh::FILES)).unwrap();
        t.settle_until("two unstaged files", |app| {
            app.store.repo.as_ref().is_some_and(|r| r.files.unstaged.len() == 2)
        });

        let ctx = t.harness.ctx.clone();
        enter(&ctx, t.app());
        t.harness.step();
        assert!(t.app().speed.active && t.app().speed.collapsed(), "the button enters collapsed");

        // C opens the changes (also from the collapsed bar); Space stages the first file.
        press(&mut t, Key::C);
        assert_eq!(t.app().speed.screen(), Screen::Changes);
        press(&mut t, Key::Space);
        t.settle_until("one file staged", |app| {
            app.store.repo.as_ref().is_some_and(|r| r.files.staged.len() == 1)
        });
        t.settle();
        // A stages the rest.
        press(&mut t, Key::A);
        t.settle_until("all staged", |app| {
            app.store
                .repo
                .as_ref()
                .is_some_and(|r| r.files.unstaged.is_empty() && r.files.staged.len() == 2)
        });

        // M opens the commit box; type a summary and press Enter.
        press(&mut t, Key::M);
        assert_eq!(t.app().speed.screen(), Screen::Commit);
        t.harness.step();
        t.harness
            .get_by(|n| n.placeholder() == Some("Summary (required)"))
            .type_text("Speed commit");
        t.harness.step();
        press(&mut t, Key::Enter);
        t.settle_until("committed", |app| {
            app.store.repo.as_ref().is_some_and(|r| r.commits.len() == 2 && !r.has_wip())
        });

        exit(&ctx, t.app());
        assert!(!t.app().speed.active);
    }

    #[test]
    fn escape_goes_back_then_collapses() {
        let mut t = TestApp::mock();
        let ctx = t.harness.ctx.clone();
        // The summon shortcut enters speed mode with the panel open.
        summon(&ctx, t.app());
        t.harness.step();
        assert!(t.app().speed.expanded());
        press(&mut t, Key::B);
        assert_eq!(t.app().speed.screen(), Screen::Branches);
        press(&mut t, Key::Escape);
        assert_eq!(t.app().speed.screen(), Screen::Home);
        assert!(!t.app().speed.collapsed());
        press(&mut t, Key::Escape);
        assert!(t.app().speed.collapsed());
        // ↑ on the bar opens the panel again.
        press(&mut t, Key::ArrowUp);
        assert!(t.app().speed.expanded());
    }

    #[test]
    fn tag_and_branch_prompts_stay_inside_the_panel() {
        let mut t = TestApp::engine();
        let (dir, repo) = temp_repo();
        t.add_and_open(dir.path());
        let ctx = t.harness.ctx.clone();
        summon(&ctx, t.app());
        t.harness.step();

        press(&mut t, Key::T);
        assert_eq!(t.app().speed.screen(), Screen::Prompt(Prompt::Tag));
        t.harness.step();
        assert!(t.harness.query_by_label("Create tag").is_none(), "no main-window dialog");
        t.harness.step();
        t.harness
            .get_by(|n| n.placeholder() == Some("Tag name, e.g. v1.0.0"))
            .type_text("v1.0");
        t.harness.step();
        press(&mut t, Key::Enter);
        t.settle_until("tag created", |app| {
            app.store.repo.as_ref().is_some_and(|r| r.commits.tag("v1.0").is_some())
        });
        // The refresh can land a frame before the tag command's completion, and a prompt ignores Enter while the
        // previous one is still running.
        t.settle();
        assert_eq!(t.app().speed.screen(), Screen::Home);

        press(&mut t, Key::N);
        t.harness.step();
        t.harness.get_by(|n| n.placeholder() == Some("Branch name")).type_text("feature");
        t.harness.step();
        press(&mut t, Key::Enter);
        t.settle_until("on the new branch", |app| {
            app.store
                .repo
                .as_ref()
                .and_then(|r| r.head_branch())
                .is_some_and(|b| b.name == "feature")
        });
        assert!(repo.find_branch("feature", git2::BranchType::Local).is_ok());
    }

    #[test]
    fn the_collapsed_bar_shows_the_menu_hints_and_toasts_stay_short() {
        let mut t = TestApp::mock();
        let ctx = t.harness.ctx.clone();
        enter(&ctx, t.app());
        t.harness.step();
        assert!(t.app().speed.collapsed());
        // The menu's hints, not every action.
        for hint in ["move", "run", "collapse", "hide / show", "exit"] {
            t.harness.get_by_label(hint);
        }
        assert!(t.harness.query_by_label("Push").is_none());
        // The bar's height is measured from its content and settles (it must not grow with the window).
        for _ in 0..5 {
            t.harness.step();
        }
        let settled = t.app().speed.bar_height;
        t.harness.set_size(egui::vec2(WIDTH, 400.0));
        for _ in 0..5 {
            t.harness.step();
        }
        assert_eq!(t.app().speed.bar_height, settled);
        assert!(settled.is_some_and(|h| (52.0..=200.0).contains(&h)), "{settled:?}");
        let resizing = |t: &TestApp| {
            t.harness
                .output()
                .viewport_output
                .values()
                .any(|v| v.commands.iter().any(|c| matches!(c, ViewportCommand::InnerSize(_))))
        };
        assert!(!resizing(&t), "a settled bar stops resizing the window");

        t.app().notify.info("hello");
        t.app().notify.error("broke", &gityas_core::GitYasError::InvalidInput("x".into()));
        t.harness.step();
        for _ in 0..80 {
            t.app().notify.tick(0.1, None);
        }
        assert_eq!(t.app().notify.toasts().count(), 0, "even the error is gone after 6 s in speed mode");
    }

    #[test]
    fn the_panel_is_kept_inside_the_work_area() {
        // A 1440 x 900 screen with an 80 pt dock at the bottom.
        let work = Rect::from_min_max(Pos2::ZERO, Pos2::new(1440.0, 820.0));
        let size = vec2(WIDTH, EXPANDED_HEIGHT);
        let bounds = corner_bounds(work, size);
        // A corner under the dock is pulled up above it; one inside stays.
        assert_eq!(Pos2::new(1424.0, 884.0).clamp(bounds.min, bounds.max), Pos2::new(1424.0, 820.0));
        assert_eq!(Pos2::new(1000.0, 700.0).clamp(bounds.min, bounds.max), Pos2::new(1000.0, 700.0));
        // Never pushed off the top / left either.
        assert_eq!(
            Pos2::new(10.0, 10.0).clamp(bounds.min, bounds.max),
            Pos2::new(WIDTH, EXPANDED_HEIGHT)
        );
    }

    #[test]
    fn a_long_branch_name_keeps_the_counts_in_view() {
        let mut t = TestApp::engine();
        let (dir, repo) = temp_repo();
        let long = "feature/a-really-long-branch-name-that-goes-on-and-on-and-never-seems-to-end";
        let head = repo.head().and_then(|h| h.peel_to_commit()).unwrap();
        repo.branch(long, &head, false).unwrap();
        repo.set_head(&format!("refs/heads/{long}")).unwrap();
        write_file(
            &repo, "a.txt", "a
",
        );
        t.add_and_open(dir.path());
        t.settle_until("on the long branch with a change", |app| {
            app.store
                .repo
                .as_ref()
                .is_some_and(|r| r.head_branch().is_some_and(|b| b.name == long) && r.files.unstaged.len() == 1)
        });
        t.harness.set_size(egui::vec2(WIDTH, 300.0));
        let ctx = t.harness.ctx.clone();
        summon(&ctx, t.app());
        for _ in 0..3 {
            t.harness.step();
        }
        let counts = t.harness.get_by_label("1·0").rect();
        let collapse = t.harness.get_by_label(icon::CARET_DOWN).rect();
        assert!(
            counts.right() <= collapse.left(),
            "counts {counts:?} run under the button {collapse:?}"
        );
        assert!(counts.left() >= 0.0 && counts.width() > 0.0);
    }

    #[test]
    fn action_keys_follow_the_settings() {
        let mut t = TestApp::mock();
        t.app().store.settings.speed.changes = "J".into();
        let ctx = t.harness.ctx.clone();
        summon(&ctx, t.app());
        t.harness.step();
        press(&mut t, Key::C);
        assert_eq!(t.app().speed.screen(), Screen::Home, "C is no longer bound");
        press(&mut t, Key::J);
        assert_eq!(t.app().speed.screen(), Screen::Changes);
    }

    /// The panel at its real size, for the README and the website.
    #[test]
    #[ignore = "needs a GPU"]
    fn snapshot_speed_mode() {
        let mut t = TestApp::mock_gpu();
        t.harness.step();
        t.harness.set_size(egui::vec2(WIDTH, EXPANDED_HEIGHT));
        let ctx = t.harness.ctx.clone();
        summon(&ctx, t.app());
        for _ in 0..3 {
            t.harness.step();
        }
        t.snapshot("speed_menu");
        press(&mut t, Key::C);
        for _ in 0..3 {
            t.harness.step();
        }
        t.snapshot("speed_changes");
        press(&mut t, Key::Escape);
        press(&mut t, Key::M);
        for _ in 0..3 {
            t.harness.step();
        }
        t.snapshot("speed_commit");
        press(&mut t, Key::Escape);
        press(&mut t, Key::Escape);
        assert!(t.app().speed.collapsed());
        for _ in 0..5 {
            t.harness.step();
        }
        let bar = t.app().speed.bar_height.unwrap_or(COLLAPSED_HEIGHT);
        t.harness.set_size(egui::vec2(WIDTH, bar));
        for _ in 0..3 {
            t.harness.step();
        }
        t.snapshot("speed_bar");
    }

    #[test]
    fn the_summon_shortcut_works_in_the_window_when_not_system_wide() {
        let mut t = TestApp::mock();
        t.harness.step();
        assert!(!t.app().speed.active);
        t.harness.key_press_modifiers(egui::Modifiers::ALT | egui::Modifiers::SHIFT, Key::G);
        t.harness.step();
        assert!(t.app().speed.expanded());
    }
}
