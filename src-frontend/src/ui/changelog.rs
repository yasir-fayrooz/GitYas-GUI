//! What's new: the release notes in `CHANGELOG.md` (repository root), compiled into the binary.
//!
//! Each release is a `## [X.Y.Z] - YYYY-MM-DD` section (Keep a Changelog style; `## X.Y.Z` works too). Sections whose
//! heading is not a version (`## [Unreleased]`) are drafts and never shown. The dialog opens by itself once after an
//! update (settings remember the last version whose notes were shown) and from the version label in the status bar.

use egui::RichText;
use semver::Version;

use crate::app::GitYasApp;
use crate::{dialogs, theme, ui, updater};

const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// One released version's notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release<'a> {
    pub version: Version,
    pub date: Option<&'a str>,
    /// The section's Markdown, without its heading.
    pub notes: &'a str,
}

/// The releases in `text`, newest first.
pub fn parse(text: &str) -> Vec<Release<'_>> {
    let mut releases = Vec::new();
    let mut current: Option<(Version, Option<&str>, usize)> = None;
    let mut in_fence = false;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        let Some(heading) = trimmed.strip_prefix("## ").filter(|_| !in_fence) else {
            continue;
        };
        if let Some((version, date, body)) = current.take() {
            releases.push(Release {
                version,
                date,
                notes: text[body..start].trim(),
            });
        }
        current = parse_heading(heading).map(|(version, date)| (version, date, offset));
    }
    if let Some((version, date, body)) = current {
        releases.push(Release {
            version,
            date,
            notes: text[body..].trim(),
        });
    }
    releases.sort_by(|a, b| b.version.cmp(&a.version));
    releases
}

/// `[1.2.0] - 2026-11-01`, `1.2.0 (2026-11-01)`, `v1.2.0`, ... → version and date. `None` for `[Unreleased]`.
fn parse_heading(heading: &str) -> Option<(Version, Option<&str>)> {
    let heading = heading.trim();
    let (version, rest) = match heading.strip_prefix('[') {
        Some(inner) => inner.split_at(inner.find(']')?),
        None => heading.split_at(heading.find(char::is_whitespace).unwrap_or(heading.len())),
    };
    let version = Version::parse(version.trim().trim_start_matches(['v', 'V'])).ok()?;
    let date = rest
        .trim_start_matches(']')
        .trim()
        .trim_start_matches(['-', '–', '—'])
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    Some((version, (!date.is_empty()).then_some(date)))
}

pub struct ChangelogUi {
    open: Option<View>,
    /// The Markdown shown (`CHANGELOG.md`; tests swap in their own).
    source: &'static str,
}

impl Default for ChangelogUi {
    fn default() -> Self {
        Self {
            open: None,
            source: CHANGELOG,
        }
    }
}

struct View {
    /// Opened after an update from this version: releases newer than it are marked new and shown first.
    since: Option<Version>,
    show_all: bool,
}

/// Opens the dialog: all releases, or (`since`) what changed after that version.
pub fn open(app: &mut GitYasApp, since: Option<Version>) {
    app.changelog_ui.open = Some(View {
        show_all: since.is_none(),
        since,
    });
}

/// After the engine started (settings loaded): shows the notes once when this version is newer than the last one
/// whose notes were shown, then remembers this version. A fresh install only remembers it.
pub fn after_start(app: &mut GitYasApp) {
    remember_version(app, updater::current_version());
}

fn remember_version(app: &mut GitYasApp, current: Version) {
    let seen = app.store.settings.last_seen_version.as_deref().and_then(|v| Version::parse(v).ok());
    if seen.as_ref().is_some_and(|seen| *seen >= current) {
        return;
    }
    if let Some(seen) = seen
        && parse(app.changelog_ui.source).iter().any(|r| r.version > seen)
    {
        open(app, Some(seen));
    }
    let version = current.to_string();
    app.store.settings.last_seen_version = Some(version.clone());
    app.modify_settings(
        "changelog-seen",
        move |settings| settings.last_seen_version = Some(version),
        |_, result| {
            if let Err(err) = result {
                tracing::warn!("cannot remember the changelog version: {err}");
            }
        },
    );
}

pub fn show_dialog(ctx: &egui::Context, app: &mut GitYasApp) {
    let Some(view) = app.changelog_ui.open.as_mut() else { return };
    let p = theme::palette(ctx);
    let title = if view.since.is_some() {
        "What's new in GitYas"
    } else {
        "Changelog"
    };
    let releases = parse(app.changelog_ui.source);
    let mut close = false;
    let outcome = dialogs::modal(ctx, "changelog", title, 560.0, |ui| {
        let is_new = |r: &Release| view.since.as_ref().is_some_and(|since| r.version > *since);
        let shown: Vec<&Release> = releases.iter().filter(|r| view.show_all || is_new(r)).collect();
        let max_height = (ctx.content_rect().height() * 0.65).max(200.0);
        egui::ScrollArea::vertical()
            .max_height(max_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if shown.is_empty() {
                    ui.label(RichText::new("No release notes yet.").color(p.text_muted));
                }
                for (i, release) in shown.iter().enumerate() {
                    if i > 0 {
                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);
                    }
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("v{}", release.version)).size(18.0).strong().color(p.text));
                        if let Some(date) = release.date {
                            ui.label(RichText::new(date).color(p.text_muted));
                        }
                        if is_new(release) {
                            ui.label(
                                RichText::new(" NEW ")
                                    .small()
                                    .strong()
                                    .color(egui::Color32::WHITE)
                                    .background_color(p.green),
                            );
                        }
                    });
                    ui.add_space(6.0);
                    ui::markdown::show(ui, release.notes);
                }
            });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if !view.show_all && ui.link("Show all versions").clicked() {
                view.show_all = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui::solid_button(ui, "Close", p.accent, true).clicked() || dialogs::enter_pressed(ui);
            });
        });
    });
    if close || outcome.closed {
        app.changelog_ui.open = None;
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::testutil::TestApp;

    const SAMPLE: &str = "# Changelog\n\nIntro.\n\n## [Unreleased]\n\n- Draft\n\n## [1.10.0] - 2026-11-02\n\n### Added\n\n- Splash\n\n```\n## not a heading\n```\n\n## v1.2.0 (2026-10-01)\n- Fix\n\n## 1.9.1\n- Patch\n";

    #[test]
    fn parses_released_sections_newest_first() {
        let releases = parse(SAMPLE);
        let versions: Vec<String> = releases.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(versions, ["1.10.0", "1.9.1", "1.2.0"]);
        assert_eq!(releases[0].date, Some("2026-11-02"));
        assert!(releases[0].notes.starts_with("### Added") && releases[0].notes.contains("## not a heading"));
        assert_eq!(releases[1].date, None);
        assert_eq!(releases[2].date, Some("2026-10-01"));
        assert_eq!(releases[2].notes, "- Fix");
    }

    #[test]
    fn shipped_changelog_parses() {
        for release in parse(CHANGELOG) {
            assert!(!release.notes.is_empty(), "v{} has no notes", release.version);
        }
    }

    #[test]
    fn version_label_opens_the_changelog() {
        let mut t = TestApp::mock();
        t.harness.step();
        t.harness.get_by_label(&format!("GitYas v{}", env!("CARGO_PKG_VERSION"))).click();
        t.harness.step();
        t.harness.step();
        assert!(t.app().changelog_ui.open.is_some());
        t.harness.get_by_label("Changelog");
    }

    #[test]
    fn notes_open_once_after_an_update_and_not_on_a_fresh_install() {
        let mut t = TestApp::mock();
        after_start(t.app());
        assert!(t.app().changelog_ui.open.is_none(), "fresh install");
        assert_eq!(t.app().store.settings.last_seen_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        after_start(t.app());
        assert!(t.app().changelog_ui.open.is_none(), "same version");
    }

    #[test]
    fn notes_after_an_update_show_only_what_is_new() {
        let mut t = TestApp::mock();
        t.app().changelog_ui.source = SAMPLE;
        t.app().store.settings.last_seen_version = Some("1.2.0".into());
        remember_version(t.app(), Version::new(1, 10, 0));
        assert_eq!(t.app().store.settings.last_seen_version.as_deref(), Some("1.10.0"));
        t.harness.step();
        t.harness.step();
        t.harness.get_by_label("What's new in GitYas");
        t.harness.get_by_label("v1.10.0");
        t.harness.get_by_label("v1.9.1");
        assert!(t.harness.query_by_label("v1.2.0").is_none(), "already seen");
        t.harness.get_by_label("Show all versions").click();
        t.harness.step();
        t.harness.step();
        t.harness.get_by_label("v1.2.0");

        // Seen everything: nothing opens.
        t.app().changelog_ui.open = None;
        remember_version(t.app(), Version::new(1, 10, 0));
        assert!(t.app().changelog_ui.open.is_none());
    }

    #[test]
    #[ignore = "needs a GPU"]
    fn snapshot_whats_new() {
        let mut t = TestApp::mock_gpu();
        t.app().changelog_ui.source = SNAPSHOT_SAMPLE;
        open(t.app(), Some(Version::new(1, 1, 0)));
        t.harness.step();
        t.harness.step();
        t.snapshot("whats_new");
    }

    const SNAPSHOT_SAMPLE: &str = "## [1.2.0] - 2026-11-02

### Added

- **Automatic updates**: GitYas checks for a new version at startup.
- Release notes after each update, also from the version in the status bar.
  - Nested detail with `inline code` and a [link](https://example.com).

### Fixed

1. Pushing a branch with a long name no longer truncates the toast.
2. ~~Old behaviour~~ replaced.

## [1.1.0] - 2026-10-01

- Earlier release.
";

    #[test]
    fn markdown_renders_text_lists_and_code() {
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            theme::install(ui.ctx());
            ui::markdown::show(ui, SAMPLE);
        });
        harness.step();
        harness.get_by_label("Splash");
        harness.get_by_label("## not a heading");
    }
}
