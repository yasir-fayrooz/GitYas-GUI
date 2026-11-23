//! Remote-operation plumbing (git facade, clone): registration for cancellation and the one place where
//! `gityas_git::remote::TransferProgress` becomes `AppEvent::RemoteProgress`.
//!
//! * Throttled to one event per [`MIN_INTERVAL`] (~10/s); the first report and the one reaching 100 % always go out.
//! * `percent` is `None` until a total is known. It never decreases within one operation (libgit2 learns the delta
//!   count only after all objects arrived, which would otherwise make the bar jump back).
//! * Fetch: receiving objects maps to 0–90 %, resolving deltas to 90–100 %. Push: written / total objects.
//! * Sideband text (the server's own progress lines) is forwarded as the message with the last known percent.
//! * After a successful operation whose total was known, a final 100 % is emitted if it wasn't already.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use gityas_core::models::{RemoteOp, RemoteProgress};
use gityas_core::{AppEvent, Result};
use gityas_git::remote::{RemoteContext, TransferProgress};

use crate::Inner;
use crate::session::SessionCtx;

pub(crate) const MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Runs one remote operation `f` for the active session: registers it (so `cancel_remote_operation` and
/// `sessions.cancel_active()` abort it), passes the repository's provider and the engine's credentials, and reports
/// progress. Blocking; call inside `with_repo`.
pub(crate) fn with_remote_ctx<T>(
    inner: &Inner,
    ctx: &SessionCtx,
    op: RemoteOp,
    f: impl FnOnce(&RemoteContext<'_>) -> Result<T>,
) -> Result<T> {
    let guard = inner.sessions.begin_remote_op(Some(ctx));
    let reporter = ProgressReporter::new(op, |progress| {
        inner.emit_scoped(
            ctx,
            AppEvent::RemoteProgress {
                repo: Some(ctx.repo_id.clone()),
                progress,
            },
        );
    });
    let on_progress = |p: &TransferProgress| reporter.report(p);
    let rctx = RemoteContext::new(&*inner.credentials)
        .with_provider(ctx.provider)
        .with_cancel(guard.flag())
        .with_progress(&on_progress);
    let result = f(&rctx);
    if result.is_ok() {
        reporter.finish();
    }
    result
}

/// `(percent, message)` for one libgit2 report; `None` for reports that carry nothing to show (empty sideband).
/// `percent` is `None` while the total is unknown.
pub(crate) fn describe(progress: &TransferProgress) -> Option<(Option<u8>, String)> {
    match progress {
        TransferProgress::Fetch {
            received_objects,
            total_objects,
            indexed_deltas,
            total_deltas,
            ..
        } => {
            let (received, total) = (*received_objects as u64, *total_objects as u64);
            if total == 0 {
                return Some((None, "Counting objects".into()));
            }
            if received < total {
                return Some((
                    Some(scaled(received, total, 0, 90)),
                    format!("Receiving objects: {received}/{total}"),
                ));
            }
            let (indexed, deltas) = (*indexed_deltas as u64, *total_deltas as u64);
            if indexed < deltas {
                return Some((
                    Some(scaled(indexed, deltas, 90, 100)),
                    format!("Resolving deltas: {indexed}/{deltas}"),
                ));
            }
            Some((Some(100), format!("Received {total} objects")))
        }
        TransferProgress::Push { current, total, .. } => {
            let (current, total) = (*current as u64, *total as u64);
            if total == 0 {
                return Some((None, "Preparing push".into()));
            }
            Some((Some(scaled(current, total, 0, 100)), format!("Writing objects: {current}/{total}")))
        }
        TransferProgress::Sideband(text) => {
            // Servers send "\r"-separated progress lines; show the last non-empty one.
            let line = text.split(['\r', '\n']).map(str::trim).rfind(|l| !l.is_empty())?;
            Some((None, line.to_owned()))
        }
    }
}

/// `from + (to - from) * done / total`, clamped to `to`. `total` must be non-zero.
fn scaled(done: u64, total: u64, from: u8, to: u8) -> u8 {
    let span = u64::from(to.saturating_sub(from));
    let value = u64::from(from) + span * done.min(total) / total.max(1);
    u8::try_from(value).unwrap_or(to).min(to)
}

#[derive(Default)]
struct State {
    last_emit: Option<Instant>,
    /// Highest percent computed so far (emitted or not).
    max_percent: Option<u8>,
    emitted_complete: bool,
}

/// Throttles and maps progress for one operation. Used from libgit2 callbacks on the operation's own thread.
pub(crate) struct ProgressReporter<E: Fn(RemoteProgress)> {
    op: RemoteOp,
    emit: E,
    state: RefCell<State>,
}

impl<E: Fn(RemoteProgress)> ProgressReporter<E> {
    pub(crate) fn new(op: RemoteOp, emit: E) -> Self {
        Self {
            op,
            emit,
            state: RefCell::new(State::default()),
        }
    }

    pub(crate) fn report(&self, progress: &TransferProgress) {
        let Some((percent, message)) = describe(progress) else { return };
        let mut state = self.state.borrow_mut();
        let percent = match (percent, state.max_percent) {
            (Some(p), Some(max)) => Some(p.max(max)),
            (Some(p), None) => Some(p),
            // Sideband / unknown total: keep showing the last known percent.
            (None, max) => max,
        };
        state.max_percent = percent;
        let completes = percent == Some(100) && !state.emitted_complete;
        let now = Instant::now();
        let due = state.last_emit.is_none_or(|t| now.duration_since(t) >= MIN_INTERVAL);
        if !(completes || due) {
            return;
        }
        state.last_emit = Some(now);
        state.emitted_complete |= percent == Some(100);
        drop(state);
        (self.emit)(RemoteProgress {
            op: self.op,
            percent,
            message,
        });
    }

    /// After success: a final 100 % if a total was ever known and 100 % was not emitted yet.
    pub(crate) fn finish(&self) {
        let mut state = self.state.borrow_mut();
        if state.max_percent.is_none() || state.emitted_complete {
            return;
        }
        state.emitted_complete = true;
        state.max_percent = Some(100);
        drop(state);
        (self.emit)(RemoteProgress {
            op: self.op,
            percent: Some(100),
            message: "Done".into(),
        });
    }
}
