//! Headless UI test harness (`egui_kittest` without a GPU: accesskit queries and clicks work; image
//! snapshots need `.wgpu()` and are `#[ignore = "needs a GPU"]`).
//!
//! * [`TestApp::mock`] — the `--mock` app (static data, in-memory engine without a session).
//! * [`TestApp::engine`] — the real app on an in-memory engine (`Engine::in_memory`); add temp repositories with
//!   [`TestApp::add_and_open`] and drive the UI against real git state.
//!
//! Use [`TestApp::settle`] / [`TestApp::settle_until`] instead of `Harness::run` (spinners keep requesting repaints,
//! so `run` would hit its step limit).

use std::path::Path;
use std::time::{Duration, Instant};

use egui_kittest::Harness;

use crate::app::{GitYasApp, Launch};
use crate::bridge::{Bridge, EguiEventSink};

pub struct TestApp {
    pub harness: Harness<'static, GitYasApp>,
    // Dropped after the harness (field order), so in-flight commands can finish.
    runtime: tokio::runtime::Runtime,
}

const TIMEOUT: Duration = Duration::from_secs(10);

impl TestApp {
    pub fn mock() -> Self {
        Self::build(true, false)
    }

    /// The mock app rendered with wgpu, for `snapshot(..)` tests (`#[ignore = "needs a GPU"]`; run them
    /// with `cargo test -p gityas-frontend -- --ignored`, `UPDATE_SNAPSHOTS=1` to write the reference images).
    pub fn mock_gpu() -> Self {
        Self::build(true, true)
    }

    pub fn engine() -> Self {
        let mut t = Self::build(false, false);
        t.settle_until("engine started", |app| app.store.started);
        t
    }

    fn build(mock: bool, gpu: bool) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap_or_else(|e| panic!("runtime: {e}"));
        let (sink, events) = EguiEventSink::new();
        let sink = std::sync::Arc::new(sink);
        let engine = gityas_engine::Engine::in_memory(sink.clone(), runtime.handle().clone()).unwrap_or_else(|e| panic!("engine: {e}"));
        let handle = runtime.handle().clone();
        let mut builder = Harness::builder().with_size([1440.0, 900.0]);
        if gpu {
            builder = builder.wgpu();
        }
        let harness = builder.build_eframe(move |cc| {
            sink.attach(&cc.egui_ctx);
            let bridge = Bridge::new(engine, handle, cc.egui_ctx.clone(), events);
            GitYasApp::new(
                &cc.egui_ctx,
                Launch {
                    bridge,
                    mock,
                    fatal: None,
                    profiles: None,
                    switching_profile: false,
                },
            )
        });
        let mut t = Self { harness, runtime };
        t.harness.step();
        t
    }

    /// `harness.snapshot(name)`, but with `UPDATE_SNAPSHOTS` set the reference image is replaced in place: kittest's
    /// `.old.png` backup and `.diff.png` / `.new.png` leftovers are removed.
    pub fn snapshot(&mut self, name: impl Into<String>) {
        let name = name.into();
        self.harness.snapshot(name.clone());
        let updating = std::env::var("UPDATE_SNAPSHOTS").is_ok_and(|v| !matches!(v.as_str(), "" | "0" | "false" | "no" | "off"));
        if updating {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
            for ext in ["old", "diff", "new"] {
                std::fs::remove_file(dir.join(format!("{name}.{ext}.png"))).ok();
            }
        }
    }

    pub fn app(&mut self) -> &mut GitYasApp {
        self.harness.state_mut()
    }

    /// Runs frames until no command is in flight, then a few more so the resulting events are applied.
    pub fn settle(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.harness.step();
            if !self.harness.state().bridge.any_running() {
                break;
            }
            assert!(Instant::now() < deadline, "commands still running after {TIMEOUT:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(5));
            self.harness.step();
        }
    }

    /// Runs frames until `done(app)` holds (events and completions arrive asynchronously). Panics with `what` on
    /// timeout.
    pub fn settle_until(&mut self, what: &str, mut done: impl FnMut(&GitYasApp) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.harness.step();
            if done(self.harness.state()) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The test runtime (for engines a test builds itself).
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// Calls an async engine method directly (test setup), on the test runtime.
    pub fn block_on<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(fut)
    }

    /// Registers `path` with the engine and opens it in a tab (through the UI's `repos::switch_to`), then waits
    /// until history and head have arrived.
    pub fn add_and_open(&mut self, path: &Path) -> gityas_core::models::Repository {
        let engine = self.harness.state().bridge.engine().clone();
        let repo = self
            .block_on(engine.add_repository(path.to_path_buf(), None))
            .unwrap_or_else(|e| panic!("add_repository: {e}"));
        let id = repo.id.clone();
        self.settle_until("repository registered", |app| app.store.repository(&id).is_some());
        crate::ui::repos::switch_to(self.app(), id.clone());
        self.settle_until("repository opened", |app| {
            app.store
                .repo
                .as_ref()
                .is_some_and(|r| r.id == id && r.is_ready() && r.loaded.files)
                // The switch's own completion has run too (the graph shows "Opening …" until then).
                && crate::ui::repos::opening_label(app).is_none()
        });
        repo
    }
}

/// A temp repository with one commit on `main` (`README.md`), via git2.
pub fn temp_repo() -> (tempfile::TempDir, git2::Repository) {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let repo = git2::Repository::init_opts(dir.path(), git2::RepositoryInitOptions::new().initial_head("main"))
        .unwrap_or_else(|e| panic!("init: {e}"));
    // Git for Windows enables `core.autocrlf` in its system config, which libgit2 honours: pin it so checkouts are byte-exact.
    repo.config()
        .and_then(|mut c| c.set_bool("core.autocrlf", false))
        .unwrap_or_else(|e| panic!("config: {e}"));
    write_file(&repo, "README.md", "hello\n");
    commit_all(&repo, "initial commit");
    (dir, repo)
}

pub fn write_file(repo: &git2::Repository, rel: &str, content: &str) {
    let path = repo.workdir().unwrap_or_else(|| panic!("bare")).join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
    }
    std::fs::write(path, content).unwrap_or_else(|e| panic!("write: {e}"));
}

/// Stages everything and commits on HEAD.
pub fn commit_all(repo: &git2::Repository, message: &str) -> git2::Oid {
    let mut index = repo.index().unwrap_or_else(|e| panic!("{e}"));
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap_or_else(|e| panic!("{e}"));
    index.write().unwrap_or_else(|e| panic!("{e}"));
    let tree = repo
        .find_tree(index.write_tree().unwrap_or_else(|e| panic!("{e}")))
        .unwrap_or_else(|e| panic!("{e}"));
    let sig = git2::Signature::now("Test", "test@example.com").unwrap_or_else(|e| panic!("{e}"));
    let parents: Vec<git2::Commit> = repo.head().ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .unwrap_or_else(|e| panic!("{e}"))
}
