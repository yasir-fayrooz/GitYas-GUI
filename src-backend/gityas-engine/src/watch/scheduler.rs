//! Periodic background fetch of the active repository (replaces `globals::run_interval_function`, bug-log #1/#12).
//!
//! * The period is `Inner::fetch_interval` (the user's setting), followed live: a change re-times the next tick
//!   from the previous one. The first tick is one period after the session starts (opening a repository already
//!   pushes fresh state); each next one is a period after the previous tick started, so a slow fetch or a suspended
//!   laptop never causes a burst of catch-up fetches.
//! * Each tick fetches every remote (`fetch_remote` per remote, configured refspecs, pruning refs deleted on the
//!   remote) under the command lock, with the repository's provider and the engine's credentials, registered via
//!   `begin_remote_op(Some(&ctx))` so a session switch aborts it. A tick is skipped while another remote operation of
//!   this session (a user fetch / pull / push) is in flight. No remotes: nothing to do.
//! * Every result is awaited and logged (never discarded). Refs updated → `Refresh::REMOTE_REFS`. Background
//!   fetches report no `RemoteProgress` (a progress bar every minute would be noise).
//! * **Auth back-off:** a remote whose fetch fails with `GitYasError::Auth` [`MAX_AUTH_FAILURES`] times in a row is
//!   no longer fetched in the background for the rest of the session, and one `AppEvent::Warning` says so. Any
//!   success resets the count. Reopening the repository (e.g. after setting a token) starts a fresh scheduler.
//!   Other errors (network down) are logged and retried on the next tick. `Cancelled` is silent.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gityas_core::models::RemoteFetch;
use gityas_core::{AppEvent, GitYasError, Result};
use gityas_git::remote::RemoteContext;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::Inner;
use crate::refresh::{Refresh, refresh_blocking};
use crate::session::SessionCtx;

/// Consecutive authentication failures after which a remote is no longer fetched in the background.
pub(crate) const MAX_AUTH_FAILURES: u32 = 3;

/// Per-remote consecutive auth-failure counter.
#[derive(Debug, Default)]
pub(crate) struct AuthBackoff {
    failures: HashMap<String, u32>,
    disabled: HashSet<String>,
}

impl AuthBackoff {
    pub(crate) fn disabled(&self) -> &HashSet<String> {
        &self.disabled
    }

    /// Records one fetch result; returns `true` if this result just disabled the remote (warn once).
    pub(crate) fn record(&mut self, remote: &str, result: &Result<RemoteFetch>) -> bool {
        match result {
            Err(GitYasError::Auth(_)) => {
                let count = self.failures.entry(remote.to_owned()).or_default();
                *count += 1;
                *count >= MAX_AUTH_FAILURES && self.disabled.insert(remote.to_owned())
            }
            Ok(_) => {
                self.failures.remove(remote);
                false
            }
            // Network / other errors say nothing about the credential.
            Err(_) => false,
        }
    }
}

/// Used if the interval channel says the scheduler is disabled after it started (not expected).
const FALLBACK_PERIOD: Duration = Duration::from_secs(60);

/// Spawns the scheduler task on `inner.runtime`.
pub(crate) fn spawn(inner: &Arc<Inner>, ctx: &SessionCtx) -> JoinHandle<()> {
    let inner = Arc::clone(inner);
    let ctx = ctx.clone();
    inner.runtime.clone().spawn(async move { run(inner, ctx).await })
}

fn current_period(rx: &mut watch::Receiver<Option<Duration>>) -> Duration {
    rx.borrow_and_update().unwrap_or(FALLBACK_PERIOD).max(Duration::from_millis(1))
}

async fn run(inner: Arc<Inner>, ctx: SessionCtx) {
    let mut period_rx = inner.fetch_interval.subscribe();
    let mut period = current_period(&mut period_rx);
    let mut last = Instant::now();
    let mut backoff = AuthBackoff::default();
    loop {
        tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => break,
            changed = period_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                period = current_period(&mut period_rx);
                continue;
            }
            () = tokio::time::sleep_until(last + period) => {}
        }
        last = Instant::now();
        if inner.sessions.has_remote_op(ctx.generation) {
            tracing::debug!(repo = %ctx.repo_id, "background fetch skipped: a remote operation is in flight");
            continue;
        }
        let skip = backoff.disabled().clone();
        let results = inner
            .with_session_repo(&ctx, move |inner, ctx, repo| fetch_all(inner, ctx, repo, &skip))
            .await;
        match results {
            Ok(results) => {
                for (remote, result) in &results {
                    log_result(&ctx, remote, result);
                    if backoff.record(remote, result) {
                        inner.emit_scoped(
                            &ctx,
                            AppEvent::Warning(format!(
                                "Background fetch of remote \"{remote}\" stopped after {MAX_AUTH_FAILURES} failed \
                                 sign-ins. Check the access token, then reopen the repository."
                            )),
                        );
                    }
                }
            }
            Err(GitYasError::Cancelled) => break,
            Err(err) => tracing::warn!(repo = %ctx.repo_id, "background fetch failed: {err}"),
        }
    }
    tracing::debug!(repo = %ctx.repo_id, "fetch scheduler stopped");
}

fn log_result(ctx: &SessionCtx, remote: &str, result: &Result<RemoteFetch>) {
    match result {
        Ok(fetch) if fetch.updated.is_empty() => {
            tracing::debug!(repo = %ctx.repo_id, remote, "background fetch: up to date");
        }
        Ok(fetch) => tracing::info!(repo = %ctx.repo_id, remote, refs = fetch.updated.len(), "background fetch updated refs"),
        Err(err) => tracing::warn!(repo = %ctx.repo_id, remote, "background fetch failed: {err}"),
    }
}

/// The provider as of now: the user may change it while the session runs (`set_repository_provider` updates the
/// active session), so the scheduler does not use its start-up snapshot.
pub(super) fn current_provider(inner: &Inner, ctx: &SessionCtx) -> Option<gityas_core::Provider> {
    match inner.sessions.active() {
        Some(active) if active.generation == ctx.generation => active.provider,
        _ => ctx.provider,
    }
}

/// One tick, blocking, under the command lock: fetches each remote not in `skip`, then refreshes remote refs if
/// anything moved. `Err(Cancelled)` as soon as the session is switched away.
fn fetch_all(
    inner: &Inner,
    ctx: &SessionCtx,
    repo: &mut git2::Repository,
    skip: &HashSet<String>,
) -> Result<Vec<(String, Result<RemoteFetch>)>> {
    let names: Vec<String> = gityas_git::repository::remote_names(repo)?
        .into_iter()
        .filter(|n| !skip.contains(n))
        .collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let op = inner.sessions.begin_remote_op(Some(ctx));
    let remote_ctx = RemoteContext::new(&*inner.credentials)
        .with_provider(current_provider(inner, ctx))
        .with_cancel(op.flag());
    let mut results = Vec::with_capacity(names.len());
    for name in names {
        let result = gityas_git::remote::fetch_remote(repo, &name, true, &remote_ctx);
        if matches!(result, Err(GitYasError::Cancelled)) {
            return Err(GitYasError::Cancelled);
        }
        results.push((name, result));
    }
    drop(op);
    if results.iter().any(|(_, r)| r.as_ref().is_ok_and(|f| !f.updated.is_empty()))
        && let Err(err) = refresh_blocking(inner, ctx, repo, Refresh::REMOTE_REFS)
    {
        tracing::warn!(repo = %ctx.repo_id, "refresh after background fetch failed: {err}");
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_backoff_disables_after_consecutive_failures_and_resets_on_success() {
        let mut b = AuthBackoff::default();
        let auth = || Err(GitYasError::Auth("bad token".into()));
        let ok = || {
            Ok(RemoteFetch {
                remote: "origin".into(),
                updated: vec![],
            })
        };
        assert!(!b.record("origin", &auth()));
        assert!(!b.record("origin", &ok()), "success resets");
        assert!(!b.record("origin", &auth()));
        assert!(
            !b.record("origin", &Err(GitYasError::Git("network".into()))),
            "other errors don't count"
        );
        assert!(!b.record("origin", &auth()));
        assert!(b.record("origin", &auth()), "third consecutive auth failure disables");
        assert!(b.disabled().contains("origin"));
        assert!(!b.record("origin", &auth()), "warned only once");
        assert!(!b.disabled().contains("upstream"));
    }
}
