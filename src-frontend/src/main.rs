//! GitYas desktop app (egui). `--mock` runs against static mock data with an in-memory engine (UI work and
//! snapshots); otherwise the engine uses the real data/cache directories and the OS keychain.

// Release builds on Windows: no console window.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod app;
mod bridge;
mod dialogs;
mod graph;
mod mock;
mod notify;
mod platform_ui;
mod store;
mod syntax;
#[cfg(test)]
mod testutil;
mod theme;
mod ui;
mod updater;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gityas_engine::{Engine, ProfileStore};
use gityas_platform::{Instance, InstanceLock};

use crate::app::{GitYasApp, Launch};
use crate::bridge::{Bridge, EguiEventSink};

#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    mock: bool,
}

impl Args {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
        let mut parsed = Args::default();
        for arg in args {
            match arg.as_str() {
                "--mock" => parsed.mock = true,
                // macOS adds `-psn_…` when launched from Finder on old systems.
                a if a.starts_with("-psn_") => {}
                other => return Err(format!("unknown argument {other:?} (supported: --mock)")),
            }
        }
        Ok(parsed)
    }
}

/// What [`build_engine`] built: the engine, a fatal error to show instead of the UI, the log writer, and the
/// profiles (real data only).
type Built = (Engine, Option<String>, Option<gityas_platform::TracingGuard>, Option<ProfileStore>);

/// Builds the engine for the launch mode, on the active profile's data. On failure the app still starts (showing
/// the error) on an in-memory engine, so the window explains what went wrong instead of the process exiting silently.
fn build_engine(args: &Args, sink: Arc<EguiEventSink>, runtime: &tokio::runtime::Runtime) -> Built {
    let handle = runtime.handle().clone();
    let fallback = |sink: Arc<EguiEventSink>| Engine::in_memory(sink, handle.clone());
    if args.mock {
        return match fallback(sink) {
            Ok(engine) => (engine, None, None, None),
            Err(err) => fatal_engine(err.to_string(), runtime),
        };
    }
    let paths = match gityas_platform::Paths::resolve() {
        Ok(paths) => paths,
        Err(err) => {
            return match fallback(sink) {
                Ok(engine) => (engine, Some(format!("Cannot use the GitYas data directories: {err}")), None, None),
                Err(e) => fatal_engine(e.to_string(), runtime),
            };
        }
    };
    let guard = match gityas_platform::init_tracing(&paths) {
        Ok(guard) => Some(guard),
        Err(err) => {
            eprintln!("gityas: logging disabled: {err}");
            None
        }
    };
    let profiles = ProfileStore::new(paths);
    let active = profiles.load().active;
    match Engine::open_profile(&profiles, &active, sink.clone(), handle.clone()) {
        Ok(engine) => (engine, None, guard, Some(profiles)),
        Err(err) => {
            tracing::error!("cannot open storage: {err}");
            match fallback(sink) {
                Ok(engine) => (
                    engine,
                    Some(format!("Cannot open the GitYas database: {err}")),
                    guard,
                    Some(profiles),
                ),
                Err(e) => fatal_engine(e.to_string(), runtime),
            }
        }
    }
}

/// Even the in-memory engine failed (SQLite could not open `:memory:`); nothing sensible is left to run.
fn fatal_engine(message: String, _runtime: &tokio::runtime::Runtime) -> ! {
    eprintln!("gityas: cannot start: {message}");
    std::process::exit(1)
}

/// Only one GitYas runs at a time (real data only; `--mock` windows can run side by side). If one is already
/// running, its window is brought to the front and this process exits. A failed check is logged and skipped, so a
/// broken lock file never stops the app from starting.
fn claim_single_instance(args: &Args) -> Option<InstanceLock> {
    if args.mock {
        return None;
    }
    // A failure here is reported by `build_engine`.
    let paths = gityas_platform::Paths::resolve().ok()?;
    match gityas_platform::claim_instance(&paths.data_dir) {
        Ok(Instance::Primary(lock)) => Some(lock),
        Ok(Instance::Secondary { activated }) => {
            if !activated {
                eprintln!("gityas: GitYas is already running");
            }
            std::process::exit(0)
        }
        Err(err) => {
            eprintln!("gityas: single-instance check skipped: {err}");
            None
        }
    }
}

/// Shows the main window again when another launch asks for it.
fn bring_to_front(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

fn main() -> eframe::Result {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("gityas: {message}");
            std::process::exit(2);
        }
    };
    let instance = claim_single_instance(&args);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("gityas-engine")
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("gityas: cannot start the async runtime: {err}");
            std::process::exit(1);
        }
    };
    let (sink, events) = EguiEventSink::new();
    let sink = Arc::new(sink);
    let (engine, fatal, _tracing_guard, profiles) = build_engine(&args, sink.clone(), &runtime);
    // The running engine; a profile switch replaces it.
    let current = Arc::new(Mutex::new(engine.clone()));

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("GitYas")
        .with_app_id("com.gityas.GitYas")
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size(app::MIN_WINDOW_SIZE);
    // Taskbar / title-bar icon (packaged builds also carry it in the bundle; `cargo run` only has this).
    match eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(err) => tracing::warn!("cannot decode the window icon: {err}"),
    }
    let options = eframe::NativeOptions {
        viewport,
        persist_window: true,
        ..Default::default()
    };
    let launch_engine = engine.clone();
    let handle = runtime.handle().clone();
    let mock = args.mock;
    let instance_lock = instance.as_ref();
    let host = profiles.map(|store| ui::profiles::ProfileHost {
        store,
        runtime: runtime.handle().clone(),
        current: current.clone(),
        seed_signature: true,
    });
    let result = eframe::run_native(
        "GitYas",
        options,
        Box::new(move |cc| {
            sink.attach(&cc.egui_ctx);
            if let Some(lock) = instance_lock {
                let ctx = cc.egui_ctx.clone();
                if let Err(err) = lock.on_activate(move || bring_to_front(&ctx)) {
                    tracing::warn!("{err}");
                }
            }
            let bridge = Bridge::new(launch_engine, handle, cc.egui_ctx.clone(), events);
            let mut app = GitYasApp::new(
                &cc.egui_ctx,
                Launch {
                    bridge,
                    mock,
                    fatal,
                    profiles: host,
                    switching_profile: false,
                },
            );
            app.saved_window = cc.storage.and_then(|s| s.get_string(app::WINDOW_KEY));
            Ok(Box::new(app))
        }),
    );

    // Stop watchers and the fetch scheduler (dropping the engine alone would leave them running). After a profile
    // switch the current engine is not the one built above.
    drop(engine);
    let engine = current.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let shutdown = runtime.block_on(async { tokio::time::timeout(Duration::from_secs(5), engine.shutdown()).await });
    match shutdown {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::warn!("engine shutdown: {err}"),
        Err(_) => tracing::warn!("engine shutdown timed out"),
    }
    runtime.shutdown_timeout(Duration::from_secs(2));
    // Release the lock first, or the relaunched app would find this one still running and exit.
    drop(instance);
    // The window closed to finish an update (macOS / AppImage): start the new version.
    updater::relaunch_if_requested();
    result
}

#[cfg(test)]
mod tests {
    use super::Args;

    #[test]
    fn parses_mock_and_rejects_unknown() {
        assert_eq!(Args::parse(Vec::new()), Ok(Args { mock: false }));
        assert_eq!(Args::parse(vec!["--mock".into()]), Ok(Args { mock: true }));
        assert!(Args::parse(vec!["--nope".into()]).is_err());
    }
}
