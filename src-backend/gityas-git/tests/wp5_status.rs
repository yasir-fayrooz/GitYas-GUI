//! WP5 status: every delta kind maps (bug-log #6), renames, untracked recursion, conflicts once, submodules,
//! ignored excluded, deterministic order; `git status --porcelain` as the oracle.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns too

mod wp5_support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use gityas_core::GitYasError;
use gityas_core::models::{Delta, DeltaChange, FileState};
use gityas_git::status::{delta_change, get_status};
use wp5_support::{TestRepo, delta, renamed};

fn code(change: DeltaChange) -> char {
    match change {
        DeltaChange::Added => 'A',
        DeltaChange::Deleted => 'D',
        DeltaChange::Modified => 'M',
        DeltaChange::Renamed => 'R',
        DeltaChange::Copied => 'C',
        DeltaChange::Typechange => 'T',
        DeltaChange::Conflicted => 'U',
        DeltaChange::Untracked => '?',
        DeltaChange::Ignored => '!',
        DeltaChange::Unreadable => 'X',
    }
}

/// Renders our `FileState` like `git status --porcelain=v1` (see `TestRepo::porcelain`).
fn as_porcelain(state: &FileState) -> Vec<String> {
    let mut rows: BTreeMap<PathBuf, (char, char, Option<PathBuf>)> = BTreeMap::new();
    for d in &state.staged {
        let e = rows.entry(d.path.clone()).or_insert((' ', ' ', None));
        e.0 = code(d.change);
        e.2 = d.old_path.clone();
    }
    for d in &state.unstaged {
        let e = rows.entry(d.path.clone()).or_insert((' ', ' ', None));
        match d.change {
            DeltaChange::Untracked => *e = ('?', '?', None),
            DeltaChange::Conflicted => *e = ('U', 'U', None),
            other => e.1 = code(other),
        }
    }
    let mut out: Vec<String> = rows
        .into_iter()
        .map(|(path, (x, y, old))| match old {
            Some(old) => format!("{x}{y} {} <- {}", path.display(), old.display()),
            None => format!("{x}{y} {}", path.display()),
        })
        .collect();
    out.sort();
    out
}

#[test]
fn status_matches_git_porcelain_for_mixed_changes() {
    let repo = TestRepo::init();
    for f in ["mod.txt", "del.txt", "staged_del.txt", "both.txt", "old_name.txt"] {
        repo.write(f, format!("content of {f}\nwith a few lines\nso renames are detected\n"));
    }
    repo.write(".gitignore", "*.log\nbuild/\n");
    repo.commit_all("base");
    repo.write("mod.txt", "changed\n");
    repo.remove("del.txt");
    repo.git(&["rm", "-q", "staged_del.txt"]);
    repo.write("both.txt", "staged\n");
    repo.git(&["add", "both.txt"]);
    repo.write("both.txt", "staged then unstaged\n");
    repo.git(&["mv", "old_name.txt", "new_name.txt"]);
    repo.write("added.txt", "added\n");
    repo.git(&["add", "added.txt"]);
    repo.write("untracked/deep/dir/file.txt", "u\n");
    repo.write("top_untracked.txt", "u\n");
    repo.write("ignored.log", "ignored\n");
    repo.write("build/out.bin", "ignored\n");

    let state = get_status(&repo.open()).unwrap();
    assert_eq!(as_porcelain(&state), repo.porcelain());
    assert!(state.unstaged.iter().all(|d| d.change != DeltaChange::Ignored), "ignored excluded");
    assert!(
        state
            .unstaged
            .contains(&delta("untracked/deep/dir/file.txt", DeltaChange::Untracked)),
        "dirs recursed"
    );
    assert!(state.staged.contains(&renamed("old_name.txt", "new_name.txt")));
}

#[test]
fn status_is_sorted_by_path() {
    let repo = TestRepo::with_commit();
    for f in ["z.txt", "B.txt", "a/b.txt", "a.txt2", "_x"] {
        repo.write(f, "x");
    }
    let state = get_status(&repo.open()).unwrap();
    let paths: Vec<&PathBuf> = state.unstaged.iter().map(|d| &d.path).collect();
    let mut sorted = paths.clone();
    sorted.sort_by(|a, b| a.as_os_str().as_encoded_bytes().cmp(b.as_os_str().as_encoded_bytes()));
    assert_eq!(paths, sorted);
}

#[cfg(unix)]
#[test]
fn bug6_typechange_does_not_abort_status() {
    let repo = TestRepo::with_commit();
    repo.write("b.txt", "b\n");
    repo.commit_all("b");
    repo.remove("b.txt");
    std::os::unix::fs::symlink("a.txt", repo.path().join("b.txt")).unwrap();
    repo.write("other.txt", "still listed\n");
    let state = get_status(&repo.open()).unwrap();
    assert_eq!(
        state.unstaged,
        vec![delta("b.txt", DeltaChange::Typechange), delta("other.txt", DeltaChange::Untracked)]
    );
    assert_eq!(as_porcelain(&state), repo.porcelain());
    // staged typechange too
    repo.git(&["add", "b.txt"]);
    let state = get_status(&repo.open()).unwrap();
    assert_eq!(state.staged, vec![delta("b.txt", DeltaChange::Typechange)]);
}

#[test]
fn bug6_every_libgit2_delta_kind_maps() {
    use git2::Delta as D;
    for kind in [
        D::Added,
        D::Deleted,
        D::Modified,
        D::Renamed,
        D::Copied,
        D::Ignored,
        D::Untracked,
        D::Typechange,
        D::Unreadable,
        D::Conflicted,
    ] {
        assert!(delta_change(kind).is_some(), "{kind:?}");
    }
    assert_eq!(delta_change(D::Unmodified), None);
}

#[cfg(unix)]
#[test]
fn bug6_unreadable_file_does_not_abort_status() {
    use std::os::unix::fs::PermissionsExt;
    let repo = TestRepo::with_commit();
    repo.write("secret.txt", "x");
    std::fs::set_permissions(repo.path().join("secret.txt"), std::fs::Permissions::from_mode(0o000)).unwrap();
    repo.write("visible.txt", "y");
    let result = get_status(&repo.open());
    std::fs::set_permissions(repo.path().join("secret.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let state = result.unwrap();
    assert!(state.unstaged.contains(&delta("visible.txt", DeltaChange::Untracked)), "{state:?}");
    assert!(
        state.unstaged.iter().any(|d| d.path == std::path::Path::new("secret.txt")),
        "{state:?}"
    );
}

#[test]
fn conflicted_file_listed_once_in_unstaged() {
    let repo = TestRepo::init();
    repo.write("c.txt", "base\n");
    repo.write("d.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-qb", "other"]);
    repo.write("c.txt", "theirs\n");
    repo.write("clean.txt", "from other\n");
    repo.commit_all("theirs");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("c.txt", "ours\n");
    repo.commit_all("ours");
    assert!(!repo.git_try(&["merge", "other"]).status.success());
    let state = get_status(&repo.open()).unwrap();
    assert_eq!(state.unstaged, vec![delta("c.txt", DeltaChange::Conflicted)]);
    assert_eq!(state.staged, vec![delta("clean.txt", DeltaChange::Added)]);
    assert_eq!(as_porcelain(&state), repo.porcelain());
}

#[test]
fn workdir_rename_detected_index_to_workdir() {
    let repo = TestRepo::init();
    repo.write("old.txt", "line\n".repeat(20));
    repo.commit_all("base");
    std::fs::rename(repo.path().join("old.txt"), repo.path().join("new.txt")).unwrap();
    let state = get_status(&repo.open()).unwrap();
    assert_eq!(state.unstaged, vec![renamed("old.txt", "new.txt")]);
}

#[test]
fn status_on_unborn_repo() {
    let repo = TestRepo::init();
    repo.write("a.txt", "a");
    repo.write("b.txt", "b");
    repo.git(&["add", "a.txt"]);
    let state = get_status(&repo.open()).unwrap();
    assert_eq!(state.staged, vec![delta("a.txt", DeltaChange::Added)]);
    assert_eq!(state.unstaged, vec![delta("b.txt", DeltaChange::Untracked)]);
}

#[test]
fn status_on_bare_repo_is_a_clear_error() {
    let src = TestRepo::with_commit();
    let bare = tempfile::tempdir().unwrap();
    wp5_support::git_ok(bare.path(), &["clone", "-q", "--bare", src.path().to_str().unwrap(), "."]);
    let err = get_status(&git2::Repository::open(bare.path()).unwrap()).unwrap_err();
    assert!(matches!(err, GitYasError::InvalidInput(ref m) if m.contains("bare")), "{err:?}");
}

#[test]
fn clean_repo_has_empty_status() {
    let repo = TestRepo::with_commit();
    assert_eq!(get_status(&repo.open()).unwrap(), FileState::default());
}

#[test]
fn deltas_never_have_empty_paths() {
    let repo = TestRepo::with_commit();
    repo.remove("a.txt");
    repo.write("n", "n");
    let state = get_status(&repo.open()).unwrap();
    let all: Vec<&Delta> = state.staged.iter().chain(&state.unstaged).collect();
    assert!(!all.is_empty());
    assert!(all.iter().all(|d| !d.path.as_os_str().is_empty()));
}

/// Tool installers link `.claude/skills/x -> .agents/skills/x` with NTFS junctions. Git for Windows walks them as
/// directories; libgit2 alone reported the junction as untracked and every tracked file under it as deleted.
#[cfg(windows)]
#[test]
fn junction_with_tracked_files_is_walked_like_git_for_windows() {
    let repo = TestRepo::init();
    repo.write(".gitignore", "real/\n*.log\n");
    repo.write("skills/x/same.txt", "same\n");
    repo.write("skills/x/sub/changed.txt", "before\n");
    repo.write("skills/x/gone.txt", "gone\n");
    repo.commit_all("base");

    let real = repo.path().join("real/x");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::rename(repo.path().join("skills/x"), &real).unwrap();
    let win = |p: std::path::PathBuf| p.to_string_lossy().replace('/', r"\");
    let mklink = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(win(repo.path().join("skills").join("x")))
        .arg(win(real))
        .output()
        .unwrap();
    assert!(mklink.status.success(), "{mklink:?}");
    repo.write("real/x/sub/changed.txt", "after\n");
    repo.remove("real/x/gone.txt");
    repo.write("real/x/new.txt", "new\n");
    repo.write("real/x/noise.log", "ignored\n");

    let state = get_status(&repo.open()).unwrap();
    assert_eq!(as_porcelain(&state), repo.porcelain());
    assert_eq!(
        state.unstaged,
        vec![
            delta("skills/x/gone.txt", DeltaChange::Deleted),
            delta("skills/x/new.txt", DeltaChange::Untracked),
            delta("skills/x/sub/changed.txt", DeltaChange::Modified),
        ]
    );

    gityas_git::worktree::stage_files(&repo.open(), &state.unstaged).unwrap();
    let state = get_status(&repo.open()).unwrap();
    assert!(state.unstaged.is_empty(), "{state:?}");
    assert_eq!(as_porcelain(&state), repo.porcelain());
}
