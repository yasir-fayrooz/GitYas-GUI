//! Pure logic of the remote-ops UI (no egui): toast texts per outcome, banner rows, the
//! status-bar progress label, the push-target decision and error classification. Unit-tested below.

use std::collections::BTreeSet;
use std::path::PathBuf;

use gityas_core::models::{
    AbortMergeOutcome, DeltaChange, FetchSummary, FileState, MergeOutcome, RemoteOp, RepoOperation, RepoStatus, shorten_refname,
};
use gityas_core::{GitYasError, Provider};

/// `n thing` / `n things`.
pub fn plural(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

// ---- Fetch -----------------------------------------------------------------------------------------------------

/// Toast for a user fetch, `None` when nothing changed (the status bar flashes "Up to date" instead).
/// E.g. "Fetched origin: 3 branches updated", "Fetched origin: 1 branch updated, 2 tags; upstream: 1 branch updated".
pub fn fetch_message(summary: &FetchSummary) -> Option<String> {
    let parts: Vec<String> = summary
        .fetched
        .iter()
        .filter(|f| !f.updated.is_empty())
        .map(|f| {
            let branches = f.updated.iter().filter(|u| u.refname.starts_with("refs/remotes/")).count();
            let tags = f.updated.iter().filter(|u| u.refname.starts_with("refs/tags/")).count();
            let other = f.updated.len() - branches - tags;
            let mut what = Vec::new();
            if branches > 0 {
                what.push(format!("{} updated", plural(branches, "branch", "branches")));
            }
            if tags > 0 {
                what.push(plural(tags, "tag", "tags"));
            }
            if other > 0 {
                what.push(format!("{} updated", plural(other, "ref", "refs")));
            }
            format!("{}: {}", f.remote, what.join(", "))
        })
        .collect();
    (!parts.is_empty()).then(|| format!("Fetched {}", parts.join("; ")))
}

/// What the status bar says while fetching: "origin", "2 remotes", "all remotes".
pub fn fetch_target(remotes: &[String]) -> String {
    match remotes {
        [only] => only.clone(),
        [] => "all remotes".into(),
        many => format!("{} remotes", many.len()),
    }
}

// ---- Progress ----------------------------------------------------------------------------------------------------

/// Verb of a remote operation for the status bar.
pub fn op_verb(op: RemoteOp) -> &'static str {
    match op {
        RemoteOp::Fetch => "Fetching",
        RemoteOp::Pull => "Pulling",
        RemoteOp::Push => "Pushing",
        RemoteOp::Clone => "Cloning",
    }
}

/// "Fetching origin… 45 %" (percent once known).
pub fn progress_label(op: RemoteOp, target: &str, percent: Option<u8>) -> String {
    let base = if target.is_empty() {
        format!("{}…", op_verb(op))
    } else {
        format!("{} {target}…", op_verb(op))
    };
    match percent {
        Some(p) => format!("{base} {p} %"),
        None => base,
    }
}

// ---- Merge / pull ------------------------------------------------------------------------------------------------

/// How a merge outcome is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeReport {
    /// Success / info toast.
    Done { title: String, info: bool },
    /// Warning toast + banner + conflicted section.
    Conflicts { title: String, first: Option<PathBuf> },
}

/// Toast for a merge (or the merge half of a pull) of `source` (short name, e.g. `origin/main`) into `target`.
pub fn merge_report(outcome: &MergeOutcome, source: &str, target: &str) -> MergeReport {
    match outcome {
        MergeOutcome::UpToDate => MergeReport::Done {
            title: "Already up to date".into(),
            info: true,
        },
        MergeOutcome::FastForward { to, .. } => MergeReport::Done {
            title: format!("Fast-forwarded {target} to {}", to.short(7)),
            info: false,
        },
        MergeOutcome::Merged { .. } => MergeReport::Done {
            title: format!("Merged {source} into {target}"),
            info: false,
        },
        MergeOutcome::Conflicts { paths } => MergeReport::Conflicts {
            title: format!("Merge has conflicts in {}", plural(paths.len(), "file", "files")),
            first: paths.first().cloned(),
        },
    }
}

/// "Merge aborted" detail: "Restored 2 files, removed 1".
pub fn abort_detail(outcome: &AbortMergeOutcome) -> Option<String> {
    let mut parts = Vec::new();
    if !outcome.restored.is_empty() {
        parts.push(format!("Restored {}", plural(outcome.restored.len(), "file", "files")));
    }
    if !outcome.removed.is_empty() {
        parts.push(format!(
            "removed {} the merge added",
            plural(outcome.removed.len(), "file", "files")
        ));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

// ---- Push ----------------------------------------------------------------------------------------------------------

/// What the Push button does for a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushPlan {
    /// `Engine::push`: the branch has an upstream, or there is a single remote (the engine pushes there and sets the
    /// upstream).
    Push,
    /// Several remotes and no upstream: the push dialog, preselecting `default` (`origin` if present).
    Dialog {
        default: String,
    },
    NoRemote,
}

/// `has_upstream` = an upstream is configured (also when it is gone on the remote); `None` = branch list not loaded
/// yet (let the engine decide).
pub fn push_plan(has_upstream: Option<bool>, remotes: &[String]) -> PushPlan {
    match has_upstream {
        None | Some(true) => PushPlan::Push,
        Some(false) => match remotes {
            [] => PushPlan::NoRemote,
            [_] => PushPlan::Push,
            many => PushPlan::Dialog {
                default: many
                    .iter()
                    .find(|r| r.as_str() == "origin")
                    .or_else(|| many.first())
                    .cloned()
                    .unwrap_or_default(),
            },
        },
    }
}

/// "Pushed main to origin" (+ " as other" when the remote name differs) (+ " and set upstream").
pub fn push_message(local: &str, remote: &str, remote_branch: &str, upstream_set: bool) -> String {
    let mut text = format!("Pushed {local} to {remote}");
    if remote_branch != local {
        text.push_str(&format!(" as {remote_branch}"));
    }
    if upstream_set {
        text.push_str(" and set upstream");
    }
    text
}

/// Force-push confirmation body, when the branch is `behind` its upstream (e.g. after a reset).
pub fn force_push_body(local: &str, upstream: &str, behind: usize) -> String {
    format!(
        "{upstream} has {} that {local} doesn't, so a normal push would be rejected. Force push {local} to replace it? \
         Cancel and pull instead to keep them.",
        commits(behind)
    )
}

/// Force-push confirmation warning (red line).
pub fn force_push_warning(upstream: &str, behind: usize) -> String {
    format!("The {} will be removed from {upstream} for everyone", commits(behind))
}

fn commits(n: usize) -> String {
    if n == 1 { "1 commit".into() } else { format!("{n} commits") }
}

/// Inline validation of the remote branch name in the push dialog (a subset of `git check-ref-format`; the engine
/// validates fully).
pub fn validate_remote_branch_name(name: &str) -> Result<(), &'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a branch name");
    }
    let bad_char = |c: char| c.is_whitespace() || c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\');
    if name.chars().any(bad_char)
        || name.contains("..")
        || name.contains("@{")
        || name.contains("//")
        || name.starts_with(['-', '/', '.'])
        || name.ends_with(['/', '.'])
        || name.ends_with(".lock")
        || name == "@"
    {
        return Err("Invalid branch name");
    }
    Ok(())
}

// ---- Errors ----------------------------------------------------------------------------------------------------------

/// "Push rejected: origin/main has new commits. Pull first."
pub fn rejected_title(upstream: &str) -> String {
    format!("Push rejected: {upstream} has new commits. Pull first.")
}

/// Toast title for an authentication failure: "GitHub rejected the token", "Couldn't sign in to GitHub" (no token
/// linked, and git's saved credentials were missing or rejected).
pub fn auth_title(err: &GitYasError, provider: Option<Provider>, connected: bool, fallback: &str) -> String {
    let GitYasError::Auth(message) = err else {
        return fallback.to_owned();
    };
    if message.contains("SSH") {
        return "SSH sign-in failed".into();
    }
    match provider {
        Some(p) if connected => format!("{p} rejected the token"),
        Some(p) => format!("Couldn't sign in to {p}"),
        None => "The remote rejected the credentials".into(),
    }
}

/// The engine's background-fetch back-off warning (`watch::scheduler`): the banner offers `[Open credentials]`.
pub fn is_background_auth_warning(warning: &str) -> bool {
    warning.starts_with("Background fetch of remote") && warning.contains("sign-in")
}

// ---- Banner ------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BannerRow {
    /// Merge in progress, with `[Abort merge]`.
    Merge(String),
    /// Rebase / cherry-pick / revert / bisect / am started outside GitYas: informational.
    Operation(String),
    /// `app.warnings[index]`, dismissible.
    Warning { index: usize, text: String, credentials: bool },
}

/// `(source, target)` from a prepared merge message: `Merge branch 'x' into main`, `Merge remote-tracking branch
/// 'origin/main'`, `Merge commit 'abc'`, `Merge tag 'v1'`. `None` if it is not one of those.
pub fn parse_merge_summary(summary: &str) -> Option<(String, Option<String>)> {
    let rest = summary.trim().strip_prefix("Merge ")?;
    let rest = ["remote-tracking branch ", "branch ", "commit ", "tag "]
        .iter()
        .find_map(|kind| rest.strip_prefix(kind))
        .unwrap_or(rest);
    let rest = rest.strip_prefix('\'')?;
    let (source, after) = rest.split_once('\'')?;
    if source.is_empty() {
        return None;
    }
    let target = after
        .trim()
        .strip_prefix("into ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    Some((source.to_owned(), target))
}

/// Distinct conflicted paths in the file lists.
pub fn conflicted_count(files: &FileState) -> usize {
    files
        .staged
        .iter()
        .chain(&files.unstaged)
        .filter(|d| d.change == DeltaChange::Conflicted)
        .map(|d| d.path.as_path())
        .collect::<BTreeSet<_>>()
        .len()
}

/// "Merging feature/x into main — 3 conflicted files". `head` = the checked-out branch's short name.
pub fn merge_banner_text(status: &RepoStatus, head: Option<&str>, conflicted: usize) -> String {
    let parsed = status.prepared_summary().and_then(parse_merge_summary);
    let what = match parsed {
        Some((source, target)) => match target.as_deref().or(head) {
            Some(target) => format!("Merging {} into {target}", shorten_refname(&source)),
            None => format!("Merging {}", shorten_refname(&source)),
        },
        None => match head {
            Some(head) => format!("Merge into {head} in progress"),
            None => "Merge in progress".into(),
        },
    };
    let state = match conflicted {
        0 => "conflicts resolved; commit to finish".to_owned(),
        1 => "1 conflicted file".to_owned(),
        n => format!("{n} conflicted files"),
    };
    format!("{what} — {state}")
}

/// "A rebase is in progress (started outside GitYas); finish or abort it with git." `None` for merge / none.
pub fn operation_banner_text(op: RepoOperation) -> Option<String> {
    if matches!(op, RepoOperation::Merge) {
        return None;
    }
    let what = op.describe()?;
    let mut chars = what.chars();
    let first = chars.next()?.to_uppercase().collect::<String>();
    Some(format!(
        "{first}{} is in progress (started outside GitYas); finish or abort it with git",
        chars.as_str()
    ))
}

pub fn banner_rows(status: Option<&RepoStatus>, head: Option<&str>, files: Option<&FileState>, warnings: &[String]) -> Vec<BannerRow> {
    let mut rows = Vec::new();
    if let Some(status) = status {
        if status.is_merging() {
            let conflicted = files.map_or(0, conflicted_count);
            rows.push(BannerRow::Merge(merge_banner_text(status, head, conflicted)));
        } else if let Some(text) = operation_banner_text(status.operation) {
            rows.push(BannerRow::Operation(text));
        }
    }
    rows.extend(warnings.iter().enumerate().map(|(index, text)| BannerRow::Warning {
        index,
        text: text.clone(),
        credentials: is_background_auth_warning(text),
    }));
    rows
}

#[cfg(test)]
mod tests {
    use gityas_core::Oid;
    use gityas_core::models::{Delta, RefUpdate, RemoteFetch};

    use super::*;

    fn oid(b: u8) -> Oid {
        Oid::from_bytes(&[b; 20]).unwrap_or_else(|e| panic!("{e}"))
    }

    fn update(refname: &str) -> RefUpdate {
        RefUpdate {
            refname: refname.into(),
            old: None,
            new: Some(oid(1)),
        }
    }

    #[test]
    fn fetch_messages() {
        assert_eq!(fetch_message(&FetchSummary::default()), None);
        let mut summary = FetchSummary {
            fetched: vec![RemoteFetch {
                remote: "origin".into(),
                updated: vec![],
            }],
            failed: vec![],
        };
        assert_eq!(fetch_message(&summary), None);
        summary.fetched[0].updated = vec![
            update("refs/remotes/origin/main"),
            update("refs/remotes/origin/a"),
            update("refs/remotes/origin/b"),
        ];
        assert_eq!(fetch_message(&summary).as_deref(), Some("Fetched origin: 3 branches updated"));
        summary.fetched.push(RemoteFetch {
            remote: "upstream".into(),
            updated: vec![update("refs/remotes/upstream/x"), update("refs/tags/v1"), update("refs/tags/v2")],
        });
        assert_eq!(
            fetch_message(&summary).as_deref(),
            Some("Fetched origin: 3 branches updated; upstream: 1 branch updated, 2 tags")
        );
        assert_eq!(fetch_target(&["origin".into()]), "origin");
        assert_eq!(fetch_target(&["a".into(), "b".into()]), "2 remotes");
        assert_eq!(fetch_target(&[]), "all remotes");
    }

    #[test]
    fn progress_labels() {
        assert_eq!(progress_label(RemoteOp::Fetch, "origin", Some(45)), "Fetching origin… 45 %");
        assert_eq!(progress_label(RemoteOp::Push, "main", None), "Pushing main…");
        assert_eq!(progress_label(RemoteOp::Pull, "", None), "Pulling…");
    }

    #[test]
    fn merge_reports() {
        assert_eq!(
            merge_report(&MergeOutcome::UpToDate, "origin/main", "main"),
            MergeReport::Done {
                title: "Already up to date".into(),
                info: true
            }
        );
        let ff = MergeOutcome::FastForward { from: None, to: oid(0xa1) };
        assert_eq!(
            merge_report(&ff, "origin/main", "main"),
            MergeReport::Done {
                title: "Fast-forwarded main to a1a1a1a".into(),
                info: false
            }
        );
        assert_eq!(
            merge_report(&MergeOutcome::Merged { commit: oid(2) }, "origin/main", "main"),
            MergeReport::Done {
                title: "Merged origin/main into main".into(),
                info: false
            }
        );
        let conflicts = MergeOutcome::Conflicts {
            paths: vec!["a.txt".into(), "b.txt".into()],
        };
        assert_eq!(
            merge_report(&conflicts, "x", "main"),
            MergeReport::Conflicts {
                title: "Merge has conflicts in 2 files".into(),
                first: Some("a.txt".into())
            }
        );
        let one = MergeOutcome::Conflicts {
            paths: vec!["a.txt".into()],
        };
        assert!(matches!(merge_report(&one, "x", "main"), MergeReport::Conflicts { title, .. } if title.ends_with("1 file")));
        let abort = AbortMergeOutcome {
            restored: vec!["a".into(), "b".into()],
            removed: vec!["c".into()],
        };
        assert_eq!(
            abort_detail(&abort).as_deref(),
            Some("Restored 2 files, removed 1 file the merge added")
        );
        assert_eq!(
            abort_detail(&AbortMergeOutcome {
                restored: vec![],
                removed: vec![]
            }),
            None
        );
    }

    #[test]
    fn push_plans_and_messages() {
        let remotes = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(push_plan(Some(true), &remotes(&["a", "b"])), PushPlan::Push);
        assert_eq!(push_plan(None, &remotes(&[])), PushPlan::Push);
        assert_eq!(push_plan(Some(false), &remotes(&[])), PushPlan::NoRemote);
        assert_eq!(push_plan(Some(false), &remotes(&["fork"])), PushPlan::Push);
        assert_eq!(
            push_plan(Some(false), &remotes(&["fork", "origin"])),
            PushPlan::Dialog { default: "origin".into() }
        );
        assert_eq!(
            push_plan(Some(false), &remotes(&["fork", "upstream"])),
            PushPlan::Dialog { default: "fork".into() }
        );
        assert_eq!(push_message("main", "origin", "main", false), "Pushed main to origin");
        assert_eq!(
            force_push_warning("origin/main", 1),
            "The 1 commit will be removed from origin/main for everyone"
        );
        assert!(force_push_body("main", "origin/main", 3).starts_with("origin/main has 3 commits that main doesn't"));
        assert_eq!(
            push_message("main", "origin", "main", true),
            "Pushed main to origin and set upstream"
        );
        assert_eq!(
            push_message("side", "fork", "other", true),
            "Pushed side to fork as other and set upstream"
        );
    }

    #[test]
    fn remote_branch_name_validation() {
        assert!(validate_remote_branch_name("feature/x").is_ok());
        assert!(validate_remote_branch_name("ünïcode-1.2").is_ok());
        for bad in [
            "", "  ", "a b", "a..b", "-x", "x/", "x.lock", "a:b", "a~1", "@", "a//b", ".x", "a@{b",
        ] {
            assert!(validate_remote_branch_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn error_classification() {
        assert_eq!(
            rejected_title("origin/main"),
            "Push rejected: origin/main has new commits. Pull first."
        );

        let auth = GitYasError::Auth("the GitHub personal access token was rejected by https://github.com/o/r".into());
        assert_eq!(auth_title(&auth, Some(Provider::Github), true, "x"), "GitHub rejected the token");
        assert_eq!(auth_title(&auth, Some(Provider::Gitlab), false, "x"), "Couldn't sign in to GitLab");
        assert_eq!(auth_title(&auth, None, false, "x"), "The remote rejected the credentials");
        assert_eq!(
            auth_title(
                &GitYasError::Auth("SSH sign-in to git@github.com:o/r.git failed (tried ssh-agent); …".into()),
                Some(Provider::Github),
                true,
                "x"
            ),
            "SSH sign-in failed"
        );
        assert_eq!(
            auth_title(&GitYasError::Git("x".into()), None, false, "Could not push"),
            "Could not push"
        );
        assert!(is_background_auth_warning(
            "Background fetch of remote \"origin\" stopped after 3 failed sign-ins. Check the access token, then reopen the repository."
        ));
        assert!(!is_background_auth_warning("The user database was recovered"));
    }

    #[test]
    fn merge_summaries() {
        assert_eq!(
            parse_merge_summary("Merge branch 'feature/x' into main"),
            Some(("feature/x".into(), Some("main".into())))
        );
        assert_eq!(
            parse_merge_summary("Merge remote-tracking branch 'origin/main'"),
            Some(("origin/main".into(), None))
        );
        assert_eq!(parse_merge_summary("Merge commit 'abc123'"), Some(("abc123".into(), None)));
        assert_eq!(parse_merge_summary("Merge 'x'"), Some(("x".into(), None)));
        for bad in ["Revert \"x\"", "Merge branch", "Merge branch ''", "Merge branch 'unterminated"] {
            assert_eq!(parse_merge_summary(bad), None, "{bad}");
        }
    }

    #[test]
    fn banner_texts_and_rows() {
        let merging = |msg: Option<&str>| RepoStatus {
            operation: RepoOperation::Merge,
            prepared_message: msg.map(str::to_owned),
            ..RepoStatus::default()
        };
        let files = FileState {
            staged: vec![
                Delta::new("a.txt", DeltaChange::Conflicted),
                Delta::new("s.txt", DeltaChange::Added),
            ],
            unstaged: vec![
                Delta::new("a.txt", DeltaChange::Conflicted),
                Delta::new("b.txt", DeltaChange::Conflicted),
                Delta::new("c.txt", DeltaChange::Conflicted),
            ],
        };
        assert_eq!(conflicted_count(&files), 3);
        assert_eq!(
            merge_banner_text(&merging(Some("Merge branch 'feature/x'\n\n# Conflicts:\n")), Some("main"), 3),
            "Merging feature/x into main — 3 conflicted files"
        );
        assert_eq!(
            merge_banner_text(&merging(Some("Merge branch 'x' into dev")), Some("main"), 1),
            "Merging x into dev — 1 conflicted file"
        );
        assert_eq!(
            merge_banner_text(&merging(Some("Merge refs/remotes/origin/main")), Some("main"), 0),
            "Merge into main in progress — conflicts resolved; commit to finish"
        );
        assert_eq!(merge_banner_text(&merging(None), None, 2), "Merge in progress — 2 conflicted files");
        assert_eq!(operation_banner_text(RepoOperation::None), None);
        assert_eq!(operation_banner_text(RepoOperation::Merge), None);
        assert_eq!(
            operation_banner_text(RepoOperation::Rebase).as_deref(),
            Some("A rebase is in progress (started outside GitYas); finish or abort it with git")
        );

        assert!(banner_rows(Some(&RepoStatus::default()), Some("main"), Some(&files), &[]).is_empty());
        let rows = banner_rows(
            Some(&merging(Some("Merge branch 'x'"))),
            Some("main"),
            Some(&files),
            &[
                "Background fetch of remote \"origin\" stopped after 3 failed sign-ins.".into(),
                "Recovered".into(),
            ],
        );
        assert_eq!(
            rows,
            [
                BannerRow::Merge("Merging x into main — 3 conflicted files".into()),
                BannerRow::Warning {
                    index: 0,
                    text: "Background fetch of remote \"origin\" stopped after 3 failed sign-ins.".into(),
                    credentials: true
                },
                BannerRow::Warning {
                    index: 1,
                    text: "Recovered".into(),
                    credentials: false
                },
            ]
        );
        let cherry = RepoStatus {
            operation: RepoOperation::CherryPick,
            ..RepoStatus::default()
        };
        assert!(matches!(&banner_rows(Some(&cherry), None, None, &[])[..], [BannerRow::Operation(t)] if t.starts_with("A cherry-pick")));
        assert!(banner_rows(None, None, None, &["w".into()]).len() == 1);
    }
}
