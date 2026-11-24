//! One read of every reference, shared by the history tips, the reference list and the tag list.
//!
//! Repositories with thousands of remote branches and tags keep them as thousands of loose files under `refs/`
//! until `git pack-refs` runs; reading those one by one dominated a refresh (~0.5 s for 3 300 refs on Windows, read
//! several times per refresh), so [`scan_refs`]:
//!
//! * lists `refs/` once (on Windows `read_dir` returns size and modification time without opening the files) and
//!   reads only the loose refs that are new or changed since the previous scan of the same [`RefScanCache`], in
//!   parallel when there are many. A file modified within [`RACY_WINDOW`] of the scan is never trusted later (the
//!   same "racy" rule git uses for its index): a second write within the file system's timestamp granularity could
//!   otherwise go unnoticed;
//! * takes `packed-refs` (and its peeled-tag hints) from gitoxide, which re-reads it only when it changed;
//! * peels every target to the commit it ultimately names with an object *header* lookup (kind only, no
//!   decompression) for the common case of a ref naming a commit, and a full peel for annotated tags. Peel results
//!   are cached by object id: objects are immutable, so they never go stale (unless replace refs are active, then
//!   nothing is cached).
//!
//! The loose/packed precedence and the name rules follow git's files backend (loose wins; `*.lock` files and
//! invalid names are ignored).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use gityas_core::models::{RefTip, Reference, Tag};
use gityas_core::{GitYasError, Result};
use gix::ObjectId;
use gix::bstr::ByteSlice;

use crate::gitoxide::{self, gix_error, oid_from_gix};

/// Loose ref files modified this recently are re-read on every scan.
pub const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Reading fewer files than this happens on the calling thread.
const PARALLEL_MIN_FILES: usize = 64;

/// One reference of a [`RefSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// Full name (`refs/heads/main`). Non-UTF-8 names are left out of the snapshot (logged).
    pub name: String,
    /// The object the ref names; `None` for a symbolic ref (`refs/remotes/origin/HEAD`).
    pub target: Option<ObjectId>,
    /// The commit the ref ultimately names (through annotated tags, and for a symbolic ref through the ref it
    /// points to); `None` when it names no commit.
    pub commit: Option<ObjectId>,
    /// `target` is an annotated tag object.
    pub annotated: bool,
}

/// Every reference, sorted by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefSnapshot {
    pub refs: Vec<RefEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Loose {
    Direct(ObjectId),
    /// `ref: <name>`
    Symbolic(String),
}

#[derive(Debug, Clone)]
struct LooseEntry {
    modified: SystemTime,
    len: u64,
    /// Read while the file was racy: re-read next time regardless of its timestamp.
    trusted: bool,
    /// `None`: the file is not a valid ref (ignored, as git does).
    value: Option<Loose>,
}

#[derive(Debug, Clone, Copy)]
struct Peeled {
    commit: Option<ObjectId>,
    annotated: bool,
}

/// State kept between scans of one repository: loose ref files already read and peel results. A fresh
/// (`Default`) cache is always correct, only slower.
#[derive(Debug, Default)]
pub struct RefScanCache {
    loose: HashMap<String, LooseEntry>,
    peeled: HashMap<ObjectId, Peeled>,
}

impl RefScanCache {
    /// Number of loose ref files and peel results held (diagnostics, tests).
    pub fn len(&self) -> (usize, usize) {
        (self.loose.len(), self.peeled.len())
    }

    pub fn is_empty(&self) -> bool {
        self.loose.is_empty() && self.peeled.is_empty()
    }
}

/// Reads every reference of `repo`, reusing what `cache` still holds. See the module docs.
pub fn scan_refs(repo: &gix::Repository, cache: &mut RefScanCache) -> Result<RefSnapshot> {
    let now = SystemTime::now();
    let files = list_loose(&repo.common_dir().join("refs"))?;

    // Files to (re)read: new, changed, or read while racy.
    let mut stale: Vec<&LooseFile> = Vec::new();
    for file in &files {
        let fresh = cache
            .loose
            .get(&file.name)
            .is_some_and(|e| e.trusted && e.modified == file.modified && e.len == file.len);
        if !fresh {
            stale.push(file);
        }
    }
    let read = read_loose(&stale)?;
    let mut keep: HashMap<String, LooseEntry> = HashMap::with_capacity(files.len());
    for file in &files {
        let entry = match read.get(file.name.as_str()) {
            Some(value) => LooseEntry {
                modified: file.modified,
                len: file.len,
                trusted: !is_racy(file.modified, now),
                value: value.clone(),
            },
            None => match cache.loose.remove(&file.name) {
                Some(entry) => entry,
                None => continue,
            },
        };
        keep.insert(file.name.clone(), entry);
    }
    cache.loose = keep;

    // name -> direct target (`None`: symbolic, see `symbolic`)
    let mut refs: HashMap<String, Option<ObjectId>> = HashMap::with_capacity(cache.loose.len());
    let mut symbolic: Vec<(String, String)> = Vec::new();
    for (name, entry) in &cache.loose {
        match &entry.value {
            Some(Loose::Direct(id)) => {
                refs.insert(name.clone(), Some(*id));
            }
            Some(Loose::Symbolic(target)) => {
                refs.insert(name.clone(), None);
                symbolic.push((name.clone(), target.clone()));
            }
            None => {}
        }
    }
    if let Some(packed) = repo.refs.cached_packed_buffer().map_err(gix_error)? {
        for reference in packed.iter().map_err(gix_error)? {
            let reference = match reference {
                Ok(reference) => reference,
                Err(err) => {
                    tracing::warn!(error = %format!("{err:#}"), "unreadable packed-refs line; skipped");
                    continue;
                }
            };
            let Ok(name) = reference.name.as_bstr().to_str() else {
                tracing::warn!(name = %reference.name.as_bstr(), "skipping reference with a non-UTF-8 name");
                continue;
            };
            if !refs.contains_key(name) {
                refs.insert(name.to_owned(), Some(reference.target()));
            }
        }
    }

    // Replace refs can change what an id peels to: then peel everything afresh.
    let use_peel_cache = !refs.keys().any(|name| name.starts_with("refs/replace/"));
    if !use_peel_cache {
        cache.peeled.clear();
    }
    let mut to_peel: Vec<ObjectId> = refs
        .values()
        .flatten()
        .filter(|id| !cache.peeled.contains_key(*id))
        .copied()
        .collect();
    to_peel.sort_unstable();
    to_peel.dedup();
    let peeled = peel_all(repo, &to_peel)?;
    if use_peel_cache {
        // Bounded by the refs' targets: drop results no ref names any more.
        let targets: std::collections::HashSet<&ObjectId> = refs.values().flatten().collect();
        cache.peeled.retain(|id, _| targets.contains(id));
    }
    cache.peeled.extend(peeled);
    let lookup = &cache.peeled;

    // A symbolic ref names whatever its referent names (followed up to 5 levels, as git does).
    let links: HashMap<&str, &str> = symbolic.iter().map(|(from, to)| (from.as_str(), to.as_str())).collect();
    let resolve = |start: &str| -> Option<ObjectId> {
        let mut name = links.get(start).copied()?;
        for _ in 0..5 {
            match refs.get(name)? {
                Some(id) => return Some(*id),
                None => name = links.get(name).copied()?,
            }
        }
        None
    };
    let resolved: HashMap<&str, ObjectId> = symbolic
        .iter()
        .filter_map(|(name, _)| Some((name.as_str(), resolve(name)?)))
        .collect();
    let mut out: Vec<RefEntry> = refs
        .iter()
        .map(|(name, target)| {
            let named = target.or_else(|| resolved.get(name.as_str()).copied());
            let peeled = named.and_then(|id| lookup.get(&id).copied());
            RefEntry {
                name: name.clone(),
                target: *target,
                commit: peeled.and_then(|p| p.commit),
                annotated: target.is_some() && peeled.is_some_and(|p| p.annotated),
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(RefSnapshot { refs: out })
}

fn is_racy(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified).map_or(true, |age| age < RACY_WINDOW)
}

struct LooseFile {
    name: String,
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

/// Every candidate loose ref file under `refs_dir` (`<common dir>/refs`), named `refs/...`.
fn list_loose(refs_dir: &Path) -> Result<Vec<LooseFile>> {
    let mut out = Vec::new();
    let mut dirs = vec![(refs_dir.to_path_buf(), "refs".to_owned())];
    while let Some((dir, prefix)) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                tracing::warn!(dir = %dir.display(), name = ?entry.file_name(), "skipping reference with a non-UTF-8 name");
                continue;
            };
            let name = format!("{prefix}/{file_name}");
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                dirs.push((entry.path(), name));
                continue;
            }
            if file_name.ends_with(".lock") || gix::validate::reference::name(name.as_bytes().as_bstr()).is_err() {
                continue;
            }
            let metadata = entry.metadata()?;
            out.push(LooseFile {
                name,
                path: entry.path(),
                modified: metadata.modified()?,
                len: metadata.len(),
            });
        }
    }
    Ok(out)
}

/// Parses a loose ref file: `ref: <name>` or a full hex object id (surrounding whitespace allowed).
fn parse_loose(content: &[u8]) -> Option<Loose> {
    let content = content.trim_ascii();
    if let Some(target) = content.strip_prefix(b"ref:") {
        return target.trim_ascii().to_str().ok().map(|name| Loose::Symbolic(name.to_owned()));
    }
    ObjectId::from_hex(content).ok().map(Loose::Direct)
}

/// Reads and parses `files` (in parallel when there are many). A file that vanished meanwhile (deleted or packed)
/// is left out; the packed entry, if any, then applies.
fn read_loose<'f>(files: &[&'f LooseFile]) -> Result<HashMap<&'f str, Option<Loose>>> {
    let read_one = |file: &LooseFile| -> Result<Option<Option<Loose>>> {
        match std::fs::read(&file.path) {
            Ok(content) => Ok(Some(parse_loose(&content))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    if files.len() < PARALLEL_MIN_FILES || threads < 2 {
        let mut out = HashMap::with_capacity(files.len());
        for file in files {
            if let Some(value) = read_one(file)? {
                out.insert(file.name.as_str(), value);
            }
        }
        return Ok(out);
    }
    type Part<'f> = Result<Vec<(&'f str, Option<Loose>)>>;
    let next = AtomicUsize::new(0);
    let parts: Vec<Part<'f>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| -> Part<'f> {
                    let mut out = Vec::new();
                    loop {
                        let start = next.fetch_add(32, Ordering::Relaxed);
                        if start >= files.len() {
                            return Ok(out);
                        }
                        for file in &files[start..(start + 32).min(files.len())] {
                            if let Some(value) = read_one(file)? {
                                out.push((file.name.as_str(), value));
                            }
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|_| Err(GitYasError::Other("ref reading thread panicked".into())))
            })
            .collect()
    });
    let mut out = HashMap::with_capacity(files.len());
    for part in parts {
        out.extend(part?);
    }
    Ok(out)
}

/// Peels each id to the commit it names. Header lookups only, except for annotated tags.
fn peel_all(repo: &gix::Repository, ids: &[ObjectId]) -> Result<Vec<(ObjectId, Peeled)>> {
    let peel = |repo: &gix::Repository, id: ObjectId| -> Result<Peeled> {
        let header = match repo.try_find_header(id).map_err(gix_error)? {
            Some(header) => header,
            None => {
                tracing::debug!(%id, "reference names a missing object");
                return Ok(Peeled {
                    commit: None,
                    annotated: false,
                });
            }
        };
        Ok(match header.kind() {
            gix::object::Kind::Commit => Peeled {
                commit: Some(id),
                annotated: false,
            },
            gix::object::Kind::Tag => Peeled {
                commit: repo.find_object(id).ok().and_then(|o| o.peel_to_commit().ok()).map(|c| c.id),
                annotated: true,
            },
            _ => Peeled {
                commit: None,
                annotated: false,
            },
        })
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    if ids.len() < 512 || threads < 2 {
        return ids.iter().map(|id| Ok((*id, peel(repo, *id)?))).collect();
    }
    let shared = repo.clone().into_sync();
    let chunk = ids.len().div_ceil(threads);
    let parts: Vec<Result<Vec<(ObjectId, Peeled)>>> = std::thread::scope(|scope| {
        let workers: Vec<_> = ids
            .chunks(chunk)
            .map(|part| {
                let shared = &shared;
                scope.spawn(move || {
                    let local = shared.to_thread_local();
                    part.iter().map(|id| Ok((*id, peel(&local, *id)?))).collect::<Result<Vec<_>>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|_| Err(GitYasError::Other("ref peeling thread panicked".into())))
            })
            .collect()
    });
    let mut out = Vec::with_capacity(ids.len());
    for part in parts {
        out.extend(part?);
    }
    Ok(out)
}

/// Ref namespaces that contribute history (see [`crate::cache::ref_tips`]).
pub const TIP_NAMESPACES: [&str; 3] = ["refs/heads/", "refs/remotes/", "refs/tags/"];

impl RefSnapshot {
    /// History tips: refs in [`TIP_NAMESPACES`] that name a commit, plus `HEAD` (as
    /// [`crate::cache::HEAD_TIP`]) when it names one; sorted by `(refname, oid)`.
    pub fn tips(&self, head_commit: Option<ObjectId>) -> Result<Vec<RefTip>> {
        let mut tips = Vec::new();
        for entry in &self.refs {
            if !TIP_NAMESPACES.iter().any(|ns| entry.name.starts_with(ns)) {
                continue;
            }
            match entry.commit {
                Some(commit) => tips.push(RefTip {
                    refname: entry.name.clone(),
                    oid: oid_from_gix(&commit)?,
                }),
                None if entry.target.is_some() => {
                    tracing::warn!(refname = entry.name, "ref does not peel to a commit; skipped");
                }
                None => {}
            }
        }
        if let Some(head) = head_commit {
            tips.push(RefTip {
                refname: crate::cache::HEAD_TIP.to_owned(),
                oid: oid_from_gix(&head)?,
            });
        }
        tips.sort();
        tips.dedup();
        Ok(tips)
    }

    /// Direct references naming (after peeling) a commit; see [`crate::refs::list_references`].
    pub fn references(&self) -> Result<Vec<Reference>> {
        let mut out = Vec::new();
        for entry in &self.refs {
            let (Some(_), Some(commit)) = (entry.target, entry.commit) else {
                continue;
            };
            out.push(Reference {
                short_name: crate::refs::shorthand(&entry.name).to_owned(),
                kind: crate::refs::kind_of(&entry.name),
                target: oid_from_gix(&commit)?,
                name: entry.name.clone(),
            });
        }
        Ok(out)
    }

    /// Tags naming (after peeling) a commit, with annotations read from `repo`; see [`crate::refs::list_tags`].
    pub fn tags(&self, repo: &gix::Repository) -> Result<Vec<Tag>> {
        let mut out = Vec::new();
        let mut buf = Vec::new();
        for entry in &self.refs {
            let Some(name) = entry.name.strip_prefix("refs/tags/") else {
                continue;
            };
            let (Some(target), Some(commit)) = (entry.target, entry.commit) else {
                continue;
            };
            out.push(Tag {
                name: name.to_owned(),
                target: oid_from_gix(&commit)?,
                annotation: if entry.annotated {
                    crate::refs::annotation(repo, &target, &mut buf)
                } else {
                    None
                },
            });
        }
        Ok(out)
    }
}

/// The commit `HEAD` names, `None` when unborn or not a commit.
pub fn head_commit(repo: &gix::Repository) -> Result<Option<ObjectId>> {
    let mut head = repo.head().map_err(gix_error)?;
    if head.is_unborn() {
        return Ok(None);
    }
    match head.peel_to_commit() {
        Ok(commit) => Ok(Some(commit.id)),
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "HEAD does not peel to a commit; skipped");
            Ok(None)
        }
    }
}

/// [`scan_refs`] with a throwaway cache (one-off callers).
pub fn scan_refs_once<R: gitoxide::GixRepo + ?Sized>(repo: &R) -> Result<(RefSnapshot, std::borrow::Cow<'_, gix::Repository>)> {
    let repo = repo.gix()?;
    let snapshot = scan_refs(&repo, &mut RefScanCache::default())?;
    Ok((snapshot, repo))
}
