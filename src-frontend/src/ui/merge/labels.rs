//! Pure label logic for the merge editor: who "ours" / "theirs" are (pane headers, action-card buttons), the
//! action card's message and which [`DeletedMergeAction`]s apply.
//!
//! The engine does not expose `MERGE_HEAD`, so the incoming side is derived from the prepared merge message
//! (`RepoStatus::prepared_message`, git's `MERGE_MSG`) and looked up among the branches; anything unknown
//! degrades to a generic label.

use gityas_core::diff::DiffKind;
use gityas_core::models::{Branch, ConflictSide, ConflictSides, DeletedMergeAction, Head, MergeConflict, RepoStatus};

const MODE_SYMLINK: u32 = 0o120_000;
const MODE_GITLINK: u32 = 0o160_000;
const SHORT_ID: usize = 7;

/// What git's `MERGE_MSG` says is being merged in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// Local or remote-tracking branch (`feature/x`, `origin/main`).
    Branch(String),
    Tag(String),
    Commit(String),
}

/// Parses the first line of a prepared merge message ("Merge branch 'x' into main", "Merge remote-tracking
/// branch 'origin/x'", "Merge tag 'v1'", "Merge commit 'abc123'"). Octopus merges and anything else → `None`.
pub fn parse_incoming(summary: &str) -> Option<Incoming> {
    let quoted = |rest: &str| -> Option<String> {
        let rest = rest.strip_prefix('\'')?;
        let end = rest.find('\'')?;
        let name = &rest[..end];
        (!name.is_empty()).then(|| name.to_owned())
    };
    let s = summary.trim();
    if let Some(rest) = s.strip_prefix("Merge branch ") {
        return quoted(rest).map(Incoming::Branch);
    }
    if let Some(rest) = s.strip_prefix("Merge remote-tracking branch ") {
        return quoted(rest).map(Incoming::Branch);
    }
    if let Some(rest) = s.strip_prefix("Merge tag ") {
        return quoted(rest).map(Incoming::Tag);
    }
    if let Some(rest) = s.strip_prefix("Merge commit ") {
        return quoted(rest).map(Incoming::Commit);
    }
    None
}

/// One side's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideLabel {
    /// Short name used in sentences and buttons ("main", "feature/x", "detached HEAD", "theirs").
    pub name: String,
    /// Short commit id, when known.
    pub commit: Option<String>,
    /// The name is a branch (pane header says "on <branch>").
    pub is_branch: bool,
}

impl SideLabel {
    fn plain(name: &str) -> Self {
        Self {
            name: name.into(),
            commit: None,
            is_branch: false,
        }
    }

    /// Source pane caption: "main · 0916fe1", "detached HEAD · 1111111", "commit 1234567890", "Theirs (incoming)".
    pub fn header(&self) -> String {
        match &self.commit {
            Some(c) if !self.name.contains(c.as_str()) => format!("{} · {c}", self.name),
            _ => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    pub ours: SideLabel,
    pub theirs: SideLabel,
}

/// Labels for A (ours = HEAD) and B (theirs = the merged-in side). Outside a merge (conflicts from a rebase,
/// cherry-pick, revert or stash pop) git's ours / theirs mean something else, so generic labels are used.
pub fn side_labels(head: Option<&Head>, status: &RepoStatus, local: &[Branch], remote: &[Branch]) -> Labels {
    if !status.is_merging() {
        return Labels {
            ours: SideLabel::plain("Ours (HEAD)"),
            theirs: SideLabel::plain("Theirs"),
        };
    }
    let ours = match head {
        Some(Head::Branch { name, target, .. }) => SideLabel {
            name: name.clone(),
            commit: Some(target.short(SHORT_ID)),
            is_branch: true,
        },
        Some(Head::Detached { target }) => SideLabel {
            name: "detached HEAD".into(),
            commit: Some(target.short(SHORT_ID)),
            is_branch: false,
        },
        Some(Head::Unborn { name, .. }) => SideLabel {
            name: name.clone(),
            commit: None,
            is_branch: true,
        },
        None => SideLabel::plain("Ours (HEAD)"),
    };
    let theirs = match status.prepared_summary().and_then(parse_incoming) {
        Some(Incoming::Branch(name)) => {
            let commit = local
                .iter()
                .chain(remote.iter())
                .find(|b| b.name == name)
                .and_then(|b| b.target.as_ref())
                .map(|t| t.short(SHORT_ID));
            SideLabel {
                name,
                commit,
                is_branch: true,
            }
        }
        Some(Incoming::Tag(name)) => SideLabel::plain(&format!("tag {name}")),
        Some(Incoming::Commit(id)) => SideLabel {
            name: format!("commit {id}"),
            commit: Some(id.chars().take(SHORT_ID).collect()),
            is_branch: false,
        },
        None => SideLabel::plain("Theirs (incoming)"),
    };
    Labels { ours, theirs }
}

/// Why a side can't be shown as text, as a sentence fragment ("is a binary file").
fn non_text_reason(side: &ConflictSide) -> Option<String> {
    if side.mode == MODE_GITLINK {
        return Some("is a submodule".into());
    }
    if side.mode == MODE_SYMLINK {
        return Some("is a symbolic link".into());
    }
    match side.kind {
        DiffKind::Text => None,
        DiffKind::Binary => Some("is a binary file".into()),
        DiffKind::NonUtf8 => Some("is not valid UTF-8 text".into()),
        DiffKind::TooLarge { bytes } => Some(format!("is too large to edit here ({})", format_size(bytes))),
    }
}

pub fn format_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / MB)
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} bytes")
    }
}

/// The text editor can't handle this conflict: show the action card instead.
pub fn needs_action_card(c: &MergeConflict) -> bool {
    c.is_delete_conflict() || !c.sides.is_text()
}

/// Explanation on the action card.
pub fn card_message(path: &str, c: &MergeConflict, l: &Labels) -> String {
    let (ours, theirs) = (&l.ours.name, &l.theirs.name);
    let mut message = match (c.ours_deleted, c.theirs_deleted) {
        (true, true) => format!("{path} was deleted on both {ours} and {theirs}."),
        (true, false) => format!("{path} was deleted on {ours} and modified on {theirs}."),
        (false, true) => format!("{path} was modified on {ours} and deleted on {theirs}."),
        (false, false) => format!("{path} can't be merged as text."),
    };
    if let Some(reason) = sides_reason(&c.sides, l) {
        message.push(' ');
        message.push_str(&reason);
    }
    message
}

fn sides_reason(sides: &ConflictSides, l: &Labels) -> Option<String> {
    let candidates = [
        (sides.ours.as_ref(), Some(l.ours.name.as_str())),
        (sides.theirs.as_ref(), Some(l.theirs.name.as_str())),
        (sides.ancestor.as_ref(), None),
    ];
    let mut reasons: Vec<String> = Vec::new();
    for (side, name) in candidates {
        let Some(reason) = side.and_then(non_text_reason) else { continue };
        let sentence = match name {
            Some(name) => format!("The version on {name} {reason}."),
            None => format!("The base version {reason}."),
        };
        if !reasons.contains(&sentence) {
            reasons.push(sentence);
        }
    }
    (!reasons.is_empty()).then(|| reasons.join(" "))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardAction {
    pub action: DeletedMergeAction,
    pub label: String,
    /// Red and confirmed.
    pub danger: bool,
}

/// The buttons of the action card: only versions that exist are offered (taking a missing version would delete
/// the file, which "Delete file" already does explicitly).
pub fn card_actions(c: &MergeConflict, l: &Labels) -> Vec<CardAction> {
    let delete_modify = c.ours_deleted != c.theirs_deleted;
    let take_label = |name: &str| {
        if delete_modify {
            format!("Keep modified version ({name})")
        } else {
            format!("Take version from {name}")
        }
    };
    let mut out = Vec::new();
    if c.sides.ours.is_some() {
        out.push(CardAction {
            action: DeletedMergeAction::TakeOurs,
            label: take_label(&l.ours.name),
            danger: false,
        });
    }
    if c.sides.theirs.is_some() {
        out.push(CardAction {
            action: DeletedMergeAction::TakeTheirs,
            label: take_label(&l.theirs.name),
            danger: false,
        });
    }
    if c.sides.ancestor.is_some() {
        out.push(CardAction {
            action: DeletedMergeAction::TakeBase,
            label: "Take base version".into(),
            danger: false,
        });
    }
    out.push(CardAction {
        action: DeletedMergeAction::DeleteFile,
        label: "Delete file".into(),
        danger: true,
    });
    out
}

/// Toast after an action resolved the file.
pub fn action_done(path: &str, action: DeletedMergeAction, l: &Labels) -> String {
    match action {
        DeletedMergeAction::TakeOurs => format!("Resolved {path} with the version from {}", l.ours.name),
        DeletedMergeAction::TakeTheirs => format!("Resolved {path} with the version from {}", l.theirs.name),
        DeletedMergeAction::TakeBase => format!("Resolved {path} with the base version"),
        DeletedMergeAction::DeleteFile => format!("Resolved {path} by deleting it"),
    }
}

#[cfg(test)]
mod tests {
    use gityas_core::Oid;
    use gityas_core::models::{BranchKind, RepoOperation};

    use super::*;

    fn oid(b: u8) -> Oid {
        Oid::from_bytes(&[b; 20]).unwrap_or_else(|e| panic!("{e}"))
    }

    fn merging(msg: &str) -> RepoStatus {
        RepoStatus {
            operation: RepoOperation::Merge,
            prepared_message: Some(msg.into()),
            ..RepoStatus::default()
        }
    }

    fn branch(name: &str, kind: BranchKind, target: u8) -> Branch {
        Branch {
            name: name.into(),
            refname: format!("refs/x/{name}"),
            kind,
            target: Some(oid(target)),
            remote_name: None,
            upstream: None,
            upstream_refname: None,
            upstream_gone: false,
            ahead: 0,
            behind: 0,
            is_head: false,
        }
    }

    fn side(kind: DiffKind, mode: u32) -> Option<ConflictSide> {
        Some(ConflictSide { mode, bytes: 10, kind })
    }

    fn conflict(sides: ConflictSides) -> MergeConflict {
        MergeConflict {
            ancestor: String::new(),
            ours: String::new(),
            theirs: String::new(),
            merged: String::new(),
            chunks: vec![],
            ours_deleted: sides.ours.is_none(),
            theirs_deleted: sides.theirs.is_none(),
            auto_resolved: false,
            sides,
        }
    }

    fn labels() -> Labels {
        Labels {
            ours: SideLabel {
                name: "main".into(),
                commit: Some("aaaaaaa".into()),
                is_branch: true,
            },
            theirs: SideLabel {
                name: "feature/x".into(),
                commit: None,
                is_branch: true,
            },
        }
    }

    #[test]
    fn parses_merge_messages() {
        assert_eq!(
            parse_incoming("Merge branch 'feature/x'"),
            Some(Incoming::Branch("feature/x".into()))
        );
        assert_eq!(parse_incoming("Merge branch 'x' into main"), Some(Incoming::Branch("x".into())));
        assert_eq!(
            parse_incoming("Merge branch 'x' of https://h/r into main"),
            Some(Incoming::Branch("x".into()))
        );
        assert_eq!(
            parse_incoming("Merge remote-tracking branch 'origin/main'"),
            Some(Incoming::Branch("origin/main".into()))
        );
        assert_eq!(parse_incoming("Merge tag 'v1.0'"), Some(Incoming::Tag("v1.0".into())));
        assert_eq!(
            parse_incoming("Merge commit 'abc1234def'"),
            Some(Incoming::Commit("abc1234def".into()))
        );
        assert_eq!(parse_incoming("Merge branches 'a' and 'b'"), None);
        assert_eq!(parse_incoming("Merge branch ''"), None);
        assert_eq!(parse_incoming("Merge branch 'unterminated"), None);
        assert_eq!(parse_incoming("Revert \"x\""), None);
    }

    #[test]
    fn side_labels_from_head_and_merge_message() {
        let head = Head::Branch {
            name: "main".into(),
            refname: "refs/heads/main".into(),
            target: oid(0xab),
        };
        let local = [branch("feature/x", BranchKind::Local, 0xcd)];
        let remote = [branch("origin/y", BranchKind::Remote, 0xef)];
        let l = side_labels(Some(&head), &merging("Merge branch 'feature/x'\n\n# Conflicts:\n"), &local, &remote);
        assert_eq!(l.ours.header(), "main · abababa");
        assert_eq!(l.theirs.header(), "feature/x · cdcdcdc");
        let l = side_labels(Some(&head), &merging("Merge remote-tracking branch 'origin/y'"), &local, &remote);
        assert_eq!(l.theirs.header(), "origin/y · efefefe");
        // Branch gone / unknown: name without commit.
        let l = side_labels(Some(&head), &merging("Merge branch 'gone'"), &local, &remote);
        assert_eq!(l.theirs.header(), "gone");
        let l = side_labels(Some(&head), &merging("Merge commit '1234567890'"), &local, &remote);
        assert_eq!(l.theirs.header(), "commit 1234567890");
        // Detached HEAD, no message.
        let detached = Head::Detached { target: oid(0x11) };
        let l = side_labels(
            Some(&detached),
            &RepoStatus {
                operation: RepoOperation::Merge,
                ..RepoStatus::default()
            },
            &[],
            &[],
        );
        assert_eq!(l.ours.header(), "detached HEAD · 1111111");
        assert_eq!(l.ours.name, "detached HEAD");
        assert_eq!(l.theirs.header(), "Theirs (incoming)");
        // Not a merge (e.g. a rebase stopped on a conflict).
        let l = side_labels(Some(&head), &RepoStatus::default(), &local, &remote);
        assert_eq!((l.ours.header().as_str(), l.theirs.header().as_str()), ("Ours (HEAD)", "Theirs"));
    }

    #[test]
    fn delete_modify_card() {
        let l = labels();
        let c = conflict(ConflictSides {
            ancestor: side(DiffKind::Text, 0o100_644),
            ours: None,
            theirs: side(DiffKind::Text, 0o100_644),
        });
        assert!(needs_action_card(&c));
        assert_eq!(
            card_message("src/a.rs", &c, &l),
            "src/a.rs was deleted on main and modified on feature/x."
        );
        let actions = card_actions(&c, &l);
        let summary: Vec<(DeletedMergeAction, &str, bool)> = actions.iter().map(|a| (a.action, a.label.as_str(), a.danger)).collect();
        assert_eq!(
            summary,
            [
                (DeletedMergeAction::TakeTheirs, "Keep modified version (feature/x)", false),
                (DeletedMergeAction::TakeBase, "Take base version", false),
                (DeletedMergeAction::DeleteFile, "Delete file", true),
            ]
        );
        let c = conflict(ConflictSides {
            ancestor: side(DiffKind::Text, 0o100_644),
            ours: side(DiffKind::Text, 0o100_644),
            theirs: None,
        });
        assert_eq!(card_message("a", &c, &l), "a was modified on main and deleted on feature/x.");
        assert_eq!(card_actions(&c, &l)[0].action, DeletedMergeAction::TakeOurs);
        let both = conflict(ConflictSides {
            ancestor: side(DiffKind::Text, 0o100_644),
            ours: None,
            theirs: None,
        });
        assert_eq!(card_message("a", &both, &l), "a was deleted on both main and feature/x.");
        assert_eq!(card_actions(&both, &l).len(), 2);
    }

    #[test]
    fn non_text_card() {
        let l = labels();
        let c = conflict(ConflictSides {
            ancestor: None,
            ours: side(DiffKind::Binary, 0o100_644),
            theirs: side(DiffKind::Binary, 0o100_644),
        });
        assert!(needs_action_card(&c));
        assert_eq!(
            card_message("img.png", &c, &l),
            "img.png can't be merged as text. The version on main is a binary file. The version on feature/x is a binary file."
        );
        let actions: Vec<DeletedMergeAction> = card_actions(&c, &l).iter().map(|a| a.action).collect();
        // Add/add: no base to take.
        assert_eq!(
            actions,
            [
                DeletedMergeAction::TakeOurs,
                DeletedMergeAction::TakeTheirs,
                DeletedMergeAction::DeleteFile
            ]
        );
        assert_eq!(card_actions(&c, &l)[0].label, "Take version from main");
        let c = conflict(ConflictSides {
            ancestor: side(DiffKind::Text, 0o100_644),
            ours: side(DiffKind::Binary, MODE_SYMLINK),
            theirs: side(DiffKind::TooLarge { bytes: 6 * 1024 * 1024 }, 0o100_644),
        });
        assert_eq!(
            card_message("x", &c, &l),
            "x can't be merged as text. The version on main is a symbolic link. The version on feature/x is too large to edit here (6.0 MB)."
        );
        let c = conflict(ConflictSides {
            ancestor: side(DiffKind::Binary, MODE_GITLINK),
            ours: side(DiffKind::Binary, MODE_GITLINK),
            theirs: side(DiffKind::NonUtf8, 0o100_644),
        });
        assert!(card_message("x", &c, &l).contains("main is a submodule. The version on feature/x is not valid UTF-8 text."));
        let text = conflict(ConflictSides {
            ancestor: side(DiffKind::Text, 0o100_644),
            ours: side(DiffKind::Text, 0o100_644),
            theirs: side(DiffKind::Text, 0o100_644),
        });
        assert!(!needs_action_card(&text));
    }

    #[test]
    fn sizes_and_toasts() {
        assert_eq!(format_size(12), "12 bytes");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
        let l = labels();
        assert_eq!(
            action_done("a", DeletedMergeAction::TakeTheirs, &l),
            "Resolved a with the version from feature/x"
        );
        assert_eq!(action_done("a", DeletedMergeAction::DeleteFile, &l), "Resolved a by deleting it");
    }
}
