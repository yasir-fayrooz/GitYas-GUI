//! WP8b: the repository watcher and the periodic fetch scheduler, both owned by the active session and stopped by
//! its `CancellationToken` (§2 items 4 and 7).
//!
//! * [`watcher`]: **one** debounced filesystem watcher per session (not four). It watches the minimal set of roots
//!   covering the working tree, the per-worktree git dir and the common git dir, and classifies every changed path
//!   into a [`WatchKind`]; a debounced batch becomes one `refresh_blocking` with the union of the kinds' [`Refresh`]
//!   sets, so a commit (index + ref + objects + logs) is one refresh, not four.
//! * [`scheduler`]: fetches every remote of the active repository every `Settings::fetch_interval` (following
//!   changes live), unless `EngineOptions::fetch_interval` disables it.
//!
//! Both run their git work on the blocking pool under the command lock (`Inner::with_session_repo`), stop as soon as
//! the session token is cancelled, and never emit for a stale session.
//!
//! [`Refresh`]: crate::Refresh

mod scheduler;
mod watcher;

use std::sync::Arc;

use gityas_core::Result;
use tokio::task::JoinHandle;

use crate::Inner;
use crate::session::SessionCtx;

/// Starts the session's background tasks (watcher + fetch scheduler, if enabled) on `inner.runtime`. Called by
/// `open_repository` after the new session is installed; the returned handles are owned by the session and awaited
/// by `Session::stop`.
///
/// Never fails today: a watcher that cannot be created is logged and reported as an `AppEvent::Warning` from
/// inside its task, and the session keeps working without live updates. The `Result` is kept for the contract.
pub(crate) fn start_session_tasks(inner: &Arc<Inner>, ctx: &SessionCtx) -> Result<Vec<JoinHandle<()>>> {
    let mut tasks = vec![watcher::spawn(inner, ctx, backend())];
    if inner.options.fetch_interval.is_some() {
        tasks.push(scheduler::spawn(inner, ctx));
    }
    Ok(tasks)
}

#[cfg(not(test))]
fn backend() -> watcher::Backend {
    watcher::Backend::Native
}

#[cfg(test)]
fn backend() -> watcher::Backend {
    tests::backend()
}

#[cfg(test)]
mod tests;
