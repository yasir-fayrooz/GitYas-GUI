//! Conflict read / resolve on real `git merge` conflicts (git CLI used only to set up and to check).
#![allow(clippy::unwrap_used, clippy::expect_used)] // test-only helpers (clippy.toml covers #[test] fns only)

mod diff_support;

use std::path::Path;

use diff_support::{git, git_ok};
use gityas_core::GitYasError;
use gityas_core::diff::DiffKind;
use gityas_core::models::{DeletedMergeAction, MergeConflict};
use gityas_git::merge::{
    conflict_from_texts, conflict_sides, get_merge_conflict, mark_resolved, render_diff3, resolve_conflict, resolve_conflict_with_action,
};

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        // Git for Windows enables `core.autocrlf` in its system config, which libgit2 honours: pin it so checkouts are byte-exact.
        git_ok(dir.path(), &["config", "core.autocrlf", "false"]);
        Repo { dir }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn write(&self, rel: &str, content: impl AsRef<[u8]>) -> &Self {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
        self
    }
    fn rm(&self, rel: &str) -> &Self {
        self.git(&["rm", "-q", rel]);
        self
    }
    fn git(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&git_ok(self.path(), args)).into_owned()
    }
    fn commit(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
    }
    fn open(&self) -> git2::Repository {
        git2::Repository::open(self.path()).unwrap()
    }
    /// base commit → `theirs` branch edit, `main` edit, then `git merge theirs` (which must conflict).
    fn conflict(&self, base: impl Fn(&Repo), ours: impl Fn(&Repo), theirs: impl Fn(&Repo)) {
        self.write(".keep", "");
        base(self);
        self.commit("base");
        self.git(&["checkout", "-q", "-b", "theirs"]);
        theirs(self);
        self.commit("theirs");
        self.git(&["checkout", "-q", "main"]);
        ours(self);
        self.commit("ours");
        let out = git(self.path())
            .args(["-c", "merge.conflictStyle=diff3", "merge", "-q", "theirs"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "merge was expected to conflict");
        assert!(self.path().join(".git/MERGE_HEAD").exists());
    }
    fn status(&self) -> String {
        self.git(&["status", "--porcelain"])
    }
}

const BASE: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
const OURS: &str = "ONE\ntwo\nthree\nfour\nfive\nsix (ours)\nseven\n";
const THEIRS: &str = "one\ntwo\nthree\nfour\nfive\nsix (theirs)\nseven\nEIGHT\n";

fn text_conflict() -> Repo {
    let r = Repo::new();
    r.conflict(
        |r| {
            r.write("f.txt", BASE);
        },
        |r| {
            r.write("f.txt", OURS);
        },
        |r| {
            r.write("f.txt", THEIRS);
        },
    );
    r
}

#[test]
fn reads_text_conflict_with_real_base_and_exact_ranges() {
    let r = text_conflict();
    let c = get_merge_conflict(&r.open(), Path::new("f.txt")).unwrap();
    assert!(c.sides.is_text() && c.sides.ancestor.is_some());
    assert_eq!((c.ancestor.as_str(), c.ours.as_str(), c.theirs.as_str()), (BASE, OURS, THEIRS));
    assert!(!c.ours_deleted && !c.theirs_deleted && !c.auto_resolved);
    assert_eq!(c.chunks.len(), 1);
    let ch = &c.chunks[0];
    assert_eq!(
        (
            (ch.ours_start, ch.ours_end),
            (ch.theirs_start, ch.theirs_end),
            (ch.ancestor_start, ch.ancestor_end)
        ),
        ((5, 6), (5, 6), (5, 6))
    );
    // Non-conflicting changes from both sides are in the template; the conflict holds the base line.
    assert_eq!(c.merged, "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n");
    assert_eq!(ch.merged_range(), 5..6);
}

#[test]
fn engine_matches_what_git_merge_wrote_into_the_working_tree() {
    let r = text_conflict();
    let on_disk = std::fs::read_to_string(r.path().join("f.txt")).unwrap();
    let label = |prefix: &str| on_disk.lines().find_map(|l| l.strip_prefix(prefix)).unwrap().to_owned();
    let labels = [label("<<<<<<< "), label("||||||| "), label(">>>>>>> ")];
    let rendered = render_diff3(BASE, OURS, THEIRS, [&labels[0], &labels[1], &labels[2]]).unwrap();
    assert_eq!(rendered, on_disk);
}

#[test]
fn resolve_conflict_stages_content_and_keeps_merge_in_progress() {
    let r = text_conflict();
    resolve_conflict(&r.open(), Path::new("f.txt"), "resolved\n").unwrap();
    assert_eq!(std::fs::read_to_string(r.path().join("f.txt")).unwrap(), "resolved\n");
    assert_eq!(r.status(), "M  f.txt\n");
    assert!(!r.open().index().unwrap().has_conflicts());
    // MERGE_HEAD survives (the old code called cleanup_state() and lost the second parent).
    assert!(r.path().join(".git/MERGE_HEAD").exists());
    r.git(&["commit", "-q", "--no-edit"]);
    assert_eq!(
        r.git(&["rev-list", "--parents", "-n1", "HEAD"]).split_whitespace().count(),
        3,
        "merge commit has two parents"
    );
    // Resolving again: not conflicted any more.
    let res = resolve_conflict(&r.open(), Path::new("f.txt"), "x");
    assert!(matches!(res, Err(GitYasError::NotFound(_))), "{res:?}");
}

#[test]
fn add_add_conflict_has_empty_ancestor() {
    let r = Repo::new();
    r.conflict(
        |_| {},
        |r| {
            r.write("n.txt", "a\nours\n");
        },
        |r| {
            r.write("n.txt", "a\ntheirs\n");
        },
    );
    let sides = conflict_sides(&r.open(), Path::new("n.txt")).unwrap();
    assert!(sides.ancestor.is_none() && sides.ours.is_some() && sides.theirs.is_some());
    let c = get_merge_conflict(&r.open(), Path::new("n.txt")).unwrap();
    assert_eq!(c.ancestor, "");
    assert_eq!(c.chunks.len(), 1);
    assert_eq!(
        c,
        MergeConflict {
            sides: c.sides.clone(),
            ..conflict_from_texts("", "a\nours\n", "a\ntheirs\n").unwrap()
        }
    );
    // TakeBase of a file that had no base deletes it.
    resolve_conflict_with_action(&r.open(), Path::new("n.txt"), DeletedMergeAction::TakeBase).unwrap();
    assert!(!r.path().join("n.txt").exists());
    assert_eq!(r.status(), "D  n.txt\n", "deletion staged relative to HEAD");
}

#[test]
fn modify_delete_conflicts_set_flags_and_keep_the_surviving_text() {
    let r = Repo::new();
    r.conflict(
        |r| {
            r.write("d.txt", BASE);
        },
        |r| {
            r.rm("d.txt");
        },
        |r| {
            r.write("d.txt", THEIRS);
        },
    );
    let c = get_merge_conflict(&r.open(), Path::new("d.txt")).unwrap();
    assert!(c.ours_deleted && !c.theirs_deleted && !c.auto_resolved);
    assert_eq!((c.ancestor.as_str(), c.ours.as_str(), c.theirs.as_str()), (BASE, "", THEIRS));
    assert!(c.chunks.is_empty());

    let r2 = Repo::new();
    r2.conflict(
        |r| {
            r.write("d.txt", BASE);
        },
        |r| {
            r.write("d.txt", OURS);
        },
        |r| {
            r.rm("d.txt");
        },
    );
    let c = get_merge_conflict(&r2.open(), Path::new("d.txt")).unwrap();
    assert!(!c.ours_deleted && c.theirs_deleted);
    assert_eq!(c.ours, OURS);
}

#[test]
fn actions_take_ours_theirs_and_delete() {
    for (action, expect) in [
        (DeletedMergeAction::TakeOurs, None),
        (DeletedMergeAction::TakeTheirs, Some(THEIRS)),
        (DeletedMergeAction::TakeBase, Some(BASE)),
        (DeletedMergeAction::DeleteFile, None),
    ] {
        let r = Repo::new();
        r.conflict(
            |r| {
                r.write("d.txt", BASE);
            },
            |r| {
                r.rm("d.txt");
            },
            |r| {
                r.write("d.txt", THEIRS);
            },
        );
        resolve_conflict_with_action(&r.open(), Path::new("d.txt"), action).unwrap();
        let on_disk = std::fs::read_to_string(r.path().join("d.txt")).ok();
        assert_eq!(on_disk.as_deref(), expect, "{action:?}");
        assert!(!r.open().index().unwrap().has_conflicts());
        // Index matches the working tree: nothing unstaged.
        let status = r.status();
        assert!(status.lines().all(|l| l.as_bytes().get(1) == Some(&b' ')), "{action:?}: {status:?}");
        assert!(r.path().join(".git/MERGE_HEAD").exists());
    }
}

#[test]
fn binary_conflict_is_reported_and_take_theirs_is_byte_exact() {
    let r = Repo::new();
    let (base, ours, theirs) = (b"\x00base\xff".to_vec(), b"\x00ours\xff\xfe".to_vec(), b"\x00theirs\x80\n".to_vec());
    r.conflict(
        |r| {
            r.write("b.bin", &base);
        },
        |r| {
            r.write("b.bin", &ours);
        },
        |r| {
            r.write("b.bin", &theirs);
        },
    );
    let sides = conflict_sides(&r.open(), Path::new("b.bin")).unwrap();
    assert_eq!(sides.ours.as_ref().unwrap().kind, DiffKind::Binary);
    assert!(!sides.is_text());
    // Returned as data (no text, no chunks), so the UI can offer take-ours / take-theirs.
    let c = get_merge_conflict(&r.open(), Path::new("b.bin")).unwrap();
    assert_eq!(c.sides, sides);
    assert!(c.ours.is_empty() && c.theirs.is_empty() && c.merged.is_empty() && c.chunks.is_empty());
    assert!(!c.ours_deleted && !c.theirs_deleted && !c.auto_resolved);
    resolve_conflict_with_action(&r.open(), Path::new("b.bin"), DeletedMergeAction::TakeTheirs).unwrap();
    assert_eq!(std::fs::read(r.path().join("b.bin")).unwrap(), theirs);
    assert_eq!(r.status(), "M  b.bin\n");
}

#[test]
fn non_utf8_conflict_is_reported() {
    let r = Repo::new();
    r.conflict(
        |r| {
            r.write("l.txt", b"caf\xe9\n");
        },
        |r| {
            r.write("l.txt", b"caf\xe9 ours\n");
        },
        |r| {
            r.write("l.txt", b"caf\xe9 theirs\n");
        },
    );
    let sides = conflict_sides(&r.open(), Path::new("l.txt")).unwrap();
    assert_eq!(sides.ours.unwrap().kind, DiffKind::NonUtf8);
    let c = get_merge_conflict(&r.open(), Path::new("l.txt")).unwrap();
    assert!(!c.sides.is_text());
    assert!(c.ancestor.is_empty() && c.ours.is_empty() && c.theirs.is_empty() && c.chunks.is_empty());
}

#[test]
fn rejects_unknown_and_escaping_paths() {
    let r = text_conflict();
    let repo = r.open();
    assert!(matches!(
        get_merge_conflict(&repo, Path::new("nope.txt")),
        Err(GitYasError::NotFound(_))
    ));
    for bad in ["../f.txt", "/etc/passwd", "", "a/../f.txt"] {
        assert!(
            matches!(resolve_conflict(&repo, Path::new(bad), "x"), Err(GitYasError::InvalidInput(_))),
            "{bad}"
        );
        assert!(
            matches!(get_merge_conflict(&repo, Path::new(bad)), Err(GitYasError::InvalidInput(_))),
            "{bad}"
        );
    }
}

#[cfg(unix)]
#[test]
fn resolve_never_writes_through_a_symlink_and_keeps_exec_bit() {
    use std::os::unix::fs::PermissionsExt;
    let r = text_conflict();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), "secret\n").unwrap();
    std::fs::remove_file(r.path().join("f.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path(), r.path().join("f.txt")).unwrap();
    resolve_conflict(&r.open(), Path::new("f.txt"), "resolved\n").unwrap();
    assert_eq!(std::fs::read_to_string(outside.path()).unwrap(), "secret\n");
    assert!(!std::fs::symlink_metadata(r.path().join("f.txt")).unwrap().file_type().is_symlink());

    // Executable file deleted from the working tree before resolving: recreated with the exec bit.
    let r = Repo::new();
    let exec = |r: &Repo, content: &str| {
        r.write("run.sh", content);
        std::fs::set_permissions(r.path().join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    r.conflict(|r| exec(r, BASE), |r| exec(r, OURS), |r| exec(r, THEIRS));
    std::fs::remove_file(r.path().join("run.sh")).unwrap();
    resolve_conflict(&r.open(), Path::new("run.sh"), "echo ok\n").unwrap();
    let mode = std::fs::metadata(r.path().join("run.sh")).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111);
    let staged = r.git(&["ls-files", "-s", "run.sh"]);
    assert!(staged.starts_with("100755"), "{staged}");
}

#[test]
fn resolving_one_of_two_conflicts_leaves_the_other() {
    let r = Repo::new();
    r.conflict(
        |r| {
            r.write("a.txt", BASE).write("b.txt", BASE);
        },
        |r| {
            r.write("a.txt", OURS).write("b.txt", OURS);
        },
        |r| {
            r.write("a.txt", THEIRS).write("b.txt", THEIRS);
        },
    );
    resolve_conflict(&r.open(), Path::new("a.txt"), "a\n").unwrap();
    assert!(r.open().index().unwrap().has_conflicts());
    assert!(get_merge_conflict(&r.open(), Path::new("b.txt")).is_ok());
    assert!(matches!(
        get_merge_conflict(&r.open(), Path::new("a.txt")),
        Err(GitYasError::NotFound(_))
    ));
}

#[test]
fn resolve_in_a_linked_worktree_writes_into_that_worktree() {
    // The old code wrote to `repo.path().parent()`, which for a linked worktree is `.git/worktrees/`.
    let r = Repo::new();
    r.write("f.txt", BASE);
    r.commit("base");
    r.git(&["branch", "theirs"]);
    let wt_dir = tempfile::tempdir().unwrap();
    let wt = wt_dir.path().join("linked");
    r.git(&["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]);
    r.git(&["checkout", "-q", "theirs"]);
    r.write("f.txt", THEIRS);
    r.commit("theirs");
    std::fs::write(wt.join("f.txt"), OURS).unwrap();
    git_ok(&wt, &["commit", "-q", "-am", "ours"]);
    let out = git(&wt).args(["merge", "-q", "theirs"]).output().unwrap();
    assert!(!out.status.success());
    let repo = git2::Repository::open(&wt).unwrap();
    assert_eq!(get_merge_conflict(&repo, Path::new("f.txt")).unwrap().chunks.len(), 1);
    resolve_conflict(&repo, Path::new("f.txt"), "done\n").unwrap();
    assert_eq!(std::fs::read_to_string(wt.join("f.txt")).unwrap(), "done\n");
    assert_eq!(std::fs::read_to_string(r.path().join("f.txt")).unwrap(), THEIRS);
    assert_eq!(String::from_utf8(git_ok(&wt, &["status", "--porcelain"])).unwrap(), "M  f.txt\n");
}

#[test]
fn mark_resolved_stages_working_tree_content_and_deletions() {
    let r = Repo::new();
    r.conflict(
        |r| {
            r.write("a.txt", BASE).write("b.txt", BASE).write("d.txt", BASE);
        },
        |r| {
            r.write("a.txt", OURS).write("b.txt", OURS).write("d.txt", OURS);
        },
        |r| {
            r.write("a.txt", THEIRS).write("b.txt", THEIRS).rm("d.txt");
        },
    );
    // a.txt keeps git's conflict markers; d.txt (modify/delete) is deleted by hand.
    let with_markers = std::fs::read_to_string(r.path().join("a.txt")).unwrap();
    std::fs::remove_file(r.path().join("d.txt")).unwrap();
    mark_resolved(&r.open(), &["a.txt".into(), "d.txt".into()]).unwrap();

    assert_eq!(r.git(&["show", ":a.txt"]), with_markers);
    assert!(r.open().index().unwrap().get_path(Path::new("d.txt"), 0).is_none());
    // b.txt is still conflicted and the merge is still in progress.
    assert!(get_merge_conflict(&r.open(), Path::new("b.txt")).is_ok());
    assert!(r.path().join(".git/MERGE_HEAD").exists());
    let status = r.status();
    assert!(
        status.contains("M  a.txt") && status.contains("D  d.txt") && status.contains("UU b.txt"),
        "{status:?}"
    );
}

#[test]
fn mark_resolved_checks_every_path_before_staging_any() {
    let r = text_conflict();
    for bad in ["nope.txt", "../x", ".keep"] {
        let err = mark_resolved(&r.open(), &["f.txt".into(), bad.into()]).unwrap_err();
        assert!(
            matches!(err, GitYasError::NotFound(_) | GitYasError::InvalidInput(_)),
            "{bad}: {err:?}"
        );
    }
    assert!(get_merge_conflict(&r.open(), Path::new("f.txt")).is_ok());
}
