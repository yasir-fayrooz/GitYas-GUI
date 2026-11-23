//! WP4: references and tags (create / delete local and remote, push, list remote tag names).
//!
//! `Reference::target` and `Tag::target` are always the **peeled commit** (the old code keyed annotated tags by
//! the tag object id, so they never decorated their commit).

use git2::{ErrorCode, ObjectType, Repository};
use gityas_core::models::{BranchKind, CommitSignature, Reference, ReferenceKind, Tag, TagAnnotation};
use gityas_core::{GitYasError, Oid, Result};

use crate::convert::{oid_from_git, oid_to_git};
use crate::git_error;
use crate::remote::{RemoteContext, list_remote_refs, push_refspecs};
use crate::repository::{git_signature, ref_exists, signature_from_git};

fn kind_of(name: &str) -> ReferenceKind {
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

/// All direct references (branches, remote branches, tags, notes, `refs/stash`, others), peeled to commits.
/// Skipped: symbolic refs (`<remote>/HEAD`, which only alias another listed ref), refs not pointing (after
/// peeling) at a commit (e.g. a tag of a tree, or the notes ref), and non-UTF-8 names (logged).
pub fn list_references(repo: &Repository) -> Result<Vec<Reference>> {
    let mut out = Vec::new();
    for r in repo.references().map_err(git_error)? {
        let r = r.map_err(git_error)?;
        if r.kind() == Some(git2::ReferenceType::Symbolic) {
            continue;
        }
        let Some(name) = r.name().map(str::to_owned) else {
            tracing::warn!(name = %String::from_utf8_lossy(r.name_bytes()), "skipping reference with a non-UTF-8 name");
            continue;
        };
        let commit = match r.peel(ObjectType::Commit) {
            Ok(c) => c,
            Err(e) if matches!(e.code(), ErrorCode::InvalidSpec | ErrorCode::NotFound | ErrorCode::Peel) => {
                tracing::debug!(%name, "reference does not point at a commit");
                continue;
            }
            Err(e) => return Err(git_error(e)),
        };
        out.push(Reference {
            short_name: r.shorthand().map_or_else(|| name.clone(), str::to_owned),
            kind: kind_of(&name),
            target: oid_from_git(commit.id())?,
            name,
        });
    }
    Ok(out)
}

/// All tags pointing (after peeling) at commits, with the annotation for annotated tags.
pub fn list_tags(repo: &Repository) -> Result<Vec<Tag>> {
    let mut out = Vec::new();
    for r in repo.references_glob("refs/tags/*").map_err(git_error)? {
        let r = r.map_err(git_error)?;
        let Some(name) = r.name().and_then(|n| n.strip_prefix("refs/tags/")).map(str::to_owned) else {
            tracing::warn!(name = %String::from_utf8_lossy(r.name_bytes()), "skipping tag with a non-UTF-8 name");
            continue;
        };
        let Some(tag) = tag_from_ref(repo, &name, &r)? else { continue };
        out.push(tag);
    }
    Ok(out)
}

fn tag_from_ref(repo: &Repository, name: &str, r: &git2::Reference<'_>) -> Result<Option<Tag>> {
    let commit = match r.peel(ObjectType::Commit) {
        Ok(c) => c,
        Err(e) if matches!(e.code(), ErrorCode::InvalidSpec | ErrorCode::NotFound | ErrorCode::Peel) => return Ok(None),
        Err(e) => return Err(git_error(e)),
    };
    let annotation = match r.target() {
        Some(oid) => match repo.find_tag(oid) {
            Ok(tag) => Some(TagAnnotation {
                // Display text; non-UTF-8 tag messages are decoded lossily.
                message: tag
                    .message_bytes()
                    .map(|m| String::from_utf8_lossy(m).into_owned())
                    .unwrap_or_default(),
                tagger: tag.tagger().map(|s| signature_from_git(&s)),
            }),
            Err(e) if matches!(e.code(), ErrorCode::NotFound | ErrorCode::InvalidSpec) => None,
            Err(e) => return Err(git_error(e)),
        },
        None => None,
    };
    Ok(Some(Tag {
        name: name.to_owned(),
        target: oid_from_git(commit.id())?,
        annotation,
    }))
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
    let r = repo.find_reference(&format!("refs/tags/{name}")).map_err(git_error)?;
    tag_from_ref(repo, &name, &r)?.ok_or_else(|| GitYasError::Other(format!("tag {name:?} was created but cannot be read back")))
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
