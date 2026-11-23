//! Test helpers: temp repositories, a local bare "origin", and the `git` CLI as a test-only oracle.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use git2::{Repository, Signature};
use gityas_core::models::CommitSignature;
use gityas_core::{Credential, CredentialProvider, Oid, Provider};
use secrecy::SecretString;
use tempfile::TempDir;

pub fn sig() -> Signature<'static> {
    Signature::now("Test User", "test@example.com").unwrap()
}

pub fn core_sig() -> CommitSignature {
    CommitSignature {
        name: "Test User".into(),
        email: "test@example.com".into(),
    }
}

pub fn oid(o: git2::Oid) -> Oid {
    Oid::from_bytes(o.as_bytes()).unwrap()
}

/// Isolates the `git` oracle from the user's config.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git CLI available for the test oracle");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

pub fn init(dir: &Path) -> Repository {
    let repo = gityas_git::repository::init(dir, false, Some("main")).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "Test User").unwrap();
    cfg.set_str("user.email", "test@example.com").unwrap();
    // Git for Windows enables `core.autocrlf` in its system config, which libgit2 honours: pin it so checkouts are byte-exact.
    cfg.set_bool("core.autocrlf", false).unwrap();
    repo
}

pub fn write(repo: &Repository, path: &str, content: &str) {
    let full = repo.workdir().unwrap().join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

pub fn read(repo: &Repository, path: &str) -> String {
    std::fs::read_to_string(repo.workdir().unwrap().join(path)).unwrap()
}

pub fn stage(repo: &Repository, path: &str) {
    let mut index = repo.index().unwrap();
    if repo.workdir().unwrap().join(path).exists() {
        index.add_path(Path::new(path)).unwrap();
    } else {
        index.remove_path(Path::new(path)).unwrap();
    }
    index.write().unwrap();
}

/// Writes `path`, stages it and commits on HEAD (works on an unborn HEAD).
pub fn commit_file(repo: &Repository, path: &str, content: &str, msg: &str) -> git2::Oid {
    write(repo, path, content);
    stage(repo, path);
    commit_index(repo, msg)
}

pub fn commit_index(repo: &Repository, msg: &str) -> git2::Oid {
    let mut index = repo.index().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parents: Vec<git2::Commit<'_>> = match repo.head() {
        Ok(h) => vec![h.peel_to_commit().unwrap()],
        Err(_) => vec![],
    };
    let refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig(), &sig(), msg, &tree, &refs).unwrap()
}

pub fn head_oid(repo: &Repository) -> git2::Oid {
    repo.head().unwrap().target().unwrap()
}

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

/// A working repo with one commit on `main`, plus a bare `origin` it has pushed `main` to (upstream set).
pub struct Fixture {
    pub _tmp: TempDir,
    pub work: Repository,
    pub remote_path: PathBuf,
    pub root: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let remote_path = root.join("remote.git");
        gityas_git::repository::init(&remote_path, true, Some("main")).unwrap();
        let work = init(&root.join("work"));
        commit_file(&work, "a.txt", "one\n", "initial");
        work.remote("origin", &file_url(&remote_path)).unwrap();
        {
            let mut remote = work.find_remote("origin").unwrap();
            remote.push(&["refs/heads/main:refs/heads/main"], None).unwrap();
            remote.fetch::<&str>(&[], None, None).unwrap();
        }
        work.find_branch("main", git2::BranchType::Local)
            .unwrap()
            .set_upstream(Some("origin/main"))
            .unwrap();
        Self {
            _tmp: tmp,
            work,
            remote_path,
            root,
        }
    }

    /// A second clone of the remote, to make commits "someone else" pushed.
    pub fn other_clone(&self, name: &str) -> Repository {
        let path = self.root.join(name);
        let repo = Repository::clone(&file_url(&self.remote_path), &path).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Other").unwrap();
        cfg.set_str("user.email", "other@example.com").unwrap();
        repo
    }

    pub fn remote(&self) -> Repository {
        Repository::open_bare(&self.remote_path).unwrap()
    }
}

/// Pushes `refspec` from `repo` to its `origin` with plain git2 (test setup only).
pub fn raw_push(repo: &Repository, refspec: &str) {
    repo.find_remote("origin").unwrap().push(&[refspec], None).unwrap();
}

/// A credential source with a single token for one provider.
pub struct OneToken(pub Provider, pub &'static str);

impl CredentialProvider for OneToken {
    fn credential(&self, provider: Provider) -> Option<Credential> {
        (provider == self.0).then(|| Credential {
            username: None,
            token: SecretString::from(self.1),
        })
    }
}
