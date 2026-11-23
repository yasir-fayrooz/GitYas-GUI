//! Shared fixtures for the engine integration tests (WP8f). Black-box: only the public API of `gityas-engine` and
//! the Wave 1 crates is used.
#![allow(dead_code)] // each test binary uses a different subset
#![allow(clippy::unwrap_used, clippy::expect_used)] // test fixtures

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use gityas_auth::MemoryCredentialStore;
use gityas_core::events::RecordingSink;
use gityas_core::{AppEvent, EventSink, RepoId};
use gityas_engine::{Engine, EngineOptions};
use gityas_storage::{Storage, StoragePaths};

/// Upper bound for any single engine call in these tests; a hang fails the test instead of the suite.
pub const STEP: Duration = Duration::from_secs(30);

/// Test options: scheduler off, short debounce, cache GC on every store (stresses the cache paths).
pub fn options() -> EngineOptions {
    EngineOptions {
        fetch_interval: None,
        watch_debounce: Duration::from_millis(50),
        cache_gc_interval: Duration::ZERO,
        ..EngineOptions::default()
    }
}

/// Records every event (via [`RecordingSink`]) and can block the emitting thread inside the first `RemoteProgress`
/// event of a given repository until [`GatedSink::release`] is called. The event is recorded before blocking.
#[derive(Default)]
pub struct GatedSink {
    pub recorded: RecordingSink,
    /// Everything emitted so far, never cleared by `take` (for whole-run assertions).
    all: Mutex<Vec<AppEvent>>,
    gate: Mutex<Gate>,
    cond: Condvar,
}

#[derive(Default)]
struct Gate {
    /// Block in the first `RemoteProgress` for this repo.
    repo: Option<RepoId>,
    /// Set once the emitting thread is blocked.
    blocked: bool,
    released: bool,
}

impl GatedSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns and clears the events recorded since the last `take`.
    pub fn take(&self) -> Vec<AppEvent> {
        self.recorded.take()
    }

    /// Every event recorded since construction.
    pub fn all(&self) -> Vec<AppEvent> {
        self.all.lock().unwrap().clone()
    }

    /// Arms the gate for the next `RemoteProgress { repo: Some(repo) }`.
    pub fn block_progress_of(&self, repo: &RepoId) {
        let mut gate = self.gate.lock().unwrap();
        *gate = Gate {
            repo: Some(repo.clone()),
            ..Gate::default()
        };
    }

    /// Waits (blocking, up to `timeout`) until an emitting thread is parked in the gate.
    pub fn wait_blocked(&self, timeout: Duration) -> bool {
        let gate = self.gate.lock().unwrap();
        let (gate, _) = self.cond.wait_timeout_while(gate, timeout, |g| !g.blocked).unwrap();
        gate.blocked
    }

    pub fn release(&self) {
        let mut gate = self.gate.lock().unwrap();
        gate.released = true;
        gate.repo = None;
        self.cond.notify_all();
    }
}

impl EventSink for GatedSink {
    fn emit(&self, event: AppEvent) {
        let gated = matches!(&event, AppEvent::RemoteProgress { repo: Some(r), .. }
            if self.gate.lock().unwrap().repo.as_ref() == Some(r));
        self.all.lock().unwrap().push(event.clone());
        self.recorded.emit(event);
        if gated {
            let mut gate = self.gate.lock().unwrap();
            if gate.blocked || gate.released {
                return; // only the first one blocks
            }
            gate.blocked = true;
            self.cond.notify_all();
            // Bounded so a broken test can never wedge the blocking pool forever.
            let (mut gate, _) = self.cond.wait_timeout_while(gate, STEP, |g| !g.released).unwrap();
            gate.repo = None;
        }
    }
}

pub struct Harness {
    pub engine: Engine,
    pub sink: Arc<GatedSink>,
    pub credentials: Arc<MemoryCredentialStore>,
}

/// Engine on in-memory storage. Call from a multi-threaded tokio runtime.
pub fn engine_in_memory(options: EngineOptions) -> Harness {
    engine_on(Storage::in_memory().unwrap(), options)
}

pub fn engine_on(storage: Storage, options: EngineOptions) -> Harness {
    let sink = GatedSink::new();
    let credentials = Arc::new(MemoryCredentialStore::new());
    let engine = Engine::with_options(
        storage,
        credentials.clone(),
        sink.clone(),
        tokio::runtime::Handle::current(),
        options,
    );
    Harness { engine, sink, credentials }
}

/// `data/` and `cache/` under `root`.
pub fn storage_paths(root: &Path) -> StoragePaths {
    StoragePaths {
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
    }
}

/// Engine on SQLite files under `root` (see [`storage_paths`]); asserts that opening reported no warnings.
pub fn engine_on_disk(root: &Path, options: EngineOptions) -> Harness {
    let (storage, report) = Storage::open(&storage_paths(root)).unwrap();
    assert!(report.warnings.is_empty(), "storage warnings: {:?}", report.warnings);
    engine_on(storage, options)
}

/// Runs `f` with [`STEP`] as the deadline.
pub async fn step<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(STEP, f).await {
        Ok(v) => v,
        Err(_) => panic!("{what}: timed out after {STEP:?} (deadlock?)"),
    }
}

// ---- git fixtures ----------------------------------------------------------------------------------------------

pub fn signature() -> git2::Signature<'static> {
    git2::Signature::new("Fixture", "fixture@example.com", &git2::Time::new(1_700_000_000, 0)).unwrap()
}

/// `canonicalize` without the Windows `\\?\` prefix, which is the form the engine registers.
pub fn canonical(path: &Path) -> PathBuf {
    let full = path.canonicalize().unwrap();
    match full.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => full,
    }
}

/// `file://` URL for a local path: `file:///C:/dir/repo.git` on Windows, `file:///tmp/repo.git` elsewhere.
pub fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text);
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// A non-bare repository with `main` checked out and one commit (`README.md`). Returns the canonical root.
pub fn init_repo(dir: &Path) -> PathBuf {
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head("main");
    let repo = git2::Repository::init_opts(dir, &opts).unwrap();
    // Git for Windows enables `core.autocrlf` in its system config, which libgit2 honours: pin it so checkouts are byte-exact.
    repo.config().unwrap().set_bool("core.autocrlf", false).unwrap();
    drop(repo);
    commit_file(dir, "README.md", "hello\n", "initial");
    canonical(dir)
}

/// Writes `name`, stages it and commits on HEAD (with git2, not the engine). Returns the new commit id.
pub fn commit_file(dir: &Path, name: &str, content: &str, message: &str) -> git2::Oid {
    let repo = git2::Repository::open(dir).unwrap();
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(name)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
    let sig = signature();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents).unwrap()
}

/// A bare repository with `commits` commits on `main`, each adding a file of `blob_kib` KiB of distinct content (so
/// a fetch transfers enough objects to report progress more than once).
pub fn bare_remote(dir: &Path, commits: usize, blob_kib: usize) -> PathBuf {
    let work = dir.join("seed");
    init_repo(&work);
    for i in 0..commits {
        let content: String = (0..blob_kib * 16).map(|j| format!("{i:08}-{j:055}\n")).collect();
        commit_file(&work, &format!("f/{i}.txt"), &content, &format!("commit {i}"));
    }
    let bare = dir.join("remote.git");
    git2::build::RepoBuilder::new()
        .bare(true)
        .clone(work.to_str().unwrap(), &bare)
        .unwrap();
    canonical(&bare)
}

/// Adds `origin` → `url` to the repository at `dir`.
pub fn add_origin(dir: &Path, url: &Path) {
    let repo = git2::Repository::open(dir).unwrap();
    repo.remote("origin", url.to_str().unwrap()).unwrap();
}

pub fn to_core(oid: git2::Oid) -> gityas_core::Oid {
    gityas_core::Oid::from_bytes(oid.as_bytes()).unwrap()
}

// ---- event helpers ---------------------------------------------------------------------------------------------

pub fn names(events: &[AppEvent]) -> Vec<&'static str> {
    events.iter().map(AppEvent::name).collect()
}

/// Index of the first event matching `pred`.
pub fn position(events: &[AppEvent], pred: impl Fn(&AppEvent) -> bool) -> Option<usize> {
    events.iter().position(pred)
}

/// Asserts that `expected` appears in `events` as a subsequence (other events, e.g. from a watcher, may be
/// interleaved).
pub fn assert_subsequence(events: &[AppEvent], expected: &[&str]) {
    let mut it = events.iter().map(AppEvent::name);
    for want in expected {
        assert!(
            it.any(|name| name == *want),
            "expected {expected:?} as a subsequence of {:?}",
            names(events)
        );
    }
}

/// Asserts that every repo-scoped event belongs to `repo`.
pub fn assert_only_repo(events: &[AppEvent], repo: &str) {
    for ev in events {
        if let Some(r) = ev.repo() {
            assert_eq!(r, repo, "{} for another repository in {:?}", ev.name(), names(events));
        }
    }
}

/// The last `CommitsUpdated` payload for `repo`.
pub fn last_commits(events: &[AppEvent], repo: &str) -> Option<gityas_core::models::CommitWrapper> {
    events.iter().rev().find_map(|ev| match ev {
        AppEvent::CommitsUpdated { repo: r, commits } if r == repo => Some(commits.clone()),
        _ => None,
    })
}

/// Polls `sink` (accumulating) until an event matches `pred` or `timeout` passes. Returns everything collected.
pub async fn collect_until(sink: &GatedSink, timeout: Duration, pred: impl Fn(&AppEvent) -> bool) -> (Vec<AppEvent>, bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let batch = sink.take();
        let hit = batch.iter().any(&pred);
        seen.extend(batch);
        if hit {
            return (seen, true);
        }
        if tokio::time::Instant::now() >= deadline {
            return (seen, false);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Global flag helper for tests that need a one-shot "stop" across tasks.
pub fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

pub fn is_set(flag: &AtomicBool) -> bool {
    flag.load(Ordering::SeqCst)
}

/// Number of commits the on-disk cache under `root` holds for `repo`'s stored tips (`None`: no snapshot). Opens a
/// second `Storage` on the same files; call while no engine is writing.
pub fn cached_history_len(root: &Path, repo: &RepoId) -> Option<usize> {
    let (storage, _) = Storage::open(&storage_paths(root)).unwrap();
    let (_, tips) = storage.cache().snapshot(repo).unwrap()?;
    let oids: Vec<_> = tips.into_iter().map(|t| t.oid).collect();
    let graph = storage.cache().load_reachable(repo, &oids).unwrap();
    assert!(graph.is_complete(), "cached history incomplete: {:?}", graph.missing);
    Some(graph.commits.len())
}
