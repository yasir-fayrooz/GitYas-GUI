//! WP6 structured diff on temp repositories, with `git diff` (test-only) as the oracle for hunk headers,
//! boundaries and line contents.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test-only helpers (clippy.toml covers #[test] fns only)

mod wp6_common;

use std::path::{Path, PathBuf};

use gityas_core::GitYasError;
use gityas_core::diff::{DiffKind, DiffTarget, FileDiff, LineOrigin};
use gityas_core::models::{Delta, DeltaChange};
use gityas_git::convert::oid_from_git;
use gityas_git::diff::{DiffConfig, NO_NEWLINE_MARKER, get_file_diff};
use wp6_common::{git, git_ok};

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
    fn git(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&git_ok(self.path(), args)).into_owned()
    }
    fn commit_all(&self, msg: &str) -> git2::Oid {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        git2::Oid::from_str(self.git(&["rev-parse", "HEAD"]).trim()).unwrap()
    }
    fn open(&self) -> git2::Repository {
        git2::Repository::open(self.path()).unwrap()
    }
    fn diff(&self, target: DiffTarget, delta: &Delta) -> FileDiff {
        get_file_diff(&self.open(), &target, delta, &DiffConfig::default()).unwrap()
    }
    /// `git diff <args>` from the first `@@` on (exit code 1 = differences is fine for `--no-index`).
    fn oracle(&self, args: &[&str]) -> String {
        let out = git(self.path()).arg("diff").args(args).output().unwrap();
        assert!(out.status.code().unwrap() <= 1, "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).unwrap();
        match text.find("\n@@") {
            Some(i) => text[i + 1..].to_owned(),
            None => String::new(),
        }
    }
}

fn delta(path: &str, change: DeltaChange) -> Delta {
    Delta {
        path: PathBuf::from(path),
        old_path: None,
        change,
    }
}

fn wt(staged: bool) -> DiffTarget {
    DiffTarget::WorkingTree { staged }
}

/// Renders the hunks as unified-diff body text (what `git diff` prints after the file header).
fn render(diff: &FileDiff) -> String {
    let mut out = String::new();
    for h in &diff.hunks {
        out.push_str(&h.header);
        out.push('\n');
        for l in &h.lines {
            let prefix = match l.origin {
                LineOrigin::Context => " ",
                LineOrigin::Add => "+",
                LineOrigin::Del => "-",
                LineOrigin::NoNewlineAtEof => "",
            };
            out.push_str(prefix);
            out.push_str(&l.text);
            out.push('\n');
        }
    }
    out
}

/// Line numbers are consistent with the hunk ranges, and the hunk ranges match its lines.
fn assert_line_numbers(diff: &FileDiff) {
    for h in &diff.hunks {
        let (mut old, mut new) = (h.id.old_start, h.id.new_start);
        let (mut old_count, mut new_count) = (0, 0);
        for l in &h.lines {
            match l.origin {
                LineOrigin::Context => {
                    assert_eq!((l.old_lineno, l.new_lineno), (Some(old), Some(new)));
                    old += 1;
                    new += 1;
                    old_count += 1;
                    new_count += 1;
                }
                LineOrigin::Del => {
                    assert_eq!((l.old_lineno, l.new_lineno), (Some(old), None));
                    old += 1;
                    old_count += 1;
                }
                LineOrigin::Add => {
                    assert_eq!((l.old_lineno, l.new_lineno), (None, Some(new)));
                    new += 1;
                    new_count += 1;
                }
                LineOrigin::NoNewlineAtEof => assert_eq!((l.old_lineno, l.new_lineno), (None, None)),
            }
        }
        assert_eq!((old_count, new_count), (h.id.old_lines, h.id.new_lines), "{}", h.header);
    }
}

fn lines(n: usize, f: impl Fn(usize) -> String) -> String {
    (1..=n).map(|i| f(i) + "\n").collect()
}

#[test]
fn unstaged_modification_matches_git_diff_and_full_sides() {
    let r = Repo::new();
    let old = lines(40, |i| format!("line {i}"));
    r.write("f.txt", &old);
    r.commit_all("init");
    let new = lines(40, |i| match i {
        3 => "changed 3".into(),
        20 => "changed 20".into(),
        21 => "changed 21".into(),
        _ => format!("line {i}"),
    }) + "appended\n";
    r.write("f.txt", &new);
    let d = r.diff(wt(false), &delta("f.txt", DeltaChange::Modified));
    assert_eq!(d.kind, DiffKind::Text);
    assert_eq!(&*d.old_text, old);
    assert_eq!(&*d.new_text, new);
    assert_eq!(d.hunks.len(), 3);
    assert_eq!(render(&d), r.oracle(&["--", "f.txt"]));
    assert_line_numbers(&d);
    // 3 lines of context.
    assert_eq!(d.hunks[0].lines.iter().take_while(|l| l.origin == LineOrigin::Context).count(), 2);
    assert_eq!(d.hunks[1].lines.iter().take_while(|l| l.origin == LineOrigin::Context).count(), 3);
}

#[test]
fn function_context_in_headers_matches_git() {
    let r = Repo::new();
    let src = |body: &str| {
        format!("fn alpha() {{\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    {body}\n    let e = 5;\n}}\n")
    };
    r.write("x.rs", src("old();"));
    r.commit_all("init");
    r.write("x.rs", src("new();"));
    let d = r.diff(wt(false), &delta("x.rs", DeltaChange::Modified));
    assert_eq!(render(&d), r.oracle(&["--", "x.rs"]));
    assert!(d.hunks[0].header.ends_with("fn alpha() {"), "{}", d.hunks[0].header);
}

#[test]
fn staged_diff_is_index_vs_head_and_unborn_head_is_empty_tree() {
    let r = Repo::new();
    r.write("new.txt", "a\nb\n");
    r.git(&["add", "new.txt"]);
    // Unborn HEAD.
    let d = r.diff(wt(true), &delta("new.txt", DeltaChange::Added));
    assert_eq!((&*d.old_text, &*d.new_text), ("", "a\nb\n"));
    assert_eq!(render(&d), r.oracle(&["--cached", "--", "new.txt"]));
    r.commit_all("init");
    r.write("new.txt", "a\nB\n");
    r.git(&["add", "new.txt"]);
    r.write("new.txt", "a\nB\nunstaged\n");
    let staged = r.diff(wt(true), &delta("new.txt", DeltaChange::Modified));
    assert_eq!((&*staged.old_text, &*staged.new_text), ("a\nb\n", "a\nB\n"));
    assert_eq!(render(&staged), r.oracle(&["--cached", "--", "new.txt"]));
    let unstaged = r.diff(wt(false), &delta("new.txt", DeltaChange::Modified));
    assert_eq!((&*unstaged.old_text, &*unstaged.new_text), ("a\nB\n", "a\nB\nunstaged\n"));
    assert_eq!(render(&unstaged), r.oracle(&["--", "new.txt"]));
}

#[test]
fn untracked_file_is_one_all_added_hunk_even_inside_untracked_dir() {
    let r = Repo::new();
    r.write("keep.txt", "k\n");
    r.commit_all("init");
    r.write("u.txt", "one\ntwo\nthree\n");
    r.write("newdir/deep/u2.txt", "x\ny");
    for (path, args) in [
        ("u.txt", ["--no-index", "/dev/null", "u.txt"]),
        ("newdir/deep/u2.txt", ["--no-index", "/dev/null", "newdir/deep/u2.txt"]),
    ] {
        let d = r.diff(wt(false), &delta(path, DeltaChange::Untracked));
        assert_eq!(d.kind, DiffKind::Text);
        assert_eq!(d.hunks.len(), 1);
        assert!(
            d.hunks[0]
                .lines
                .iter()
                .all(|l| matches!(l.origin, LineOrigin::Add | LineOrigin::NoNewlineAtEof))
        );
        assert_eq!(&*d.old_text, "");
        assert_eq!(*d.new_text, *std::fs::read_to_string(r.path().join(path)).unwrap());
        assert_eq!(render(&d), r.oracle(&args));
        assert_line_numbers(&d);
    }
}

/// libgit2's path-limited working-tree diff skips untracked files below a directory holding a `.git` entry (here an
/// empty `.git` folder in a tracked subdirectory); status still lists them, so the diff must find them too.
#[test]
fn untracked_file_below_a_stray_dot_git_dir_is_diffed() {
    let r = Repo::new();
    r.write("app/keep.txt", "k\n");
    r.commit_all("init");
    std::fs::create_dir_all(r.path().join("app/.git")).unwrap();
    r.write("app/new.txt", "one\ntwo\n").write("app/sub/deep.txt", "x\n");
    for (path, text) in [("app/new.txt", "one\ntwo\n"), ("app/sub/deep.txt", "x\n")] {
        let d = r.diff(wt(false), &delta(path, DeltaChange::Untracked));
        assert_eq!(d.kind, DiffKind::Text, "{path}");
        assert_eq!(d.hunks.len(), 1, "{path}");
        assert_eq!((&*d.old_text, &*d.new_text), ("", text), "{path}");
    }
}

#[test]
fn deleted_and_added_files_have_an_empty_side() {
    let r = Repo::new();
    r.write("gone.txt", "bye\n").write("staged-gone.txt", "s\n");
    r.commit_all("init");
    std::fs::remove_file(r.path().join("gone.txt")).unwrap();
    r.git(&["rm", "-q", "staged-gone.txt"]);
    let d = r.diff(wt(false), &delta("gone.txt", DeltaChange::Deleted));
    assert_eq!((&*d.old_text, &*d.new_text), ("bye\n", ""));
    assert_eq!(render(&d), r.oracle(&["--", "gone.txt"]));
    let d = r.diff(wt(true), &delta("staged-gone.txt", DeltaChange::Deleted));
    assert_eq!((&*d.old_text, &*d.new_text), ("s\n", ""));
    assert_eq!(render(&d), r.oracle(&["--cached", "--", "staged-gone.txt"]));
}

#[test]
fn commit_diffs_root_normal_and_merge_first_parent() {
    let r = Repo::new();
    r.write("f.txt", "1\n2\n3\n");
    let root = r.commit_all("root");
    let d = r.diff(DiffTarget::Commit(oid_from_git(root).unwrap()), &delta("f.txt", DeltaChange::Added));
    assert_eq!((&*d.old_text, &*d.new_text), ("", "1\n2\n3\n"));
    assert_eq!(
        render(&d),
        r.oracle(&["4b825dc642cb6eb9a060e54bf8d69288fbee4904", &root.to_string(), "--", "f.txt"])
    );

    r.write("f.txt", "1\n2\n3\n4\n");
    let second = r.commit_all("second");
    let d = r.diff(
        DiffTarget::Commit(oid_from_git(second).unwrap()),
        &delta("f.txt", DeltaChange::Modified),
    );
    assert_eq!(render(&d), r.oracle(&[&format!("{second}^"), &second.to_string(), "--", "f.txt"]));

    // Merge commit: side branch changes f.txt, main changes g.txt; the merge's diff is vs first parent (main).
    r.git(&["checkout", "-q", "-b", "side"]);
    r.write("f.txt", "1\n2\nside\n3\n4\n");
    r.commit_all("side");
    r.git(&["checkout", "-q", "main"]);
    r.write("g.txt", "main\n");
    r.commit_all("main");
    r.git(&["merge", "-q", "--no-edit", "side"]);
    let merge = git2::Oid::from_str(r.git(&["rev-parse", "HEAD"]).trim()).unwrap();
    let d = r.diff(
        DiffTarget::Commit(oid_from_git(merge).unwrap()),
        &delta("f.txt", DeltaChange::Modified),
    );
    assert_eq!(render(&d), r.oracle(&[&format!("{merge}^1"), &merge.to_string(), "--", "f.txt"]));
    // g.txt came from the first parent, so it is not part of the merge's first-parent diff.
    let err = get_file_diff(
        &r.open(),
        &DiffTarget::Commit(oid_from_git(merge).unwrap()),
        &delta("g.txt", DeltaChange::Modified),
        &DiffConfig::default(),
    );
    assert!(matches!(err, Err(GitYasError::NotFound(_))), "{err:?}");
}

#[test]
fn stash_diff_tracked_and_untracked_parts() {
    let r = Repo::new();
    r.write("t.txt", "a\nb\n");
    r.commit_all("init");
    r.write("t.txt", "a\nB\n");
    r.write("u.txt", "untracked\n");
    r.git(&["stash", "push", "-q", "-u"]);
    let stash = git2::Oid::from_str(r.git(&["rev-parse", "stash@{0}"]).trim()).unwrap();
    let target = DiffTarget::Stash(oid_from_git(stash).unwrap());
    let d = r.diff(target.clone(), &delta("t.txt", DeltaChange::Modified));
    assert_eq!((&*d.old_text, &*d.new_text), ("a\nb\n", "a\nB\n"));
    assert_eq!(render(&d), r.oracle(&["stash@{0}^1", "stash@{0}", "--", "t.txt"]));
    let d = r.diff(target, &delta("u.txt", DeltaChange::Added));
    assert_eq!((&*d.old_text, &*d.new_text), ("", "untracked\n"));
    assert_eq!(
        render(&d),
        r.oracle(&["4b825dc642cb6eb9a060e54bf8d69288fbee4904", "stash@{0}^3", "--", "u.txt"])
    );
}

#[test]
fn staged_rename_uses_old_path() {
    let r = Repo::new();
    let body = lines(20, |i| format!("content {i}"));
    r.write("old/name.txt", &body);
    r.commit_all("init");
    r.git(&["mv", "old/name.txt", "new-name.txt"]);
    r.write("new-name.txt", body.replace("content 10", "content ten"));
    r.git(&["add", "new-name.txt"]);
    let mut dl = delta("new-name.txt", DeltaChange::Renamed);
    dl.old_path = Some(PathBuf::from("old/name.txt"));
    let d = r.diff(wt(true), &dl);
    assert_eq!(&*d.old_text, body);
    assert_eq!(d.hunks.len(), 1);
    assert_eq!(render(&d), r.oracle(&["--cached", "-M", "--", "old/name.txt", "new-name.txt"]));
    // A pure rename has no hunks and equal sides.
    let r2 = Repo::new();
    r2.write("a.txt", &body);
    r2.commit_all("init");
    r2.git(&["mv", "a.txt", "b.txt"]);
    let mut dl = delta("b.txt", DeltaChange::Renamed);
    dl.old_path = Some(PathBuf::from("a.txt"));
    let d = r2.diff(wt(true), &dl);
    assert!(d.hunks.is_empty());
    assert_eq!(d.old_text, d.new_text);
}

#[test]
fn crlf_lines_keep_raw_sides_and_strip_terminators_in_lines() {
    let r = Repo::new();
    r.write("w.txt", "one\r\ntwo\r\nthree\r\n");
    r.commit_all("init");
    r.write("w.txt", "one\r\nTWO\r\nthree\r\n");
    let d = r.diff(wt(false), &delta("w.txt", DeltaChange::Modified));
    assert_eq!(&*d.new_text, "one\r\nTWO\r\nthree\r\n");
    assert!(d.hunks[0].lines.iter().all(|l| !l.text.contains('\r')));
    assert_eq!(render(&d), r.oracle(&["--", "w.txt"]).replace('\r', ""));
}

#[test]
fn no_newline_at_eof_markers_all_three_kinds() {
    let r = Repo::new();
    // '<': old has a final newline, new does not; '>': the reverse; '=': neither (context at EOF).
    r.write("del.txt", "a\nb\n")
        .write("add.txt", "a\nb")
        .write("ctx.txt", "1\n2\n3\n4\n5\nend");
    r.commit_all("init");
    r.write("del.txt", "a\nb")
        .write("add.txt", "a\nb\n")
        .write("ctx.txt", "1\n2\nX\n4\n5\nend");
    for f in ["del.txt", "add.txt", "ctx.txt"] {
        let d = r.diff(wt(false), &delta(f, DeltaChange::Modified));
        assert_eq!(render(&d), r.oracle(&["--", f]), "{f}");
        assert_eq!(*d.new_text, *std::fs::read_to_string(r.path().join(f)).unwrap(), "{f}");
        let markers = d
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.origin == LineOrigin::NoNewlineAtEof)
            .count();
        assert!(markers >= 1, "{f}");
        assert!(
            d.hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.origin == LineOrigin::NoNewlineAtEof)
                .all(|l| l.text == NO_NEWLINE_MARKER)
        );
        assert_line_numbers(&d);
    }
}

#[cfg(unix)]
#[test]
fn mode_only_change_is_text_without_hunks() {
    use std::os::unix::fs::PermissionsExt;
    let r = Repo::new();
    r.write("run.sh", "echo hi\n");
    r.commit_all("init");
    let p = r.path().join("run.sh");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let d = r.diff(wt(false), &delta("run.sh", DeltaChange::Modified));
    assert_eq!(d.kind, DiffKind::Text);
    assert!(d.hunks.is_empty());
    assert_eq!((&*d.old_text, &*d.new_text), ("echo hi\n", "echo hi\n"));
}

#[test]
fn submodule_entry_shows_subproject_commit_lines() {
    let r = Repo::new();
    r.write("f", "x\n");
    r.commit_all("init");
    let repo = r.open();
    let (a, b) = (
        git2::Oid::from_str("1111111111111111111111111111111111111111").unwrap(),
        git2::Oid::from_str("2222222222222222222222222222222222222222").unwrap(),
    );
    let commit_with = |gitlink: git2::Oid, parent: Option<&git2::Commit>| {
        let head_tree = repo.head().unwrap().peel_to_tree().unwrap();
        let mut tb = repo.treebuilder(Some(&head_tree)).unwrap();
        tb.insert("sub", gitlink, 0o160000).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        let parents: Vec<&git2::Commit> = parent.into_iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, "sub", &tree, &parents).unwrap()
    };
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    let c1 = commit_with(a, Some(&head));
    let c1c = repo.find_commit(c1).unwrap();
    let c2 = commit_with(b, Some(&c1c));
    let d = r.diff(DiffTarget::Commit(oid_from_git(c2).unwrap()), &delta("sub", DeltaChange::Modified));
    assert_eq!(d.kind, DiffKind::Text);
    assert_eq!(&*d.old_text, format!("Subproject commit {a}\n"));
    assert_eq!(&*d.new_text, format!("Subproject commit {b}\n"));
    assert_eq!(render(&d), r.oracle(&[&c1.to_string(), &c2.to_string(), "--", "sub"]));
}

#[test]
fn binary_too_large_and_non_utf8_are_reported() {
    let r = Repo::new();
    r.write("bin.dat", b"\x00\x01\x02binary\n")
        .write("big.txt", "small\n")
        .write("latin1.txt", b"caf\xe9\n");
    r.commit_all("init");
    r.write("bin.dat", b"\x00\x01\x03binary\n")
        .write("big.txt", "x\n".repeat(600_000))
        .write("latin1.txt", b"caf\xe9 au lait\n");
    let d = r.diff(wt(false), &delta("bin.dat", DeltaChange::Modified));
    assert_eq!(d.kind, DiffKind::Binary);
    assert!(d.hunks.is_empty() && d.old_text.is_empty() && d.new_text.is_empty());

    let repo = r.open();
    let small_cap = DiffConfig { max_bytes: 1_000_000 };
    let d = get_file_diff(&repo, &wt(false), &delta("big.txt", DeltaChange::Modified), &small_cap).unwrap();
    assert_eq!(d.kind, DiffKind::TooLarge { bytes: 1_200_000 });
    assert!(d.hunks.is_empty());
    // "Show anyway": a larger cap returns the full diff.
    let d = get_file_diff(
        &repo,
        &wt(false),
        &delta("big.txt", DeltaChange::Modified),
        &DiffConfig { max_bytes: 2_000_000 },
    )
    .unwrap();
    assert_eq!(d.kind, DiffKind::Text);

    let d = r.diff(wt(false), &delta("latin1.txt", DeltaChange::Modified));
    assert_eq!(d.kind, DiffKind::NonUtf8);
    assert!(d.old_text.is_empty() && d.new_text.is_empty());
    assert_eq!(d.hunks.len(), 1, "hunks kept so hunk staging still works");
}

#[test]
fn content_hash_tracks_hunk_content() {
    let r = Repo::new();
    r.write("h.txt", lines(30, |i| format!("l{i}")));
    r.commit_all("init");
    let edit = |line3: &str| {
        r.write("h.txt", lines(30, |i| if i == 3 { line3.to_owned() } else { format!("l{i}") }));
        r.diff(wt(false), &delta("h.txt", DeltaChange::Modified)).hunks[0].id
    };
    let a = edit("first");
    let a_again = edit("first");
    let b = edit("other");
    let c = edit("firsT");
    assert_eq!(a, a_again, "stable for identical content");
    // Same ranges, different content: only the hash tells them apart.
    assert_eq!(
        (a.old_start, a.old_lines, a.new_start, a.new_lines),
        (b.old_start, b.old_lines, b.new_start, b.new_lines)
    );
    assert_ne!(a.content_hash, b.content_hash);
    assert_ne!(a.content_hash, c.content_hash);
    // Same change, but a context line changed in the index too.
    r.write("h.txt", lines(30, |i| if i == 1 { "L1".into() } else { format!("l{i}") }));
    r.git(&["add", "h.txt"]);
    let d = edit("first");
    assert_ne!(a.content_hash, d.content_hash);
}

#[test]
fn hash_is_computed_from_git2_lines_as_documented() {
    let r = Repo::new();
    r.write("h.txt", "a\nb\nc");
    r.commit_all("init");
    r.write("h.txt", "a\nB\nc\n");
    let d = r.diff(wt(false), &delta("h.txt", DeltaChange::Modified));
    // Recompute independently from a git2 Patch exactly as WP5 will.
    let repo = r.open();
    let mut opts = git2::DiffOptions::new();
    gityas_git::convert::apply_hunk_diff_options(&mut opts).pathspec("h.txt");
    let diff = repo.diff_index_to_workdir(None, Some(&mut opts)).unwrap();
    let patch = git2::Patch::from_diff(&diff, 0).unwrap().unwrap();
    let n = patch.num_lines_in_hunk(0).unwrap();
    let lines: Vec<(char, Vec<u8>)> = (0..n)
        .map(|i| patch.line_in_hunk(0, i).unwrap())
        .map(|l| (l.origin(), l.content().to_vec()))
        .collect();
    let expected = gityas_git::convert::hunk_content_hash(lines.iter().map(|(o, c)| (*o, c.as_slice())));
    assert_eq!(d.hunks[0].id.content_hash, expected);
    assert!(lines.iter().any(|(o, _)| *o == '>'), "EOF marker lines are part of the hash input");
}

#[test]
#[cfg_attr(windows, ignore = "`*` is not allowed in Windows file names")]
fn paths_with_glob_characters_match_exactly() {
    let r = Repo::new();
    r.write("[ab].txt", "1\n")
        .write("a.txt", "1\n")
        .write("star*.txt", "1\n")
        .write("starX.txt", "1\n");
    r.commit_all("init");
    r.write("[ab].txt", "2\n")
        .write("a.txt", "2\n")
        .write("star*.txt", "2\n")
        .write("starX.txt", "3\n");
    let d = r.diff(wt(false), &delta("[ab].txt", DeltaChange::Modified));
    assert_eq!(&*d.old_text, "1\n");
    let d = r.diff(wt(false), &delta("starX.txt", DeltaChange::Modified));
    assert_eq!(&*d.new_text, "3\n");
    let d = r.diff(wt(false), &delta("star*.txt", DeltaChange::Modified));
    assert_eq!(&*d.new_text, "2\n");
}

#[test]
fn unchanged_path_is_not_found_and_conflicts_are_rejected() {
    let r = Repo::new();
    r.write("same.txt", "x\n");
    r.commit_all("init");
    let res = get_file_diff(
        &r.open(),
        &wt(false),
        &delta("same.txt", DeltaChange::Modified),
        &DiffConfig::default(),
    );
    assert!(matches!(res, Err(GitYasError::NotFound(_))), "{res:?}");
    let res = get_file_diff(
        &r.open(),
        &wt(false),
        &delta("same.txt", DeltaChange::Conflicted),
        &DiffConfig::default(),
    );
    assert!(matches!(res, Err(GitYasError::InvalidInput(_))), "{res:?}");
}

#[test]
fn bare_repository_rejects_working_tree_but_diffs_commits() {
    let r = Repo::new();
    r.write("f.txt", "a\n");
    let c = r.commit_all("init");
    let bare = tempfile::tempdir().unwrap();
    git_ok(r.path(), &["clone", "-q", "--bare", ".", bare.path().to_str().unwrap()]);
    let repo = git2::Repository::open_bare(bare.path()).unwrap();
    let res = get_file_diff(&repo, &wt(false), &delta("f.txt", DeltaChange::Modified), &DiffConfig::default());
    assert!(matches!(res, Err(GitYasError::InvalidInput(_))));
    let d = get_file_diff(
        &repo,
        &DiffTarget::Commit(oid_from_git(c).unwrap()),
        &delta("f.txt", DeltaChange::Added),
        &DiffConfig::default(),
    )
    .unwrap();
    assert_eq!(&*d.new_text, "a\n");
}

#[test]
fn unicode_content_and_empty_file() {
    let r = Repo::new();
    r.write("u.txt", "héllo\n日本語\n").write("empty.txt", "");
    r.commit_all("init");
    r.write("u.txt", "héllo\n日本\n").write("empty.txt", "now text\n");
    let d = r.diff(wt(false), &delta("u.txt", DeltaChange::Modified));
    assert_eq!(render(&d), r.oracle(&["--", "u.txt"]));
    let d = r.diff(wt(false), &delta("empty.txt", DeltaChange::Modified));
    assert_eq!((&*d.old_text, &*d.new_text), ("", "now text\n"));
    assert_eq!(render(&d), r.oracle(&["--", "empty.txt"]));
}

#[test]
fn random_edits_match_git_diff() {
    let mut rng = wp6_common::Rng(0xfeed_face_1234_5678);
    let r = Repo::new();
    for round in 0..40 {
        let base = wp6_common::gen_lines(&mut rng, 30);
        let mid = wp6_common::mutate(&mut rng, &base);
        let new = wp6_common::mutate(&mut rng, &mid);
        let trailing = rng.below(4) != 0;
        let (old_s, new_s) = (wp6_common::join(&base, "\n", true), wp6_common::join(&new, "\n", trailing));
        r.write("r.txt", &old_s);
        r.git(&["add", "r.txt"]);
        r.write("r.txt", &new_s);
        if old_s == new_s {
            continue;
        }
        let d = r.diff(wt(false), &delta("r.txt", DeltaChange::Modified));
        assert_eq!(render(&d), r.oracle(&["--", "r.txt"]), "round {round}: {old_s:?} -> {new_s:?}");
        assert_eq!(&*d.new_text, new_s);
        assert_line_numbers(&d);
    }
}

#[test]
fn autocrlf_new_side_is_the_filtered_content_git_diffs() {
    let r = Repo::new();
    r.git(&["config", "core.autocrlf", "true"]);
    r.write("c.txt", "a\nb\nc\n");
    r.commit_all("init");
    r.write("c.txt", "a\r\nB\r\nc\r\n");
    let d = r.diff(wt(false), &delta("c.txt", DeltaChange::Modified));
    // libgit2 diffs the clean-filtered (LF) working-tree content; new_text is exactly that postimage.
    assert_eq!(&*d.new_text, "a\nB\nc\n");
    assert_eq!(render(&d), r.oracle(&["--", "c.txt"]));
}

#[cfg(unix)]
#[test]
fn symlink_and_typechange_diffs() {
    let r = Repo::new();
    r.write("target.txt", "t\n").write("kind", "regular file\n");
    std::os::unix::fs::symlink("target.txt", r.path().join("link")).unwrap();
    r.commit_all("init");
    std::fs::remove_file(r.path().join("link")).unwrap();
    std::os::unix::fs::symlink("elsewhere.txt", r.path().join("link")).unwrap();
    let d = r.diff(wt(false), &delta("link", DeltaChange::Modified));
    assert_eq!((&*d.old_text, &*d.new_text), ("target.txt", "elsewhere.txt"));
    assert_eq!(render(&d), r.oracle(&["--", "link"]));
    // Regular file -> symlink: reported (as text of both sides), never an error.
    std::fs::remove_file(r.path().join("kind")).unwrap();
    std::os::unix::fs::symlink("target.txt", r.path().join("kind")).unwrap();
    let d = r.diff(wt(false), &delta("kind", DeltaChange::Typechange));
    assert_eq!(d.kind, DiffKind::Text);
    assert_eq!(&*d.old_text, "regular file\n");
}

#[test]
fn linked_worktree_and_detached_head() {
    let r = Repo::new();
    r.write("f.txt", "a\n");
    r.commit_all("init");
    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("linked");
    r.git(&["worktree", "add", "-q", "--detach", wt_path.to_str().unwrap()]);
    std::fs::write(wt_path.join("f.txt"), "a\nlinked\n").unwrap();
    git_ok(&wt_path, &["add", "f.txt"]);
    std::fs::write(wt_path.join("f.txt"), "a\nlinked\nmore\n").unwrap();
    let repo = git2::Repository::open(&wt_path).unwrap();
    assert!(repo.head_detached().unwrap());
    let staged = get_file_diff(&repo, &wt(true), &delta("f.txt", DeltaChange::Modified), &DiffConfig::default()).unwrap();
    assert_eq!(&*staged.new_text, "a\nlinked\n");
    let unstaged = get_file_diff(&repo, &wt(false), &delta("f.txt", DeltaChange::Modified), &DiffConfig::default()).unwrap();
    assert_eq!(&*unstaged.new_text, "a\nlinked\nmore\n");
    // The main working tree is untouched.
    let main = get_file_diff(
        &r.open(),
        &wt(false),
        &delta("f.txt", DeltaChange::Modified),
        &DiffConfig::default(),
    );
    assert!(matches!(main, Err(GitYasError::NotFound(_))));
}
