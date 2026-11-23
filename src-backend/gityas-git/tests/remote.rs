//! Fetch / push / connection test / credential callback against local and fake HTTP remotes.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test crate: panics are the assertions

mod repo_support;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use gityas_core::{GitYasError, Provider};
use gityas_git::remote::{self, NoCredentials, RemoteContext};
use repo_support::*;
use tempfile::TempDir;

fn ctx(creds: &NoCredentials) -> RemoteContext<'_> {
    RemoteContext::new(creds).with_credential_helper(false)
}

/// Bug #1: fetch on a repo without remotes must not panic; it's a no-op.
#[test]
fn fetch_without_remotes_is_noop() {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    let creds = NoCredentials;
    let summary = remote::fetch(&repo, None, false, &ctx(&creds)).unwrap();
    assert!(summary.fetched.is_empty() && summary.failed.is_empty());
    assert!(!summary.updated_any());
    assert!(matches!(
        remote::fetch(&repo, Some("origin"), false, &ctx(&creds)),
        Err(GitYasError::NotFound(_))
    ));
}

#[test]
fn fetch_reports_updates_and_prunes() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    let other = fx.other_clone("other");
    let c2 = commit_file(&other, "b", "b\n", "c2");
    raw_push(&other, "refs/heads/main:refs/heads/main");
    raw_push(&other, "refs/heads/main:refs/heads/temp");

    let summary = remote::fetch(&fx.work, None, false, &ctx(&creds)).unwrap();
    assert_eq!(summary.fetched.len(), 1);
    let updated = &summary.fetched[0].updated;
    let main = updated.iter().find(|u| u.refname == "refs/remotes/origin/main").unwrap();
    assert_eq!(main.new, Some(oid(c2)));
    assert!(main.old.is_some());
    let temp = updated.iter().find(|u| u.refname == "refs/remotes/origin/temp").unwrap();
    assert_eq!(temp.old, None);

    // Nothing new: no updates.
    let again = remote::fetch(&fx.work, Some("origin"), false, &ctx(&creds)).unwrap();
    assert!(!again.updated_any());

    // Delete on the remote, fetch with prune.
    other.find_remote("origin").unwrap().push(&[":refs/heads/temp"], None).unwrap();
    let pruned = remote::fetch(&fx.work, Some("origin"), true, &ctx(&creds)).unwrap();
    assert!(
        pruned.fetched[0]
            .updated
            .iter()
            .any(|u| u.refname == "refs/remotes/origin/temp" && u.new.is_none())
    );
    assert!(fx.work.find_reference("refs/remotes/origin/temp").is_err());
}

#[test]
fn fetch_all_survives_one_broken_remote() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    fx.work.remote("broken", &file_url(&fx.root.join("does-not-exist.git"))).unwrap();
    let summary = remote::fetch(&fx.work, None, false, &ctx(&creds)).unwrap();
    assert_eq!(summary.fetched.len(), 1);
    assert_eq!(summary.failed.len(), 1);
    assert_eq!(summary.failed[0].0, "broken");
    // Explicitly chosen broken remote: error.
    assert!(remote::fetch(&fx.work, Some("broken"), false, &ctx(&creds)).is_err());
    // Only broken remotes: error.
    fx.work.remote_delete("origin").unwrap();
    assert!(remote::fetch(&fx.work, None, false, &ctx(&creds)).is_err());
}

/// Fails the test if a stored PAT is ever looked up.
struct NoTokenLookups;

impl gityas_core::CredentialProvider for NoTokenLookups {
    fn credential(&self, provider: Provider) -> Option<gityas_core::Credential> {
        panic!("a {provider} token was looked up for an SSH remote");
    }
}

/// SSH remotes are no longer refused up front: the connection is attempted (here to a closed local port, so no
/// network) and a stored PAT is never read for them.
#[test]
fn fetch_ssh_remote_connects_without_reading_tokens() {
    let fx = Fixture::new();
    let creds = NoTokenLookups;
    fx.work.remote("gh", "ssh://git@127.0.0.1:1/o/r.git").unwrap();
    let ctx = RemoteContext::new(&creds)
        .with_provider(Some(Provider::Github))
        .with_credential_helper(false);
    for err in [
        remote::fetch(&fx.work, Some("gh"), false, &ctx).unwrap_err(),
        remote::test_connection(&fx.work, "gh", &ctx).unwrap_err(),
    ] {
        let message = err.to_string();
        assert!(!message.contains("not supported"), "{message}");
    }
}

#[test]
fn fetch_cancelled() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    let cancel = AtomicBool::new(true);
    let c = ctx(&creds).with_cancel(&cancel);
    assert_eq!(remote::fetch(&fx.work, None, false, &c).unwrap_err(), GitYasError::Cancelled);
}

#[test]
fn push_existing_upstream_and_first_push_sets_upstream() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    let c2 = commit_file(&fx.work, "b", "b\n", "c2");
    let out = remote::push_branch(&fx.work, "refs/heads/main", false, &ctx(&creds)).unwrap();
    assert_eq!(out.remote, "origin");
    assert_eq!(out.remote_ref, "refs/heads/main");
    assert!(!out.upstream_set);
    assert_eq!(fx.remote().refname_to_id("refs/heads/main").unwrap(), c2);
    // Tags are not pushed implicitly.
    fx.work
        .tag_lightweight("v1", &fx.work.find_object(c2, None).unwrap(), false)
        .unwrap();
    remote::push_branch(&fx.work, "refs/heads/main", false, &ctx(&creds)).unwrap();
    assert!(fx.remote().find_reference("refs/tags/v1").is_err());

    // A new branch without upstream.
    gityas_git::branches::create_branch(&fx.work, "new", None, true).unwrap();
    let out = remote::push_branch(&fx.work, "refs/heads/new", false, &ctx(&creds)).unwrap();
    assert!(out.upstream_set);
    assert_eq!(
        git(fx.work.workdir().unwrap(), &["rev-parse", "--abbrev-ref", "new@{upstream}"]).trim(),
        "origin/new"
    );
    let b = gityas_git::branches::list_branches(&fx.work, Some(gityas_core::models::BranchKind::Local)).unwrap();
    assert_eq!(b.iter().find(|b| b.name == "new").unwrap().upstream.as_deref(), Some("origin/new"));
}

#[test]
fn push_non_fast_forward_is_rejected_then_force_works() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    let other = fx.other_clone("other");
    commit_file(&other, "o", "o\n", "theirs");
    raw_push(&other, "refs/heads/main:refs/heads/main");
    let mine = commit_file(&fx.work, "m", "m\n", "mine");

    let err = remote::push_branch(&fx.work, "refs/heads/main", false, &ctx(&creds)).unwrap_err();
    assert!(matches!(&err, GitYasError::PushRejected(m) if m.contains("pull")), "{err:?}");
    assert_ne!(fx.remote().refname_to_id("refs/heads/main").unwrap(), mine);

    remote::push_branch(&fx.work, "refs/heads/main", true, &ctx(&creds)).unwrap();
    assert_eq!(fx.remote().refname_to_id("refs/heads/main").unwrap(), mine);
}

#[test]
fn push_errors() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    assert!(matches!(
        remote::push_branch(&fx.work, "refs/remotes/origin/main", false, &ctx(&creds)),
        Err(GitYasError::InvalidInput(_))
    ));
    assert!(matches!(
        remote::push_branch(&fx.work, "refs/heads/missing", false, &ctx(&creds)),
        Err(GitYasError::NotFound(_))
    ));
    // No remotes at all.
    let tmp = TempDir::new().unwrap();
    let lonely = init(tmp.path());
    commit_file(&lonely, "a", "a\n", "c");
    assert!(matches!(
        remote::push_branch(&lonely, "refs/heads/main", false, &ctx(&creds)),
        Err(GitYasError::InvalidInput(m)) if m.contains("no remotes")
    ));
    // Unborn branch.
    let tmp2 = TempDir::new().unwrap();
    let unborn = init(tmp2.path());
    unborn.remote("origin", &file_url(&fx.remote_path)).unwrap();
    assert!(matches!(
        remote::push_branch(&unborn, "refs/heads/main", false, &ctx(&creds)),
        Err(GitYasError::NotFound(_))
    ));
    // Several remotes, no upstream, no origin: must choose.
    let tmp3 = TempDir::new().unwrap();
    let multi = init(tmp3.path());
    commit_file(&multi, "a", "a\n", "c");
    multi.remote("a", &file_url(&fx.remote_path)).unwrap();
    multi.remote("b", &file_url(&fx.remote_path)).unwrap();
    assert!(remote::push_branch(&multi, "refs/heads/main", false, &ctx(&creds)).is_err());
    assert_eq!(remote::default_remote(&fx.work).unwrap(), "origin");
}

#[test]
fn push_from_detached_head_pushes_the_named_branch() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    let c2 = commit_file(&fx.work, "b", "b\n", "c2");
    fx.work.set_head_detached(c2).unwrap();
    remote::push_branch(&fx.work, "refs/heads/main", false, &ctx(&creds)).unwrap();
    assert_eq!(fx.remote().refname_to_id("refs/heads/main").unwrap(), c2);
}

#[test]
fn test_connection_local() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    remote::test_connection(&fx.work, "origin", &ctx(&creds)).unwrap();
    remote::test_connection_url(&file_url(&fx.remote_path), &ctx(&creds)).unwrap();
    assert!(remote::test_connection(&fx.work, "nope", &ctx(&creds)).is_err());
    assert!(remote::test_connection_url(&file_url(&fx.root.join("missing")), &ctx(&creds)).is_err());
    let refs = remote::list_remote_refs(&fx.work, "origin", &ctx(&creds)).unwrap();
    assert!(refs.contains(&"refs/heads/main".to_owned()), "{refs:?}");
}

/// An empty remote (no refs, e.g. a freshly created hosted repository): the connection test must not list refs,
/// because `git2::RemoteConnection::list` aborts the process in debug builds on an empty advertisement.
#[test]
fn test_connection_to_an_empty_remote() {
    let root = TempDir::new().unwrap();
    let empty = root.path().join("empty.git");
    git2::Repository::init_bare(&empty).unwrap();
    let work = git2::Repository::init(root.path().join("work")).unwrap();
    work.remote("origin", empty.to_str().unwrap()).unwrap();
    let creds = NoCredentials;
    remote::test_connection(&work, "origin", &ctx(&creds)).unwrap();
    remote::test_connection_url(&file_url(&empty), &ctx(&creds)).unwrap();
}

#[test]
fn pull_fast_forwards_from_upstream() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    let other = fx.other_clone("other");
    let c2 = commit_file(&other, "b", "b\n", "c2");
    raw_push(&other, "refs/heads/main:refs/heads/main");
    let out = remote::pull(&fx.work, "refs/heads/main", Some(&core_sig()), &ctx(&creds)).unwrap();
    assert!(matches!(out.merge, remote::MergeOutcome::FastForward { .. }));
    assert_eq!(head_oid(&fx.work), c2);
    assert_eq!(read(&fx.work, "b"), "b\n");
    // No upstream.
    gityas_git::branches::create_branch(&fx.work, "solo", None, false).unwrap();
    assert!(matches!(
        remote::pull(&fx.work, "refs/heads/solo", None, &ctx(&creds)),
        Err(GitYasError::InvalidInput(_))
    ));
    // Upstream deleted on the remote.
    raw_push(&fx.work, "refs/heads/main:refs/heads/gone");
    gityas_git::branches::create_branch(&fx.work, "g", None, false).unwrap();
    fx.work
        .find_branch("g", git2::BranchType::Local)
        .unwrap()
        .set_upstream(Some("origin/main"))
        .unwrap();
    let mut cfg = fx.work.config().unwrap();
    cfg.set_str("branch.g.merge", "refs/heads/gone").unwrap();
    fx.work.find_remote("origin").unwrap().push(&[":refs/heads/gone"], None).unwrap();
    let mut fo = git2::FetchOptions::new();
    fo.prune(git2::FetchPrune::On);
    fx.work
        .find_remote("origin")
        .unwrap()
        .fetch::<&str>(&[], Some(&mut fo), None)
        .unwrap();
    assert!(matches!(
        remote::pull(&fx.work, "refs/heads/g", None, &ctx(&creds)),
        Err(GitYasError::NotFound(_))
    ));
}

// ---- fake HTTP remote that always answers 401 ------------------------------------------------------------

struct FakeServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

/// Returns `None` when the sandbox forbids binding a local port (the test is then skipped with a note).
fn always_401_server() -> Option<FakeServer> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("skipping fake-HTTP test: cannot bind a local port ({e})");
            return None;
        }
    };
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            log.lock().unwrap().push(head);
            let body = "denied";
            let _ = write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    Some(FakeServer {
        url: format!("http://{addr}/o/r.git"),
        requests,
    })
}

/// Runs `f` on a thread and fails the test if it doesn't finish in time (a looping callback would hang).
fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("remote operation did not terminate: credential callback is looping")
}

/// Bug #10: a remote that rejects every credential must terminate with an auth error after the attempt cap.
#[test]
fn rejected_token_terminates_with_auth_error() {
    let Some(server) = always_401_server() else { return };
    let url = server.url.clone();
    let err = within(60, move || {
        let tmp = TempDir::new().unwrap();
        let repo = init(tmp.path());
        repo.remote("origin", &url).unwrap();
        let creds = OneToken(Provider::Github, "bad-token");
        let ctx = RemoteContext::new(&creds)
            .with_provider(Some(Provider::Github))
            .with_credential_helper(false);
        remote::fetch(&repo, Some("origin"), false, &ctx).unwrap_err()
    });
    assert!(matches!(&err, GitYasError::Auth(m) if m.contains("token was rejected")), "{err:?}");
    let requests = server.requests.lock().unwrap();
    let with_auth = requests
        .iter()
        .filter(|r| r.to_ascii_lowercase().contains("authorization: basic"))
        .count();
    assert_eq!(with_auth, 1, "the token is offered exactly once: {requests:?}");
    assert!(requests.len() <= 1 + remote::MAX_CREDENTIAL_ATTEMPTS as usize, "{requests:?}");
    assert!(
        !requests.iter().any(|r| r.contains("bad-token")),
        "token must not be sent in clear text"
    );
}

/// Never send a token to a remote whose provider isn't resolved (the old callback sent the GitHub
/// token to any host).
#[test]
fn unknown_provider_gets_no_token() {
    let Some(server) = always_401_server() else { return };
    let url = server.url.clone();
    let err = within(60, move || {
        let tmp = TempDir::new().unwrap();
        let repo = init(tmp.path());
        repo.remote("origin", &url).unwrap();
        let creds = OneToken(Provider::Github, "gh-token");
        let ctx = RemoteContext::new(&creds).with_credential_helper(false); // provider: detect -> None
        remote::test_connection(&repo, "origin", &ctx).unwrap_err()
    });
    assert!(matches!(&err, GitYasError::Auth(m) if m.contains("provider")), "{err:?}");
    let requests = server.requests.lock().unwrap();
    assert!(
        !requests.iter().any(|r| r.to_ascii_lowercase().contains("authorization")),
        "{requests:?}"
    );
}

#[test]
fn rejected_token_on_push_and_clone_terminates() {
    let Some(server) = always_401_server() else { return };
    let url = server.url.clone();
    let (push_err, clone_err) = within(60, move || {
        let tmp = TempDir::new().unwrap();
        let repo = init(&tmp.path().join("w"));
        commit_file(&repo, "a", "a\n", "c");
        repo.remote("origin", &url).unwrap();
        let creds = OneToken(Provider::Gitlab, "t");
        let ctx = RemoteContext::new(&creds)
            .with_provider(Some(Provider::Gitlab))
            .with_credential_helper(false);
        let push = remote::push_branch(&repo, "refs/heads/main", false, &ctx).unwrap_err();
        let clone = gityas_git::repository::clone(&url, &tmp.path().join("c"), &ctx).err().unwrap();
        (push, clone)
    });
    assert!(matches!(push_err, GitYasError::Auth(_)), "{push_err:?}");
    assert!(matches!(clone_err, GitYasError::Auth(_)), "{clone_err:?}");
}
