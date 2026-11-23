//! WP4: references and tags (local + remote).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test crate: panics are the assertions

mod wp4_support;

use gityas_core::GitYasError;
use gityas_core::models::{BranchKind, CommitSignature, ReferenceKind};
use gityas_git::refs;
use gityas_git::remote::{NoCredentials, RemoteContext};
use tempfile::TempDir;
use wp4_support::*;

fn ctx(creds: &NoCredentials) -> RemoteContext<'_> {
    RemoteContext::new(creds).with_credential_helper(false)
}

#[test]
fn unborn_repo_has_no_refs() {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    assert!(refs::list_references(&repo).unwrap().is_empty());
    assert!(refs::list_tags(&repo).unwrap().is_empty());
}

#[test]
fn references_are_peeled_and_classified() {
    let mut fx = Fixture::new();
    let c = head_oid(&fx.work);
    refs::create_tag(&fx.work, "light", &oid(c), None, None).unwrap();
    refs::create_tag(&fx.work, "v1.0", &oid(c), Some("Release 1.0"), Some(&core_sig())).unwrap();
    // Tag of a tree: not a commit, skipped.
    {
        let tree = fx.work.head().unwrap().peel_to_tree().unwrap();
        fx.work.tag_lightweight("tree-tag", tree.as_object(), false).unwrap();
    }
    fx.work
        .reference_symbolic("refs/remotes/origin/HEAD", "refs/remotes/origin/main", true, "t")
        .unwrap();
    // A stash ref.
    write(&fx.work, "a.txt", "changed\n");
    fx.work.stash_save(&sig(), "wip", None).unwrap();

    let all = refs::list_references(&fx.work).unwrap();
    let get = |n: &str| all.iter().find(|r| r.name == n);
    let annotated = get("refs/tags/v1.0").unwrap();
    assert_eq!(
        annotated.target,
        oid(c),
        "annotated tag must decorate its commit, not the tag object"
    );
    assert_eq!(annotated.kind, ReferenceKind::Tag);
    assert_eq!(annotated.short_name, "v1.0");
    assert_eq!(get("refs/heads/main").unwrap().kind, ReferenceKind::Branch(BranchKind::Local));
    assert_eq!(
        get("refs/remotes/origin/main").unwrap().kind,
        ReferenceKind::Branch(BranchKind::Remote)
    );
    assert_eq!(get("refs/stash").unwrap().kind, ReferenceKind::Stash);
    assert!(get("refs/remotes/origin/HEAD").is_none());
    assert!(get("refs/tags/tree-tag").is_none());

    let tags = refs::list_tags(&fx.work).unwrap();
    assert_eq!(tags.len(), 2);
    let v1 = tags.iter().find(|t| t.name == "v1.0").unwrap();
    assert_eq!(v1.target, oid(c));
    let ann = v1.annotation.as_ref().unwrap();
    assert_eq!(ann.message.trim(), "Release 1.0");
    assert_eq!(ann.tagger.as_ref().unwrap().name, "Test User");
    assert!(tags.iter().find(|t| t.name == "light").unwrap().annotation.is_none());
    // Oracle.
    let cli = git(fx.work.workdir().unwrap(), &["rev-parse", "v1.0^{commit}"]);
    assert_eq!(cli.trim(), c.to_string());
    assert_eq!(git(fx.work.workdir().unwrap(), &["cat-file", "-t", "v1.0"]).trim(), "tag");
}

#[test]
fn create_tag_validation() {
    let fx = Fixture::new();
    let c = oid(head_oid(&fx.work));
    // Annotated without / with an empty signature: clear error, no panic, nothing created.
    assert!(matches!(
        refs::create_tag(&fx.work, "a1", &c, Some("msg"), None),
        Err(GitYasError::SignatureRequired(_))
    ));
    let empty = CommitSignature {
        name: String::new(),
        email: String::new(),
    };
    assert!(matches!(
        refs::create_tag(&fx.work, "a1", &c, Some("msg"), Some(&empty)),
        Err(GitYasError::SignatureRequired(_))
    ));
    assert!(fx.work.find_reference("refs/tags/a1").is_err());
    assert!(matches!(
        refs::create_tag(&fx.work, "a1", &c, Some("  \n"), Some(&core_sig())),
        Err(GitYasError::InvalidInput(_))
    ));
    for bad in ["", "  ", "a..b", "x\0y", "has space", "end.lock"] {
        assert!(
            matches!(refs::create_tag(&fx.work, bad, &c, None, None), Err(GitYasError::InvalidInput(_))),
            "{bad:?}"
        );
    }
    let t = refs::create_tag(&fx.work, "  refs/tags/trimmed ", &c, None, None).unwrap();
    assert_eq!(t.name, "trimmed");
    assert!(matches!(
        refs::create_tag(&fx.work, "trimmed", &c, None, None),
        Err(GitYasError::AlreadyExists(m)) if m.contains("tag")
    ));
    let missing = gityas_core::Oid::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
    assert!(matches!(
        refs::create_tag(&fx.work, "nope", &missing, None, None),
        Err(GitYasError::NotFound(_))
    ));
}

#[test]
fn delete_local_tag() {
    let fx = Fixture::new();
    let c = oid(head_oid(&fx.work));
    refs::create_tag(&fx.work, "t1", &c, None, None).unwrap();
    refs::delete_tag(&fx.work, "refs/tags/t1").unwrap();
    assert!(refs::list_tags(&fx.work).unwrap().is_empty());
    assert!(matches!(refs::delete_tag(&fx.work, "t1"), Err(GitYasError::NotFound(_))));
}

#[test]
fn push_list_and_delete_remote_tags() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    let c = oid(head_oid(&fx.work));
    refs::create_tag(&fx.work, "v1", &c, Some("one"), Some(&core_sig())).unwrap();
    refs::create_tag(&fx.work, "light", &c, None, None).unwrap();
    assert!(refs::list_remote_tag_names(&fx.work, "origin", &ctx(&creds)).unwrap().is_empty());

    refs::push_tag(&fx.work, "origin", "v1", &ctx(&creds)).unwrap();
    refs::push_tag(&fx.work, "origin", "light", &ctx(&creds)).unwrap();
    // The annotated tag is advertised as `v1` and `v1^{}`; listed once.
    assert_eq!(
        refs::list_remote_tag_names(&fx.work, "origin", &ctx(&creds)).unwrap(),
        ["light", "v1"]
    );
    assert!(matches!(
        refs::push_tag(&fx.work, "origin", "missing", &ctx(&creds)),
        Err(GitYasError::NotFound(_))
    ));

    refs::delete_remote_tag(&fx.work, "origin", "v1", &ctx(&creds)).unwrap();
    assert_eq!(refs::list_remote_tag_names(&fx.work, "origin", &ctx(&creds)).unwrap(), ["light"]);
    // The local tag is kept.
    assert!(fx.work.find_reference("refs/tags/v1").is_ok());
    // Oracle.
    let out = git(fx.work.workdir().unwrap(), &["ls-remote", "--tags", "origin"]);
    assert!(out.contains("refs/tags/light") && !out.contains("refs/tags/v1"), "{out}");
}

/// git2 0.20 `Remote::list` is UB (debug: process abort) on an empty advertisement; see `local_remote_has_no_refs`.
#[test]
fn listing_an_empty_local_remote_is_empty_not_an_abort() {
    let work_dir = TempDir::new().unwrap();
    let bare_dir = TempDir::new().unwrap();
    git2::Repository::init_bare(bare_dir.path()).unwrap();
    let work = init(work_dir.path());
    work.remote("plain", bare_dir.path().to_str().unwrap()).unwrap();
    work.remote("url", &file_url(bare_dir.path())).unwrap();
    let creds = NoCredentials;
    for remote in ["plain", "url"] {
        assert!(
            refs::list_remote_tag_names(&work, remote, &ctx(&creds)).unwrap().is_empty(),
            "{remote}"
        );
        assert!(
            gityas_git::remote::list_remote_refs(&work, remote, &ctx(&creds))
                .unwrap()
                .is_empty(),
            "{remote}"
        );
    }
}

#[test]
fn moved_remote_tag_push_is_rejected() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    let c1 = head_oid(&fx.work);
    refs::create_tag(&fx.work, "t", &oid(c1), None, None).unwrap();
    refs::push_tag(&fx.work, "origin", "t", &ctx(&creds)).unwrap();
    // Re-point the local tag at an unrelated root commit: pushing it would rewrite the remote tag.
    let tree = fx.work.head().unwrap().peel_to_tree().unwrap();
    let root = fx.work.commit(None, &sig(), &sig(), "unrelated", &tree, &[]).unwrap();
    refs::delete_tag(&fx.work, "t").unwrap();
    refs::create_tag(&fx.work, "t", &oid(root), None, None).unwrap();
    let err = refs::push_tag(&fx.work, "origin", "t", &ctx(&creds)).unwrap_err();
    assert!(matches!(&err, GitYasError::Git(m) if m.contains("rejected")), "{err:?}");
    assert_eq!(fx.remote().refname_to_id("refs/tags/t").unwrap(), c1);
}

#[test]
fn remote_ops_on_missing_remote() {
    let fx = Fixture::new();
    let creds = NoCredentials;
    assert!(matches!(
        refs::list_remote_tag_names(&fx.work, "nope", &ctx(&creds)),
        Err(GitYasError::NotFound(_))
    ));
    assert!(matches!(
        refs::delete_remote_tag(&fx.work, "nope", "t", &ctx(&creds)),
        Err(GitYasError::NotFound(_))
    ));
}
