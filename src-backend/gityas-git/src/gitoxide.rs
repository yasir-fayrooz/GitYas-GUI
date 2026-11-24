//! gitoxide (`gix`) plumbing shared by the history ([`crate::commits`], [`crate::cache`]), graph queries
//! ([`crate::branches`], merge-base) and transfer (fetch / clone) code.
//!
//! * [`GixRepo`] lets every history function take either a `gix::Repository` (the engine passes its cached handle,
//!   so packs stay mapped between commands) or a `git2::Repository` (tests and the git2-based modules; gix is then
//!   opened from the same path).
//! * [`decode_rows`] turns commit ids into [`Commit`] rows, in parallel for long lists: `gix::Repository` is cheap
//!   to clone per thread, unlike `git2::Repository`, which is `!Sync`.
//! * Rows are byte-identical to what libgit2 produced ([`summary`] and [`body`] are ports of `git_commit_summary` /
//!   `git_commit_body`, signatures are trimmed like libgit2's `extract_trimmed`), so histories cached by earlier
//!   versions stay valid and the cache-vs-live parity tests keep their meaning.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use gityas_core::models::Commit;
use gityas_core::{GitYasError, Oid, Result};
use gix::ObjectId;
use gix::objs::FindExt;

/// Decoded-object cache of each handle (bytes). The newest-first walk reads every commit twice (time, then row).
pub const OBJECT_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Lists shorter than this are decoded on the calling thread (thread start-up would cost more than it saves).
const PARALLEL_MIN_COMMITS: usize = 4096;

/// Ids handed to a worker at a time (small enough to balance uneven delta chains, large enough to amortise the
/// shared counter).
const BLOCK: usize = 1024;

/// Maps any gitoxide error (alternate `Display` joins the whole cause chain on one line).
pub fn gix_error(err: impl std::fmt::Display) -> GitYasError {
    GitYasError::Git(format!("{err:#}"))
}

/// Identity gitoxide falls back to when neither `user.*` nor `committer.*` is configured (fresh machines, CI). It
/// only ever appears in reflog entries of refs that fetch / clone update: gitoxide refuses to write those without a
/// committer, where libgit2 and the git CLI fall back to a default identity. Commits are never made with it (they
/// go through libgit2 and require the commit signature set in Settings).
pub const IDENTITY_FALLBACK: [&str; 2] = [
    "gitoxide.committer.nameFallback=GitYas",
    "gitoxide.committer.emailFallback=gityas@localhost",
];

/// Opens `path` (a working tree, a `.git` dir or a bare repository) with gitoxide, with the full configuration
/// (global and system included: fetch needs credential helpers, proxies and `core.sshCommand`) plus
/// [`IDENTITY_FALLBACK`].
pub fn open(path: &Path) -> Result<gix::Repository> {
    let options = gix::open::Options::default().config_overrides(IDENTITY_FALLBACK);
    let mut repo = gix::open_opts(path, options).map_err(|err| {
        if !path.exists() {
            GitYasError::NotFound(format!("{} is not a git repository", path.display()))
        } else {
            gix_error(err)
        }
    })?;
    repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
    Ok(repo)
}

/// A repository the history functions can read with gitoxide.
pub trait GixRepo {
    /// The gitoxide handle: borrowed when this already is one, freshly opened otherwise.
    fn gix(&self) -> Result<Cow<'_, gix::Repository>>;
}

impl GixRepo for gix::Repository {
    fn gix(&self) -> Result<Cow<'_, gix::Repository>> {
        Ok(Cow::Borrowed(self))
    }
}

impl GixRepo for git2::Repository {
    fn gix(&self) -> Result<Cow<'_, gix::Repository>> {
        // The working tree when there is one: for a linked worktree, `path()` is `.git/worktrees/<name>`.
        open(self.workdir().unwrap_or_else(|| self.path())).map(Cow::Owned)
    }
}

pub fn oid_from_gix(id: &gix::oid) -> Result<Oid> {
    Oid::from_bytes(id.as_bytes())
}

pub fn oid_to_gix(oid: &Oid) -> Result<ObjectId> {
    ObjectId::try_from(oid.as_bytes()).map_err(gix_error)
}

/// For the git2-based modules: a libgit2 id as a gitoxide one.
pub fn oid_from_git2(oid: git2::Oid) -> Result<ObjectId> {
    ObjectId::try_from(oid.as_bytes()).map_err(gix_error)
}

/// For the git2-based modules: a gitoxide id as a libgit2 one.
pub fn oid_to_git2(id: &gix::oid) -> Result<git2::Oid> {
    git2::Oid::from_bytes(id.as_bytes()).map_err(crate::git_error)
}

/// The best common ancestor of `one` and `two` (commit-graph accelerated), or `None` for unrelated histories.
pub fn merge_base(repo: &gix::Repository, one: ObjectId, two: ObjectId) -> Result<Option<ObjectId>> {
    Ok(repo.merge_base(one, two).map_err(gix_error)?.map(|id| id.detach()))
}

/// `true` when `ancestor` is reachable from `descendant` by parents; a commit is its own ancestor (unlike libgit2's
/// `graph_descendant_of`).
pub fn is_ancestor(repo: &gix::Repository, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
    repo.is_ancestor(ancestor, descendant).map_err(gix_error)
}

/// Ahead / behind counts of several `(tip, base)` pairs, sharing one revision graph (and the commit-graph file)
/// across pairs: branches usually share most of their history, so later pairs reuse the commits already loaded.
pub struct AheadBehind<'repo> {
    repo: &'repo gix::Repository,
    graph: gix::revwalk::Graph<'repo, 'repo, gix::revwalk::graph::Commit<gix::revision::plumbing::merge_base::Flags>>,
}

impl<'repo> AheadBehind<'repo> {
    /// `cache` is the commit-graph file (from `repo.commit_graph_if_enabled()`), kept alive by the caller.
    pub fn new(repo: &'repo gix::Repository, cache: Option<&'repo gix::commitgraph::Graph>) -> Self {
        Self {
            repo,
            graph: repo.revision_graph(cache),
        }
    }

    /// `(ahead, behind)`: commits only in `tip`, commits only in `base` (libgit2's `graph_ahead_behind(tip, base)`).
    pub fn counts(&mut self, tip: ObjectId, base: ObjectId) -> Result<(usize, usize)> {
        let outcome = self
            .repo
            .ahead_behind_counts_with_graph(tip, base, &mut self.graph)
            .map_err(gix_error)?;
        Ok((outcome.ahead, outcome.behind))
    }
}

/// `git__isspace`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r' | b'\x0b')
}

/// `git__isspace_nonlf`.
fn is_space_nonlf(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\x0c' | b'\r' | b'\x0b')
}

/// `git_commit_message`: the raw message as libgit2 sees it (a C string, so cut at the first NUL) without its
/// leading newlines.
fn libgit2_message(raw: &[u8]) -> &[u8] {
    let raw = raw.iter().position(|&b| b == 0).map_or(raw, |nul| &raw[..nul]);
    let start = raw.iter().position(|&b| b != b'\n').unwrap_or(raw.len());
    &raw[start..]
}

/// Port of libgit2's `git_commit_summary`: the first paragraph, whitespace runs that contain a newline collapsed to
/// one space, other runs kept, leading and trailing whitespace dropped. A paragraph also ends at a line holding only
/// whitespace.
pub fn summary(raw_message: &[u8]) -> Vec<u8> {
    let msg = libgit2_message(raw_message);
    let mut out = Vec::with_capacity(msg.len().min(128));
    let mut space: Option<usize> = None;
    let mut space_contains_newline = false;
    for (i, &c) in msg.iter().enumerate() {
        if c == b'\n' {
            let rest = &msg[i + 1..];
            match rest.iter().position(|&b| !is_space_nonlf(b)) {
                None => break,
                Some(n) if rest[n] == b'\n' => break,
                Some(_) => {}
            }
        }
        if is_space(c) {
            if space.is_none() {
                space = Some(i);
                space_contains_newline = false;
            }
            space_contains_newline |= c == b'\n';
        } else {
            if let Some(start) = space.take() {
                if space_contains_newline {
                    out.push(b' ');
                } else {
                    out.extend_from_slice(&msg[start..i]);
                }
            }
            out.push(c);
        }
    }
    out
}

/// Port of libgit2's `git_commit_body`: everything after the first paragraph (which ends at an empty line or the
/// end), trimmed; `None` when that is empty.
pub fn body(raw_message: &[u8]) -> Option<&[u8]> {
    let msg = libgit2_message(raw_message);
    let mut start = 0;
    while start < msg.len() {
        if msg[start] == b'\n' && (start + 1 == msg.len() || msg[start + 1] == b'\n') {
            break;
        }
        start += 1;
    }
    while start < msg.len() && is_space(msg[start]) {
        start += 1;
    }
    let mut end = msg.len();
    while end > start && is_space(msg[end - 1]) {
        end -= 1;
    }
    (start < end).then(|| &msg[start..end])
}

/// libgit2's `extract_trimmed` (ASCII whitespace only, as `git__isspace`).
pub(crate) fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| !is_space(b)).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|&b| !is_space(b)).map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// The listed/cached row of commit `id`, from its decoded object.
pub fn row_from_commit(id: &gix::oid, commit: &gix::objs::CommitRef<'_>) -> Result<Commit> {
    let author = commit.author().map_err(gix_error)?;
    let committer = commit.committer().map_err(gix_error)?;
    let mut lossy = false;
    let mut decode = |bytes: &[u8]| match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => {
            lossy = true;
            String::from_utf8_lossy(bytes).into_owned()
        }
    };
    let author_name = decode(trim(author.name));
    let author_email = decode(trim(author.email));
    let summary = decode(&summary(commit.message));
    let description = body(commit.message)
        .map(|body| Commit::description_preview(&String::from_utf8_lossy(body)))
        .unwrap_or_default();
    Ok(Commit {
        id: oid_from_gix(id)?,
        parents: commit.parents().map(|p| oid_from_gix(&p)).collect::<Result<Vec<_>>>()?,
        author_name,
        author_email,
        author_time: author.seconds(),
        committer_time: committer.seconds(),
        summary,
        description,
        lossy,
    })
}

/// Reads and converts one commit, reusing `buf`.
pub fn read_row(repo: &gix::Repository, id: &gix::oid, buf: &mut Vec<u8>) -> Result<Commit> {
    let commit = repo.objects.find_commit(id, buf).map_err(gix_error)?;
    row_from_commit(id, &commit)
}

/// Rows of `ids`, in **unspecified order** (callers sort with `commits::sort_commits`). Long lists are split
/// across up to `available_parallelism` threads, each with its own thread-local handle (own object and pack
/// caches); the packs themselves are shared and memory-mapped once.
pub fn decode_rows(repo: &gix::Repository, ids: &[ObjectId]) -> Result<Vec<Commit>> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(ids.len() / BLOCK + 1);
    if ids.len() < PARALLEL_MIN_COMMITS || threads < 2 {
        let mut buf = Vec::new();
        return ids.iter().map(|id| read_row(repo, id, &mut buf)).collect();
    }
    let shared = repo.clone().into_sync();
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<GitYasError>> = Mutex::new(None);
    let parts: Vec<Vec<Commit>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut local = shared.to_thread_local();
                    local.object_cache_size(None);
                    let mut buf = Vec::new();
                    let mut rows = Vec::with_capacity(ids.len() / threads + BLOCK);
                    loop {
                        let start = next.fetch_add(BLOCK, Ordering::Relaxed);
                        if start >= ids.len() || failed.lock().map_or(true, |f| f.is_some()) {
                            return rows;
                        }
                        for id in &ids[start..(start + BLOCK).min(ids.len())] {
                            match read_row(&local, id, &mut buf) {
                                Ok(row) => rows.push(row),
                                Err(err) => {
                                    if let Ok(mut slot) = failed.lock() {
                                        slot.get_or_insert(err);
                                    }
                                    return rows;
                                }
                            }
                        }
                    }
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap_or_default()).collect()
    });
    if let Some(err) = failed.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(err);
    }
    let mut rows = Vec::with_capacity(ids.len());
    for part in parts {
        rows.extend(part);
    }
    if rows.len() != ids.len() {
        return Err(GitYasError::Other("internal error: a commit decoding thread stopped early".into()));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw messages covering every branch of `git_commit_summary` / `git_commit_body`.
    const MESSAGES: &[&[u8]] = &[
        b"subject",
        b"subject\n",
        b"\n\nsubject\n\nbody",
        b"one\ntwo\n\nbody",
        b"a  b\t c",
        b"a \n  b",
        b"  lead",
        b"\t\r lead\nnext",
        b"trail   ",
        b"trail \n",
        b"para\n   \nbody",
        b"para\n \t\x0b\x0c\r\nbody\n",
        b"cut\0hidden\n\nbody",
        b"",
        b"\n\n",
        b"   ",
        b"one\r\ntwo\r\n\r\nbody\r\n",
        b"one\ntwo\nthree\n\n\n  body line 1\n\n  body line 2  \n\n\n",
        b"subject\n\n   \n",
        b"x\n\ny\n\nz",
        b"\xff\xfe not utf-8\n\nbody \xc3",
        "unicode \u{a0}nbsp\u{2003}em\n\nbody".as_bytes(),
    ];

    /// Builds the commit object in memory and asks libgit2 (the reference implementation).
    fn libgit2_view(message: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
        let mut raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            author A U Thor <a@example.com> 1700000000 +0000\n\
            committer C O Mitter <c@example.com> 1700000001 +0000\n\n"
            .to_vec();
        raw.extend_from_slice(message);
        let tmp = tempfile::TempDir::new().unwrap_or_else(|e| panic!("{e}"));
        let repo = git2::Repository::init_bare(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
        let id = repo
            .odb()
            .and_then(|odb| odb.write(git2::ObjectType::Commit, &raw))
            .unwrap_or_else(|e| panic!("{e}"));
        let commit = repo.find_commit(id).unwrap_or_else(|e| panic!("{e}"));
        (
            commit.summary_bytes().unwrap_or_default().to_vec(),
            commit.body_bytes().map(<[u8]>::to_vec),
        )
    }

    #[test]
    fn summary_and_body_match_libgit2() {
        for message in MESSAGES {
            let (want_summary, want_body) = libgit2_view(message);
            let shown = String::from_utf8_lossy(message);
            assert_eq!(summary(message), want_summary, "summary of {shown:?}");
            assert_eq!(body(message).map(<[u8]>::to_vec), want_body, "body of {shown:?}");
        }
    }

    #[test]
    fn rows_match_the_libgit2_rows() {
        let tmp = tempfile::TempDir::new().unwrap_or_else(|e| panic!("{e}"));
        let repo = git2::Repository::init_bare(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
        let odb = repo.odb().unwrap_or_else(|e| panic!("{e}"));
        let mut parent: Option<git2::Oid> = None;
        let mut ids = Vec::new();
        for (i, message) in MESSAGES.iter().enumerate() {
            let mut raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n".to_vec();
            if let Some(parent) = parent {
                raw.extend_from_slice(format!("parent {parent}\n").as_bytes());
            }
            raw.extend_from_slice(format!("author  \tName {i}  <  mail{i}@example.com > 17000{i:05} +0100\n").as_bytes());
            raw.extend_from_slice(format!("committer C <c@example.com> 18000{i:05} -0230\n\n").as_bytes());
            raw.extend_from_slice(message);
            let id = odb.write(git2::ObjectType::Commit, &raw).unwrap_or_else(|e| panic!("{e}"));
            parent = Some(id);
            ids.push(id);
        }
        let gix_repo = open(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
        let mut buf = Vec::new();
        for id in ids {
            let want =
                crate::commits::commit_row(&repo.find_commit(id).unwrap_or_else(|e| panic!("{e}"))).unwrap_or_else(|e| panic!("{e}"));
            let gid = ObjectId::try_from(id.as_bytes()).unwrap_or_else(|e| panic!("{e}"));
            let got = read_row(&gix_repo, &gid, &mut buf).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(got, want);
        }
    }

    /// CI machines and fresh installs have no `user.name`: fetch must still be able to write reflog entries.
    #[test]
    fn identity_fallback_gives_gitoxide_a_committer() {
        let tmp = tempfile::TempDir::new().unwrap_or_else(|e| panic!("{e}"));
        git2::Repository::init(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
        let bare = gix::open::Options::isolated().open(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
        assert!(bare.committer().is_none(), "isolated: no identity configured");
        let repo = gix::open::Options::isolated()
            .config_overrides(IDENTITY_FALLBACK)
            .open(tmp.path())
            .unwrap_or_else(|e| panic!("{e}"));
        let committer = repo.committer().and_then(Result::ok).unwrap_or_else(|| panic!("no committer"));
        assert_eq!(
            (committer.name.to_string(), committer.email.to_string()),
            ("GitYas".into(), "gityas@localhost".into())
        );
    }

    #[test]
    fn trim_is_ascii_whitespace_only() {
        assert_eq!(trim(b"  a b \t"), b"a b");
        assert_eq!(trim(b"   "), b"");
        assert_eq!(trim("\u{a0}x".as_bytes()), "\u{a0}x".as_bytes());
    }
}
