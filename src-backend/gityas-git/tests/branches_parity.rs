//! `branches::list_branches` (gitoxide, one ref scan) against the previous libgit2 implementation, kept here as the
//! oracle, on a repository with awkward remotes and upstreams.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use git2::{BranchType, ErrorCode, Repository};
use gityas_core::models::{Branch, BranchKind};

/// The previous `list_branches` (libgit2), verbatim apart from error handling.
fn libgit2_list_branches(repo: &Repository, kind: Option<BranchKind>) -> Vec<Branch> {
    let filter = kind.map(|k| match k {
        BranchKind::Local => BranchType::Local,
        BranchKind::Remote => BranchType::Remote,
    });
    let remote_of = |refname: &str| match repo.branch_remote_name(refname) {
        Ok(buf) => buf.as_str().map(str::to_owned),
        Err(e) if matches!(e.code(), ErrorCode::NotFound | ErrorCode::Ambiguous) => None,
        // Deliberate fix, not parity: a branch tracking a *local* branch (`branch.x.remote = .`) made libgit2 fail
        // with "not a remote branch", and the previous implementation returned that error for the whole list.
        Err(e) if !refname.starts_with("refs/remotes/") && e.class() == git2::ErrorClass::Invalid => None,
        Err(e) => panic!("{e}"),
    };
    let mut out = Vec::new();
    for item in repo.branches(filter).unwrap() {
        let (branch, branch_type) = item.unwrap();
        let reference = branch.get();
        if reference.kind() == Some(git2::ReferenceType::Symbolic) {
            continue;
        }
        let refname = reference.name().unwrap().to_owned();
        let mut b = Branch {
            name: reference.shorthand().unwrap().to_owned(),
            refname: refname.clone(),
            kind: match branch_type {
                BranchType::Local => BranchKind::Local,
                BranchType::Remote => BranchKind::Remote,
            },
            target: reference.target().map(|o| gityas_core::Oid::from_bytes(o.as_bytes()).unwrap()),
            remote_name: None,
            upstream: None,
            upstream_refname: None,
            upstream_gone: false,
            ahead: 0,
            behind: 0,
            is_head: branch.is_head(),
        };
        match branch_type {
            BranchType::Remote => b.remote_name = remote_of(&refname),
            BranchType::Local => {
                let up_name = match repo.branch_upstream_name(&refname) {
                    Ok(buf) => Some(buf.as_str().unwrap().to_owned()),
                    Err(e) if e.code() == ErrorCode::NotFound => None,
                    Err(e) => panic!("{e}"),
                };
                if let Some(up_name) = up_name {
                    b.upstream_refname = Some(up_name.clone());
                    match repo.find_reference(&up_name) {
                        Ok(up) => {
                            let up = up.resolve().unwrap();
                            b.upstream = up.shorthand().map(str::to_owned);
                            b.remote_name = remote_of(&up_name);
                            if let (Some(local), Some(remote)) = (reference.target(), up.target()) {
                                let (ahead, behind) = repo.graph_ahead_behind(local, remote).unwrap();
                                b.ahead = ahead;
                                b.behind = behind;
                            }
                        }
                        Err(e) if e.code() == ErrorCode::NotFound => {
                            b.upstream_gone = true;
                            b.upstream = Some(gityas_core::models::shorten_refname(&up_name).to_owned());
                            b.remote_name = remote_of(&up_name);
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
            }
        }
        out.push(b);
    }
    out
}

fn commit(repo: &Repository, message: &str, parent: Option<git2::Oid>) -> git2::Oid {
    let sig = git2::Signature::new("T", "t@example.com", &git2::Time::new(1_700_000_000, 0)).unwrap();
    let tree = repo.find_tree(repo.treebuilder(None).unwrap().write().unwrap()).unwrap();
    let parents: Vec<git2::Commit<'_>> = parent.map(|p| repo.find_commit(p).unwrap()).into_iter().collect();
    let parents: Vec<&git2::Commit<'_>> = parents.iter().collect();
    repo.commit(None, &sig, &sig, message, &tree, &parents).unwrap()
}

fn sorted(mut branches: Vec<Branch>) -> Vec<Branch> {
    branches.sort_by(|a, b| (a.kind == BranchKind::Remote, &a.refname).cmp(&(b.kind == BranchKind::Remote, &b.refname)));
    branches
}

#[test]
fn list_branches_matches_libgit2() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let base = commit(&repo, "base", None);
    let mut local = base;
    for i in 0..3 {
        local = commit(&repo, &format!("local {i}"), Some(local));
    }
    let mut remote = base;
    for i in 0..2 {
        remote = commit(&repo, &format!("remote {i}"), Some(remote));
    }
    repo.reference("refs/heads/main", local, true, "").unwrap();
    repo.set_head("refs/heads/main").unwrap();
    repo.reference("refs/heads/feature/deep/name", remote, true, "").unwrap();
    repo.reference("refs/heads/tracks-local", base, true, "").unwrap();
    repo.reference("refs/heads/gone", base, true, "").unwrap();
    repo.reference("refs/heads/no-refspec", base, true, "").unwrap();
    repo.reference("refs/heads/unknown-remote", base, true, "").unwrap();

    repo.remote("origin", "https://example.com/a.git").unwrap();
    repo.remote("team/upstream", "https://example.com/b.git").unwrap();
    // A second remote whose refspec also maps onto origin's refs: those become ambiguous.
    repo.remote_with_fetch("mirror", "https://example.com/c.git", "+refs/heads/*:refs/remotes/origin/*")
        .unwrap();
    repo.remote_with_fetch("narrow", "https://example.com/d.git", "+refs/heads/only:refs/remotes/narrow/only")
        .unwrap();
    repo.reference("refs/remotes/origin/main", remote, true, "").unwrap();
    repo.reference_symbolic("refs/remotes/origin/HEAD", "refs/remotes/origin/main", true, "")
        .unwrap();
    repo.reference("refs/remotes/team/upstream/feature/deep/name", base, true, "")
        .unwrap();
    repo.reference("refs/remotes/narrow/only", base, true, "").unwrap();
    repo.reference("refs/remotes/stale/x", base, true, "").unwrap();

    let mut config = repo.config().unwrap();
    for (branch, remote, merge) in [
        ("main", "origin", "refs/heads/main"),
        ("feature/deep/name", "team/upstream", "refs/heads/feature/deep/name"),
        ("tracks-local", ".", "refs/heads/main"),
        ("gone", "origin", "refs/heads/deleted-on-remote"),
        ("no-refspec", "narrow", "refs/heads/other"),
        ("unknown-remote", "nope", "refs/heads/main"),
    ] {
        config.set_str(&format!("branch.{branch}.remote"), remote).unwrap();
        config.set_str(&format!("branch.{branch}.merge"), merge).unwrap();
    }

    for kind in [None, Some(BranchKind::Local), Some(BranchKind::Remote)] {
        let want = sorted(libgit2_list_branches(&repo, kind));
        let got = sorted(gityas_git::branches::list_branches(&repo, kind).unwrap());
        assert_eq!(got, want, "kind {kind:?}");
    }
    // Ahead/behind counted in a loaded history (the engine's in-memory path) gives the same answers.
    let gix_repo = gityas_git::gitoxide::open(dir.path()).unwrap();
    let history = gityas_git::commits::get_commits_live(&gix_repo).unwrap();
    let (snapshot, _) = gityas_git::refscan::scan_refs_once(&gix_repo).unwrap();
    let in_memory = gityas_git::branches::list_branches_from(&gix_repo, &snapshot, None, Some(&history)).unwrap();
    assert_eq!(sorted(in_memory), sorted(libgit2_list_branches(&repo, None)));

    // Sanity: the oracle exercised the interesting cases.
    let all = libgit2_list_branches(&repo, None);
    let main = all.iter().find(|b| b.refname == "refs/heads/main").unwrap();
    assert_eq!((main.ahead, main.behind, main.upstream.as_deref()), (3, 2, Some("origin/main")));
    assert!(all.iter().any(|b| b.upstream_gone));
    assert!(
        all.iter()
            .any(|b| b.refname == "refs/remotes/origin/main" && b.remote_name.is_none()),
        "ambiguous"
    );
}
