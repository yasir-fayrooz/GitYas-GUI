//! Speed mode's shortcuts: parsing and showing the text stored in `SpeedSettings` (`"Alt+Shift+G"`, `"P"`), the
//! system-wide summon shortcut (`global-hotkey`), and the editor on the Git settings page.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, PoisonError};

use egui::{Event, Key, Modifiers, RichText, Ui};
use gityas_core::models::SpeedSettings;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use super::{HOME, Item};
use crate::app::GitYasApp;
use crate::bridge::TaskKey;
use crate::theme;

const TASK_SAVE: TaskKey = "speed_keys";

/// Keys the panel keeps for itself (navigation, and the changes list's own keys): an action bound to one of them
/// would never run there.
const RESERVED: [(Key, &str); 12] = [
    (Key::ArrowUp, "moves the cursor"),
    (Key::ArrowDown, "moves the cursor"),
    (Key::ArrowLeft, "goes back"),
    (Key::ArrowRight, "opens"),
    (Key::Enter, "opens"),
    (Key::Space, "opens / stages"),
    (Key::Escape, "goes back"),
    (Key::Backspace, "goes back"),
    (Key::Tab, "next file"),
    (Key::A, "stages all in the changes list"),
    (Key::U, "unstages all in the changes list"),
    (Key::D, "discards in the changes list"),
];

/// One shortcut: modifiers and a key. `cmd` is ⌘ on macOS (egui's `mac_cmd`); elsewhere Ctrl is `ctrl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Combo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub cmd: bool,
    pub key: Key,
}

impl Combo {
    /// Parses `"Ctrl+Alt+Shift+Cmd+Key"` (any subset of modifiers, any order before the key; case-insensitive
    /// modifier names, `Option` = Alt, `Super` / `Meta` / `Command` = Cmd). `None` for empty or unknown text.
    pub fn parse(text: &str) -> Option<Combo> {
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let (key, mods) = parts.split_last()?;
        let mut combo = Combo {
            ctrl: false,
            alt: false,
            shift: false,
            cmd: false,
            key: Key::from_name(key)
                .or_else(|| Key::from_name(&key.to_uppercase()))
                .or_else(|| Key::from_name(&title_case(key)))
                .filter(|k| !is_modifier_key(*k))?,
        };
        for m in mods {
            match m.to_lowercase().as_str() {
                "ctrl" | "control" => combo.ctrl = true,
                "alt" | "option" => combo.alt = true,
                "shift" => combo.shift = true,
                "cmd" | "command" | "super" | "meta" => combo.cmd = true,
                _ => return None,
            }
        }
        Some(combo)
    }

    /// The pressed key with egui's modifiers. Callers skip [`is_modifier_key`] presses.
    pub fn from_event(key: Key, modifiers: Modifiers) -> Combo {
        Combo {
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
            cmd: modifiers.mac_cmd,
            key,
        }
    }

    /// Stored form, e.g. `"Alt+Shift+G"`.
    pub fn text(self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.cmd {
            parts.push("Cmd");
        }
        parts.push(self.key.name());
        parts.join("+")
    }

    /// For people: `Ctrl+Alt+Shift+G` (`Option` and `Cmd` on macOS), arrows as `↑ ↓ ← →`. Words, not ⌃ / ⌥: the
    /// app's fonts have no glyph for those.
    pub fn label(self) -> String {
        let mac = cfg!(target_os = "macos");
        let mut parts: Vec<&str> = Vec::new();
        for (on, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, if mac { "Option" } else { "Alt" }),
            (self.shift, "Shift"),
            (self.cmd, "Cmd"),
        ] {
            if on {
                parts.push(name);
            }
        }
        parts.push(match self.key {
            Key::ArrowUp => "↑",
            Key::ArrowDown => "↓",
            Key::ArrowLeft => "←",
            Key::ArrowRight => "→",
            other => other.name(),
        });
        parts.join("+")
    }

    /// Has Ctrl, Alt or Cmd: safe to take system-wide (Shift alone would swallow capital letters everywhere).
    pub fn has_command_modifier(self) -> bool {
        self.ctrl || self.alt || self.cmd
    }

    /// Plain key, no modifiers (the panel's action keys).
    pub fn is_plain(self) -> bool {
        !(self.ctrl || self.alt || self.shift || self.cmd)
    }

    /// `global-hotkey`'s form of this shortcut.
    pub fn hotkey(self) -> Option<HotKey> {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("control");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.shift {
            parts.push("shift");
        }
        if self.cmd {
            parts.push("super");
        }
        let key = match self.key {
            Key::Equals => "Equal",
            Key::Plus => "Equal",
            Key::OpenBracket => "BracketLeft",
            Key::CloseBracket => "BracketRight",
            Key::Backtick => "Backquote",
            other => other.name(),
        };
        parts.push(key);
        parts.join("+").parse().ok()
    }
}

/// Shift, Ctrl, Alt / Option and Super / Cmd themselves: egui reports their presses as keys too, but they only ever
/// modify the key of a shortcut.
pub fn is_modifier_key(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
    )
}

/// The modifiers held while recording, as a preview: `"Alt+Shift+…"`.
fn held_label(m: Modifiers) -> String {
    let mac = cfg!(target_os = "macos");
    let mut parts: Vec<&str> = Vec::new();
    for (on, name) in [
        (m.ctrl, "Ctrl"),
        (m.alt, if mac { "Option" } else { "Alt" }),
        (m.shift, "Shift"),
        (m.mac_cmd, "Cmd"),
    ] {
        if on {
            parts.push(name);
        }
    }
    parts.push("…");
    parts.join("+")
}

/// `"up"` → `"Up"` (egui's key names).
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect()
    })
}

/// Label of a stored shortcut: its [`Combo::label`], "—" when unbound, the raw text when it does not parse.
pub fn label(text: &str) -> String {
    if text.trim().is_empty() {
        return "—".into();
    }
    Combo::parse(text).map_or_else(|| text.to_owned(), Combo::label)
}

/// What can be bound: the summon shortcut, or a menu action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    Summon,
    Action(Item),
}

/// The stored text of `binding`.
pub fn get(s: &SpeedSettings, binding: Binding) -> &str {
    match binding {
        Binding::Summon => &s.summon,
        Binding::Action(item) => match item {
            Item::Changes => &s.changes,
            Item::Commit => &s.commit,
            Item::Push => &s.push,
            Item::Pull => &s.pull,
            Item::Fetch => &s.fetch,
            Item::Branches => &s.branches,
            Item::NewBranch => &s.new_branch,
            Item::Stash => &s.stash,
            Item::Pop => &s.pop,
            Item::Tag => &s.tag,
            Item::Exit => &s.exit,
        },
    }
}

pub fn get_mut(s: &mut SpeedSettings, binding: Binding) -> &mut String {
    match binding {
        Binding::Summon => &mut s.summon,
        Binding::Action(item) => match item {
            Item::Changes => &mut s.changes,
            Item::Commit => &mut s.commit,
            Item::Push => &mut s.push,
            Item::Pull => &mut s.pull,
            Item::Fetch => &mut s.fetch,
            Item::Branches => &mut s.branches,
            Item::NewBranch => &mut s.new_branch,
            Item::Stash => &mut s.stash,
            Item::Pop => &mut s.pop,
            Item::Tag => &mut s.tag,
            Item::Exit => &mut s.exit,
        },
    }
}

/// Why `combo` cannot be used for `binding` given the other bindings in `s`; `None` when it can.
pub fn problem(s: &SpeedSettings, binding: Binding, combo: Combo) -> Option<String> {
    match binding {
        Binding::Summon if !combo.has_command_modifier() => {
            return Some("Use Ctrl, Alt / Option or Cmd with it, so typing in other apps is not taken over".into());
        }
        Binding::Summon => return None,
        Binding::Action(_) if !combo.is_plain() => {
            return Some("Panel keys are single keys without modifiers".into());
        }
        Binding::Action(_) => {}
    }
    if let Some((_, what)) = RESERVED.iter().find(|(k, _)| *k == combo.key) {
        return Some(format!("{} {what}", combo.label()));
    }
    HOME.iter()
        .map(|m| Binding::Action(m.item))
        .filter(|b| *b != binding)
        .find(|b| Combo::parse(get(s, *b)) == Some(combo))
        .map(|b| match b {
            Binding::Action(item) => format!("Already used by “{}”", super::item_label(item)),
            Binding::Summon => String::new(),
        })
}

// ---- System-wide shortcut ---------------------------------------------------------------------------------------

/// Set by `global-hotkey`'s thread when the summon shortcut is pressed; taken by the next frame.
static PRESSED: AtomicBool = AtomicBool::new(false);
/// The context to wake when it is pressed (the app may be idle or in the background).
static WAKE: Mutex<Option<egui::Context>> = Mutex::new(None);
static HANDLER: Once = Once::new();

/// The registered summon shortcut. Only real launches have one (not `--mock` or tests, which must not take system
/// shortcuts).
#[derive(Default)]
pub struct GlobalShortcut {
    manager: Option<GlobalHotKeyManager>,
    registered: Option<HotKey>,
    /// The shortcut text last applied (`None` = off), to re-register only on change.
    applied: Option<Option<String>>,
    /// Why the shortcut is not active (shown on the settings page).
    pub error: Option<String>,
}

impl GlobalShortcut {
    /// Creates the hotkey manager (on the main thread, as macOS requires).
    pub fn install(ctx: &egui::Context) -> Self {
        *WAKE.lock().unwrap_or_else(PoisonError::into_inner) = Some(ctx.clone());
        HANDLER.call_once(|| {
            GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
                if event.state() == HotKeyState::Pressed {
                    PRESSED.store(true, Ordering::SeqCst);
                    if let Some(ctx) = WAKE.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                        ctx.request_repaint();
                    }
                }
            }));
        });
        match GlobalHotKeyManager::new() {
            Ok(manager) => Self {
                manager: Some(manager),
                ..Self::default()
            },
            Err(err) => {
                tracing::warn!("system-wide shortcuts are unavailable: {err}");
                Self {
                    error: Some(format!("System-wide shortcuts are not available here ({err})")),
                    ..Self::default()
                }
            }
        }
    }

    /// Registers the shortcut `settings` asks for (or none), when that changed.
    pub fn sync(&mut self, settings: &SpeedSettings) {
        let Some(manager) = self.manager.as_ref() else { return };
        let wanted = settings
            .global_shortcut
            .then(|| settings.summon.trim().to_owned())
            .filter(|t| !t.is_empty());
        if self.applied.as_ref() == Some(&wanted) {
            return;
        }
        self.applied = Some(wanted.clone());
        if let Some(old) = self.registered.take()
            && let Err(err) = manager.unregister(old)
        {
            tracing::warn!("could not release the speed mode shortcut: {err}");
        }
        self.error = None;
        let Some(text) = wanted else { return };
        let Some(hotkey) = Combo::parse(&text).filter(|c| c.has_command_modifier()).and_then(Combo::hotkey) else {
            self.error = Some(format!("{} can't be used as a system-wide shortcut", label(&text)));
            return;
        };
        match manager.register(hotkey) {
            Ok(()) => self.registered = Some(hotkey),
            Err(err) => {
                self.error = Some(format!(
                    "{} is already used by another app or the system; pick another ({err})",
                    label(&text)
                ));
            }
        }
    }

    /// Whether the shortcut is registered system-wide (it then never reaches the window as a key press).
    pub fn active(&self) -> bool {
        self.registered.is_some()
    }
}

/// True once per press of the system-wide shortcut.
pub fn take_pressed() -> bool {
    PRESSED.swap(false, Ordering::SeqCst)
}

// ---- Settings editor --------------------------------------------------------------------------------------------

/// The "Speed mode" section of the Git settings page: the summon shortcut (system-wide or not) and one key per
/// panel action. Click a shortcut, then press the new one (Esc cancels, Backspace / Delete unbinds an action).
pub fn settings_section(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Speed mode").strong().size(14.0));
        if app.bridge.is_running(TASK_SAVE) {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    ui.label(
        RichText::new(
            "A small always-on-top panel you drive from the keyboard. Open it from anywhere with the shortcut below; \
             press it again to tuck the panel away.",
        )
        .small()
        .color(p.text_muted),
    );
    ui.add_space(6.0);

    let speed = app.store.settings.speed.clone();
    let mut global = speed.global_shortcut;
    if ui
        .checkbox(&mut global, "Works in every app (system-wide)")
        .on_hover_text("Off: the shortcut only works while GitYas has the focus")
        .changed()
    {
        save(app, move |s| s.speed.global_shortcut = global);
    }
    if speed.global_shortcut
        && let Some(error) = &app.speed.global.error
    {
        ui.label(RichText::new(error).small().color(p.orange));
    }
    ui.add_space(4.0);

    egui::Grid::new("speed_keys").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        binding_row(ui, app, &speed, Binding::Summon, "Open / focus speed mode");
        ui.end_row();
        for entry in HOME.iter() {
            binding_row(ui, app, &speed, Binding::Action(entry.item), entry.label);
            ui.end_row();
        }
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("Reset to defaults").clicked() {
            save(app, |s| s.speed = SpeedSettings::default());
        }
    });
    ui.label(
        RichText::new("In the panel: ↑ ↓ move · Enter / → open · Esc / ← back · Space stages in the changes list.")
            .small()
            .color(p.text_muted),
    );
    if let Some((binding, message)) = &app.speed.key_error {
        let name = match binding {
            Binding::Summon => "Open / focus speed mode",
            Binding::Action(item) => super::item_label(*item),
        };
        ui.label(RichText::new(format!("{name}: {message}")).small().color(p.red));
    }
}

fn binding_row(ui: &mut Ui, app: &mut GitYasApp, speed: &SpeedSettings, binding: Binding, name: &str) {
    let p = theme::pal(ui);
    ui.label(name);
    let recording = app.speed.recording == Some(binding);
    let text = if recording {
        let held = ui.input(|i| i.modifiers);
        if held.any() { held_label(held) } else { "Press keys…".to_owned() }
    } else {
        label(get(speed, binding))
    };
    let button = egui::Button::new(RichText::new(text).monospace().color(if recording { p.accent } else { p.text }))
        .min_size(egui::vec2(140.0, 0.0))
        .selected(recording);
    let resp = ui.add(button).on_hover_text("Click, then press the new shortcut (Esc cancels)");
    if resp.clicked() {
        app.speed.recording = if recording { None } else { Some(binding) };
        app.speed.key_error = None;
    }
    if recording {
        record(ui, app, speed, binding);
    }
}

/// Takes the next key press as the new shortcut for `binding`.
fn record(ui: &mut Ui, app: &mut GitYasApp, speed: &SpeedSettings, binding: Binding) {
    // Holding Alt / Shift / … alone is not a shortcut yet: wait for the key that goes with them.
    let pressed = ui.input_mut(|i| {
        let found = i.events.iter().find_map(|e| match e {
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if !is_modifier_key(*key) => Some((*key, *modifiers)),
            _ => None,
        });
        if found.is_some() {
            i.events.retain(|e| !matches!(e, Event::Key { .. }));
        }
        found
    });
    let Some((key, modifiers)) = pressed else { return };
    app.speed.recording = None;
    if key == Key::Escape {
        return;
    }
    if matches!(key, Key::Backspace | Key::Delete) && modifiers.is_none() && binding != Binding::Summon {
        save(app, move |s| get_mut(&mut s.speed, binding).clear());
        return;
    }
    let combo = Combo::from_event(key, modifiers);
    if let Some(message) = problem(speed, binding, combo) {
        app.speed.key_error = Some((binding, message));
        return;
    }
    app.speed.key_error = None;
    let text = combo.text();
    save(app, move |s| *get_mut(&mut s.speed, binding) = text);
}

fn save(app: &mut GitYasApp, change: impl FnOnce(&mut gityas_core::models::Settings) + Send + 'static) {
    app.modify_settings(TASK_SAVE, change, |app, result| {
        if let Err(err) = result {
            app.report_error("Could not save the shortcut", &err);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_parse_and_print() {
        let c = Combo::parse("Alt+Shift+G").unwrap();
        assert!(c.alt && c.shift && !c.ctrl && !c.cmd && c.key == Key::G);
        assert_eq!(c.text(), "Alt+Shift+G");
        assert_eq!(Combo::parse("option+shift+up").map(Combo::text).as_deref(), Some("Alt+Shift+Up"));
        assert_eq!(Combo::parse("p").map(|c| c.key), Some(Key::P));
        assert_eq!(Combo::parse(""), None);
        assert_eq!(Combo::parse("Hyper+G"), None);
        assert_eq!(Combo::parse("Ctrl+Nope"), None);
        assert!(c.hotkey().is_some());
        assert!(Combo::parse("Ctrl+Alt+Up").and_then(Combo::hotkey).is_some());
        assert_eq!(label(""), "—");
        assert_eq!(Combo::parse("Alt+AltLeft"), None, "a modifier is not a shortcut's key");
        assert!(is_modifier_key(Key::AltLeft) && !is_modifier_key(Key::G));
    }

    #[test]
    fn problems_explain_bad_bindings() {
        let s = SpeedSettings::default();
        let combo = |t: &str| Combo::parse(t).unwrap();
        assert!(problem(&s, Binding::Summon, combo("Shift+G")).is_some(), "needs Ctrl / Alt / Cmd");
        assert!(problem(&s, Binding::Summon, combo("Alt+Shift+Up")).is_none());
        assert!(
            problem(&s, Binding::Action(Item::Push), combo("Ctrl+P")).is_some(),
            "plain keys only"
        );
        assert!(problem(&s, Binding::Action(Item::Push), combo("A")).is_some(), "reserved");
        assert!(problem(&s, Binding::Action(Item::Push), combo("L")).is_some(), "Pull has L");
        assert!(problem(&s, Binding::Action(Item::Push), combo("P")).is_none(), "its own key");
        assert!(problem(&s, Binding::Action(Item::Push), combo("K")).is_none());
    }

    #[test]
    fn recording_waits_past_modifier_presses() {
        let mut t = crate::testutil::TestApp::engine();
        crate::ui::settings::open(t.app(), crate::ui::settings::Page::Git);
        t.app().speed.recording = Some(Binding::Summon);
        t.harness.step();
        // Pressing Alt alone (egui reports it as the key AltLeft, with alt held) records nothing yet.
        t.harness.key_press_modifiers(Modifiers::ALT, Key::AltLeft);
        t.harness.step();
        assert_eq!(t.app().speed.recording, Some(Binding::Summon));
        t.harness.key_press_modifiers(Modifiers::ALT | Modifiers::SHIFT, Key::K);
        t.settle_until("saved", |app| app.store.settings.speed.summon == "Alt+Shift+K");
        assert_eq!(t.app().speed.recording, None);
    }

    #[test]
    fn default_action_keys_are_valid() {
        let s = SpeedSettings::default();
        for entry in HOME.iter() {
            let binding = Binding::Action(entry.item);
            let combo = Combo::parse(get(&s, binding)).unwrap();
            assert_eq!(problem(&s, binding, combo), None, "{:?}", entry.item);
        }
    }
}
