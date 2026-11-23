//! WP5 cache primitives: `ref_tips`, `fingerprint`, `walk_new_commits`, `should_bypass_cache`, and the differential
//! test: incremental walks over successive snapshots, unioned and filtered to reachable, equal the live walk.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns too

mod wp5_support;

use std::collections::{HashMap, HashSet};

use git2::Repository;
use gityas_core::Oid;
use gityas_core::models::{Commit, RefTip, RefsFingerprint};
use gityas_git::cache::{HEAD_TIP, fingerprint, ref_tips, should_bypass_cache, walk_new_commits, walk_until_known};
use gityas_git::commits::{get_commits_live, retain_reachable, sort_commits};
use wp5_support::{TestRepo, file_url, git_ok};

fn oid(hex: &str) -> Oid {
    Oid::from_hex(hex.trim()).unwrap()
}

fn tip(name: &str, hex: &str) -> RefTip {
    RefTip {
        refname: name.into(),
        oid: oid(hex),
    }
}

// ---------------------------------------------------------------- ref_tips

#[test]
fn ref_tips_sorted_peeled_and_filtered() {
    let repo = TestRepo::with_commit();
    let c1 = repo.rev("HEAD");
    repo.write("b", "b");
    let c2 = repo.commit_all("second");
    repo.git(&["tag", "light", &c1]);
    repo.git(&["tag", "-a", "-m", "annotated", "annotated", &c1]);
    // A tag pointing to a tree and one to a blob: skipped, not an error.
    let tree = repo.rev("HEAD^{tree}");
    repo.git(&["tag", "tree-tag", &tree]);
    let blob = repo.git(&["hash-object", "-w", "b"]);
    repo.git(&["tag", "-a", "-m", "blob", "blob-tag", blob.trim()]);
    repo.git(&["update-ref", "refs/remotes/origin/main", &c1]);
    repo.git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
    repo.git(&["update-ref", "refs/notes/commits", &c2]);
    repo.git(&["update-ref", "refs/pull/1/head", &c2]);
    repo.write("a.txt", "stash me");
    repo.git(&["stash", "-q"]);

    let tips = ref_tips(&repo.open()).unwrap();
    assert_eq!(
        tips,
        vec![
            tip(HEAD_TIP, &c2),
            tip("refs/heads/main", &c2),
            tip("refs/remotes/origin/HEAD", &c1),
            tip("refs/remotes/origin/main", &c1),
            tip("refs/tags/annotated", &c1),
            tip("refs/tags/light", &c1),
        ]
    );
}

#[test]
fn ref_tips_detached_and_unborn() {
    let repo = TestRepo::init();
    assert!(ref_tips(&repo.open()).unwrap().is_empty());
    repo.write("a", "a");
    let c1 = repo.commit_all("one");
    repo.git(&["checkout", "-q", "--detach"]);
    repo.write("b", "b");
    let c2 = repo.commit_all("detached");
    assert_eq!(
        ref_tips(&repo.open()).unwrap(),
        vec![tip(HEAD_TIP, &c2), tip("refs/heads/main", &c1)]
    );
}

// ---------------------------------------------------------------- fingerprint

#[test]
fn fingerprint_is_order_independent_stable_and_sensitive() {
    let a = tip("refs/heads/a", "0101010101010101010101010101010101010101");
    let b = tip("refs/heads/b", "0202020202020202020202020202020202020202");
    let fp = fingerprint(&[a.clone(), b.clone()]);
    assert_eq!(fp, fingerprint(&[b.clone(), a.clone()]));
    assert_eq!(fp, fingerprint(&[a.clone(), b.clone(), a.clone()]), "duplicates ignored");
    // Pinned: the fingerprint is persisted in cache.db, so it must not change between builds.
    assert_eq!(
        fp,
        RefsFingerprint(vec![1, 2, 0, 0, 0, 0, 0, 0, 0, 180, 8, 170, 189, 56, 196, 165, 51])
    );
    let moved = tip("refs/heads/b", "0303030303030303030303030303030303030303");
    assert_ne!(fp, fingerprint(&[a.clone(), moved]));
    let renamed = tip("refs/heads/c", "0202020202020202020202020202020202020202");
    assert_ne!(fp, fingerprint(&[a.clone(), renamed]));
    assert_ne!(fp, fingerprint(&[a]));
    assert_ne!(fingerprint(&[]), fp);
    // Length prefixes: moving bytes between refname and oid changes the hash.
    let x = RefTip {
        refname: "ab".into(),
        oid: Oid::from_bytes(b"c").unwrap(),
    };
    let y = RefTip {
        refname: "a".into(),
        oid: Oid::from_bytes(b"bc").unwrap(),
    };
    assert_ne!(fingerprint(&[x]), fingerprint(&[y]));
}

// ---------------------------------------------------------------- walk_new_commits + differential

/// What the engine (WP8e) will do, with an in-memory "cache".
#[derive(Default)]
struct SimCache {
    rows: HashMap<Oid, Commit>,
    snapshot: Vec<RefTip>,
    walks: usize,
    /// Walk with `walk_until_known` (stop at cached commits) instead of `walk_new_commits` (hide old tips).
    until_known: bool,
}

impl SimCache {
    fn refresh(&mut self, repo: &Repository) -> Vec<Commit> {
        let tips = ref_tips(repo).unwrap();
        if fingerprint(&tips) != fingerprint(&self.snapshot) {
            self.walks += 1;
            let rows = if self.until_known {
                let known: HashSet<&Oid> = self.rows.keys().collect();
                walk_until_known(repo, &tips, &known).unwrap()
            } else {
                walk_new_commits(repo, &tips, &self.snapshot).unwrap()
            };
            for row in rows {
                self.rows.insert(row.id.clone(), row);
            }
            self.snapshot = tips.clone();
        }
        let tip_ids: Vec<Oid> = tips.into_iter().map(|t| t.oid).collect();
        sort_commits(retain_reachable(self.rows.values().cloned().collect(), &tip_ids))
    }
}

fn check(repo: &TestRepo, cache: &mut SimCache, step: &str) {
    let r = repo.open();
    let cached = cache.refresh(&r);
    let live = get_commits_live(&r).unwrap();
    assert_eq!(cached, live, "cache != live after {step}");
}

#[test]
fn differential_linear_merge_octopus() {
    for until_known in [false, true] {
        differential_linear_merge_octopus_with(until_known);
    }
}

fn differential_linear_merge_octopus_with(until_known: bool) {
    let repo = TestRepo::init();
    let mut cache = SimCache {
        until_known,
        ..SimCache::default()
    };
    check(&repo, &mut cache, "unborn");
    for i in 0..3 {
        repo.write("f", format!("{i}"));
        repo.commit_all(&format!("linear {i}"));
        check(&repo, &mut cache, "linear commit");
    }
    for b in ["b1", "b2", "b3"] {
        repo.git(&["checkout", "-qb", b, "main"]);
        repo.write(b, b);
        repo.commit_all(b);
        check(&repo, &mut cache, "branch commit");
    }
    repo.git(&["checkout", "-q", "main"]);
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge b1", "b1"]);
    check(&repo, &mut cache, "merge");
    repo.git(&["merge", "-q", "-m", "octopus", "b2", "b3"]);
    assert_eq!(repo.git(&["log", "-1", "--format=%P"]).split_whitespace().count(), 3);
    check(&repo, &mut cache, "octopus");
    let walks = cache.walks;
    check(&repo, &mut cache, "no change");
    assert_eq!(cache.walks, walks, "unchanged refs must not walk");
}

#[test]
fn differential_amend_rebase_force_push_branch_delete_tag_move() {
    for until_known in [false, true] {
        differential_amend_rebase_force_push_branch_delete_tag_move_with(until_known);
    }
}

fn differential_amend_rebase_force_push_branch_delete_tag_move_with(until_known: bool) {
    let repo = TestRepo::init();
    let mut cache = SimCache {
        until_known,
        ..SimCache::default()
    };
    repo.write("f", "0");
    repo.commit_all("root");
    repo.write("f", "1");
    repo.commit_all("one");
    check(&repo, &mut cache, "setup");

    repo.write("f", "1 amended");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "--amend", "-m", "one amended"]);
    check(&repo, &mut cache, "amend");

    repo.git(&["checkout", "-qb", "topic", "HEAD~1"]);
    for i in 0..3 {
        repo.write(format!("t{i}"), "t");
        repo.commit_all(&format!("topic {i}"));
    }
    check(&repo, &mut cache, "topic");
    repo.git(&["rebase", "-q", "main"]);
    check(&repo, &mut cache, "rebase");

    let before = repo.rev("topic");
    repo.git(&["update-ref", "refs/remotes/origin/topic", &before]);
    repo.git(&["reset", "-q", "--hard", "HEAD~2"]);
    repo.write("x", "x");
    repo.commit_all("rewritten");
    check(&repo, &mut cache, "local rewrite");
    repo.git(&["update-ref", "refs/remotes/origin/topic", &repo.rev("topic")]);
    check(&repo, &mut cache, "force push (remote ref rewound)");

    repo.git(&["tag", "moving", "main"]);
    check(&repo, &mut cache, "tag");
    repo.git(&["tag", "-f", "moving", "topic~1"]);
    check(&repo, &mut cache, "tag move");
    repo.git(&["tag", "-d", "moving"]);
    check(&repo, &mut cache, "tag delete");

    repo.git(&["checkout", "-q", "main"]);
    repo.git(&["branch", "-D", "topic"]);
    repo.git(&["update-ref", "-d", "refs/remotes/origin/topic"]);
    check(&repo, &mut cache, "branch delete");

    repo.git(&["checkout", "-q", "--detach", "HEAD~1"]);
    repo.write("d", "d");
    repo.commit_all("detached");
    check(&repo, &mut cache, "detached commit");
    repo.git(&["checkout", "-q", "main"]);
    check(&repo, &mut cache, "left detached commit");
}

#[test]
fn walk_tolerates_gc_d_old_tips() {
    let repo = TestRepo::with_commit();
    repo.git(&["checkout", "-qb", "doomed"]);
    repo.write("d", "d");
    let doomed = repo.commit_all("doomed");
    repo.git(&["checkout", "-q", "main"]);
    let old = ref_tips(&repo.open()).unwrap();
    repo.git(&["branch", "-D", "doomed"]);
    repo.git(&["reflog", "expire", "--expire=now", "--all"]);
    repo.git(&["gc", "-q", "--prune=now"]);
    assert!(!repo.git_try(&["cat-file", "-e", &doomed]).status.success(), "object pruned");
    repo.write("n", "n");
    let new_commit = repo.commit_all("new");
    let r = repo.open();
    let new = ref_tips(&r).unwrap();
    let rows = walk_new_commits(&r, &new, &old).unwrap();
    assert_eq!(rows.iter().map(|c| c.id.clone()).collect::<Vec<_>>(), vec![oid(&new_commit)]);
    // Old tips with bogus ids are tolerated as well.
    let bogus = vec![tip("refs/heads/x", "00000000000000000000000000000000000000ff")];
    assert_eq!(walk_new_commits(&r, &new, &bogus).unwrap().len(), 2);
}

#[test]
fn walk_with_no_old_tips_is_full_walk() {
    let repo = TestRepo::with_commit();
    repo.write("b", "b");
    repo.commit_all("two");
    let r = repo.open();
    let tips = ref_tips(&r).unwrap();
    let mut rows = walk_new_commits(&r, &tips, &[]).unwrap();
    rows = sort_commits(rows);
    assert_eq!(rows, get_commits_live(&r).unwrap());
    assert!(walk_new_commits(&r, &tips, &tips).unwrap().is_empty());
}

// ---------------------------------------------------------------- bypass

#[test]
fn bypass_for_shallow_grafts_and_replace_refs() {
    let src = TestRepo::with_commit();
    for i in 0..3 {
        src.write("f", format!("{i}"));
        src.commit_all(&format!("c{i}"));
    }
    assert!(!should_bypass_cache(&src.open()).unwrap());

    let shallow = tempfile::tempdir().unwrap();
    let url = file_url(src.path());
    git_ok(shallow.path(), &["clone", "-q", "--depth", "1", &url, "."]);
    let r = Repository::open(shallow.path()).unwrap();
    assert!(should_bypass_cache(&r).unwrap());
    // The live walk still works on a shallow clone (the oracle/fallback path).
    let live = get_commits_live(&r).unwrap();
    assert_eq!(live.len(), 1);

    let replaced = src.duplicate();
    let head = replaced.rev("HEAD");
    let parent = replaced.rev("HEAD~1");
    replaced.git(&["replace", &head, &parent]);
    assert!(should_bypass_cache(&replaced.open()).unwrap());

    let grafted = src.duplicate();
    grafted.write(".git/info/grafts", format!("{}\n", grafted.rev("HEAD")));
    assert!(should_bypass_cache(&grafted.open()).unwrap());
}
