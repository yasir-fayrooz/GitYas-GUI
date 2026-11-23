//! Startup splash while [`crate::updater`] checks for, downloads and installs an update. Covers the whole window;
//! the engine starts once it is done (`GitYasApp::frame`).

use std::time::{Duration, Instant};

use egui::{RichText, Ui};

use crate::theme;
use crate::updater::{Outcome, Phase, UpdateCheck};

/// Shortest time the splash stays up, so a fast "no update" does not flash it.
const MIN_VISIBLE: Duration = Duration::from_millis(700);
/// After this long, "Continue without updating" appears (slow network, large download).
const OFFER_SKIP_AFTER: Duration = Duration::from_secs(8);
const LOGO_SIZE: f32 = 96.0;

pub struct Splash {
    check: UpdateCheck,
    shown_at: Instant,
    logo: Option<egui::TextureHandle>,
}

impl Splash {
    /// `None` when this build has no updater (no splash then).
    pub fn start(ctx: &egui::Context) -> Option<Self> {
        let check = UpdateCheck::start(ctx)?;
        Some(Self {
            check,
            shown_at: Instant::now(),
            logo: load_logo(ctx),
        })
    }

    #[cfg(test)]
    pub fn fixed(ctx: &egui::Context, phase: Phase) -> Self {
        Self {
            check: UpdateCheck::fixed(phase),
            shown_at: Instant::now(),
            logo: load_logo(ctx),
        }
    }
}

fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png")).ok()?;
    let size = [icon.width as usize, icon.height as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, &icon.rgba);
    Some(ctx.load_texture("splash-logo", image, egui::TextureOptions::LINEAR))
}

/// Draws the splash for one frame. `Some` once the check is over and the app should continue with the outcome.
pub fn show(ui: &mut Ui, splash: &mut Splash) -> Option<Outcome> {
    let phase = splash.check.phase();
    let elapsed = splash.shown_at.elapsed();
    if let Phase::Done(outcome) = &phase {
        let hurry = matches!(outcome, Outcome::Skipped | Outcome::Relaunch(_));
        if hurry || elapsed >= MIN_VISIBLE {
            return Some(outcome.clone());
        }
        ui.ctx().request_repaint_after(MIN_VISIBLE - elapsed);
    }

    let p = theme::pal(ui);
    let mut skip = false;
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.bg_graph))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.5 - 140.0).max(16.0));
                if let Some(logo) = &splash.logo {
                    ui.add(egui::Image::new((logo.id(), egui::Vec2::splat(LOGO_SIZE))));
                }
                ui.add_space(12.0);
                ui.label(RichText::new("GitYas").size(26.0).strong().color(p.text));
                ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).color(p.text_muted));
                ui.add_space(20.0);

                let status = |ui: &mut Ui, text: String| {
                    ui.add(egui::Spinner::new());
                    ui.add_space(6.0);
                    ui.label(RichText::new(text).color(p.text_muted));
                };
                match &phase {
                    Phase::Checking | Phase::Done(_) => status(ui, "Checking for updates…".into()),
                    Phase::Downloading { version, received, total } => {
                        ui.label(RichText::new(format!("Downloading GitYas {version}…")).color(p.text_muted));
                        ui.add_space(6.0);
                        let (fraction, text) = match total {
                            Some(total) if *total > 0 => (
                                *received as f32 / *total as f32,
                                format!("{} / {}", megabytes(*received), megabytes(*total)),
                            ),
                            _ => (0.0, megabytes(*received)),
                        };
                        ui.add(
                            egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                                .desired_width(280.0)
                                .fill(p.accent)
                                .text(text),
                        );
                    }
                    Phase::Installing { version } => status(ui, format!("Installing GitYas {version}…")),
                }

                let can_skip = matches!(phase, Phase::Checking | Phase::Downloading { .. });
                if can_skip && elapsed >= OFFER_SKIP_AFTER {
                    ui.add_space(16.0);
                    skip = ui.link(RichText::new("Continue without updating").color(p.text_faint)).clicked();
                } else if can_skip {
                    ui.ctx().request_repaint_after(OFFER_SKIP_AFTER - elapsed);
                }
            });
        });
    if skip {
        splash.check.skip();
        return Some(Outcome::Skipped);
    }
    None
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

#[cfg(test)]
mod tests {
    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::testutil::TestApp;

    fn downloading() -> Phase {
        Phase::Downloading {
            version: "1.2.0".into(),
            received: 14 * 1_048_576,
            total: Some(38 * 1_048_576),
        }
    }

    #[test]
    fn formats_sizes_in_megabytes() {
        assert_eq!(megabytes(0), "0.0 MB");
        assert_eq!(megabytes(3 * 1_048_576 + 524_288), "3.5 MB");
    }

    #[test]
    fn splash_covers_the_app_until_the_check_is_done() {
        let mut t = TestApp::mock();
        let ctx = t.harness.ctx.clone();
        t.app().splash = Some(Splash::fixed(&ctx, downloading()));
        t.harness.step();
        t.harness.get_by_label("Downloading GitYas 1.2.0…");
        assert!(
            t.harness.query_by_label("MOCK DATA").is_none(),
            "status bar hidden behind the splash"
        );

        t.app().splash = Some(Splash::fixed(&ctx, Phase::Installing { version: "1.2.0".into() }));
        t.harness.step();
        t.harness.get_by_label("Installing GitYas 1.2.0…");
        assert!(t.harness.query_by_label("Continue without updating").is_none());
    }

    #[test]
    #[ignore = "needs a GPU"]
    fn snapshot_splash_downloading() {
        let mut t = TestApp::mock_gpu();
        let ctx = t.harness.ctx.clone();
        t.app().splash = Some(Splash::fixed(&ctx, downloading()));
        t.harness.step();
        t.harness.step();
        t.snapshot("splash_downloading");
    }
}
