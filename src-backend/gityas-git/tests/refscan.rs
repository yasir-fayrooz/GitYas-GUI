//! `refscan::scan_refs`: one read of every ref, with a cache reused between scans. The oracle is libgit2's own view
//! of the same repository (`git2::Repository::references`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use gityas_git::gitoxide;
use gityas_git::refscan::{RefScanCache, RefSnapshot, scan_refs};

fn commit(repo: &git2::Repository, message: &str, parent: Option<git2::Oid>) -> git2::Oid {
    let sig = git2::Signature::new("T", "t@example.com", &git2::Time::new(1_700_000_000, 0)).unwrap();
    let tree = repo.treebuilder(None).unwrap().write().unwrap();
    let tree = repo.find_tree(tree).unwrap();
    let parents: Vec<git2::Commit<'_>> = parent.map(|p| repo.find_commit(p).unwrap()).into_iter().collect();
    let parents: Vec<&git2::Commit<'_>> = parents.iter().collect();
    repo.commit(None, &sig, &sig, message, &tree, &parents).unwrap()
}

/// libgit2's view: full name -> commit it peels to (symbolic refs resolved), for refs that peel to a commit.
fn libgit2_view(path: &Path) -> BTreeMap<String, String> {
    let repo = git2::Repository::open(path).unwrap();
    let mut out = BTreeMap::new();
    for r in repo.references().unwrap() {
        let r = r.unwrap();
        if let (Some(name), Ok(c)) = (r.name(), r.peel_to_commit()) {
            out.insert(name.to_owned(), c.id().to_string());
        }
    }
    out
}

fn our_view(snapshot: &RefSnapshot) -> BTreeMap<String, String> {
    snapshot
        .refs
        .iter()
        .filter_map(|e| Some((e.name.clone(), e.commit?.to_string())))
        .collect()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

fn set_mtime_past(path: &Path) {
    // Make a file look old (outside the racy window) so the cache may trust it.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(old).unwrap();
}

#[test]
fn snapshot_matches_libgit2_across_loose_packed_symbolic_and_annotated() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = git2::Repository::init(dir.path()).unwrap();
    let a = commit(&repo, "a", None);
    let b = commit(&repo, "b", Some(a));
    repo.reference("refs/heads/main", b, true, "").unwrap();
    repo.set_head("refs/heads/main").unwrap();
    repo.reference("refs/heads/feature/x", a, true, "").unwrap();
    repo.reference("refs/remotes/origin/main", a, true, "").unwrap();
    repo.reference_symbolic("refs/remotes/origin/HEAD", "refs/remotes/origin/main", true, "")
        .unwrap();
    let sig = git2::Signature::new("T", "t@example.com", &git2::Time::new(1_700_000_000, 0)).unwrap();
    repo.tag("annotated", &repo.find_object(a, None).unwrap(), &sig, "msg", false)
        .unwrap();
    repo.tag_lightweight("light", &repo.find_object(b, None).unwrap(), false).unwrap();
    // A tag of a tree names no commit.
    let tree = repo.find_commit(a).unwrap().tree_id();
    repo.tag_lightweight("tree-tag", &repo.find_object(tree, None).unwrap(), false)
        .unwrap();
    // Pack some, then make the loose copy of one of them win.
    git(dir.path(), &["pack-refs", "--all"]);
    repo.reference("refs/heads/feature/x", b, true, "").unwrap();
    // Junk git ignores.
    std::fs::write(dir.path().join(".git/refs/heads/main.lock"), "garbage").unwrap();

    let gix_repo = gitoxide::open(dir.path()).unwrap();
    let snapshot = scan_refs(&gix_repo, &mut RefScanCache::default()).unwrap();
    assert_eq!(our_view(&snapshot), libgit2_view(dir.path()));
    let annotated = snapshot.refs.iter().find(|e| e.name == "refs/tags/annotated").unwrap();
    assert!(annotated.annotated);
    let head = snapshot.refs.iter().find(|e| e.name == "refs/remotes/origin/HEAD").unwrap();
    assert_eq!(head.target, None, "symbolic refs have no direct target");
    assert_eq!(head.commit.map(|c| c.to_string()), Some(a.to_string()));
    assert!(snapshot.refs.iter().all(|e| !e.name.ends_with(".lock")));
    // References list (direct only) and tags agree with the public listing functions.
    assert_eq!(snapshot.references().unwrap(), gityas_git::refs::list_references(&repo).unwrap());
    assert_eq!(snapshot.tags(&gix_repo).unwrap(), gityas_git::refs::list_tags(&repo).unwrap());
    assert!(snapshot.references().unwrap().iter().all(|r| r.name != "refs/remotes/origin/HEAD"));
}

#[test]
fn cached_scans_see_every_change() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = git2::Repository::init(dir.path()).unwrap();
    let a = commit(&repo, "a", None);
    let b = commit(&repo, "b", Some(a));
    for i in 0..100 {
        repo.reference(&format!("refs/remotes/origin/b{i:03}"), a, true, "").unwrap();
    }
    repo.reference("refs/heads/main", a, true, "").unwrap();
    let gix_repo = gitoxide::open(dir.path()).unwrap();
    let mut cache = RefScanCache::default();

    let first = scan_refs(&gix_repo, &mut cache).unwrap();
    assert_eq!(our_view(&first), libgit2_view(dir.path()));
    assert_eq!(cache.len().0, 101);

    // Old files are trusted from now on; a rewrite (new mtime) must still be seen.
    for i in 0..100 {
        set_mtime_past(&dir.path().join(format!(".git/refs/remotes/origin/b{i:03}")));
    }
    set_mtime_past(&dir.path().join(".git/refs/heads/main"));
    let _ = scan_refs(&gix_repo, &mut cache).unwrap();
    repo.reference("refs/remotes/origin/b007", b, true, "").unwrap();
    repo.find_reference("refs/remotes/origin/b042").unwrap().delete().unwrap();
    repo.reference("refs/tags/new", b, true, "").unwrap();
    let second = scan_refs(&gix_repo, &mut cache).unwrap();
    assert_eq!(our_view(&second), libgit2_view(dir.path()));
    assert_eq!(cache.len().0, 101);

    // Packing removes the loose files: the packed entries take over, nothing is lost.
    git(dir.path(), &["pack-refs", "--all"]);
    let third = scan_refs(&gix_repo, &mut cache).unwrap();
    assert_eq!(our_view(&third), libgit2_view(dir.path()));
    assert_eq!(cache.len().0, 0);
}

#[test]
fn racy_files_are_reread_even_with_an_unchanged_timestamp() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = git2::Repository::init(dir.path()).unwrap();
    let a = commit(&repo, "a", None);
    let b = commit(&repo, "b", Some(a));
    repo.reference("refs/heads/main", a, true, "").unwrap();
    let path = dir.path().join(".git/refs/heads/main");
    let gix_repo = gitoxide::open(dir.path()).unwrap();
    let mut cache = RefScanCache::default();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let _ = scan_refs(&gix_repo, &mut cache).unwrap();
    // Rewrite within the same timestamp (a coarse-grained file system): only the racy rule catches it.
    std::fs::write(&path, format!("{b}\n")).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let snapshot = scan_refs(&gix_repo, &mut cache).unwrap();
    let main = snapshot.refs.iter().find(|e| e.name == "refs/heads/main").unwrap();
    assert_eq!(main.commit.map(|c| c.to_string()), Some(b.to_string()));
}
