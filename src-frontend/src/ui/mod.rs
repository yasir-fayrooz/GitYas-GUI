//! Screens. One module per area:
//!
//! | module | WP | |
//! |---|---|---|
//! | `repos` | WP10 | tab bar, repository picker, add / clone dialog, welcome screen |
//! | `history` | WP11 | commit graph, refs sidebar, commit / stash details, branch / tag dialogs |
//! | `worktree` | WP12 | WIP panel: staged / unstaged / conflicted files, commit box, stash dialog |
//! | `diff` | WP13 | diff viewer (Hunk / Inline / Split) |
//! | `merge` | WP14 | merge-conflict editor |
//! | `settings` | WP15 | settings page (credentials, signature, appearance) |
//! | `remote` | WP16 | fetch / pull / push / merge / abort, banner, remote progress |
//! | `side`, `context_bar`, `git_actions`, `status_bar`, this file | WP9 (shared) | layout that calls into the modules above (`side`: the pinned Changes panel and the right-hand drawer; `context_bar`: the profile switcher, the git actions and the Settings gear; `git_actions`: Fetch / Pull / Push / Stash / Pop) |
//! | `splash`, `changelog`, `markdown` | release | update splash screen, What's new dialog and its Markdown renderer |
//! | `profiles` | profiles | profile switcher, New / Rename profile dialogs, switching the engine to another profile |
//! | `speed` | speed mode | compact always-on-top keyboard panel (stage / commit / push / branches / stash / tag) |

pub mod changelog;
pub mod context_bar;
pub mod diff;
pub mod git_actions;
pub mod history;
pub mod markdown;
pub mod merge;
pub mod profiles;
pub mod remote;
pub mod repos;
pub mod settings;
pub mod side;
pub mod speed;
pub mod splash;
pub mod status_bar;
pub mod worktree;

use egui::{Align2, Color32, FontId, Key, KeyboardShortcut, Modifiers, Response, RichText, Sense, Ui, Vec2};
use egui_phosphor::regular as icon;
use gityas_core::AppEvent;
use gityas_core::models::DeltaChange;

use crate::app::GitYasApp;
use crate::theme;

/// Lets each screen react to an engine event after the store applied it (re-fetch a diff, drop a stale
/// selection, close a dialog, ...).
pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    repos::on_event(app, event);
    history::on_event(app, event);
    worktree::on_event(app, event);
    diff::on_event(app, event);
    merge::on_event(app, event);
    settings::on_event(app, event);
    remote::on_event(app, event);
    side::on_event(app, event);
    speed::on_event(app, event);
}

/// Modal dialogs and popups owned by the screen modules (drawn after the main layout).
pub fn show_dialogs(ctx: &egui::Context, app: &mut GitYasApp) {
    repos::show_dialogs(ctx, app);
    history::show_dialogs(ctx, app);
    worktree::show_dialogs(ctx, app);
    settings::show_dialogs(ctx, app);
    remote::show_dialogs(ctx, app);
    changelog::show_dialog(ctx, app);
    profiles::show_dialogs(ctx, app);
}

pub const fn cmd(key: Key) -> KeyboardShortcut {
    KeyboardShortcut::new(Modifiers::COMMAND, key)
}

pub const fn cmd_shift(key: Key) -> KeyboardShortcut {
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), key)
}

/// Global shortcuts (reference UI §16). View-local keys (Cmd+F, J/K, Esc, Cmd+Enter) are handled by the views.
pub fn shortcuts(ctx: &egui::Context, app: &mut GitYasApp) {
    let pressed = |s: KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(&s));
    if pressed(cmd(Key::T)) {
        repos::open_picker(app);
    }
    if pressed(cmd(Key::O)) {
        repos::open_add_dialog(app, repos::AddTab::Local);
    }
    if pressed(cmd(Key::Comma)) {
        settings::open(app, settings::Page::Credentials);
    }
    if !app.store.started || app.store.fatal.is_some() {
        return;
    }
    if pressed(cmd(Key::W)) {
        repos::close_active_tab(app);
    }
    let digits = [
        Key::Num1,
        Key::Num2,
        Key::Num3,
        Key::Num4,
        Key::Num5,
        Key::Num6,
        Key::Num7,
        Key::Num8,
        Key::Num9,
    ];
    for (i, key) in digits.into_iter().enumerate() {
        if pressed(cmd(key)) {
            repos::switch_to_index(app, i);
        }
    }
    if pressed(KeyboardShortcut::new(Modifiers::CTRL, Key::Tab)) {
        repos::switch_relative(app, 1);
    }
    if pressed(KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Tab)) {
        repos::switch_relative(app, -1);
    }
    if app.store.repo.is_none() {
        return;
    }
    if pressed(cmd_shift(Key::F)) {
        remote::fetch_all(app);
    }
    if pressed(cmd_shift(Key::L)) {
        remote::pull_current(app);
    }
    if pressed(cmd_shift(Key::P)) {
        remote::push_current(app);
    }
    if pressed(cmd(Key::B)) {
        history::open_create_branch(app, None);
    }
}

/// Central area before `Engine::start` finished (replaces the old splashscreen), or the fatal startup error.
pub fn loading_frame(ui: &mut Ui, app: &GitYasApp) {
    let p = theme::pal(ui);
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.bg_graph))
        .show(ui, |ui| {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.35);
                    ui.label(RichText::new(icon::GIT_BRANCH).size(48.0).color(p.accent));
                    ui.add_space(8.0);
                    match &app.store.fatal {
                        Some(message) => {
                            ui.label(RichText::new("GitYas could not start").heading().color(p.red));
                            ui.add(egui::Label::new(RichText::new(message).color(p.text_muted)).wrap());
                        }
                        None => {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new());
                                ui.label(RichText::new("Starting GitYas…").color(p.text_muted));
                            });
                        }
                    }
                });
            });
        });
}

// ---- Shared widgets ---------------------------------------------------------------------------------------------
// Small helpers every screen uses. Add new shared widgets here only via a `shared:` commit (see the brief).

pub fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// Circular avatar with initials, coloured deterministically from the name.
pub fn avatar(ui: &mut Ui, name: &str, size: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_avatar(ui.painter(), theme::pal(ui), rect.center(), size / 2.0, name, None);
    resp
}

pub fn paint_avatar(painter: &egui::Painter, p: &theme::Palette, center: egui::Pos2, radius: f32, name: &str, ring: Option<Color32>) {
    let hash = name.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
    let fill = p.lane(hash).gamma_multiply(0.75);
    painter.circle_filled(center, radius, fill);
    if let Some(ring) = ring {
        painter.circle_stroke(center, radius, egui::Stroke::new(2.0, ring));
    }
    painter.text(
        center,
        Align2::CENTER_CENTER,
        initials(name),
        FontId::proportional(radius * 0.9),
        Color32::WHITE,
    );
}

/// Icon and colour for a change type (reference UI §0).
pub fn change_icon(p: &theme::Palette, change: DeltaChange) -> (&'static str, Color32) {
    match change {
        DeltaChange::Added | DeltaChange::Untracked | DeltaChange::Copied => (icon::PLUS, p.green),
        DeltaChange::Modified => (icon::PENCIL_SIMPLE, p.orange),
        DeltaChange::Deleted => (icon::MINUS, p.red),
        DeltaChange::Renamed => (icon::ARROW_RIGHT, p.accent),
        DeltaChange::Typechange => (icon::SWAP, p.orange),
        DeltaChange::Conflicted => (icon::WARNING, p.orange),
        DeltaChange::Ignored | DeltaChange::Unreadable => (icon::QUESTION, p.text_muted),
    }
}

/// Text colour that reads on `fill`: white on deep fills, near-black on light / pastel ones (the dark theme's accent
/// and lane colours).
pub fn text_on(fill: Color32) -> Color32 {
    let lum = 0.2126 * f32::from(fill.r()) + 0.7152 * f32::from(fill.g()) + 0.0722 * f32::from(fill.b());
    if lum > 150.0 {
        Color32::from_rgb(0x12, 0x14, 0x1a)
    } else {
        Color32::WHITE
    }
}

/// Small rounded button filled with a solid colour (e.g. "Save").
/// Disabled: a translucent fill in the dark theme; in the light theme a translucent fill under white text is
/// unreadable, so it becomes a neutral grey button with faint text.
pub fn solid_button(ui: &mut Ui, text: &str, fill: Color32, enabled: bool) -> Response {
    let p = theme::pal(ui);
    let (fill, text_color) = match (enabled, p.dark) {
        (true, _) => (fill, text_on(fill)),
        (false, true) => (fill.gamma_multiply(0.35), Color32::WHITE.gamma_multiply(0.7)),
        (false, false) => (p.bg_hover, p.text_faint),
    };
    let button = egui::Button::new(RichText::new(text).color(text_color).strong())
        .fill(fill)
        .corner_radius(6);
    ui.add_enabled(enabled, button)
}

/// Quiet button: a light tint of `color` with `color` text (section actions such as "Stage all changes").
pub fn tinted_button(ui: &mut Ui, text: &str, color: Color32, enabled: bool) -> Response {
    let p = theme::pal(ui);
    let (fill, text_color) = if enabled {
        (color.gamma_multiply(if p.dark { 0.16 } else { 0.12 }), color)
    } else {
        (p.bg_hover.gamma_multiply(0.6), p.text_faint)
    };
    let button = egui::Button::new(RichText::new(text).color(text_color).size(12.0).strong())
        .fill(fill)
        .stroke(egui::Stroke::NONE)
        .corner_radius(6);
    ui.add_enabled(enabled, button)
}

/// Frameless icon-only button (always has a tooltip, §0).
pub fn icon_button(ui: &mut Ui, glyph: &str, tooltip: &str) -> Response {
    ui.add(egui::Button::new(RichText::new(glyph).size(15.0)).frame(false))
        .on_hover_text(tooltip)
}

/// Clickable widgets that are not button-like (diff lines, graph rows) and keep the default cursor this frame.
#[derive(Clone, Default)]
struct PlainCursor(Vec<egui::Id>);

/// Opts `resp` out of [`pointer_cursor`] for this frame.
pub fn plain_cursor(resp: &Response) {
    resp.ctx
        .data_mut(|d| d.get_temp_mut_or_default::<PlainCursor>(egui::Id::NULL).0.push(resp.id));
}

/// End of frame: a pointing hand over anything enabled and clickable (buttons, file rows, pills, tabs) that set no
/// cursor of its own. Covers the custom-painted rows too, which egui's `Visuals::interact_cursor` does not.
pub fn pointer_cursor(ctx: &egui::Context) {
    let plain = ctx.data_mut(|d| std::mem::take(&mut d.get_temp_mut_or_default::<PlainCursor>(egui::Id::NULL).0));
    if ctx.output(|o| o.cursor_icon) != egui::CursorIcon::Default || ctx.dragged_id().is_some() {
        return;
    }
    let hovered: Vec<egui::Id> = ctx.interaction_snapshot(|i| i.hovered.iter().copied().collect());
    let clickable = hovered.into_iter().filter(|id| !plain.contains(id)).any(|id| {
        // An area's own response (popup / menu background, "click to bring to front") is not a button.
        ctx.read_response(id)
            .is_some_and(|r| r.enabled() && r.sense.senses_click() && r.id != r.layer_id.id)
    });
    if clickable {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

/// A row with right-aligned controls: `trailing` is laid out first (right to left), then `leading` gets the width
/// that is left, left to right. Truncated labels in `leading` then shrink instead of running under the controls
/// when the panel is narrow.
pub fn trailing_row(ui: &mut Ui, trailing: impl FnOnce(&mut Ui), leading: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            trailing(ui);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), leading);
        });
    });
}

/// Paints single-line text, truncated with "…" to fit `max_width`. Returns the painted rect.
pub fn paint_truncated(
    painter: &egui::Painter,
    left_center: egui::Pos2,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> egui::Rect {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width.max(0.0));
    let galley = painter.layout_job(job);
    let pos = left_center - egui::vec2(0.0, galley.size().y / 2.0);
    let rect = egui::Rect::from_min_size(pos, galley.size());
    painter.galley(pos, galley, color);
    rect
}

/// Local date/time of a git timestamp, e.g. "Oct 17, 2026 6:15 PM".
pub fn format_time(seconds: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(seconds, 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.format("%b %-d, %Y %-I:%M %p").to_string(),
        chrono::LocalResult::None => "—".into(),
    }
}

/// Short age of a git timestamp for timelines: "just now", "12m ago", "5h ago", "3d ago", then the date ("Oct 17",
/// with the year when it is not this year).
pub fn format_relative(seconds: i64) -> String {
    use chrono::{Datelike, TimeZone};
    let now = chrono::Local::now();
    let age = now.timestamp() - seconds;
    match age {
        i64::MIN..60 => "just now".into(),
        60..3_600 => format!("{}m ago", age / 60),
        3_600..86_400 => format!("{}h ago", age / 3_600),
        86_400..604_800 => format!("{}d ago", age / 86_400),
        _ => match chrono::Local.timestamp_opt(seconds, 0) {
            chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) if t.year() == now.year() => {
                t.format("%b %-d").to_string()
            }
            chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.format("%b %-d, %Y").to_string(),
            chrono::LocalResult::None => "—".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time() {
        let now = chrono::Local::now().timestamp();
        assert_eq!(format_relative(now), "just now");
        assert_eq!(format_relative(now - 120), "2m ago");
        assert_eq!(format_relative(now - 3 * 3_600), "3h ago");
        assert_eq!(format_relative(now - 2 * 86_400), "2d ago");
        assert!(format_relative(1_700_000_000).contains("2023"));
    }

    #[test]
    fn initials_and_time() {
        assert_eq!(initials("Ada  lovelace"), "AL");
        assert_eq!(initials(""), "");
        assert!(format_time(1_700_000_000).contains("2023"));
        assert_eq!(format_time(i64::MAX), "—");
    }
}
