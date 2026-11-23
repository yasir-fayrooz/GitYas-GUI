//! Differential tests (§3.4 #7): for every scenario and every step of a mutation sequence on one cache,
//! `load_commits` through the cache must equal the live walk, and an unchanged second call must not walk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use git2::{ObjectType, Oid, Repository, Signature, Time};
use gityas_core::models::{CommitWrapper, Delta, DeltaChange};
use gityas_core::{AppEvent, GitYasError};
use gityas_storage::{Storage, StoragePaths};
use tempfile::TempDir;

use super::{Source, load_commits, load_history, purge_repository};
use crate::session::SessionCtx;
use crate::testutil::{TestEngine, test_engine, test_engine_with};
use crate::{Engine, EngineOptions, Inner};

// ------------------------------------------------------------------------------------------------ fixture

/// A temp repository with a deterministic clock (each commit one minute later unless given a time).
struct Fx {
    dir: TempDir,
    path: PathBuf,
    repo: Repository,
    clock: i64,
    n: u32,
}

impl Fx {
    fn new() -> Fx {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("repo");
        let repo = Repository::init(&path).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        Fx {
            dir,
            path,
            repo,
            clock: 1_700_000_000,
            n: 0,
        }
    }

    fn bare() -> Fx {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("repo.git");
        let repo = Repository::init_bare(&path).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        Fx {
            dir,
            path,
            repo,
            clock: 1_700_000_000,
            n: 0,
        }
    }

    fn gitdir(&self) -> PathBuf {
        self.repo.path().to_path_buf()
    }

    /// A commit with the given parents and committer time (`None`: next tick). Does not move any ref.
    fn commit_at(&mut self, parents: &[Oid], time: Option<i64>) -> Oid {
        self.n += 1;
        let time = time.unwrap_or_else(|| {
            self.clock += 60;
            self.clock
        });
        let sig = Signature::new("Ann", "ann@example.com", &Time::new(time, 0)).unwrap();
        let blob = self.repo.blob(format!("content {}\n", self.n).as_bytes()).unwrap();
        let mut tb = self.repo.treebuilder(None).unwrap();
        tb.insert(format!("f{}", self.n % 3), blob, 0o100644).unwrap();
        let tree = self.repo.find_tree(tb.write().unwrap()).unwrap();
        let parents: Vec<git2::Commit<'_>> = parents.iter().map(|p| self.repo.find_commit(*p).unwrap()).collect();
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        self.repo
            .commit(None, &sig, &sig, &format!("commit {}", self.n), &tree, &parent_refs)
            .unwrap()
    }

    fn commit(&mut self, parents: &[Oid]) -> Oid {
        self.commit_at(parents, None)
    }

    fn tip(&self, name: &str) -> Option<Oid> {
        self.repo.refname_to_id(name).ok()
    }

    /// Commits on top of `branch` (full ref name) and moves it; extra parents make a merge.
    fn on(&mut self, branch: &str, extra_parents: &[Oid]) -> Oid {
        let mut parents: Vec<Oid> = self.tip(branch).into_iter().collect();
        parents.extend_from_slice(extra_parents);
        let oid = self.commit(&parents);
        self.set(branch, oid);
        oid
    }

    fn set(&self, name: &str, oid: Oid) {
        self.repo.reference(name, oid, true, "test").unwrap();
    }

    fn del(&self, name: &str) {
        self.repo.find_reference(name).unwrap().delete().unwrap();
    }

    fn annotated_tag(&self, name: &str, target: Oid) {
        let obj = self.repo.find_object(target, None).unwrap();
        let sig = Signature::new("Tagger", "t@example.com", &Time::new(self.clock, 0)).unwrap();
        self.repo.tag(name, &obj, &sig, "annotated", true).unwrap();
    }

    /// A commit whose author and message are not UTF-8 (written as a raw object).
    fn raw_non_utf8_commit(&mut self, parent: Oid) -> Oid {
        self.clock += 60;
        let tree = self.repo.find_commit(parent).unwrap().tree_id();
        let mut buf = Vec::new();
        buf.extend_from_slice(format!("tree {tree}\nparent {parent}\n").as_bytes());
        buf.extend_from_slice(b"author J\xf6rg <j@example.com> ");
        buf.extend_from_slice(format!("{} +0000\n", self.clock).as_bytes());
        buf.extend_from_slice(b"committer J\xf6rg <j@example.com> ");
        buf.extend_from_slice(format!("{} +0000\n", self.clock).as_bytes());
        buf.extend_from_slice(b"\nGr\xfc\xdfe\n");
        self.repo.odb().unwrap().write(ObjectType::Commit, &buf).unwrap()
    }
}

// ------------------------------------------------------------------------------------------------ harness

fn open(path: &Path) -> Repository {
    gityas_git::repository::open(path).unwrap()
}

fn walks(inner: &Inner) -> u64 {
    inner.commit_graph.stats.walks.load(Ordering::SeqCst)
}

fn sqlite_loads(inner: &Inner) -> u64 {
    inner.commit_graph.stats.sqlite_loads.load(Ordering::SeqCst)
}

/// The oracle: live walk plus the same live refs `load_commits` reads.
fn live_wrapper(repo: &Repository) -> CommitWrapper {
    CommitWrapper {
        commits: Arc::from(gityas_git::commits::get_commits_live(repo).unwrap()),
        references: Arc::from(gityas_git::refs::list_references(repo).unwrap()),
        tags: Arc::from(gityas_git::refs::list_tags(repo).unwrap()),
        head: Some(gityas_git::commits::get_head(repo).unwrap()),
        partial: false,
    }
}

/// A second engine on the same storage: simulates an app restart (no hot entry, cache DB kept).
fn twin(t: &TestEngine) -> Engine {
    let inner = &t.engine.inner;
    Engine::with_options(
        inner.storage.clone(),
        t.credentials.clone(),
        t.sink.clone(),
        tokio::runtime::Handle::current(),
        inner.options.clone(),
    )
}

struct H {
    t: TestEngine,
    ctx: SessionCtx,
    fx: Fx,
    /// Drop the hot entry before each check, so incremental loads go through SQLite instead of the in-memory
    /// extension.
    cold_memory: bool,
}

impl H {
    fn new(options: EngineOptions) -> H {
        Self::with_fx(options, Fx::new())
    }

    fn with_fx(options: EngineOptions, fx: Fx) -> H {
        let t = test_engine_with(EngineOptions {
            fetch_interval: None,
            ..options
        });
        let ctx = t.install_session(&fx.path);
        H {
            t,
            ctx,
            fx,
            cold_memory: false,
        }
    }

    fn inner(&self) -> &Inner {
        &self.t.engine.inner
    }

    /// Asserts cache == live for the current repository state, that the load came from `expect`, that a repeated
    /// load neither walks nor reads SQLite, and that a fresh engine on the same cache serves it without walking.
    #[track_caller]
    fn check(&self, step: &str, expect: Source) {
        let repo = open(&self.fx.path);
        let live = live_wrapper(&repo);
        let inner = self.inner();
        let mut expect = expect;
        if self.cold_memory {
            super::hot_clear(&inner.commit_graph, None);
            if expect == Source::Hot {
                expect = Source::Hit;
            }
        }

        let loads = sqlite_loads(inner);
        let (commits, source) = load_history(inner, &self.ctx, &repo).unwrap();
        assert_eq!(source, expect, "{step}: source");
        if expect == Source::Incremental {
            let expected_loads = if self.cold_memory { loads + 1 } else { loads };
            assert_eq!(
                sqlite_loads(inner),
                expected_loads,
                "{step}: incremental path (hot extension vs SQLite)"
            );
        }
        assert_eq!(&commits[..], &live.commits[..], "{step}: cached history differs from the live walk");

        let cached = matches!(
            expect,
            Source::Hot | Source::Hit | Source::ColdFill | Source::Incremental | Source::RebuiltIncomplete | Source::RebuiltAfterError
        );
        let (w0, l0) = (walks(inner), sqlite_loads(inner));
        let wrapper = load_commits(inner, &self.ctx, &repo).unwrap();
        assert_eq!(wrapper, live, "{step}: CommitWrapper differs from the live oracle");
        if cached {
            assert_eq!(walks(inner), w0, "{step}: unchanged refs must not walk");
            assert_eq!(sqlite_loads(inner), l0, "{step}: unchanged refs must not reload SQLite (hot path)");
            let (_, again) = load_history(inner, &self.ctx, &repo).unwrap();
            assert_eq!(again, Source::Hot, "{step}: second load");

            let other = twin(&self.t);
            let (commits, source) = load_history(&other.inner, &self.ctx, &repo).unwrap();
            assert_eq!(source, Source::Hit, "{step}: restarted engine");
            assert_eq!(walks(&other.inner), 0, "{step}: restarted engine must not walk");
            assert_eq!(&commits[..], &live.commits[..], "{step}: restarted engine history");
        }
    }
}

fn option_sets() -> Vec<(&'static str, EngineOptions)> {
    vec![
        ("default", EngineOptions::default()),
        (
            "gc every refresh, max_age 0",
            EngineOptions {
                cache_gc_interval: Duration::ZERO,
                cache_gc_max_age: Duration::ZERO,
                ..EngineOptions::default()
            },
        ),
        (
            "gc every refresh, size cap 1 byte",
            EngineOptions {
                cache_gc_interval: Duration::ZERO,
                cache_max_bytes: 1,
                ..EngineOptions::default()
            },
        ),
    ]
}

// ------------------------------------------------------------------------------------------------ sequence

/// Every §3.4 #7 mutation applied one after the other on the same cache, under three GC policies.
#[tokio::test(flavor = "multi_thread")]
async fn differential_mutation_sequence_on_one_cache() {
    for (policy, options) in option_sets() {
        mutation_sequence(policy, options.clone(), false);
        mutation_sequence(policy, options, true);
    }
}

fn mutation_sequence(policy: &str, options: EngineOptions, cold_memory: bool) {
    use Source::*;
    let mut h = H::new(options);
    h.cold_memory = cold_memory;
    let policy = format!("{policy}, cold memory: {cold_memory}");
    let step = |name: &str| format!("[{policy}] {name}");
    const MAIN: &str = "refs/heads/main";

    h.check(&step("unborn / empty"), ColdFill);

    for _ in 0..3 {
        h.fx.on(MAIN, &[]);
    }
    h.check(&step("linear"), Incremental);

    let base = h.fx.tip(MAIN).unwrap();
    h.fx.set("refs/heads/feature", base);
    let f1 = h.fx.on("refs/heads/feature", &[]);
    h.fx.on("refs/heads/feature", &[]);
    h.fx.on(MAIN, &[]);
    let feature = h.fx.tip("refs/heads/feature").unwrap();
    h.fx.on(MAIN, &[feature]);
    h.check(&step("merge"), Incremental);

    let main = h.fx.tip(MAIN).unwrap();
    h.fx.set("refs/heads/b1", main);
    h.fx.set("refs/heads/b2", main);
    let b1 = h.fx.on("refs/heads/b1", &[]);
    let b2 = h.fx.on("refs/heads/b2", &[]);
    let b3 = h.fx.commit(&[main]);
    h.fx.on(MAIN, &[b1, b2, b3]);
    h.check(&step("octopus merge"), Incremental);

    h.fx.set("refs/tags/light", base);
    h.fx.annotated_tag("annotated", f1);
    let tree = h.fx.repo.find_commit(base).unwrap().tree_id();
    h.fx.annotated_tag("tree-tag", tree); // does not peel to a commit: skipped
    h.check(&step("tags"), Incremental);
    let main = h.fx.tip(MAIN).unwrap();
    h.fx.set("refs/tags/light", main);
    h.fx.annotated_tag("annotated", b2);
    h.check(&step("tag moves (lightweight + annotated)"), Incremental);

    // Rebase feature onto main: new commits, the old ones become unreachable (except via the tag at f1, moved above).
    let old_feature = h.fx.tip("refs/heads/feature").unwrap();
    let main = h.fx.tip(MAIN).unwrap();
    let r1 = h.fx.commit(&[main]);
    let r2 = h.fx.commit(&[r1]);
    h.fx.set("refs/heads/feature", r2);
    h.check(&step("rebase"), Incremental);

    let main = h.fx.tip(MAIN).unwrap();
    let parents: Vec<Oid> = h.fx.repo.find_commit(main).unwrap().parent_ids().collect();
    let amended = h.fx.commit(&parents);
    h.fx.set(MAIN, amended);
    h.check(&step("amend"), Incremental);

    h.fx.set("refs/remotes/origin/main", amended);
    h.fx.repo
        .reference_symbolic("refs/remotes/origin/HEAD", "refs/remotes/origin/main", true, "test")
        .unwrap();
    h.check(&step("remote-tracking ref"), Incremental);
    h.fx.set("refs/remotes/origin/main", base);
    h.check(&step("force-push: remote moved backwards"), Incremental);
    let u1 = h.fx.commit(&[]);
    let u2 = h.fx.commit(&[u1]);
    h.fx.set("refs/remotes/origin/main", u2);
    h.check(&step("force-push: remote moved to unrelated history"), Incremental);

    h.fx.del("refs/heads/b1");
    h.fx.del("refs/heads/b2");
    h.fx.del("refs/heads/feature");
    h.check(&step("branch delete"), Incremental);

    // Restore a branch to commits that were unreachable (and possibly gc'd from the cache) for a while.
    h.fx.set("refs/heads/restored", old_feature);
    h.check(&step("restore previously unreachable commits"), Incremental);

    let main = h.fx.tip(MAIN).unwrap();
    h.fx.repo.set_head_detached(main).unwrap();
    let d1 = h.fx.commit(&[main]);
    h.fx.repo.set_head_detached(d1).unwrap();
    h.check(&step("detached HEAD with a HEAD-only commit"), Incremental);

    h.fx.repo.set_head("refs/heads/orphan").unwrap();
    h.check(&step("orphan branch, unborn"), Incremental);
    let o1 = h.fx.commit(&[]);
    h.fx.set("refs/heads/orphan", o1);
    h.fx.on("refs/heads/orphan", &[]);
    h.check(&step("orphan branch with commits"), Incremental);

    // Clock skew (child older than parent) and equal timestamps exercise the sort's tie-breaking.
    let main = h.fx.tip(MAIN).unwrap();
    let skew = h.fx.commit_at(&[main], Some(1_000));
    let same_a = h.fx.commit_at(&[skew], Some(2_000_000_000));
    let same_b = h.fx.commit_at(&[skew], Some(2_000_000_000));
    h.fx.set(MAIN, same_a);
    h.fx.set("refs/heads/same", same_b);
    h.check(&step("clock skew and equal timestamps"), Incremental);

    let main = h.fx.tip(MAIN).unwrap();
    let raw = h.fx.raw_non_utf8_commit(main);
    h.fx.set(MAIN, raw);
    h.check(&step("non-UTF-8 author and message"), Incremental);
    let repo = open(&h.fx.path);
    let (commits, _) = load_history(h.inner(), &h.ctx, &repo).unwrap();
    assert!(commits.iter().any(|c| c.lossy), "lossy flag survives the cache");

    std::fs::write(h.fx.gitdir().join("MERGE_HEAD"), format!("{o1}\n")).unwrap();
    h.check(&step("merge in progress (MERGE_HEAD is not a tip)"), Hot);
    std::fs::remove_file(h.fx.gitdir().join("MERGE_HEAD")).unwrap();

    let main = h.fx.tip(MAIN).unwrap();
    std::fs::write(h.fx.gitdir().join("shallow"), format!("{main}\n")).unwrap();
    h.check(&step("shallow: bypass"), Bypassed);
    h.fx.on(MAIN, &[]);
    h.check(&step("shallow: bypass after a commit"), Bypassed);
    std::fs::remove_file(h.fx.gitdir().join("shallow")).unwrap();
    h.check(&step("no longer shallow"), Incremental);

    std::fs::create_dir_all(h.fx.gitdir().join("info")).unwrap();
    let main = h.fx.tip(MAIN).unwrap();
    std::fs::write(h.fx.gitdir().join("info/grafts"), format!("{main}\n")).unwrap();
    h.check(&step("grafts: bypass"), Bypassed);
    std::fs::remove_file(h.fx.gitdir().join("info/grafts")).unwrap();
    h.check(&step("grafts removed"), Hot);

    h.fx.set(&format!("refs/replace/{main}"), o1);
    h.check(&step("replace ref: bypass"), Bypassed);
    h.fx.del(&format!("refs/replace/{main}"));
    h.check(&step("replace ref removed"), Hot);

    h.inner().storage.cache().reset().unwrap();
    h.fx.on(MAIN, &[]);
    h.check(&step("cache reset externally, then a commit"), ColdFill);

    h.fx.set("refs/remotes/origin/main", u1);
    h.check(&step("final remote rewind"), Incremental);
}

// ------------------------------------------------------------------------------------------------ scenarios

#[tokio::test(flavor = "multi_thread")]
async fn hot_extension_refuses_a_history_that_does_not_close_over_parents() {
    let mut fx = Fx::new();
    let c1 = fx.on("refs/heads/main", &[]);
    let c2 = fx.on("refs/heads/main", &[]);
    let repo = open(&fx.path);
    let all = gityas_git::commits::get_commits_live(&repo).unwrap();
    let tips = gityas_git::cache::ref_tips(&repo).unwrap();
    let (top, root): (Vec<_>, Vec<_>) = all.into_iter().partition(|c| c.id.as_bytes() == c2.as_bytes());
    assert_eq!(root[0].id.as_bytes(), c1.as_bytes());
    let none = std::collections::HashSet::new();
    assert!(super::extend(&[], &none, top.clone(), &tips, &[]).is_none(), "parent c1 missing");
    let known = root.iter().map(|c| &c.id).collect();
    let full = super::extend(&root, &known, top, &tips, &[]).unwrap();
    assert_eq!(full, gityas_git::commits::get_commits_live(&repo).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_repository_has_no_history_and_is_cached() {
    let h = H::new(EngineOptions::default());
    h.check("unborn", Source::ColdFill);
    let snapshot = h.inner().storage.cache().snapshot(&h.ctx.repo_id).unwrap();
    assert!(snapshot.is_some_and(|(_, tips)| tips.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn bare_repository_is_cached() {
    let mut fx = Fx::bare();
    let c1 = fx.commit(&[]);
    fx.set("refs/heads/main", c1);
    let mut h = H::with_fx(EngineOptions::default(), fx);
    h.check("bare", Source::ColdFill);
    h.fx.on("refs/heads/main", &[]);
    h.check("bare + 1", Source::Incremental);
}

#[tokio::test(flavor = "multi_thread")]
async fn linked_worktree_uses_its_own_head() {
    let mut fx = Fx::new();
    let c1 = fx.on("refs/heads/main", &[]);
    fx.set("refs/heads/wt", c1);
    let wt_path = fx.dir.path().join("wt");
    {
        let wt_ref = fx.repo.find_reference("refs/heads/wt").unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&wt_ref));
        fx.repo.worktree("wt", &wt_path, Some(&opts)).unwrap();
    }
    let wt_repo = Repository::open(&wt_path).unwrap();
    // A HEAD-only commit in the worktree.
    let detached = fx.commit(&[c1]);
    wt_repo.set_head_detached(detached).unwrap();

    let t = test_engine();
    let ctx = t.install_session(&wt_path);
    let repo = open(&wt_path);
    let (commits, source) = load_history(&t.engine.inner, &ctx, &repo).unwrap();
    assert_eq!(source, Source::ColdFill);
    assert_eq!(&commits[..], &gityas_git::commits::get_commits_live(&repo).unwrap()[..]);
    assert!(commits.iter().any(|c| c.id.as_bytes() == detached.as_bytes()));
}

/// A real shallow clone made with the git CLI (test-only oracle use): bypassed, equal to the live walk, and nothing
/// is written to the cache.
// Redundant (the module is test-only) but lets `scripts/check.sh` see that the git CLI use below is test code.
#[cfg(test)]
#[tokio::test(flavor = "multi_thread")]
async fn shallow_clone_bypasses_the_cache() {
    let mut fx = Fx::new();
    for _ in 0..5 {
        fx.on("refs/heads/main", &[]);
    }
    let clone = fx.dir.path().join("shallow");
    let status = std::process::Command::new("git")
        .args(["clone", "-q", "--depth", "2", "--no-local"])
        .arg(crate::testutil::file_url(&fx.path))
        .arg(&clone)
        .status();
    let Ok(status) = status else {
        eprintln!("git CLI not available; skipping");
        return;
    };
    assert!(status.success());
    let repo = open(&clone);
    assert!(repo.is_shallow());
    let t = test_engine();
    let ctx = t.install_session(&clone);
    let (commits, source) = load_history(&t.engine.inner, &ctx, &repo).unwrap();
    assert_eq!(source, Source::Bypassed);
    assert_eq!(&commits[..], &gityas_git::commits::get_commits_live(&repo).unwrap()[..]);
    assert_eq!(t.engine.inner.storage.cache().snapshot(&ctx.repo_id).unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn cache_disabled_always_walks_live() {
    let mut h = H::new(EngineOptions {
        use_commit_cache: false,
        ..EngineOptions::default()
    });
    h.fx.on("refs/heads/main", &[]);
    h.check("disabled", Source::Disabled);
    h.check("disabled again", Source::Disabled);
    assert_eq!(h.inner().storage.cache().snapshot(&h.ctx.repo_id).unwrap(), None);
    assert_eq!(walks(h.inner()), 4, "every load walks (2 × load_history + 2 × load_commits)");
}

// ------------------------------------------------------------------------------------------------ fallbacks

#[tokio::test(flavor = "multi_thread")]
async fn incomplete_cache_is_rebuilt_from_a_live_walk() {
    let mut h = H::new(EngineOptions::default());
    let c1 = h.fx.on("refs/heads/main", &[]);
    let c2 = h.fx.on("refs/heads/main", &[]);
    h.check("filled", Source::ColdFill);

    // Corrupt the cache behind the engine's back: a snapshot for the current tips whose history only has c2
    // (c1 is referenced as a parent but has no row), as after a partial gc by another process.
    let repo = open(&h.fx.path);
    let tips = gityas_git::cache::ref_tips(&repo).unwrap();
    let fp = gityas_git::cache::fingerprint(&tips);
    let cache = h.inner().storage.cache();
    cache.purge_repository(&h.ctx.repo_id).unwrap();
    let only_c2: Vec<_> = gityas_git::commits::get_commits_live(&repo)
        .unwrap()
        .into_iter()
        .filter(|c| c.id.as_bytes() == c2.as_bytes())
        .collect();
    cache.store_refresh(&h.ctx.repo_id, &only_c2, &tips, &fp).unwrap();

    // A restarted engine (no hot entry) detects it, rebuilds, and serves the full history.
    let other = twin(&h.t);
    let (commits, source) = load_history(&other.inner, &h.ctx, &repo).unwrap();
    assert_eq!(source, Source::RebuiltIncomplete);
    assert_eq!(commits.len(), 2);
    assert!(commits.iter().any(|c| c.id.as_bytes() == c1.as_bytes()));
    let (_, source) = load_history(&twin(&h.t).inner, &h.ctx, &repo).unwrap();
    assert_eq!(source, Source::Hit, "rebuilt cache is complete");
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_error_falls_back_to_live_walk_and_resets_the_cache() {
    let mut h = H::new(EngineOptions::default());
    h.fx.on("refs/heads/main", &[]);
    h.check("filled", Source::ColdFill);
    h.fx.on("refs/heads/main", &[]);

    h.inner().commit_graph.fail_next_load.store(true, Ordering::SeqCst);
    h.check("after a storage error", Source::RebuiltAfterError);
    assert_eq!(h.inner().commit_graph.stats.resets.load(Ordering::SeqCst), 1);
    h.fx.on("refs/heads/main", &[]);
    h.check("next refresh is incremental again", Source::Incremental);
}

/// A real file cache whose database file is replaced by garbage mid-session: the refresh still succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn corrupt_cache_file_mid_session_never_fails_the_refresh() {
    let dir = TempDir::new().unwrap();
    let paths = StoragePaths {
        data_dir: dir.path().join("data"),
        cache_dir: dir.path().join("cache"),
    };
    let (storage, _) = Storage::open(&paths).unwrap();
    let sink = Arc::new(gityas_core::events::RecordingSink::new());
    let engine = Engine::with_options(
        storage.clone(),
        Arc::new(gityas_auth::MemoryCredentialStore::new()),
        sink,
        tokio::runtime::Handle::current(),
        EngineOptions::default(),
    );
    let mut fx = Fx::new();
    fx.on("refs/heads/main", &[]);
    let repo_row = storage.user().insert_repository("r", &fx.path, None).unwrap();
    let ctx = engine.inner.sessions.new_ctx(repo_row.id, fx.path.clone(), None);
    let repo = open(&fx.path);
    assert_eq!(load_history(&engine.inner, &ctx, &repo).unwrap().1, Source::ColdFill);

    let db_path = storage.cache().path().unwrap();
    let len = std::fs::metadata(&db_path).unwrap().len() as usize;
    std::fs::write(&db_path, vec![0x5a; len.max(4096)]).unwrap();
    for name in ["cache.db-wal", "cache.db-shm"] {
        let side = db_path.with_file_name(name);
        if side.exists() {
            // Best effort: SQLite keeps `-shm` mapped, and Windows refuses to overwrite a mapped file.
            let _ = std::fs::write(&side, vec![0x5a; 4096]);
        }
    }
    fx.on("refs/heads/main", &[]);
    let repo = open(&fx.path);
    let (commits, source) = load_history(&engine.inner, &ctx, &repo).unwrap();
    assert_eq!(&commits[..], &gityas_git::commits::get_commits_live(&repo).unwrap()[..]);
    assert!(
        matches!(source, Source::RebuiltAfterError | Source::RebuiltIncomplete | Source::Incremental),
        "{source:?}"
    );
    // Whatever SQLite noticed, the cache is usable afterwards.
    fx.on("refs/heads/main", &[]);
    let repo = open(&fx.path);
    let (commits, _) = load_history(&engine.inner, &ctx, &repo).unwrap();
    assert_eq!(&commits[..], &gityas_git::commits::get_commits_live(&repo).unwrap()[..]);
    let other = Engine::with_options(
        storage.clone(),
        Arc::new(gityas_auth::MemoryCredentialStore::new()),
        Arc::new(gityas_core::events::RecordingSink::new()),
        tokio::runtime::Handle::current(),
        EngineOptions::default(),
    );
    assert_eq!(load_history(&other.inner, &ctx, &repo).unwrap().1, Source::Hit);
}

#[tokio::test(flavor = "multi_thread")]
async fn dangling_ref_is_handled_like_the_live_walk() {
    let mut h = H::new(EngineOptions::default());
    h.fx.on("refs/heads/main", &[]);
    h.check("filled", Source::ColdFill);
    // A ref to a commit object that does not exist in the odb (corrupt repository).
    let missing = Oid::from_str("1234567890123456789012345678901234567890").unwrap();
    std::fs::write(h.fx.gitdir().join("refs/heads/broken"), format!("{missing}\n")).unwrap();
    let repo = open(&h.fx.path);
    let live = gityas_git::commits::get_commits_live(&repo);
    let cached = load_history(h.inner(), &h.ctx, &repo);
    // Same outcome as the live walk (ref_tips skips refs that don't peel, so both succeed here).
    assert_eq!(cached.map(|(c, _)| c.to_vec()), live);
}

// ------------------------------------------------------------------------------------------------ purge / refresh

#[tokio::test(flavor = "multi_thread")]
async fn purge_repository_drops_rows_and_hot_entry() {
    let mut h = H::new(EngineOptions::default());
    let c1 = h.fx.on("refs/heads/main", &[]);
    h.check("filled", Source::ColdFill);
    let cache = h.inner().storage.cache();
    cache
        .put_commit_files(&h.ctx.repo_id, &gityas_core::Oid::from_bytes(c1.as_bytes()).unwrap(), &[])
        .unwrap();
    purge_repository(h.inner(), &h.ctx.repo_id).unwrap();
    assert_eq!(cache.snapshot(&h.ctx.repo_id).unwrap(), None);
    assert_eq!(
        cache
            .commit_files(&h.ctx.repo_id, &gityas_core::Oid::from_bytes(c1.as_bytes()).unwrap())
            .unwrap(),
        None
    );
    h.check("after purge", Source::ColdFill);
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_commits_emits_commits_updated() {
    let t = test_engine();
    assert_eq!(t.engine.refresh_commits().await.err(), Some(GitYasError::NoActiveRepository));
    let mut fx = Fx::new();
    fx.on("refs/heads/main", &[]);
    fx.on("refs/heads/main", &[]);
    let ctx = t.install_session(&fx.path);
    let wrapper = t.engine.refresh_commits().await.unwrap();
    assert_eq!(wrapper, live_wrapper(&open(&fx.path)));
    assert_eq!(
        t.sink.take(),
        vec![AppEvent::CommitsUpdated {
            repo: ctx.repo_id.clone(),
            commits: wrapper.clone()
        }]
    );
    // Unchanged: same Arc served from the hot entry.
    let again = t.engine.refresh_commits().await.unwrap();
    assert!(Arc::ptr_eq(&again.commits, &wrapper.commits));
}

// ------------------------------------------------------------------------------------------------ commit files

fn core_oid(oid: Oid) -> gityas_core::Oid {
    gityas_core::Oid::from_bytes(oid.as_bytes()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_files_miss_then_hit() {
    let t = test_engine();
    let mut fx = Fx::new();
    let c1 = fx.on("refs/heads/main", &[]);
    let c2 = fx.on("refs/heads/main", &[]);
    let ctx = t.install_session(&fx.path);
    let cache = t.engine.inner.storage.cache();
    let oid = core_oid(c2);
    assert_eq!(cache.commit_files(&ctx.repo_id, &oid).unwrap(), None);

    let files = t.engine.commit_files(oid.clone()).await.unwrap();
    assert_eq!(files, gityas_git::commits::get_commit_files_list(&open(&fx.path), &oid).unwrap());
    assert_eq!(
        cache.commit_files(&ctx.repo_id, &oid).unwrap(),
        Some(files.clone()),
        "stored on miss"
    );

    // A hit is served from the cache: plant a sentinel list and observe it.
    let sentinel = vec![Delta {
        path: PathBuf::from("sentinel"),
        old_path: None,
        change: DeltaChange::Added,
    }];
    cache.put_commit_files(&ctx.repo_id, &oid, &sentinel).unwrap();
    assert_eq!(t.engine.commit_files(oid).await.unwrap(), sentinel);

    // Root commit and an unknown commit.
    let root = t.engine.commit_files(core_oid(c1)).await.unwrap();
    assert!(root.iter().all(|d| d.change == DeltaChange::Added));
    let unknown = gityas_core::Oid::from_hex("1234567890123456789012345678901234567890").unwrap();
    assert!(t.engine.commit_files(unknown.clone()).await.is_err());
    assert_eq!(cache.commit_files(&ctx.repo_id, &unknown).unwrap(), None);
    assert!(t.sink.take().is_empty(), "commit_files emits no events");
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_files_bypass_and_disabled_do_not_touch_the_cache() {
    for disabled in [true, false] {
        let t = test_engine_with(EngineOptions {
            fetch_interval: None,
            use_commit_cache: !disabled,
            ..EngineOptions::default()
        });
        let mut fx = Fx::new();
        let c1 = fx.on("refs/heads/main", &[]);
        let c2 = fx.on("refs/heads/main", &[]);
        if !disabled {
            fx.set(&format!("refs/replace/{c1}"), c2);
        }
        let ctx = t.install_session(&fx.path);
        let oid = core_oid(c2);
        let files = t.engine.commit_files(oid.clone()).await.unwrap();
        assert_eq!(files, gityas_git::commits::get_commit_files_list(&open(&fx.path), &oid).unwrap());
        assert_eq!(t.engine.inner.storage.cache().commit_files(&ctx.repo_id, &oid).unwrap(), None);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_a_long_history_sends_its_newest_page_first_then_the_rest() {
    let t = test_engine();
    t.engine.modify_settings(|s| s.graph.max_commits = 3).await.unwrap();
    let mut fx = Fx::new();
    for _ in 0..6 {
        fx.on("refs/heads/main", &[]);
    }
    let topic = fx.commit(&[fx.tip("refs/heads/main").unwrap()]);
    fx.set("refs/heads/topic", topic);
    t.install_session(&fx.path);
    let live = live_wrapper(&open(&fx.path));

    t.engine.refresh(crate::Refresh::ALL).await.unwrap();
    let events = t.sink.take();
    let names: Vec<&str> = events.iter().map(AppEvent::name).collect();
    assert_eq!(names.first(), Some(&"CommitsUpdated"));
    assert_eq!(
        names.last(),
        Some(&"CommitsUpdated"),
        "the full history comes after the other parts"
    );
    let commits: Vec<&CommitWrapper> = events
        .iter()
        .filter_map(|e| match e {
            AppEvent::CommitsUpdated { commits, .. } => Some(commits),
            _ => None,
        })
        .collect();
    let [page, full] = commits[..] else {
        panic!("expected two CommitsUpdated, got {names:?}")
    };
    assert!(page.partial && !full.partial);
    assert_eq!(page.commits[..], live.commits[..3], "the newest commits, in display order");
    assert_eq!((&page.references, &page.head), (&live.references, &live.head));
    assert_eq!(*full, live);

    // The history is now in memory: a refresh sends it once, and so does a history no longer than the page.
    t.engine.refresh(crate::Refresh::ALL).await.unwrap();
    let again = t.sink.take();
    assert_eq!(again.iter().filter(|e| e.name() == "CommitsUpdated").count(), 1);
    assert_eq!(again.first().map(AppEvent::name), Some("CommitsUpdated"));
    let short = test_engine();
    let mut fx = Fx::new();
    for _ in 0..3 {
        fx.on("refs/heads/main", &[]);
    }
    short.engine.modify_settings(|s| s.graph.max_commits = 3).await.unwrap();
    short.install_session(&fx.path);
    short.engine.refresh(crate::Refresh::ALL).await.unwrap();
    let events = short.sink.take();
    assert_eq!(events.iter().filter(|e| e.name() == "CommitsUpdated").count(), 1);
}
