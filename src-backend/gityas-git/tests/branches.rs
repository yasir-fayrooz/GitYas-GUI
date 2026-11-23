//! Branch listing, create / delete / checkout, create remote branch.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test crate: panics are the assertions

mod repo_support;

use gityas_core::GitYasError;
use gityas_core::models::BranchKind;
use gityas_git::branches::{self, CheckoutOutcome};
use gityas_git::remote::{NoCredentials, RemoteContext};
use repo_support::*;
use tempfile::TempDir;

fn ctx(creds: &NoCredentials) -> RemoteContext<'_> {
    RemoteContext::new(creds).with_credential_helper(false)
}

#[test]
fn unborn_repo_has_no_branches_and_cannot_branch() {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    assert!(branches::list_branches(&repo, None).unwrap().is_empty());
    assert!(matches!(
        branches::create_branch(&repo, "x", None, true),
        Err(GitYasError::InvalidInput(_))
    ));
}

#[test]
fn list_with_upstream_ahead_behind_and_hidden_remote_head() {
    let fx = Fixture::new();
    // Remote moves ahead by 1, local ahead by 2.
    let other = fx.other_clone("other");
    commit_file(&other, "o.txt", "o\n", "other");
    raw_push(&other, "refs/heads/main:refs/heads/main");
    commit_file(&fx.work, "l1", "1\n", "l1");
    commit_file(&fx.work, "l2", "2\n", "l2");
    fx.work.find_remote("origin").unwrap().fetch::<&str>(&[], None, None).unwrap();
    // Symbolic origin/HEAD must be hidden and not panic (the old code panicked on it).
    fx.work
        .reference_symbolic("refs/remotes/origin/HEAD", "refs/remotes/origin/main", true, "test")
        .unwrap();

    let all = branches::list_branches(&fx.work, None).unwrap();
    let main = all.iter().find(|b| b.refname == "refs/heads/main").unwrap();
    assert_eq!((main.ahead, main.behind), (2, 1));
    assert_eq!(main.upstream.as_deref(), Some("origin/main"));
    assert_eq!(main.remote_name.as_deref(), Some("origin"));
    assert!(main.is_head);
    assert_eq!(main.kind, BranchKind::Local);
    assert!(all.iter().all(|b| !b.name.ends_with("/HEAD")), "{all:?}");
    let remote = all.iter().find(|b| b.refname == "refs/remotes/origin/main").unwrap();
    assert_eq!(remote.kind, BranchKind::Remote);
    assert_eq!(remote.remote_name.as_deref(), Some("origin"));
    assert_eq!(remote.name, "origin/main");

    // Oracle: `git rev-list --left-right --count`.
    let counts = git(
        fx.work.workdir().unwrap(),
        &["rev-list", "--left-right", "--count", "main...origin/main"],
    );
    assert_eq!(counts.split_whitespace().collect::<Vec<_>>(), ["2", "1"]);

    let local_only = branches::list_branches(&fx.work, Some(BranchKind::Local)).unwrap();
    assert!(local_only.iter().all(|b| b.kind == BranchKind::Local));
}

#[test]
fn remote_name_with_slash_is_resolved_from_refspecs() {
    let fx = Fixture::new();
    fx.work.remote("team/fork", &file_url(&fx.remote_path)).unwrap();
    fx.work.find_remote("team/fork").unwrap().fetch::<&str>(&[], None, None).unwrap();
    fx.work
        .reference_symbolic("refs/remotes/team/fork/HEAD", "refs/remotes/team/fork/main", true, "t")
        .unwrap();
    let all = branches::list_branches(&fx.work, Some(BranchKind::Remote)).unwrap();
    let fork = all.iter().find(|b| b.refname == "refs/remotes/team/fork/main").unwrap();
    assert_eq!(fork.remote_name.as_deref(), Some("team/fork"));
    assert!(all.iter().all(|b| !b.refname.ends_with("/HEAD")));

    // Checking it out creates local `main`? No: `main` exists and tracks origin/main -> clear error.
    let err = branches::checkout_branch(&fx.work, "refs/remotes/team/fork/main").unwrap_err();
    assert!(
        matches!(err, GitYasError::InvalidInput(ref m) if m.contains("does not track")),
        "{err:?}"
    );
    // A new branch on the fork is checked out as local `feature` tracking `team/fork/feature`.
    let c = head_oid(&fx.work);
    fx.work.reference("refs/remotes/team/fork/feature", c, false, "t").unwrap();
    let out = branches::checkout_branch(&fx.work, "refs/remotes/team/fork/feature").unwrap();
    assert_eq!(
        out,
        CheckoutOutcome::Tracking {
            refname: "refs/heads/feature".into(),
            upstream: "team/fork/feature".into(),
            created: true
        }
    );
}

#[test]
fn gone_upstream_is_flagged() {
    let fx = Fixture::new();
    let main = |repo: &git2::Repository| {
        branches::list_branches(repo, Some(BranchKind::Local))
            .unwrap()
            .into_iter()
            .find(|b| b.name == "main")
            .unwrap()
    };
    let live = main(&fx.work);
    assert_eq!(live.upstream.as_deref(), Some("origin/main"));
    assert_eq!(live.upstream_refname.as_deref(), Some("refs/remotes/origin/main"));
    assert!(!live.upstream_gone);

    let mut r = fx.work.find_reference("refs/remotes/origin/main").unwrap();
    r.delete().unwrap();
    let gone = main(&fx.work);
    assert!(gone.upstream_gone);
    assert_eq!(gone.upstream.as_deref(), Some("origin/main"));
    assert_eq!(gone.upstream_refname.as_deref(), Some("refs/remotes/origin/main"));
    assert_eq!(gone.remote_name.as_deref(), Some("origin"));
    assert_eq!((gone.ahead, gone.behind), (0, 0));
    assert!(!gone.is_up_to_date());
}

#[cfg(unix)]
#[test]
fn non_utf8_branch_names_are_skipped_not_fatal() {
    let fx = Fixture::new();
    let c = head_oid(&fx.work);
    let mut packed = Vec::new();
    packed.extend_from_slice(b"# pack-refs with: peeled fully-peeled sorted \n");
    packed.extend_from_slice(c.to_string().as_bytes());
    packed.extend_from_slice(b" refs/heads/bad-\xff-name\n");
    std::fs::write(fx.work.path().join("packed-refs"), packed).unwrap();
    let all = branches::list_branches(&fx.work, None).unwrap();
    assert!(all.iter().any(|b| b.name == "main"));
    assert!(all.iter().all(|b| !b.name.contains("bad")));
    // Reference listing too.
    let refs = gityas_git::refs::list_references(&fx.work).unwrap();
    assert!(refs.iter().any(|r| r.name == "refs/heads/main"));
}

#[test]
fn create_branch_variants() {
    let fx = Fixture::new();
    let c1 = head_oid(&fx.work);
    commit_file(&fx.work, "a.txt", "two\n", "c2");
    write(&fx.work, "a.txt", "dirty\n");

    // Checkout at HEAD keeps local changes.
    assert_eq!(branches::create_branch(&fx.work, "feat", None, true).unwrap(), "refs/heads/feat");
    assert_eq!(fx.work.head().unwrap().name(), Some("refs/heads/feat"));
    assert_eq!(read(&fx.work, "a.txt"), "dirty\n");

    // Duplicate / invalid names.
    assert!(matches!(
        branches::create_branch(&fx.work, "feat", None, false),
        Err(GitYasError::AlreadyExists(_))
    ));
    assert!(matches!(
        branches::create_branch(&fx.work, "a..b", None, false),
        Err(GitYasError::InvalidInput(_))
    ));
    assert!(matches!(
        branches::create_branch(&fx.work, "nul\0", None, false),
        Err(GitYasError::InvalidInput(_))
    ));

    // At an older commit, with a dirty file that differs there: refused and rolled back.
    let err = branches::create_branch(&fx.work, "old", Some(&oid(c1)), true).unwrap_err();
    assert!(
        matches!(&err, GitYasError::Conflict(paths) if paths == &[std::path::PathBuf::from("a.txt")]),
        "{err:?}"
    );
    assert!(
        fx.work.find_branch("old", git2::BranchType::Local).is_err(),
        "branch not rolled back"
    );
    assert_eq!(read(&fx.work, "a.txt"), "dirty\n");

    // Without checkout it is fine.
    branches::create_branch(&fx.work, "old", Some(&oid(c1)), false).unwrap();
    assert_eq!(fx.work.find_reference("refs/heads/old").unwrap().target(), Some(c1));
}

/// A directory the target branch drops is open in another process (a shell's current directory, an editor): like
/// git, the checkout leaves the emptied directory behind instead of failing with "could not rmdir".
#[cfg(windows)]
#[test]
fn checkout_leaves_a_locked_directory_behind() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    commit_file(&repo, "a.txt", "a\n", "base");
    commit_file(&repo, "kept/keep.txt", "v1\n", "add keep");
    branches::create_branch(&repo, "feature", None, true).unwrap();
    commit_file(&repo, "locked/deeper/one.txt", "1\n", "add one");
    commit_file(&repo, "locked/deeper/sub/four.txt", "4\n", "add four");
    commit_file(&repo, "locked/two.txt", "2\n", "add two");
    commit_file(&repo, "other/three.txt", "3\n", "add three");
    commit_file(&repo, "kept/gone.txt", "g\n", "add gone");
    commit_file(&repo, "kept/keep.txt", "v2\n", "edit keep");
    commit_file(&repo, "z.txt", "z\n", "add z");
    write(&repo, "untracked.txt", "u\n");

    // Opened without FILE_SHARE_DELETE, as cmd.exe holds its current directory. `kept` does not become empty, but
    // Windows refuses to remove an open directory before it checks whether it is empty.
    let open = |dir: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(tmp.path().join(dir))
            .unwrap()
    };
    let _handles = [open("locked/deeper"), open("kept")];
    branches::checkout_branch(&repo, "refs/heads/main").unwrap();
    assert_eq!(repo.head().unwrap().name(), Some("refs/heads/main"));
    assert!(tmp.path().join("locked/deeper").is_dir());
    for gone in [
        "locked/deeper/one.txt",
        "locked/deeper/sub",
        "locked/two.txt",
        "other",
        "kept/gone.txt",
        "z.txt",
    ] {
        assert!(!tmp.path().join(gone).exists(), "{gone}");
    }
    assert_eq!(read(&repo, "kept/keep.txt"), "v1\n");
    assert_eq!(read(&repo, "untracked.txt"), "u\n");
    assert_eq!(git(tmp.path(), &["status", "--porcelain"]), "?? untracked.txt\n");
}

#[test]
fn checkout_branch_and_commit_dirty_tree_rules() {
    let fx = Fixture::new();
    let c1 = head_oid(&fx.work);
    branches::create_branch(&fx.work, "other", None, false).unwrap();
    commit_file(&fx.work, "a.txt", "two\n", "c2");
    // Unrelated local change is carried over.
    write(&fx.work, "new-untracked.txt", "u\n");
    assert_eq!(
        branches::checkout_branch(&fx.work, "refs/heads/other").unwrap(),
        CheckoutOutcome::Switched {
            refname: "refs/heads/other".into()
        }
    );
    assert_eq!(read(&fx.work, "a.txt"), "one\n");
    assert_eq!(read(&fx.work, "new-untracked.txt"), "u\n");

    // A change to a file that differs between the commits: refused, nothing touched.
    write(&fx.work, "a.txt", "mine\n");
    let err = branches::checkout_branch(&fx.work, "refs/heads/main").unwrap_err();
    assert!(
        matches!(&err, GitYasError::Conflict(paths) if paths == &[std::path::PathBuf::from("a.txt")]),
        "{err:?}"
    );
    assert_eq!(read(&fx.work, "a.txt"), "mine\n");
    assert_eq!(fx.work.head().unwrap().name(), Some("refs/heads/other"));

    // Same rule for detached checkout.
    let main_tip = fx.work.refname_to_id("refs/heads/main").unwrap();
    assert!(branches::checkout_commit(&fx.work, &oid(main_tip)).is_err());
    write(&fx.work, "a.txt", "one\n");
    branches::checkout_commit(&fx.work, &oid(main_tip)).unwrap();
    assert!(fx.work.head_detached().unwrap());
    assert_eq!(head_oid(&fx.work), main_tip);
    assert_eq!(read(&fx.work, "a.txt"), "two\n");
    // Oracle: CLI agrees nothing is modified (only the untracked file).
    assert_eq!(
        git(fx.work.workdir().unwrap(), &["status", "--porcelain"]),
        "?? new-untracked.txt\n"
    );

    // Back from detached HEAD to a branch.
    branches::checkout_branch(&fx.work, "refs/heads/main").unwrap();
    assert!(!fx.work.head_detached().unwrap());
    let _ = c1;
    assert!(matches!(
        branches::checkout_branch(&fx.work, "refs/heads/nope"),
        Err(GitYasError::NotFound(_))
    ));
    assert!(matches!(
        branches::checkout_commit(
            &fx.work,
            &gityas_core::Oid::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap()
        ),
        Err(GitYasError::NotFound(_))
    ));
}

#[test]
fn checkout_refused_during_merge_and_in_bare_repo() {
    let fx = Fixture::new();
    branches::create_branch(&fx.work, "x", None, false).unwrap();
    std::fs::write(fx.work.path().join("MERGE_HEAD"), format!("{}\n", head_oid(&fx.work))).unwrap();
    assert!(matches!(
        branches::checkout_branch(&fx.work, "refs/heads/x"),
        Err(GitYasError::InvalidInput(m)) if m.contains("merge")
    ));
    let bare = fx.remote();
    assert!(matches!(
        branches::checkout_branch(&bare, "refs/heads/main"),
        Err(GitYasError::InvalidInput(m)) if m.contains("bare")
    ));
}

#[test]
fn checkout_remote_branch_creates_or_reuses_tracking_branch() {
    let fx = Fixture::new();
    let other = fx.other_clone("other");
    commit_file(&other, "f.txt", "f\n", "feature work");
    raw_push(&other, "refs/heads/main:refs/heads/feature/x");
    fx.work.find_remote("origin").unwrap().fetch::<&str>(&[], None, None).unwrap();

    let out = branches::checkout_branch(&fx.work, "refs/remotes/origin/feature/x").unwrap();
    assert_eq!(
        out,
        CheckoutOutcome::Tracking {
            refname: "refs/heads/feature/x".into(),
            upstream: "origin/feature/x".into(),
            created: true
        }
    );
    assert_eq!(read(&fx.work, "f.txt"), "f\n");
    assert_eq!(
        git(fx.work.workdir().unwrap(), &["rev-parse", "--abbrev-ref", "feature/x@{upstream}"]).trim(),
        "origin/feature/x"
    );

    // Second time: the existing tracking branch is reused as is, even if it is behind the remote
    // (the old code checked out the remote tree while HEAD stayed on the stale local commit).
    branches::checkout_branch(&fx.work, "refs/heads/main").unwrap();
    commit_file(&other, "f.txt", "f2\n", "more");
    raw_push(&other, "refs/heads/main:refs/heads/feature/x");
    fx.work.find_remote("origin").unwrap().fetch::<&str>(&[], None, None).unwrap();
    let out = branches::checkout_branch(&fx.work, "refs/remotes/origin/feature/x").unwrap();
    assert!(matches!(out, CheckoutOutcome::Tracking { created: false, .. }));
    assert_eq!(read(&fx.work, "f.txt"), "f\n");
    assert_eq!(git(fx.work.workdir().unwrap(), &["status", "--porcelain"]), "");

    // Symbolic origin/HEAD resolves to origin/main, whose local `main` already tracks it.
    fx.work
        .reference_symbolic("refs/remotes/origin/HEAD", "refs/remotes/origin/main", true, "t")
        .unwrap();
    let out = branches::checkout_branch(&fx.work, "refs/remotes/origin/HEAD").unwrap();
    assert!(matches!(out, CheckoutOutcome::Tracking { ref refname, created: false, .. } if refname == "refs/heads/main"));
}

#[test]
fn delete_local_branch_safe_and_force() {
    let fx = Fixture::new();
    branches::create_branch(&fx.work, "merged", None, false).unwrap();
    branches::create_branch(&fx.work, "wip", None, true).unwrap();
    commit_file(&fx.work, "w", "w\n", "unmerged work");
    branches::checkout_branch(&fx.work, "refs/heads/main").unwrap();

    branches::delete_local_branch(&fx.work, "refs/heads/merged", false).unwrap();
    assert!(matches!(
        branches::delete_local_branch(&fx.work, "refs/heads/wip", false),
        Err(GitYasError::NotMerged(name)) if name == "wip"
    ));
    branches::delete_local_branch(&fx.work, "refs/heads/wip", true).unwrap();
    assert!(fx.work.find_branch("wip", git2::BranchType::Local).is_err());
    assert!(matches!(
        branches::delete_local_branch(&fx.work, "refs/heads/main", true),
        Err(GitYasError::InvalidInput(m)) if m.contains("checked-out")
    ));
    assert!(matches!(
        branches::delete_local_branch(&fx.work, "refs/heads/nope", true),
        Err(GitYasError::NotFound(_))
    ));
    assert!(branches::delete_local_branch(&fx.work, "refs/remotes/origin/main", true).is_err());
}

#[test]
fn rename_local_branch_moves_head_and_upstream() {
    let fx = Fixture::new();
    branches::create_branch(&fx.work, "other", None, false).unwrap();

    // Checked-out branch with an upstream: HEAD follows and the upstream config moves with it.
    let renamed = branches::rename_local_branch(&fx.work, "refs/heads/main", "trunk").unwrap();
    assert_eq!(renamed, "refs/heads/trunk");
    assert_eq!(fx.work.head().unwrap().name(), Some("refs/heads/trunk"));
    assert!(fx.work.find_branch("main", git2::BranchType::Local).is_err());
    let trunk = branches::list_branches(&fx.work, Some(BranchKind::Local)).unwrap();
    let trunk = trunk.iter().find(|b| b.name == "trunk").unwrap();
    assert!(trunk.is_head);
    assert_eq!(trunk.upstream.as_deref(), Some("origin/main"));

    // Not checked out; same name is a no-op.
    assert_eq!(
        branches::rename_local_branch(&fx.work, "refs/heads/other", "feature/x").unwrap(),
        "refs/heads/feature/x"
    );
    assert_eq!(
        branches::rename_local_branch(&fx.work, "refs/heads/trunk", "trunk").unwrap(),
        "refs/heads/trunk"
    );

    assert!(matches!(
        branches::rename_local_branch(&fx.work, "refs/heads/feature/x", "trunk"),
        Err(GitYasError::AlreadyExists(_))
    ));
    assert!(matches!(
        branches::rename_local_branch(&fx.work, "refs/heads/trunk", "bad..name"),
        Err(GitYasError::InvalidInput(_))
    ));
    assert!(matches!(
        branches::rename_local_branch(&fx.work, "refs/heads/nope", "x"),
        Err(GitYasError::NotFound(_))
    ));
    assert!(branches::rename_local_branch(&fx.work, "refs/remotes/origin/main", "x").is_err());
}

#[test]
fn delete_branch_checked_out_in_linked_worktree_is_refused() {
    let fx = Fixture::new();
    branches::create_branch(&fx.work, "wt-branch", None, false).unwrap();
    let r = fx.work.find_reference("refs/heads/wt-branch").unwrap();
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(&r));
    fx.work.worktree("wt", &fx.root.join("wt"), Some(&opts)).unwrap();
    assert!(branches::delete_local_branch(&fx.work, "refs/heads/wt-branch", true).is_err());
}

#[test]
fn create_and_delete_remote_branch() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    branches::create_branch(&fx.work, "topic", None, true).unwrap();
    commit_file(&fx.work, "t", "t\n", "topic");
    let upstream = branches::create_remote_branch(&fx.work, "refs/heads/topic", "origin", None, &ctx(&creds)).unwrap();
    assert_eq!(upstream, "origin/topic");
    assert_eq!(fx.remote().refname_to_id("refs/heads/topic").unwrap(), head_oid(&fx.work));
    assert_eq!(
        git(fx.work.workdir().unwrap(), &["rev-parse", "--abbrev-ref", "topic@{upstream}"]).trim(),
        "origin/topic"
    );
    // Listing now shows the upstream with 0/0.
    let topic = branches::list_branches(&fx.work, Some(BranchKind::Local))
        .unwrap()
        .into_iter()
        .find(|b| b.name == "topic")
        .unwrap();
    assert_eq!(topic.upstream.as_deref(), Some("origin/topic"));
    // Already exists.
    assert!(matches!(
        branches::create_remote_branch(&fx.work, "refs/heads/topic", "origin", None, &ctx(&creds)),
        Err(GitYasError::InvalidInput(_))
    ));
    // Different remote name.
    branches::create_remote_branch(&fx.work, "refs/heads/topic", "origin", Some("renamed"), &ctx(&creds)).unwrap();
    assert!(fx.remote().find_reference("refs/heads/renamed").is_ok());
    // Unknown remote.
    assert!(matches!(
        branches::create_remote_branch(&fx.work, "refs/heads/topic", "nope", Some("z"), &ctx(&creds)),
        Err(GitYasError::NotFound(_))
    ));

    branches::delete_remote_branch(&fx.work, "refs/remotes/origin/renamed", &ctx(&creds)).unwrap();
    assert!(fx.remote().find_reference("refs/heads/renamed").is_err());
    assert!(fx.work.find_reference("refs/remotes/origin/renamed").is_err());
    assert!(branches::delete_remote_branch(&fx.work, "refs/heads/topic", &ctx(&creds)).is_err());
}

/// Replaces the tracked directory `link` with a directory junction to `target` (both workdir-relative), the way tool
/// installers link `.claude/skills/x -> .agents/skills/x`.
#[cfg(windows)]
fn junction(root: &std::path::Path, link: &str, target: &str) {
    std::fs::remove_dir_all(root.join(link)).unwrap();
    let win = |p: std::path::PathBuf| p.to_string_lossy().replace('/', r"\");
    let out = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(win(root.join(link)))
        .arg(win(root.join(target)))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

/// libgit2 saw each junction as a file standing where the trees have a directory and refused the checkout, naming
/// the first file under every junction; Git for Windows deletes the files through the junction.
#[cfg(windows)]
#[test]
fn checkout_deletes_tracked_files_behind_a_junction() {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    commit_file(&repo, "base.txt", "base\n", "base");
    git(tmp.path(), &["checkout", "-qb", "skills"]);
    for dir in ["real/x", "skills/x"] {
        write(&repo, &format!("{dir}/SKILL.md"), "skill\n");
        write(&repo, &format!("{dir}/references/a.md"), "a\n");
    }
    write(&repo, "skills/y/kept.md", "kept\n");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "skills"]);
    junction(tmp.path(), "skills/x", "real/x");
    assert!(gityas_git::status::get_status(&repo).unwrap().unstaged.is_empty());

    // A local edit to a file the checkout deletes still blocks it, naming the file.
    write(&repo, "real/x/SKILL.md", "edited\n");
    let err = branches::checkout_branch(&repo, "refs/heads/main").unwrap_err();
    assert!(err.to_string().contains("skills/x/SKILL.md"), "{err}");
    write(&repo, "real/x/SKILL.md", "skill\n");

    branches::checkout_branch(&repo, "refs/heads/main").unwrap();
    assert_eq!(repo.head().unwrap().name(), Some("refs/heads/main"));
    for gone in ["real", "skills"] {
        assert!(!tmp.path().join(gone).exists(), "{gone} left behind");
    }
    assert!(git(tmp.path(), &["status", "--porcelain"]).is_empty());

    // And back: a plain checkout recreates both trees as regular directories.
    branches::checkout_branch(&repo, "refs/heads/skills").unwrap();
    assert_eq!(read(&repo, "skills/x/references/a.md"), "a\n");
    assert!(git(tmp.path(), &["status", "--porcelain"]).is_empty());
}

/// Files behind a junction that the target leaves unchanged are not touched, and the junction survives.
#[cfg(windows)]
#[test]
fn checkout_keeps_unchanged_files_behind_a_junction() {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    write(&repo, "real/x/SKILL.md", "skill\n");
    write(&repo, "skills/x/SKILL.md", "skill\n");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "skills"]);
    git(tmp.path(), &["checkout", "-qb", "other"]);
    commit_file(&repo, "other.txt", "other\n", "other");
    junction(tmp.path(), "skills/x", "real/x");

    branches::checkout_branch(&repo, "refs/heads/main").unwrap();
    assert_eq!(repo.head().unwrap().name(), Some("refs/heads/main"));
    assert!(!tmp.path().join("other.txt").exists());
    assert!(
        std::fs::symlink_metadata(tmp.path().join("skills/x"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(git(tmp.path(), &["status", "--porcelain"]).is_empty());
}
