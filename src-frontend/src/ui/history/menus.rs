//! Context menus shared by the sidebar and the graph's ref pills: local branch, remote branch, tag.

use egui::Ui;
use gityas_core::models::{Branch, Head, Tag};

use super::model::TagRow;
use super::{Intent, RemoteTags};
use crate::store::RepoData;

/// What the menus need to know about the repository.
pub(super) struct MenuCtx {
    /// Short name of the checked-out (born) branch: merge target. `None` when detached or unborn.
    pub current: Option<String>,
    pub remotes: Vec<String>,
    pub unborn: bool,
    /// Full ref name of the soloed branch, if any.
    pub solo: Option<String>,
    /// Short names of the branches checked out in a worktree (this one included).
    pub in_worktrees: Vec<String>,
}

impl MenuCtx {
    pub fn new(repo: &RepoData) -> Self {
        let current = match repo.head.as_ref() {
            Some(Head::Branch { name, .. }) => Some(name.clone()),
            _ => None,
        };
        Self {
            current,
            remotes: repo.remotes.clone(),
            unborn: repo.head.as_ref().is_some_and(Head::is_unborn),
            solo: repo.solo.clone(),
            in_worktrees: repo.worktrees.iter().filter_map(|w| w.branch.clone()).collect(),
        }
    }
}

/// "Solo" (show only this branch and its ancestors), or "Show all branches" when it is the soloed one.
fn solo_item(ui: &mut Ui, branch: &Branch, ctx: &MenuCtx, out: &mut Vec<Intent>) {
    if ctx.solo.as_deref() == Some(branch.refname.as_str()) {
        if item(ui, "Show all branches", None) {
            out.push(Intent::Solo(None));
        }
    } else if item(
        ui,
        &format!("Solo {}", branch.name),
        branch.target.is_none().then_some("The branch has no commit"),
    ) {
        out.push(Intent::Solo(Some(branch.refname.clone())));
    }
}

/// A menu entry, disabled with `disabled` as its tooltip. Returns true when clicked.
pub(super) fn item(ui: &mut Ui, label: &str, disabled: Option<&str>) -> bool {
    let resp = ui.add_enabled(disabled.is_none(), egui::Button::new(label));
    match disabled {
        Some(reason) => {
            resp.on_disabled_hover_text(reason);
            false
        }
        None => resp.clicked(),
    }
}

pub(super) fn local_branch(ui: &mut Ui, branch: &Branch, ctx: &MenuCtx, out: &mut Vec<Intent>) {
    let name = &branch.name;
    let head = branch.is_head;
    if item(ui, "Checkout", head.then_some("Already checked out")) {
        out.push(Intent::CheckoutBranch {
            refname: branch.refname.clone(),
            label: name.clone(),
        });
    }
    let pull_disabled = if !head {
        Some("Only the checked-out branch can be pulled")
    } else if !branch.has_upstream() {
        Some("This branch has no upstream")
    } else if branch.upstream_gone {
        Some("The upstream branch is gone")
    } else {
        None
    };
    if item(ui, "Pull", pull_disabled) {
        out.push(Intent::Pull(branch.refname.clone()));
    }
    if item(ui, "Push", ctx.remotes.is_empty().then_some("This repository has no remotes")) {
        out.push(Intent::Push(branch.refname.clone()));
    }
    if !head {
        let label = match &ctx.current {
            Some(current) => format!("Merge {name} into {current}"),
            None => format!("Merge {name} into current branch"),
        };
        if item(ui, &label, ctx.current.is_none().then_some("Check out a branch first")) {
            out.push(Intent::Merge(branch.refname.clone()));
        }
    }
    if item(
        ui,
        &format!("Create branch from {name}…"),
        branch.target.is_none().then_some("The branch has no commit"),
    ) {
        out.push(Intent::CreateBranch {
            from: branch.target.clone(),
            label: Some(name.clone()),
        });
    }
    let worktree_disabled = if head {
        Some("Already checked out here")
    } else if ctx.in_worktrees.iter().any(|b| b == name) {
        Some("Already checked out in another worktree")
    } else {
        None
    };
    if item(ui, "Check out in new worktree…", worktree_disabled) {
        out.push(Intent::AddWorktree(Some(branch.refname.clone())));
    }
    solo_item(ui, branch, ctx, out);
    if item(ui, "Copy name", None) {
        ui.ctx().copy_text(name.clone());
    }
    ui.separator();
    if item(ui, &format!("Rename {name}…"), None) {
        out.push(Intent::RenameBranch {
            refname: branch.refname.clone(),
            name: name.clone(),
        });
    }
    if item(
        ui,
        &format!("Delete {name}…"),
        head.then_some("Can't delete the checked-out branch"),
    ) {
        out.push(Intent::DeleteBranch {
            refname: branch.refname.clone(),
            name: name.clone(),
        });
    }
}

pub(super) fn remote_branch(ui: &mut Ui, branch: &Branch, ctx: &MenuCtx, out: &mut Vec<Intent>) {
    let name = &branch.name;
    if item(ui, "Checkout", None) {
        out.push(Intent::CheckoutBranch {
            refname: branch.refname.clone(),
            label: name.clone(),
        });
    }
    let label = match &ctx.current {
        Some(current) => format!("Merge {name} into {current}"),
        None => format!("Merge {name} into current branch"),
    };
    if item(ui, &label, ctx.current.is_none().then_some("Check out a branch first")) {
        out.push(Intent::Merge(branch.refname.clone()));
    }
    solo_item(ui, branch, ctx, out);
    if item(ui, "Copy name", None) {
        ui.ctx().copy_text(name.clone());
    }
    ui.separator();
    if item(ui, &format!("Delete {name} from remote…"), None) {
        out.push(Intent::DeleteRemoteBranch {
            refname: branch.refname.clone(),
            name: name.clone(),
        });
    }
}

/// Tag menu. Opening it starts loading the remotes' tag lists once (needed for "Delete tag from <remote>").
pub(super) fn tag(ui: &mut Ui, row: &TagRow, tag: Option<&Tag>, ctx: &MenuCtx, remote_tags: &RemoteTags, out: &mut Vec<Intent>) {
    if matches!(remote_tags, RemoteTags::Unknown) && !ctx.remotes.is_empty() && !out.contains(&Intent::LoadRemoteTags) {
        out.push(Intent::LoadRemoteTags);
    }
    let name = &row.name;
    let local = tag.is_some();
    let push_disabled = if ctx.remotes.is_empty() {
        Some("This repository has no remotes")
    } else if !local {
        Some("The tag is not in this repository")
    } else {
        None
    };
    match push_disabled {
        Some(reason) => {
            item(ui, "Push tag to", Some(reason));
        }
        None => {
            ui.menu_button("Push tag to", |ui| {
                for remote in &ctx.remotes {
                    if item(ui, remote, None) {
                        out.push(Intent::PushTag {
                            remote: remote.clone(),
                            name: name.clone(),
                        });
                    }
                }
            });
        }
    }
    if item(ui, "Copy name", None) {
        ui.ctx().copy_text(name.clone());
    }
    ui.separator();
    if item(ui, "Delete local tag…", (!local).then_some("The tag only exists on the remote")) {
        out.push(Intent::DeleteTag(name.clone()));
    }
    for remote in &ctx.remotes {
        let reason = match remote_tags {
            RemoteTags::Loaded(_) if row.on_remotes.contains(remote) => None,
            RemoteTags::Loaded(_) => Some(format!("{remote} doesn't have this tag")),
            RemoteTags::Unknown | RemoteTags::Loading => Some(format!("Checking {remote}…")),
            RemoteTags::Failed(err) => Some(format!("Couldn't list the tags on the remotes: {err}")),
        };
        if item(ui, &format!("Delete tag from {remote}…"), reason.as_deref()) {
            out.push(Intent::DeleteRemoteTag {
                remote: remote.clone(),
                name: name.clone(),
            });
        }
    }
}
