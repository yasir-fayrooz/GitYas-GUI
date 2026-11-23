//! Self-update at startup, behind the splash screen (`ui::splash`).
//!
//! Only builds made by the release workflow have an updater: it compiles in the manifest URL and the minisign public
//! key (`GITYAS_UPDATE_ENDPOINT`, `GITYAS_UPDATE_PUBKEY`). Other builds (`cargo run`, tests) skip the splash.
//!
//! The manifest is cargo-packager-updater's static JSON (`latest.json`, written by the release workflow): the newest
//! version and, per `<os>-<arch>`, the bundle URL, its minisign signature and its format. A bundle is installed only
//! after its signature checks out against the compiled-in key:
//! - Windows: the NSIS installer runs silently (`/S /R`: no window), replaces the per-user install and starts GitYas
//!   again; this process exits as soon as the installer is launched (cargo-packager-updater does that).
//! - macOS: the `.app` bundle is replaced in place (an admin prompt if it is not writable), then GitYas relaunches.
//! - Linux: the AppImage file is replaced in place, then relaunched. Other installs (.deb) cannot replace
//!   themselves; they get an "update available" toast instead.
//!
//! The check runs on a plain thread (the updater uses blocking reqwest, which must not run inside the tokio
//! runtime). The engine is not started until the splash is done, so an install never interrupts a git operation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cargo_packager_updater::semver::Version;
use cargo_packager_updater::{Config, Update, UpdateFormat, UpdaterBuilder, WindowsConfig, WindowsUpdateInstallMode};

/// Compile-time configuration (set by the release workflow; empty or unset = no updater).
const ENDPOINT: Option<&str> = option_env!("GITYAS_UPDATE_ENDPOINT");
const PUBKEY: Option<&str> = option_env!("GITYAS_UPDATE_PUBKEY");

/// Fetching the manifest. Offline or slow networks fall through to the app after this.
const CHECK_TIMEOUT: Duration = Duration::from_secs(6);
/// Downloading the bundle (tens of MB).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// This build's version (`[workspace.package] version`).
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

fn config() -> Option<(String, String)> {
    let endpoint = ENDPOINT.map(str::trim).filter(|s| !s.is_empty())?;
    let pubkey = PUBKEY.map(str::trim).filter(|s| !s.is_empty())?;
    Some((endpoint.to_owned(), pubkey.to_owned()))
}

/// Where the update stands (shown by the splash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Checking,
    Downloading {
        version: String,
        received: u64,
        total: Option<u64>,
    },
    Installing {
        version: String,
    },
    Done(Outcome),
}

/// How the update check ended; the app continues (or restarts) accordingly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    /// Offline, server error, bad signature, ... Logged; the app starts anyway.
    Failed(String),
    /// The user chose to continue while the check or download was still running.
    Skipped,
    /// A newer version exists but this install cannot replace itself (e.g. a .deb package).
    Available(String),
    /// The update is installed; quit and start this program (`.app` bundle or AppImage).
    Relaunch(PathBuf),
}

struct Shared {
    phase: Phase,
    /// Set by "Continue without updating"; the worker then never starts installing.
    skipped: bool,
}

/// A running (or finished) update check. Cheap to poll every frame.
pub struct UpdateCheck {
    shared: Arc<Mutex<Shared>>,
}

impl UpdateCheck {
    /// Starts checking in the background, or `None` when this build has no updater.
    pub fn start(ctx: &egui::Context) -> Option<Self> {
        let (endpoint, pubkey) = config()?;
        let shared = Arc::new(Mutex::new(Shared {
            phase: Phase::Checking,
            skipped: false,
        }));
        let worker = Worker {
            shared: shared.clone(),
            ctx: ctx.clone(),
        };
        let spawned = std::thread::Builder::new().name("gityas-updater".into()).spawn(move || {
            let outcome = match worker.run(&endpoint, &pubkey) {
                Ok(outcome) => outcome,
                Err(err) => {
                    tracing::warn!("update check failed: {err}");
                    Outcome::Failed(err.to_string())
                }
            };
            worker.set(Phase::Done(outcome));
        });
        if let Err(err) = spawned {
            tracing::warn!("cannot start the update check: {err}");
            return None;
        }
        Some(Self { shared })
    }

    pub fn phase(&self) -> Phase {
        lock(&self.shared).phase.clone()
    }

    /// Stops waiting for the update: the app starts now and the worker will not install what it downloads.
    /// No effect once installing has begun.
    pub fn skip(&self) {
        let mut shared = lock(&self.shared);
        if matches!(shared.phase, Phase::Checking | Phase::Downloading { .. }) {
            shared.skipped = true;
            shared.phase = Phase::Done(Outcome::Skipped);
        }
    }

    /// A check stuck in `phase` (no worker), for splash tests.
    #[cfg(test)]
    pub fn fixed(phase: Phase) -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared { phase, skipped: false })),
        }
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Worker {
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
}

impl Worker {
    /// Updates the phase unless the user skipped (the splash is gone; nothing reads it any more).
    fn set(&self, phase: Phase) {
        let mut shared = lock(&self.shared);
        if !shared.skipped {
            shared.phase = phase;
        }
        drop(shared);
        self.ctx.request_repaint();
    }

    fn run(&self, endpoint: &str, pubkey: &str) -> Result<Outcome, cargo_packager_updater::Error> {
        let config = Config {
            endpoints: vec![endpoint.parse()?],
            pubkey: pubkey.to_owned(),
            // Silent NSIS (`/S /R`): no installer window, relaunch when done. Fine without elevation: the install is
            // per-user. The default, passive (`/P`), shows the installer's progress page.
            windows: Some(WindowsConfig {
                installer_args: None,
                install_mode: Some(WindowsUpdateInstallMode::Quiet),
            }),
        };
        let updater = UpdaterBuilder::new(current_version(), config)
            .timeout(CHECK_TIMEOUT)
            .header("User-Agent", format!("GitYas/{}", env!("CARGO_PKG_VERSION")))?
            .build()?;
        let Some(mut update) = updater.check()? else {
            return Ok(Outcome::UpToDate);
        };
        tracing::info!(version = %update.version, format = %update.format, "update available");
        if !can_install(&update) {
            return Ok(Outcome::Available(update.version));
        }

        update.timeout = Some(DOWNLOAD_TIMEOUT);
        let version = update.version.clone();
        let received = AtomicU64::new(0);
        self.set(Phase::Downloading {
            version: version.clone(),
            received: 0,
            total: None,
        });
        // Verifies the minisign signature before returning the bytes.
        let bytes = update.download_extended(
            |chunk, total| {
                let received = received.fetch_add(chunk as u64, Ordering::Relaxed) + chunk as u64;
                self.set(Phase::Downloading {
                    version: version.clone(),
                    received,
                    total,
                });
            },
            || {},
        )?;

        {
            let mut shared = lock(&self.shared);
            if shared.skipped {
                tracing::info!("update downloaded but skipped by the user");
                return Ok(Outcome::Skipped);
            }
            shared.phase = Phase::Installing { version: version.clone() };
        }
        self.ctx.request_repaint();
        // Windows: launches the installer and exits this process.
        update.install(bytes)?;
        tracing::info!(%version, "update installed");
        Ok(Outcome::Relaunch(update.extract_path.clone()))
    }
}

/// Whether this installation can replace itself with `update`'s bundle (the manifest has one bundle per platform).
fn can_install(update: &Update) -> bool {
    match update.format {
        UpdateFormat::Nsis | UpdateFormat::Wix => cfg!(windows),
        // `extract_path` is the bundle when running from one (`…/GitYas.app/Contents/MacOS/gityas`).
        UpdateFormat::App => cfg!(target_os = "macos") && update.extract_path.extension().is_some_and(|e| e == "app"),
        // `extract_path` is `$APPIMAGE` when running from an AppImage, else the bare executable (.deb install).
        UpdateFormat::AppImage => cfg!(target_os = "linux") && std::env::var_os("APPIMAGE").is_some(),
    }
}

/// Set when the app quits to finish an update; `main` starts it once the engine has shut down.
static RELAUNCH: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn request_relaunch(path: PathBuf) {
    *RELAUNCH.lock().unwrap_or_else(PoisonError::into_inner) = Some(path);
}

/// Starts the updated app if an update asked for it (end of `main`).
pub fn relaunch_if_requested() {
    let Some(path) = RELAUNCH.lock().unwrap_or_else(PoisonError::into_inner).take() else {
        return;
    };
    // macOS: through Launch Services, so the new bundle is registered and gets its own Dock entry.
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg("-n").arg(&path).spawn()
    } else {
        std::process::Command::new(&path).spawn()
    };
    if let Err(err) = result {
        tracing::error!("cannot relaunch {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builds_have_no_updater() {
        // The release workflow is the only place that sets the compile-time configuration.
        assert!(config().is_none());
        assert!(UpdateCheck::start(&egui::Context::default()).is_none());
    }

    #[test]
    fn skip_finishes_the_check_and_stays_finished() {
        let shared = Arc::new(Mutex::new(Shared {
            phase: Phase::Checking,
            skipped: false,
        }));
        let check = UpdateCheck { shared: shared.clone() };
        check.skip();
        assert_eq!(check.phase(), Phase::Done(Outcome::Skipped));
        let worker = Worker {
            shared,
            ctx: egui::Context::default(),
        };
        worker.set(Phase::Installing { version: "9.0.0".into() });
        assert_eq!(check.phase(), Phase::Done(Outcome::Skipped));
    }

    #[test]
    fn current_version_is_the_package_version() {
        assert_eq!(current_version().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
