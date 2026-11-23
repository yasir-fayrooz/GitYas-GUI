//! Test helpers: temp repositories driven by the `git` CLI (test-only oracle and fixture builder) and
//! git2, plus a reference `HunkRef` computation matching the diff contract.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicI64, Ordering};

use git2::Repository;
use gityas_core::diff::HunkRef;
use gityas_core::models::{CommitSignature, Delta, DeltaChange};
use gityas_git::convert::hunk_content_hash;
use gityas_git::worktree::hunk_diff_options;
use tempfile::TempDir;

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

static CLOCK: AtomicI64 = AtomicI64::new(1_700_000_000);

/// Strictly increasing fake timestamps so commit order is deterministic.
pub fn tick() -> i64 {
    CLOCK.fetch_add(10, Ordering::SeqCst)
}

pub fn sig() -> CommitSignature {
    CommitSignature {
        name: "Test User".into(),
        email: "test@example.com".into(),
    }
}

pub fn delta(path: &str, change: DeltaChange) -> Delta {
    Delta {
        path: PathBuf::from(path),
        old_path: None,
        change,
    }
}

pub fn renamed(old: &str, new: &str) -> Delta {
    Delta {
        path: PathBuf::from(new),
        old_path: Some(PathBuf::from(old)),
        change: DeltaChange::Renamed,
    }
}

/// Runs `git` in `dir` with an isolated config and deterministic identity/dates.
pub fn git_in(dir: &Path, args: &[&str]) -> Output {
    let t = tick();
    let date = format!("@{t} +0000");
    Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
            "-c",
            "protocol.file.allow=always",
            "-c",
            "advice.detachedHead=false",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("spawn git")
}

pub fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = git_in(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub struct TestRepo {
    pub dir: TempDir,
}

impl TestRepo {
    /// Fresh non-bare repository on `main` with an unborn HEAD.
    pub fn init() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        git_ok(dir.path(), &["init", "-q"]);
        // Git for Windows enables `core.autocrlf` in its system config, which libgit2 honours: pin it so checkouts are byte-exact.
        git_ok(dir.path(), &["config", "core.autocrlf", "false"]);
        Self { dir }
    }

    /// Repository with one commit containing `a.txt`.
    pub fn with_commit() -> Self {
        let repo = Self::init();
        repo.write("a.txt", "one\ntwo\nthree\n");
        repo.commit_all("initial");
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn open(&self) -> Repository {
        Repository::open(self.path()).expect("open repo")
    }

    pub fn git(&self, args: &[&str]) -> String {
        git_ok(self.path(), args)
    }

    pub fn git_try(&self, args: &[&str]) -> Output {
        git_in(self.path(), args)
    }

    pub fn write(&self, rel: impl AsRef<Path>, content: impl AsRef<[u8]>) {
        let full = self.path().join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(full, content).expect("write");
    }

    pub fn read(&self, rel: impl AsRef<Path>) -> Vec<u8> {
        std::fs::read(self.path().join(rel)).expect("read")
    }

    pub fn remove(&self, rel: impl AsRef<Path>) {
        std::fs::remove_file(self.path().join(rel)).expect("remove");
    }

    pub fn commit_all(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.rev("HEAD")
    }

    pub fn rev(&self, spec: &str) -> String {
        self.git(&["rev-parse", spec]).trim().to_owned()
    }

    /// `git status --porcelain=v1 -z --untracked-files=all`, parsed into sorted `XY path` strings.
    pub fn porcelain(&self) -> Vec<String> {
        let raw = self.git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"]);
        let mut out = Vec::new();
        let mut parts = raw.split('\0').filter(|s| !s.is_empty());
        while let Some(entry) = parts.next() {
            let (xy, path) = entry.split_at(3);
            if xy.starts_with('R') || xy.starts_with('C') {
                let from = parts.next().unwrap_or_default();
                out.push(format!("{}{path} <- {from}", xy));
            } else {
                out.push(format!("{xy}{path}"));
            }
        }
        out.sort();
        out
    }

    /// Recursive copy of the whole repository (working tree + `.git`) into a new temp dir.
    pub fn duplicate(&self) -> TestRepo {
        let dir = tempfile::tempdir().expect("tempdir");
        copy_dir(self.path(), dir.path());
        TestRepo { dir }
    }
}

pub fn copy_dir(src: &Path, dst: &Path) {
    for entry in std::fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let ty = entry.file_type().expect("file type");
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            std::fs::create_dir_all(&to).expect("mkdir");
            copy_dir(&entry.path(), &to);
        } else if ty.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path()).expect("readlink"), &to).expect("symlink");
        } else {
            std::fs::copy(entry.path(), &to).expect("copy");
        }
    }
}

/// Reference implementation of the diff's hunk identification: the same diff options as `get_file_diff` and
/// `hunk_content_hash` over `(origin, raw content)` of each line.
pub fn hunk_refs(repo: &Repository, file: &Delta, staged: bool) -> Vec<HunkRef> {
    let mut paths: Vec<&Path> = vec![file.path.as_path()];
    if let Some(old) = &file.old_path {
        paths.push(old.as_path());
    }
    let mut opts = hunk_diff_options(&paths, false);
    let mut diff = if staged {
        let tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        repo.diff_tree_to_index(tree.as_ref(), None, Some(&mut opts)).expect("diff")
    } else {
        repo.diff_index_to_workdir(None, Some(&mut opts)).expect("diff")
    };
    if file.old_path.is_some() {
        let mut find = git2::DiffFindOptions::new();
        find.renames(true).for_untracked(true).copies(file.change == DeltaChange::Copied);
        diff.find_similar(Some(&mut find)).expect("find_similar");
    }
    let idx = diff
        .deltas()
        .position(|d| d.new_file().path() == Some(file.path.as_path()) || d.old_file().path() == Some(file.path.as_path()))
        .expect("delta for file");
    let patch = git2::Patch::from_diff(&diff, idx).expect("patch").expect("text patch");
    (0..patch.num_hunks())
        .map(|h| {
            let (hunk, n) = patch.hunk(h).expect("hunk");
            let lines: Vec<(char, Vec<u8>)> = (0..n)
                .map(|l| {
                    let line = patch.line_in_hunk(h, l).expect("line");
                    (line.origin(), line.content().to_vec())
                })
                .collect();
            HunkRef {
                old_start: hunk.old_start(),
                old_lines: hunk.old_lines(),
                new_start: hunk.new_start(),
                new_lines: hunk.new_lines(),
                content_hash: hunk_content_hash(lines.iter().map(|(o, c)| (*o, c.as_slice()))),
            }
        })
        .collect()
}

/// Splits `git diff` output for one file into (file header, hunks).
pub fn split_cli_diff(diff: &str) -> (String, Vec<String>) {
    let mut header = String::new();
    let mut hunks: Vec<String> = Vec::new();
    for line in diff.split_inclusive('\n') {
        if line.starts_with("@@ ") {
            hunks.push(line.to_owned());
        } else if let Some(last) = hunks.last_mut() {
            last.push_str(line);
        } else {
            header.push_str(line);
        }
    }
    (header, hunks)
}
