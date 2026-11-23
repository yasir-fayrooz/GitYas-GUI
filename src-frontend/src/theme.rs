//! GitYas theme, dark (the template) and light (reference UI §0 / §15).
//!
//! [`install`] registers both palettes with egui (`set_visuals_of`), and [`set_preference`] maps the user's
//! `Theme` setting onto egui's `ThemePreference`, so "System" follows the OS with no extra plumbing. Colours are
//! read through [`palette`] / [`pal`], which pick the palette from the current visuals.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};

use crate::syntax::Highlight;

/// Every colour the UI uses. Read it with `theme::palette(ui.ctx())` (or [`pal`] with a `Ui`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub bg_app: Color32,
    pub bg_titlebar: Color32,
    pub bg_toolbar: Color32,
    pub bg_panel: Color32,
    pub bg_graph: Color32,
    pub bg_graph_alt: Color32,
    pub bg_input: Color32,
    pub bg_hover: Color32,
    pub bg_selected: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_faint: Color32,
    pub accent: Color32,
    pub green: Color32,
    pub red: Color32,
    pub orange: Color32,
    /// Unresolved conflict chunks in the merge output (distinct from the ours / theirs pane tints).
    pub violet: Color32,
    /// Background tint for added / removed lines (diff, merge editor): 18 % (dark) / 12 % (light) of GREEN / RED.
    pub add_bg: Color32,
    pub del_bg: Color32,
    /// Banner background (merge in progress, warnings).
    pub banner_bg: Color32,
    lanes: [Color32; 10],
}

impl Palette {
    /// Lane colour (graph lanes, ref pills, avatars).
    pub fn lane(&self, lane: usize) -> Color32 {
        self.lanes[lane % self.lanes.len()]
    }
}

impl Palette {
    /// Text colour of a syntax-highlight category: One Dark / One Light `syntax` colours. `None` = the plain
    /// text colour (variables and embedded code read as ordinary text).
    pub fn syntax(&self, h: Highlight) -> Option<Color32> {
        let (dark, light) = match h {
            Highlight::Variable | Highlight::Embedded => return None,
            Highlight::Attribute | Highlight::Emphasis | Highlight::Label | Highlight::Tag => (0x74ade8, 0x5c78e2),
            Highlight::Boolean | Highlight::Number | Highlight::EmphasisStrong | Highlight::VariableSpecial => (0xbf956a, 0xad6e25),
            Highlight::StringRegex | Highlight::StringSpecial => (0xbf956a, 0xad6e26),
            Highlight::Comment => (0x5d636f, 0xa2a3a7),
            Highlight::CommentDoc | Highlight::StringEscape => (0x878e98, 0x7c7e86),
            Highlight::Constant => (0xdfc184, 0xc18401),
            Highlight::Constructor => (0x73ade9, 0x5c78e2),
            Highlight::Function | Highlight::LinkText | Highlight::Variant => (0x73ade9, 0x5b79e3),
            Highlight::Enum | Highlight::Type | Highlight::Operator | Highlight::LinkUri => (0x6eb4bf, 0x3882b7),
            Highlight::Keyword | Highlight::Preproc => (0xb477cf, 0xa449ab),
            Highlight::Namespace => (0xdce0e5, 0x242529),
            Highlight::Property | Highlight::Title | Highlight::VariableParameter | Highlight::PunctuationListMarker => {
                (0xd07277, 0xd3604f)
            }
            Highlight::Punctuation => (0xacb2be, 0x242529),
            Highlight::PunctuationBracket | Highlight::PunctuationDelimiter => (0xb2b9c6, 0x4d4f52),
            Highlight::PunctuationSpecial => (0xb1574b, 0xb92b46),
            Highlight::String | Highlight::TextLiteral => (0xa1c181, 0x649f57),
            Highlight::DiffPlus => (0x98c379, 0x50a14f),
            Highlight::DiffMinus => (0xe06c75, 0xe45649),
        };
        Some(rgb(if self.dark { dark } else { light }))
    }
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `Color32::from_rgba_unmultiplied` is not `const`; premultiply by hand.
const fn tint(c: Color32, alpha: u8) -> Color32 {
    let a = alpha as u16;
    Color32::from_rgba_premultiplied(
        ((c.r() as u16 * a) / 255) as u8,
        ((c.g() as u16 * a) / 255) as u8,
        ((c.b() as u16 * a) / 255) as u8,
        alpha,
    )
}

/// Lane colours used by the commit graph: soft, evenly spaced hues that read as one family on the dark canvas.
const LANES_DARK: [Color32; 10] = [
    rgb(0x8b93ff),
    rgb(0x4fd1c5),
    rgb(0xf6ad55),
    rgb(0xf687b3),
    rgb(0x68d391),
    rgb(0x63b3ed),
    rgb(0xb794f4),
    rgb(0xfc8181),
    rgb(0xe9d66b),
    rgb(0x7fd8e6),
];

/// Same hues, darker so they read on white (and under white pill text).
const LANES_LIGHT: [Color32; 10] = [
    rgb(0x4b55d1),
    rgb(0x23867d),
    rgb(0xb8680f),
    rgb(0xc2427a),
    rgb(0x2b8a4e),
    rgb(0x2a6fb0),
    rgb(0x7b4cc4),
    rgb(0xc54242),
    rgb(0x8f7d0f),
    rgb(0x1f8597),
];

pub const DARK: Palette = Palette {
    dark: true,
    bg_app: rgb(0x111318),
    bg_titlebar: rgb(0x0d0f13),
    bg_toolbar: rgb(0x15181e),
    bg_panel: rgb(0x171a20),
    bg_graph: rgb(0x13161b),
    bg_graph_alt: rgb(0x161920),
    bg_input: rgb(0x0e1014),
    bg_hover: rgb(0x21252d),
    bg_selected: rgb(0x262b47),
    border: rgb(0x252931),
    text: rgb(0xe4e7ec),
    text_muted: rgb(0x8d95a3),
    text_faint: rgb(0x5b626e),
    accent: rgb(0x8b93ff),
    green: rgb(0x4cc36d),
    red: rgb(0xe5605e),
    orange: rgb(0xeba43e),
    violet: rgb(0xb794f4),
    add_bg: tint(rgb(0x4cc36d), 46),
    del_bg: tint(rgb(0xe5605e), 46),
    banner_bg: tint(rgb(0xeba43e), 40),
    lanes: LANES_DARK,
};

/// Light palette. `accent`, `green`, `orange` and `text_faint` are dark enough for the contrast checks below:
/// accent / green reach 4.5:1 as text on the light backgrounds and under white button text, orange 4.5:1 on white,
/// faint text 3:1. The add / delete / banner tints keep the lighter hues (backgrounds, not text).
pub const LIGHT: Palette = Palette {
    dark: false,
    bg_app: rgb(0xeef0f4),
    bg_titlebar: rgb(0xe6e9ef),
    bg_toolbar: rgb(0xf5f6f9),
    bg_panel: rgb(0xfafbfc),
    bg_graph: rgb(0xffffff),
    bg_graph_alt: rgb(0xf6f7fa),
    bg_input: rgb(0xffffff),
    bg_hover: rgb(0xe8eaf1),
    bg_selected: rgb(0xdfe2fb),
    border: rgb(0xd7dae2),
    text: rgb(0x1d2028),
    text_muted: rgb(0x5a6170),
    text_faint: rgb(0x7f8590),
    accent: rgb(0x4b55d1),
    green: rgb(0x1d7537),
    red: rgb(0xc93c3c),
    orange: rgb(0xa86610),
    violet: rgb(0x7a4fb8),
    add_bg: tint(rgb(0x2f9e4f), 31),
    del_bg: tint(rgb(0xc93c3c), 31),
    banner_bg: tint(rgb(0xc77d14), 36),
    lanes: LANES_LIGHT,
};

/// The palette for the context's current visuals.
pub fn palette(ctx: &egui::Context) -> &'static Palette {
    if ctx.global_style().visuals.dark_mode { &DARK } else { &LIGHT }
}

/// Shorthand for `palette(ui.ctx())`.
pub fn pal(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// Installs fonts (Phosphor icons, system CJK fallbacks), both visuals and the shared text styles / spacing. Call
/// once at startup.
pub fn install(ctx: &egui::Context) {
    let mut fonts = font_definitions();
    add_fallbacks(&mut fonts, system_fallback_fonts());
    ctx.set_fonts(fonts);
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&DARK));
    ctx.set_visuals_of(egui::Theme::Light, visuals(&LIGHT));
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(16.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style.interaction.selectable_labels = false;
        // Floating scroll bars keep their thin look, but the content leaves room for the full (hovered) bar so it
        // never covers text or right-aligned buttons.
        let scroll = &mut style.spacing.scroll;
        scroll.floating_allocated_width = scroll.bar_width + scroll.bar_outer_margin;
    });
    set_preference(ctx, gityas_core::models::Theme::Dark);
}

/// egui's default fonts plus Phosphor icons, with each family falling back to the other's fonts.
pub fn font_definitions() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    // Each family falls back to the other's fonts: Hack has arrows (`↑ ↓ →`) Ubuntu lacks; Ubuntu and Phosphor have
    // typographic characters and icons Hack lacks.
    let proportional = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let monospace = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    for (family, extra) in [(FontFamily::Monospace, proportional), (FontFamily::Proportional, monospace)] {
        if let Some(fonts) = fonts.families.get_mut(&family) {
            for font in extra {
                if !fonts.contains(&font) {
                    fonts.push(font);
                }
            }
        }
    }
    fonts
}

/// Candidate system fonts for scripts egui's bundled fonts lack. Each group is one script coverage (Han + kana,
/// Hangul); the first file that exists in a group is used.
#[cfg(windows)]
const FALLBACK_GROUPS: &[&[&str]] = &[&["msyh.ttc", "msjh.ttc", "YuGothM.ttc", "simsun.ttc"], &["malgun.ttf"]];
#[cfg(target_os = "macos")]
const FALLBACK_GROUPS: &[&[&str]] = &[
    &[
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    ],
    &["/System/Library/Fonts/AppleSDGothicNeo.ttc"],
];
/// Noto Sans CJK covers Han, kana and Hangul; paths differ per distribution.
#[cfg(not(any(windows, target_os = "macos")))]
const FALLBACK_GROUPS: &[&[&str]] = &[&[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
]];

/// Reads the first existing font of each [`FALLBACK_GROUPS`] group (on Windows, relative to `%WINDIR%\Fonts`).
/// Nothing found means those characters render as boxes, as without this.
fn system_fallback_fonts() -> Vec<(String, egui::FontData)> {
    let base = if cfg!(windows) {
        std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts")
    } else {
        std::path::PathBuf::new()
    };
    let mut found = Vec::new();
    for group in FALLBACK_GROUPS {
        let loaded = group
            .iter()
            .map(|name| base.join(name))
            .find_map(|path| match std::fs::read(&path) {
                Ok(bytes) => Some((format!("system:{}", path.display()), egui::FontData::from_owned(bytes))),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => {
                    tracing::warn!("cannot read fallback font {}: {err}", path.display());
                    None
                }
            });
        match loaded {
            Some(font) => found.push(font),
            None => tracing::debug!("no system font found for fallback group {group:?}"),
        }
    }
    found
}

/// Appends `extra` after every font of both families. epaint takes row metrics from a family's first font, so
/// layout is unchanged; these only serve characters the bundled fonts lack.
fn add_fallbacks(fonts: &mut egui::FontDefinitions, extra: Vec<(String, egui::FontData)>) {
    for (name, data) in extra {
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.clone());
        }
        fonts.font_data.insert(name, std::sync::Arc::new(data));
    }
}

/// Applies the user's theme setting (`System` follows the OS).
pub fn set_preference(ctx: &egui::Context, theme: gityas_core::models::Theme) {
    use gityas_core::models::Theme;
    ctx.set_theme(match theme {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    });
}

fn visuals(p: &Palette) -> Visuals {
    let dark = p.dark;
    let mut visuals = if dark { Visuals::dark() } else { Visuals::light() };
    visuals.panel_fill = p.bg_panel;
    visuals.window_fill = p.bg_panel;
    visuals.extreme_bg_color = p.bg_input;
    visuals.faint_bg_color = p.bg_graph_alt;
    visuals.override_text_color = Some(p.text);
    visuals.hyperlink_color = p.accent;
    visuals.selection.bg_fill = p.bg_selected;
    visuals.selection.stroke = Stroke::new(1.0, p.accent);
    visuals.window_stroke = Stroke::new(1.0, p.border);

    let radius = CornerRadius::same(6);
    let w = &mut visuals.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.weak_bg_fill = p.bg_toolbar;
    w.inactive.bg_fill = p.bg_toolbar;
    w.inactive.bg_stroke = if dark { Stroke::NONE } else { Stroke::new(1.0, p.border) };
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.weak_bg_fill = p.bg_hover;
    w.hovered.bg_fill = p.bg_hover;
    w.hovered.bg_stroke = Stroke::NONE;
    w.active.weak_bg_fill = p.bg_selected;
    w.active.bg_fill = p.bg_selected;
    w.active.bg_stroke = Stroke::NONE;
    for state in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        state.corner_radius = radius;
    }
    visuals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_follows_visuals() {
        let ctx = egui::Context::default();
        install(&ctx);
        assert!(palette(&ctx).dark);
        set_preference(&ctx, gityas_core::models::Theme::Light);
        assert!(!palette(&ctx).dark);
        assert_eq!(LIGHT.lane(10), LIGHT.lane(0));
    }

    /// WCAG relative-luminance contrast ratio.
    fn contrast(a: Color32, b: Color32) -> f32 {
        fn lum(c: Color32) -> f32 {
            let f = |v: u8| {
                let v = f32::from(v) / 255.0;
                if v <= 0.039_28 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        }
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn light_palette_text_is_readable() {
        let p = &LIGHT;
        for bg in [p.bg_app, p.bg_titlebar, p.bg_toolbar, p.bg_panel, p.bg_graph] {
            assert!(contrast(p.text, bg) >= 7.0);
            assert!(contrast(p.text_muted, bg) >= 4.5);
            assert!(contrast(p.text_faint, bg) >= 3.0, "faint on {bg:?}");
            assert!(contrast(p.accent, bg) >= 4.5, "accent on {bg:?}");
            assert!(contrast(p.green, bg) >= 4.5, "green on {bg:?}");
            assert!(contrast(p.red, bg) >= 4.0, "red on {bg:?}");
        }
        assert!(contrast(p.orange, p.bg_panel) >= 4.0);
        // White text on solid buttons / badges.
        for fill in [p.accent, p.green, p.red] {
            assert!(contrast(Color32::WHITE, fill) >= 4.5, "white on {fill:?}");
        }
    }
}

#[cfg(test)]
mod glyph_tests {
    use super::*;

    /// Every non-ASCII character in the frontend's source (outside comments) must render in both families:
    /// egui's bundled fonts lack many symbols (`↑ ↓ → ✓ ▸ ⇧ ●`), which then show as boxes. Use Phosphor icons
    /// (`egui_phosphor::regular`) instead. CJK appears only as test data (user names): the bundled fonts have no
    /// CJK, which is a known open item (system font fallback), so it is skipped here.
    /// `Fonts::has_glyph` compares font faces and gives false negatives when a character lives in the same face as
    /// the replacement glyph (Hack, for monospace). Compare the laid-out glyph with the replacement box instead.
    fn renders_as_missing(ctx: &egui::Context, font: &FontId, ch: char) -> bool {
        let uv = |c: char| {
            ctx.fonts_mut(|f| {
                let galley = f.layout_no_wrap(c.to_string(), font.clone(), Color32::WHITE);
                galley
                    .rows
                    .first()
                    .and_then(|r| r.glyphs.first())
                    .map(|g| (g.uv_rect.min, g.uv_rect.max))
            })
        };
        // A private-use codepoint no bundled font has (Phosphor uses U+E000..U+F8FF only).
        let replacement = uv('\u{10FFFD}');
        uv(ch) == replacement
    }

    #[test]
    fn every_symbol_in_the_ui_source_has_a_glyph() {
        let ctx = egui::Context::default();
        ctx.set_fonts(font_definitions());
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        let mut missing = Vec::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{e}")) {
                let path = entry.unwrap_or_else(|e| panic!("{e}")).path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
                for (n, line) in text.lines().enumerate() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") {
                        continue;
                    }
                    for ch in line.chars().filter(|c| !c.is_ascii()) {
                        let cjk = ('\u{2E80}'..='\u{9FFF}').contains(&ch);
                        if cjk {
                            continue;
                        }
                        for family in [FontId::proportional(13.0), FontId::monospace(12.0)] {
                            if renders_as_missing(&ctx, &family, ch) {
                                missing.push(format!("{ch:?} ({:?}) at {}:{}", family.family, path.display(), n + 1));
                            }
                        }
                    }
                }
            }
        }
        assert!(missing.is_empty(), "characters without a glyph:\n{}", missing.join("\n"));
    }

    #[test]
    fn fallbacks_go_last_in_both_families() {
        let mut fonts = font_definitions();
        let firsts = [FontFamily::Proportional, FontFamily::Monospace].map(|f| fonts.families[&f][0].clone());
        add_fallbacks(&mut fonts, vec![("system:x".into(), egui::FontData::from_owned(Vec::new()))]);
        for (family, first) in [FontFamily::Proportional, FontFamily::Monospace].into_iter().zip(firsts) {
            let list = &fonts.families[&family];
            assert_eq!(list[0], first, "first font (row metrics) unchanged");
            assert_eq!(list.last().map(String::as_str), Some("system:x"));
        }
        assert!(fonts.font_data.contains_key("system:x"));
    }

    /// Where the OS has a CJK font (Windows and macOS always do; Linux needs Noto CJK or WenQuanYi installed).
    #[test]
    fn system_fallback_renders_cjk() {
        let extra = system_fallback_fonts();
        if extra.is_empty() {
            eprintln!("no system CJK font on this machine; skipped");
            return;
        }
        let ctx = egui::Context::default();
        let mut fonts = font_definitions();
        add_fallbacks(&mut fonts, extra);
        ctx.set_fonts(fonts);
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        for ch in ['中', 'あ'] {
            assert!(!renders_as_missing(&ctx, &FontId::proportional(13.0), ch), "{ch}");
        }
    }
}
