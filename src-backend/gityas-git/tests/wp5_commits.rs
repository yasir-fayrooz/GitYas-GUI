//! WP5 commits: live walk + sort order, head, commit staged files (bugs #3, #4, #5), commit file lists, bodies.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns too

mod wp5_support;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gityas_core::models::{Commit, CommitSignature, Delta, DeltaChange, Head, ResetMode};
use gityas_core::{GitYasError, Oid};
use gityas_git::commits::{
    build_commit_message, commit_body, commit_staged_files, get_commit_files_list, get_commits_live, get_head, reset_to_commit,
    retain_reachable, reword_commit, sort_commits,
};
use wp5_support::{TestRepo, delta, sig};

fn oid(hex: &str) -> Oid {
    Oid::from_hex(hex.trim()).unwrap()
}

fn row(id: u8, parents: &[u8], time: i64) -> Commit {
    let o = |b: u8| Oid::from_bytes(&[b; 20]).unwrap();
    Commit {
        id: o(id),
        parents: parents.iter().map(|p| o(*p)).collect(),
        author_name: String::new(),
        author_email: String::new(),
        author_time: time,
        committer_time: time,
        summary: format!("c{id}"),
        description: String::new(),
        lossy: false,
    }
}

fn ids(commits: &[Commit]) -> Vec<u8> {
    commits.iter().map(|c| c.id.as_bytes()[0]).collect()
}

fn assert_topological(commits: &[Commit]) {
    let pos: HashMap<&Oid, usize> = commits.iter().enumerate().map(|(i, c)| (&c.id, i)).collect();
    for (i, c) in commits.iter().enumerate() {
        for p in &c.parents {
            if let Some(&pi) = pos.get(p) {
                assert!(pi > i, "parent {p} listed before child {}", c.id);
            }
        }
    }
}

// ---------------------------------------------------------------- sort_commits (pure)

#[test]
fn sort_is_newest_first_for_linear_history() {
    let commits = vec![row(1, &[], 10), row(3, &[2], 30), row(2, &[1], 20)];
    assert_eq!(ids(&sort_commits(commits)), vec![3, 2, 1]);
}

#[test]
fn sort_keeps_children_before_parents_under_clock_skew() {
    // child 2 has an older timestamp than its parent 1.
    let commits = vec![row(1, &[], 50), row(2, &[1], 10), row(3, &[], 30)];
    let sorted = sort_commits(commits);
    assert_eq!(ids(&sorted), vec![2, 1, 3]);
    assert_topological(&sorted);
}

#[test]
fn sort_interleaves_branches_by_date() {
    // main: 1 <- 2 <- 5(merge 2,4); feature: 1 <- 3 <- 4
    let commits = vec![
        row(1, &[], 1),
        row(2, &[1], 2),
        row(3, &[1], 3),
        row(4, &[3], 4),
        row(5, &[2, 4], 5),
    ];
    assert_eq!(ids(&sort_commits(commits)), vec![5, 4, 3, 2, 1]);
}

#[test]
fn sort_ties_break_by_id_and_is_deterministic() {
    let a = vec![row(9, &[], 7), row(4, &[], 7), row(6, &[], 7)];
    let mut b = a.clone();
    b.reverse();
    assert_eq!(ids(&sort_commits(a)), vec![4, 6, 9]);
    assert_eq!(ids(&sort_commits(b)), vec![4, 6, 9]);
}

#[test]
fn sort_ignores_missing_parents_and_duplicates() {
    let commits = vec![row(2, &[1], 2), row(3, &[2, 7], 3), row(2, &[1], 2)];
    assert_eq!(ids(&sort_commits(commits)), vec![3, 2]);
}

#[test]
fn sort_survives_very_deep_history() {
    // The old recursive DFS would overflow the stack here (bug-log WP5.2).
    let n: u32 = 200_000;
    let o = |i: u32| Oid::from_bytes(&i.to_be_bytes()).unwrap();
    let commits: Vec<Commit> = (0..n)
        .map(|i| Commit {
            id: o(i),
            parents: if i == 0 { vec![] } else { vec![o(i - 1)] },
            author_name: String::new(),
            author_email: String::new(),
            author_time: 0,
            committer_time: i64::from(n - i), // fully reversed clock: worst case for the DFS
            summary: String::new(),
            description: String::new(),
            lossy: false,
        })
        .collect();
    let sorted = sort_commits(commits);
    assert_eq!(sorted.len(), n as usize);
    assert_eq!(sorted.first().unwrap().id, o(n - 1));
    assert_topological(&sorted);
}

#[test]
fn retain_reachable_drops_unreachable() {
    let commits = vec![row(1, &[], 1), row(2, &[1], 2), row(3, &[1], 3), row(4, &[3], 4)];
    let tips = [Oid::from_bytes(&[2; 20]).unwrap()];
    assert_eq!(ids(&retain_reachable(commits, &tips)), vec![1, 2]);
}

// ---------------------------------------------------------------- live walk

#[test]
fn live_walk_covers_branches_remotes_tags_and_detached_head_but_not_stash() {
    let repo = TestRepo::with_commit();
    let base = repo.rev("HEAD");
    repo.git(&["checkout", "-qb", "feature"]);
    repo.write("f", "f");
    let feature = repo.commit_all("feature");
    repo.git(&["checkout", "-q", "main"]);
    repo.git(&["checkout", "-q", "--detach"]);
    repo.write("d", "d");
    let detached = repo.commit_all("detached only");
    repo.git(&["checkout", "-q", "main"]);
    repo.git(&["checkout", "-q", "--detach", &detached]);
    repo.git(&["update-ref", "refs/remotes/origin/x", &feature]);
    // A tag on a commit only reachable from the tag, annotated.
    repo.git(&["checkout", "-q", "-b", "tmp", &base]);
    repo.write("t", "t");
    let tagged = repo.commit_all("tag only");
    repo.git(&["tag", "-a", "-m", "annotated", "v1", &tagged]);
    repo.git(&["checkout", "-q", "--detach", &detached]);
    repo.git(&["branch", "-D", "tmp"]);
    // A stash must not appear.
    repo.write("a.txt", "stashed\n");
    repo.git(&["stash", "-q"]);
    let stash = repo.rev("refs/stash");

    let live = get_commits_live(&repo.open()).unwrap();
    let got: HashSet<Oid> = live.iter().map(|c| c.id.clone()).collect();
    let expected: HashSet<Oid> = repo
        .git(&["rev-list", "--branches", "--remotes", "--tags", "HEAD"])
        .lines()
        .map(oid)
        .collect();
    assert_eq!(got, expected);
    assert!(got.contains(&oid(&detached)) && got.contains(&oid(&tagged)) && !got.contains(&oid(&stash)));
    assert_topological(&live);
}

#[test]
fn live_walk_order_matches_git_date_order_for_simple_histories() {
    let repo = TestRepo::with_commit();
    repo.git(&["checkout", "-qb", "feature"]);
    for i in 0..3 {
        repo.write(format!("f{i}"), "x");
        repo.commit_all(&format!("feature {i}"));
    }
    repo.git(&["checkout", "-q", "main"]);
    for i in 0..2 {
        repo.write(format!("m{i}"), "x");
        repo.commit_all(&format!("main {i}"));
    }
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge", "feature"]);
    let live: Vec<Oid> = get_commits_live(&repo.open()).unwrap().into_iter().map(|c| c.id).collect();
    let git: Vec<Oid> = repo.git(&["rev-list", "--date-order", "--all"]).lines().map(oid).collect();
    assert_eq!(live, git);
}

#[test]
fn commit_rows_have_parents_in_order_and_summary() {
    let repo = TestRepo::with_commit();
    repo.git(&["checkout", "-qb", "side"]);
    repo.write("s", "s");
    let side = repo.commit_all("side");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("m", "m");
    let main = repo.commit_all("main work");
    repo.git(&["merge", "-q", "--no-ff", "-m", "Merge side\n\nbody text", "side"]);
    let live = get_commits_live(&repo.open()).unwrap();
    let merge = &live[0];
    assert_eq!(merge.parents, vec![oid(&main), oid(&side)]);
    assert_eq!(merge.summary, "Merge side");
    assert_eq!(merge.author_name, "Test User");
    assert_eq!(merge.author_email, "test@example.com");
}

#[test]
fn live_walk_on_unborn_repo_is_empty() {
    assert!(get_commits_live(&TestRepo::init().open()).unwrap().is_empty());
}

// ---------------------------------------------------------------- head

#[test]
fn head_reports_branch_detached_and_unborn() {
    let repo = TestRepo::init();
    assert_eq!(
        get_head(&repo.open()).unwrap(),
        Head::Unborn {
            name: "main".into(),
            refname: "refs/heads/main".into()
        }
    );
    repo.write("a", "a");
    let c = repo.commit_all("first");
    assert_eq!(
        get_head(&repo.open()).unwrap(),
        Head::Branch {
            name: "main".into(),
            refname: "refs/heads/main".into(),
            target: oid(&c)
        }
    );
    repo.git(&["checkout", "-q", "--detach"]);
    // The old get_head used `shorthand()`, which is "HEAD" when detached, so it never reported detached.
    assert_eq!(get_head(&repo.open()).unwrap(), Head::Detached { target: oid(&c) });
    repo.git(&["checkout", "-q", "--orphan", "fresh/branch"]);
    assert_eq!(
        get_head(&repo.open()).unwrap(),
        Head::Unborn {
            name: "fresh/branch".into(),
            refname: "refs/heads/fresh/branch".into()
        }
    );
}

#[test]
fn head_works_on_bare_repo() {
    let src = TestRepo::with_commit();
    let bare = tempfile::tempdir().unwrap();
    wp5_support::git_ok(bare.path(), &["clone", "-q", "--bare", src.path().to_str().unwrap(), "."]);
    let r = git2::Repository::open(bare.path()).unwrap();
    assert!(matches!(get_head(&r).unwrap(), Head::Branch { .. }));
}

// ---------------------------------------------------------------- commit_staged_files

#[test]
fn bug3_first_commit_on_unborn_head() {
    let repo = TestRepo::init();
    repo.write("a.txt", "a\n");
    repo.git(&["add", "a.txt"]);
    let mut r = repo.open();
    let id = commit_staged_files(&mut r, &sig(), "first", None).unwrap();
    assert_eq!(id, oid(&repo.rev("HEAD")));
    assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).trim(), "1");
    assert!(repo.porcelain().is_empty());
}

#[test]
fn bug4_message_has_no_trailing_blank_lines() {
    let repo = TestRepo::with_commit();
    let mut r = repo.open();
    for (description, expected) in [
        (None, "subject\n"),
        (Some(""), "subject\n"),
        (Some("   \n\n"), "subject\n"),
        (Some("body line\n\n\n"), "subject\n\nbody line\n"),
        (Some("#123 is not a comment"), "subject\n\n#123 is not a comment\n"),
    ] {
        repo.write("a.txt", format!("{description:?}"));
        repo.git(&["add", "a.txt"]);
        commit_staged_files(&mut r, &sig(), "  subject  ", description).unwrap();
        assert_eq!(
            repo.git(&["log", "-1", "--format=%B"]).trim_end_matches('\n').to_owned() + "\n",
            expected
        );
        assert_eq!(
            repo.git(&["cat-file", "commit", "HEAD"])
                .split("\n\n")
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n\n"),
            expected
        );
    }
    assert_eq!(build_commit_message("s", Some("a\n\n\n\nb")).unwrap(), "s\n\na\n\nb\n");
}

#[test]
fn bug5_empty_or_invalid_signature_is_a_clear_error() {
    let repo = TestRepo::with_commit();
    repo.write("a.txt", "x");
    repo.git(&["add", "a.txt"]);
    let mut r = repo.open();
    for bad in [
        CommitSignature {
            name: String::new(),
            email: String::new(),
        },
        CommitSignature {
            name: "  ".into(),
            email: "a@b".into(),
        },
        CommitSignature {
            name: "ok".into(),
            email: "<bad>".into(),
        },
    ] {
        let err = commit_staged_files(&mut r, &bad, "msg", None).unwrap_err();
        assert!(matches!(err, GitYasError::SignatureRequired(_)), "{bad:?}: {err:?}");
    }
    assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).trim(), "1");
}

#[test]
fn empty_message_and_nothing_staged_are_refused() {
    let repo = TestRepo::with_commit();
    let mut r = repo.open();
    assert!(matches!(
        commit_staged_files(&mut r, &sig(), "  \n ", Some("body")),
        Err(GitYasError::InvalidInput(_))
    ));
    repo.write("a.txt", "unstaged only");
    assert!(matches!(
        commit_staged_files(&mut r, &sig(), "msg", None),
        Err(GitYasError::InvalidInput(_))
    ));
    let fresh = TestRepo::init();
    assert!(matches!(
        commit_staged_files(&mut fresh.open(), &sig(), "msg", None),
        Err(GitYasError::InvalidInput(_))
    ));
}

#[test]
fn commit_on_detached_head_moves_head_only() {
    let repo = TestRepo::with_commit();
    let main = repo.rev("main");
    repo.git(&["checkout", "-q", "--detach"]);
    repo.write("a.txt", "x");
    repo.git(&["add", "a.txt"]);
    let id = commit_staged_files(&mut repo.open(), &sig(), "detached", None).unwrap();
    assert_eq!(oid(&repo.rev("HEAD")), id);
    assert_eq!(repo.rev("main"), main);
}

#[test]
fn merge_in_progress_commit_has_both_parents_and_cleans_state() {
    let repo = TestRepo::init();
    repo.write("c.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-qb", "other"]);
    repo.write("c.txt", "theirs\n");
    let theirs = repo.commit_all("theirs");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("c.txt", "ours\n");
    let ours = repo.commit_all("ours");
    assert!(!repo.git_try(&["merge", "other"]).status.success());
    let mut r = repo.open();
    // Conflicts present: refused.
    assert!(matches!(
        commit_staged_files(&mut r, &sig(), "merge", None),
        Err(GitYasError::InvalidInput(_))
    ));
    // Resolve to "ours": tree identical to HEAD, still a valid merge commit.
    repo.write("c.txt", "ours\n");
    repo.git(&["add", "c.txt"]);
    let mut r = repo.open();
    let id = commit_staged_files(&mut r, &sig(), "Merge other", None).unwrap();
    let parents: Vec<String> = repo
        .git(&["log", "-1", "--format=%P"])
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    assert_eq!(parents, vec![ours, theirs]);
    assert_eq!(oid(&repo.rev("HEAD")), id);
    assert_eq!(r.state(), git2::RepositoryState::Clean);
    assert!(!repo.path().join(".git/MERGE_HEAD").exists());
}

#[test]
fn commit_refused_during_rebase() {
    let repo = TestRepo::init();
    repo.write("c.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-qb", "topic"]);
    repo.write("c.txt", "topic\n");
    repo.commit_all("topic");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("c.txt", "main\n");
    repo.commit_all("main");
    repo.git(&["checkout", "-q", "topic"]);
    assert!(!repo.git_try(&["rebase", "main"]).status.success());
    repo.write("c.txt", "resolved\n");
    repo.git(&["add", "c.txt"]);
    let err = commit_staged_files(&mut repo.open(), &sig(), "msg", None).unwrap_err();
    assert!(matches!(err, GitYasError::InvalidInput(_)), "{err:?}");
}

#[test]
fn cherry_pick_in_progress_commit_cleans_state() {
    let repo = TestRepo::init();
    repo.write("c.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-qb", "other"]);
    repo.write("c.txt", "other\n");
    let pick = repo.commit_all("other");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("c.txt", "main\n");
    repo.commit_all("main");
    assert!(!repo.git_try(&["cherry-pick", &pick]).status.success());
    repo.write("c.txt", "resolved\n");
    repo.git(&["add", "c.txt"]);
    let mut r = repo.open();
    assert_eq!(r.state(), git2::RepositoryState::CherryPick);
    commit_staged_files(&mut r, &sig(), "picked", None).unwrap();
    assert_eq!(r.state(), git2::RepositoryState::Clean);
}

// ---------------------------------------------------------------- commit files list

fn files(repo: &TestRepo, rev: &str) -> Vec<Delta> {
    get_commit_files_list(&repo.open(), &oid(&repo.rev(rev))).unwrap()
}

#[test]
fn files_of_root_commit_are_all_added_and_sorted() {
    let repo = TestRepo::init();
    repo.write("b.txt", "b");
    repo.write("a/z.txt", "z");
    repo.commit_all("root");
    assert_eq!(
        files(&repo, "HEAD"),
        vec![delta("a/z.txt", DeltaChange::Added), delta("b.txt", DeltaChange::Added)]
    );
}

#[test]
fn files_detect_renames_and_all_kinds() {
    let repo = TestRepo::init();
    repo.write("old.txt", "some reasonably long content\nthat is kept\nacross the rename\n");
    repo.write("mod.txt", "1\n");
    repo.write("del.txt", "d\n");
    repo.write("link", "target");
    repo.commit_all("base");
    std::fs::rename(repo.path().join("old.txt"), repo.path().join("new.txt")).unwrap();
    repo.write("mod.txt", "2\n");
    repo.remove("del.txt");
    repo.write("add.txt", "a\n");
    #[cfg(unix)]
    {
        repo.remove("link");
        std::os::unix::fs::symlink("target", repo.path().join("link")).unwrap();
    }
    repo.commit_all("changes");
    #[cfg_attr(not(unix), allow(unused_mut))] // the symlink typechange row is Unix-only
    let mut expected = vec![
        delta("add.txt", DeltaChange::Added),
        delta("del.txt", DeltaChange::Deleted),
        delta("mod.txt", DeltaChange::Modified),
        Delta {
            path: PathBuf::from("new.txt"),
            old_path: Some(PathBuf::from("old.txt")),
            change: DeltaChange::Renamed,
        },
    ];
    #[cfg(unix)]
    expected.insert(2, delta("link", DeltaChange::Typechange));
    assert_eq!(files(&repo, "HEAD"), expected);
}

#[test]
fn files_of_merge_commit_are_relative_to_first_parent() {
    let repo = TestRepo::with_commit();
    repo.git(&["checkout", "-qb", "side"]);
    repo.write("side.txt", "s");
    repo.commit_all("side");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("main.txt", "m");
    repo.commit_all("main");
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    assert_eq!(files(&repo, "HEAD"), vec![delta("side.txt", DeltaChange::Added)]);
    let cli = repo.git(&["diff", "--name-only", "HEAD^1", "HEAD"]);
    assert_eq!(cli.trim(), "side.txt");
}

#[test]
fn files_of_unknown_commit_is_an_error() {
    let repo = TestRepo::with_commit();
    let missing = Oid::from_bytes(&[7; 20]).unwrap();
    assert!(get_commit_files_list(&repo.open(), &missing).is_err());
}

// ---------------------------------------------------------------- commit body

#[test]
fn commit_body_returns_full_message() {
    let repo = TestRepo::with_commit();
    repo.write("a.txt", "x");
    repo.git(&["add", "a.txt"]);
    let id = commit_staged_files(&mut repo.open(), &sig(), "subject", Some("line 1\nline 2")).unwrap();
    let body = commit_body(&repo.open(), &id).unwrap();
    assert_eq!(body.message, "subject\n\nline 1\nline 2\n");
    assert!(!body.lossy);
    assert_eq!(body.encoding, None);
}

#[test]
fn commit_body_flags_non_utf8_and_summary_is_not_empty() {
    let repo = TestRepo::with_commit();
    let r = repo.open();
    let head = r.head().unwrap().peel_to_commit().unwrap();
    let tree = head.tree().unwrap();
    // Build a Latin-1 commit object by hand.
    let mut raw = Vec::new();
    raw.extend_from_slice(format!("tree {}\nparent {}\n", tree.id(), head.id()).as_bytes());
    raw.extend_from_slice(b"author Ren\xe9 <r@example.com> 1700000000 +0000\n");
    raw.extend_from_slice(b"committer Ren\xe9 <r@example.com> 1700000000 +0000\nencoding ISO-8859-1\n\nCaf\xe9 subject\n\nbody\n");
    let id = r.odb().unwrap().write(git2::ObjectType::Commit, &raw).unwrap();
    r.reference("refs/heads/latin1", id, true, "test").unwrap();
    let body = commit_body(&r, &Oid::from_bytes(id.as_bytes()).unwrap()).unwrap();
    assert!(body.lossy);
    assert_eq!(body.encoding.as_deref(), Some("ISO-8859-1"));
    assert!(body.message.starts_with("Caf\u{fffd} subject"));
    let live = get_commits_live(&r).unwrap();
    let row = live.iter().find(|c| c.id.as_bytes() == id.as_bytes()).unwrap();
    assert_eq!(row.summary, "Caf\u{fffd} subject");
    assert_eq!(row.author_name, "Ren\u{fffd}");
}

// ---- reword ---------------------------------------------------------------------------------------------------

#[test]
fn reword_head_keeps_tree_author_and_branch() {
    let repo = TestRepo::with_commit();
    let old = repo.rev("HEAD");
    let new = reword_commit(&repo.open(), &oid(&old), "  better subject\n\nwith body  \n\n").unwrap();
    assert_ne!(new.to_hex(), old);
    assert_eq!(repo.rev("main"), new.to_hex());
    assert_eq!(repo.git(&["log", "-1", "--format=%B"]), "better subject\n\nwith body\n\n");
    assert_eq!(repo.rev("HEAD^{tree}"), repo.rev(&format!("{old}^{{tree}}")));
    let who = "--format=%an %ae %at %cn %ce %ct";
    assert_eq!(repo.git(&["log", "-1", who, "HEAD"]), repo.git(&["log", "-1", who, &old]));
    assert!(repo.porcelain().is_empty());
}

#[test]
fn reword_ancestor_rewrites_descendants_including_merges() {
    let repo = TestRepo::with_commit();
    let first = repo.rev("HEAD");
    repo.write("b.txt", "b");
    repo.commit_all("second");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("c.txt", "c");
    repo.commit_all("side work");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("d.txt", "d");
    repo.commit_all("third");
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    let side = repo.rev("side");
    let tree = repo.rev("HEAD^{tree}");

    let second = repo.rev("HEAD~2");
    let new = reword_commit(&repo.open(), &oid(&second), "second, reworded").unwrap();
    assert_eq!(repo.rev("HEAD~2"), new.to_hex());
    assert_eq!(repo.rev("HEAD~3"), first, "commits below the reworded one are kept");
    assert_eq!(
        repo.git(&["log", "--format=%s", "--first-parent", "main"]),
        "merge side\nthird\nsecond, reworded\ninitial\n"
    );
    assert_eq!(repo.rev("HEAD^{tree}"), tree);
    // The merge's second parent descends from the reworded commit, so it is rewritten too; the old branch stays.
    assert_ne!(repo.rev("HEAD^2"), side);
    assert_eq!(repo.rev("HEAD^2~1"), new.to_hex());
    assert_eq!(repo.rev("side"), side);
}

#[test]
fn reword_refuses_commits_off_head_and_empty_messages() {
    let repo = TestRepo::with_commit();
    repo.git(&["checkout", "-q", "-b", "other"]);
    repo.write("b.txt", "b");
    let other = repo.commit_all("other");
    repo.git(&["checkout", "-q", "main"]);
    let head = repo.rev("HEAD");
    assert!(matches!(
        reword_commit(&repo.open(), &oid(&other), "x"),
        Err(GitYasError::InvalidInput(_))
    ));
    assert!(matches!(
        reword_commit(&repo.open(), &oid(&head), " \n "),
        Err(GitYasError::InvalidInput(_))
    ));
    assert_eq!(repo.rev("HEAD"), head);
    // An unchanged (already prettified) message rewrites nothing.
    assert_eq!(reword_commit(&repo.open(), &oid(&head), "initial").unwrap().to_hex(), head);
}

#[test]
fn reword_detached_head_moves_head_only() {
    let repo = TestRepo::with_commit();
    repo.write("b.txt", "b");
    let tip = repo.commit_all("second");
    repo.git(&["checkout", "-q", "--detach"]);
    let new = reword_commit(&repo.open(), &oid(&tip), "detached").unwrap();
    assert_eq!(repo.rev("HEAD"), new.to_hex());
    assert_eq!(repo.rev("main"), tip);
}

/// The WP4 merge path: libgit2 `Repository::merge` leaves MERGE_HEAD (possibly several entries) and conflicts in
/// the index; committing must refuse while conflicted, then use HEAD + every MERGE_HEAD and clean up the state.
#[test]
fn libgit2_merge_state_commit_uses_all_merge_heads() {
    let repo = TestRepo::init();
    repo.write("c.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-qb", "one"]);
    repo.write("one.txt", "1\n");
    repo.write("c.txt", "one\n");
    let one = repo.commit_all("one");
    repo.git(&["checkout", "-q", "-b", "two", "main"]);
    repo.write("two.txt", "2\n");
    let two = repo.commit_all("two");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("c.txt", "main\n");
    let main = repo.commit_all("main");

    let mut r = repo.open();
    {
        // libgit2 merges one branch at a time.
        let a = r.find_annotated_commit(git2::Oid::from_str(&one).unwrap()).unwrap();
        r.merge(&[&a], None, None).unwrap();
    }
    assert_eq!(r.state(), git2::RepositoryState::Merge);
    assert!(r.index().unwrap().has_conflicts());
    let err = commit_staged_files(&mut r, &sig(), "merge", None).unwrap_err();
    assert!(matches!(err, GitYasError::InvalidInput(ref m) if m.contains("conflict")), "{err:?}");
    assert_eq!(r.state(), git2::RepositoryState::Merge, "state kept after refusal");

    repo.write("c.txt", "resolved\n");
    repo.git(&["add", "c.txt"]);
    // A second MERGE_HEAD entry (as an octopus merge writes) must become a third parent.
    let merge_head = repo.path().join(".git/MERGE_HEAD");
    let mut heads = std::fs::read_to_string(&merge_head).unwrap();
    heads.push_str(&format!("{two}\n"));
    std::fs::write(&merge_head, heads).unwrap();
    repo.git(&["checkout", &two, "--", "two.txt"]);
    let mut r = repo.open();
    let id = commit_staged_files(&mut r, &sig(), "Merge one and two", None).unwrap();
    let parents: Vec<String> = repo
        .git(&["log", "-1", "--format=%P"])
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    assert_eq!(parents, vec![main, one, two]);
    assert_eq!(oid(&repo.rev("HEAD")), id);
    assert_eq!(r.state(), git2::RepositoryState::Clean);
    assert!(repo.porcelain().is_empty());
}

// ---- reset ----------------------------------------------------------------------------------------------------

#[test]
fn soft_reset_moves_branch_and_keeps_changes_staged() {
    let repo = TestRepo::with_commit();
    let first = repo.rev("HEAD");
    repo.write("b.txt", "b");
    repo.commit_all("second");
    repo.write("c.txt", "c");
    reset_to_commit(&repo.open(), &oid(&first), ResetMode::Soft).unwrap();
    assert_eq!(repo.rev("main"), first);
    assert_eq!(repo.read("c.txt"), b"c");
    let status = repo.porcelain();
    assert!(status.iter().any(|l| l.starts_with("A ") && l.ends_with("b.txt")), "{status:?}");
    assert!(status.iter().any(|l| l.starts_with("??") && l.ends_with("c.txt")), "{status:?}");
}

#[test]
fn hard_reset_discards_tracked_changes() {
    let repo = TestRepo::with_commit();
    let first = repo.rev("HEAD");
    repo.write("b.txt", "b");
    repo.commit_all("second");
    repo.write("b.txt", "edited");
    reset_to_commit(&repo.open(), &oid(&first), ResetMode::Hard).unwrap();
    assert_eq!(repo.rev("main"), first);
    assert!(!repo.path().join("b.txt").exists());
    assert!(repo.porcelain().is_empty());
}
