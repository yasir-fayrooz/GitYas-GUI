//! References and tags (create / delete local and remote, push, list remote tag names).
//!
//! `Reference::target` and `Tag::target` are always the **peeled commit** (the old code keyed annotated tags by
//! the tag object id, so they never decorated their commit).

use git2::{ErrorCode, Repository};
use gityas_core::models::{BranchKind, CommitSignature, Reference, ReferenceKind, Tag, TagAnnotation};
use gityas_core::{GitYasError, Oid, Result};

use crate::convert::oid_to_git;
use crate::git_error;
use crate::gitoxide::{self, GixRepo, gix_error, oid_from_gix};
use crate::remote::{RemoteContext, list_remote_refs, push_refspecs};
use crate::repository::{git_signature, ref_exists};
use gix::objs::Find as _;

pub(crate) fn kind_of(name: &str) -> ReferenceKind {
    if name.starts_with("refs/heads/") {
        ReferenceKind::Branch(BranchKind::Local)
    } else if name.starts_with("refs/remotes/") {
        ReferenceKind::Branch(BranchKind::Remote)
    } else if name.starts_with("refs/tags/") {
        ReferenceKind::Tag
    } else if name.starts_with("refs/notes/") {
        ReferenceKind::Note
    } else if name == "refs/stash" {
        ReferenceKind::Stash
    } else {
        ReferenceKind::Other
    }
}

/// libgit2's `git_reference__shorthand`.
pub(crate) fn shorthand(name: &str) -> &str {
    ["refs/heads/", "refs/tags/", "refs/remotes/", "refs/"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name)
}

/// A direct reference's name and the commit it peels to; `None` for symbolic refs, non-UTF-8 names (logged) and
/// refs that do not peel to a commit.
fn peeled_ref(reference: &mut gix::Reference<'_>) -> Option<(String, gix::ObjectId)> {
    if matches!(reference.target(), gix::refs::TargetRef::Symbolic(_)) {
        return None;
    }
    let bytes: &[u8] = reference.name().as_bstr();
    let Ok(name) = std::str::from_utf8(bytes) else {
        tracing::warn!(name = %String::from_utf8_lossy(bytes), "skipping reference with a non-UTF-8 name");
        return None;
    };
    let name = name.to_owned();
    match reference.peel_to_commit() {
        Ok(commit) => Some((name, commit.id)),
        Err(err) => {
            tracing::debug!(%name, error = %format!("{err:#}"), "reference does not point at a commit");
            None
        }
    }
}

/// All direct references (branches, remote branches, tags, notes, `refs/stash`, others), peeled to commits.
/// Skipped: symbolic refs (`<remote>/HEAD`, which only alias another listed ref), refs not pointing (after
/// peeling) at a commit (e.g. a tag of a tree, or the notes ref), and non-UTF-8 names (logged). Read with gitoxide.
pub fn list_references<R: GixRepo + ?Sized>(repo: &R) -> Result<Vec<Reference>> {
    crate::refscan::scan_refs_once(repo)?.0.references()
}

/// All tags pointing (after peeling) at commits, with the annotation for annotated tags. Read with gitoxide.
pub fn list_tags<R: GixRepo + ?Sized>(repo: &R) -> Result<Vec<Tag>> {
    let (snapshot, repo) = crate::refscan::scan_refs_once(repo)?;
    snapshot.tags(&repo)
}

/// The tag behind `reference` (a `refs/tags/*` ref), or `None` if it does not peel to a commit.
fn tag_from_reference(repo: &gix::Repository, reference: &mut gix::Reference<'_>, buf: &mut Vec<u8>) -> Result<Option<Tag>> {
    let direct = reference.try_id().map(|id| id.detach());
    let Some((refname, commit)) = peeled_ref(reference) else {
        return Ok(None);
    };
    let Some(name) = refname.strip_prefix("refs/tags/") else {
        return Ok(None);
    };
    Ok(Some(Tag {
        name: name.to_owned(),
        target: oid_from_gix(&commit)?,
        annotation: direct.and_then(|id| annotation(repo, &id, buf)),
    }))
}

/// The annotation of tag object `id` (`None` when `id` is not a tag object). The message is everything after the
/// header, signature included, as libgit2's `git_tag_message` (gitoxide splits the signature off); display text,
/// decoded lossily.
pub(crate) fn annotation(repo: &gix::Repository, id: &gix::oid, buf: &mut Vec<u8>) -> Option<TagAnnotation> {
    let data = match repo.objects.try_find(id, buf) {
        Ok(Some(data)) if data.kind == gix::object::Kind::Tag => data,
        Ok(_) => return None,
        Err(err) => {
            tracing::debug!(error = %format!("{err:#}"), "unreadable tag object");
            return None;
        }
    };
    let raw = data.data;
    let tagger = gix::objs::TagRef::from_bytes(raw, data.object_hash)
        .ok()
        .and_then(|tag| tag.tagger().ok().flatten())
        .map(|sig| CommitSignature {
            name: String::from_utf8_lossy(gitoxide::trim(sig.name)).into_owned(),
            email: String::from_utf8_lossy(gitoxide::trim(sig.email)).into_owned(),
        });
    let message = raw.windows(2).position(|w| w == b"\n\n").map_or(&[][..], |at| &raw[at + 2..]);
    let message = message.iter().position(|&b| b == 0).map_or(message, |nul| &message[..nul]);
    Some(TagAnnotation {
        message: String::from_utf8_lossy(message).into_owned(),
        tagger,
    })
}

fn normalize_tag_name(name: &str) -> Result<String> {
    let name = name.trim();
    let name = name.strip_prefix("refs/tags/").unwrap_or(name);
    // `git2::Tag::is_valid_name` unwraps a CString, so reject NUL first.
    if name.is_empty() || name.contains('\0') || !git2::Tag::is_valid_name(name) {
        return Err(GitYasError::InvalidInput(format!("{name:?} is not a valid tag name")));
    }
    Ok(name.to_owned())
}

/// Creates a tag on `target`. With `message` it is annotated and needs a valid `signature` (never panics on an
/// empty one); without, it is lightweight. Fails if the tag already exists.
pub fn create_tag(repo: &Repository, name: &str, target: &Oid, message: Option<&str>, signature: Option<&CommitSignature>) -> Result<Tag> {
    let name = normalize_tag_name(name)?;
    let commit = repo.find_commit(oid_to_git(target)?).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("commit {target} does not exist")),
        _ => git_error(e),
    })?;
    let exists = |e: git2::Error| match e.code() {
        ErrorCode::Exists => GitYasError::AlreadyExists(format!("tag {name:?}")),
        _ => git_error(e),
    };
    match message {
        Some(message) => {
            let message = git2::message_prettify(message, None).map_err(git_error)?;
            if message.trim().is_empty() {
                return Err(GitYasError::InvalidInput("an annotated tag needs a message".to_owned()));
            }
            let sig = git_signature(signature, "create an annotated tag")?;
            repo.tag(&name, commit.as_object(), &sig, &message, false).map_err(exists)?;
        }
        None => {
            repo.tag_lightweight(&name, commit.as_object(), false).map_err(exists)?;
        }
    }
    let gix_repo = repo.gix()?;
    let mut reference = gix_repo.find_reference(format!("refs/tags/{name}").as_str()).map_err(gix_error)?;
    tag_from_reference(&gix_repo, &mut reference, &mut Vec::new())?
        .ok_or_else(|| GitYasError::Other(format!("tag {name:?} was created but cannot be read back")))
}

/// Deletes local tag `name` (`v1` or `refs/tags/v1`).
pub fn delete_tag(repo: &Repository, name: &str) -> Result<()> {
    let name = normalize_tag_name(name)?;
    repo.tag_delete(&name).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("tag {name:?} does not exist")),
        _ => git_error(e),
    })
}

/// Pushes local tag `name` to `remote` (non-forced: an existing different remote tag is rejected).
pub fn push_tag(repo: &Repository, remote: &str, name: &str, ctx: &RemoteContext<'_>) -> Result<()> {
    let name = normalize_tag_name(name)?;
    let refname = format!("refs/tags/{name}");
    if !ref_exists(repo, &refname)? {
        return Err(GitYasError::NotFound(format!("tag {name:?} does not exist locally")));
    }
    push_refspecs(repo, remote, &[format!("{refname}:{refname}")], ctx).map_err(|err| match err {
        // libgit2 reports a moved tag as non-fast-forward; pulling would not help.
        GitYasError::PushRejected(_) => GitYasError::Git(format!(
            "the remote rejected the push: tag {name:?} already exists there at a different commit"
        )),
        other => other,
    })
}

/// Deletes tag `name` on `remote` (the local tag is kept).
pub fn delete_remote_tag(repo: &Repository, remote: &str, name: &str, ctx: &RemoteContext<'_>) -> Result<()> {
    let name = normalize_tag_name(name)?;
    push_refspecs(repo, remote, &[format!(":refs/tags/{name}")], ctx)
}

/// Tag names advertised by `remote` (without `refs/tags/`; annotated tags' peeled `^{}` entries removed —
/// the old code listed every annotated tag twice).
pub fn list_remote_tag_names(repo: &Repository, remote: &str, ctx: &RemoteContext<'_>) -> Result<Vec<String>> {
    Ok(list_remote_refs(repo, remote, ctx)?
        .into_iter()
        .filter_map(|r| r.strip_prefix("refs/tags/").map(str::to_owned))
        .collect())
}
