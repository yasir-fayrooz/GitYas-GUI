//! The link between the egui thread and the [`Engine`].
//!
//! * [`EguiEventSink`] pushes engine events into a channel and requests a repaint; [`Bridge::drain_events`] is
//!   called at the top of every frame.
//! * [`Bridge::run`] starts an async engine command on the tokio runtime and queues a completion closure that runs
//!   on the UI thread with `&mut GitYasApp`, so results land in app state without locks.
//! * Commands are keyed ([`TaskKey`]) so the UI can disable a button / show a spinner while that command runs
//!   ([`Bridge::is_running`]); other buttons stay usable.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{OnceLock, mpsc};

use gityas_core::{AppEvent, EventSink, Result};
use gityas_engine::Engine;

use crate::app::GitYasApp;

/// Engine → UI events. `Send + Sync` (the engine emits from worker threads). Created before the window exists (the
/// engine is built first so `main` can shut it down after the event loop); [`EguiEventSink::attach`] sets the
/// context to wake once eframe has one. Events emitted before that are queued and drained on the first frame.
pub struct EguiEventSink {
    tx: mpsc::Sender<AppEvent>,
    ctx: OnceLock<egui::Context>,
}

impl EguiEventSink {
    pub fn new() -> (Self, mpsc::Receiver<AppEvent>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx, ctx: OnceLock::new() }, rx)
    }

    pub fn attach(&self, ctx: &egui::Context) {
        if self.ctx.set(ctx.clone()).is_err() {
            tracing::debug!("event sink already attached");
        }
        ctx.request_repaint();
    }
}

impl EventSink for EguiEventSink {
    fn emit(&self, event: AppEvent) {
        // The receiver lives as long as the app; after it is dropped (shutdown) events have nowhere to go.
        if self.tx.send(event).is_err() {
            return;
        }
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }
}

/// Identifies a running command for "is it running?" checks: e.g. `"fetch"`, `"push"`, `"stage"`. Several
/// commands may share a key; [`Bridge::is_running`] is true while any of them runs.
pub type TaskKey = &'static str;

/// Runs on the UI thread once a command finished.
pub type Completion = Box<dyn FnOnce(&mut GitYasApp) + Send>;

pub struct Bridge {
    engine: Engine,
    runtime: tokio::runtime::Handle,
    ctx: egui::Context,
    events: mpsc::Receiver<AppEvent>,
    done_tx: mpsc::Sender<(TaskKey, Completion)>,
    done_rx: mpsc::Receiver<(TaskKey, Completion)>,
    running: HashMap<TaskKey, usize>,
}

impl Bridge {
    pub fn new(engine: Engine, runtime: tokio::runtime::Handle, ctx: egui::Context, events: mpsc::Receiver<AppEvent>) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        Self {
            engine,
            runtime,
            ctx,
            events,
            done_tx,
            done_rx,
            running: HashMap::new(),
        }
    }

    /// The engine, for synchronous calls (`provider_help`, `active_repository`, `cancel_remote_operation`).
    /// Never `block_on` an async engine method from the UI thread; use [`Bridge::run`].
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Starts `command(engine)` on the runtime; `done(app, result)` then runs on the UI thread (next frame).
    ///
    /// ```ignore
    /// app.bridge.run("fetch", |e| async move { e.fetch(None).await }, |app, result| match result {
    ///     Ok(summary) => app.notify.success(format!("Fetched {} refs", summary.updated.len())),
    ///     Err(err) => app.notify.error("Fetch failed", &err),
    /// });
    /// ```
    pub fn run<T, Fut>(
        &mut self,
        key: TaskKey,
        command: impl FnOnce(Engine) -> Fut,
        done: impl FnOnce(&mut GitYasApp, Result<T>) + Send + 'static,
    ) where
        T: Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        *self.running.entry(key).or_default() += 1;
        let fut = command(self.engine.clone());
        let tx = self.done_tx.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let result = fut.await;
            let completion: Completion = Box::new(move |app: &mut GitYasApp| done(app, result));
            // The receiver is dropped only when the app is gone; nothing left to update then.
            if tx.send((key, completion)).is_ok() {
                ctx.request_repaint();
            }
        });
    }

    /// Like [`Bridge::run`] for commands whose result only needs reporting on failure: errors become an error
    /// toast titled `failure` (e.g. "Could not stage files"); success does nothing (events update the UI).
    pub fn run_reporting<T, Fut>(&mut self, key: TaskKey, failure: &'static str, command: impl FnOnce(Engine) -> Fut)
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        self.run(key, command, move |app, result| {
            if let Err(err) = result {
                app.report_error(failure, &err);
            }
        });
    }

    pub fn is_running(&self, key: TaskKey) -> bool {
        self.running.get(key).is_some_and(|n| *n > 0)
    }

    #[cfg(test)]
    pub fn any_running(&self) -> bool {
        self.running.values().any(|n| *n > 0)
    }

    /// Events received since the last call (top of each frame).
    pub(crate) fn drain_events(&mut self) -> Vec<AppEvent> {
        self.events.try_iter().collect()
    }

    /// Completions of finished commands, with their keys already released.
    pub(crate) fn drain_completions(&mut self) -> Vec<Completion> {
        let mut out = Vec::new();
        for (key, completion) in self.done_rx.try_iter() {
            if let Some(n) = self.running.get_mut(key) {
                *n = n.saturating_sub(1);
            }
            out.push(completion);
        }
        out
    }
}
