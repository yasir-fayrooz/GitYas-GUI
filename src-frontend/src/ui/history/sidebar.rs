//! §5 Refs sidebar (the Branches panel pinned on the right; its TAGS / STASHES rows also fill the context bar's popups): "Create branch", filter box and collapsible LOCAL / REMOTE / TAGS / STASHES sections. Section and folder
//! open state persist in egui memory (keyed by section and folder path).

use egui::{Align2, Color32, FontId, Margin, Rect, RichText, Sense, Ui, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;
use gityas_core::models::{Branch, StashDetails};

use super::menus::{self, MenuCtx};
use super::model::{TagRow, TreeRow, ahead_behind, filter_matches, folder_rows, tag_rows};
use super::{HistoryTab, Intent, RemoteTags};
use crate::app::{GitYasApp, Selection};
use crate::store::RepoData;
use crate::theme::{self, Palette};

const ROW_H: f32 = 24.0;
const HEADER_H: f32 = 28.0;
const INDENT: f32 = 14.0;

pub(super) fn show(ui: &mut Ui, app: &mut GitYasApp) -> Vec<Intent> {
    let mut out = Vec::new();
    let p = theme::pal(ui);
    let GitYasApp { store, tabs, .. } = app;
    let (Some(repo), Some(active)) = (store.repo.as_ref(), store.active.clone()) else {
        return out;
    };
    let tab = tabs.entry(active).or_default();
    let selection = tab.selection.clone();
    egui::Frame::new().inner_margin(Margin::symmetric(0, 8)).show(ui, |ui| {
        egui::Frame::new().inner_margin(Margin::symmetric(10, 0)).show(ui, |ui| {
            let unborn = repo.head.as_ref().is_some_and(|h| h.is_unborn());
            ui.vertical_centered(|ui| {
                let label = format!("{} Create branch", icon::PLUS);
                let resp = crate::ui::solid_button(ui, &label, p.accent, !unborn)
                    .on_hover_text("Create a branch from HEAD (Cmd+B)")
                    .on_disabled_hover_text("No commits yet");
                if resp.clicked() {
                    out.push(Intent::CreateBranch { from: None, label: None });
                }
            });
            ui.add_space(6.0);
            filter_box(ui, p, &mut tab.history.filter);
        });
        ui.add_space(6.0);
        let ctx = MenuCtx::new(repo);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            local_section(ui, p, repo, &tab.history, &ctx, &mut out);
            remote_section(ui, p, repo, &tab.history, &ctx, &mut out);
            tags_section(ui, p, repo, &tab.history, &tab.history.filter, &ctx, true, &mut out);
            stash_section(ui, p, repo, &tab.history.filter, &selection, true, &mut out);
        });
    });
    out
}

fn filter_box(ui: &mut Ui, p: &Palette, text: &mut String) {
    egui::Frame::new()
        .fill(p.bg_input)
        .corner_radius(4)
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(p.text_muted));
                ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text("Filter")
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY),
                );
            });
        });
}

/// Persisted open state (default open).
fn is_open(ctx: &egui::Context, key: &str) -> bool {
    ctx.data_mut(|d| d.get_persisted::<bool>(egui::Id::new(("sidebar_open", key))))
        .unwrap_or(true)
}

fn set_open(ctx: &egui::Context, key: &str, open: bool) {
    ctx.data_mut(|d| d.insert_persisted(egui::Id::new(("sidebar_open", key)), open));
}

struct Plus<'a> {
    tooltip: &'a str,
    disabled: Option<&'a str>,
}

/// Section header: caret, icon, title, count and an optional `+` button. Returns `(open, plus_clicked)`.
fn section(ui: &mut Ui, p: &Palette, glyph: &str, title: &str, count: usize, plus: Option<Plus<'_>>) -> (bool, bool) {
    let key = format!("section:{title}");
    let mut open = is_open(ui.ctx(), &key);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), HEADER_H), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::CollapsingHeader, true, title));
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, 0, p.bg_hover);
    }
    let caret = if open { icon::CARET_DOWN } else { icon::CARET_RIGHT };
    let y = rect.center().y;
    painter.text(
        egui::pos2(rect.left() + 10.0, y),
        Align2::LEFT_CENTER,
        caret,
        FontId::proportional(11.0),
        p.text_muted,
    );
    painter.text(
        egui::pos2(rect.left() + 26.0, y),
        Align2::LEFT_CENTER,
        glyph,
        FontId::proportional(14.0),
        p.text_muted,
    );
    painter.text(
        egui::pos2(rect.left() + 46.0, y),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(11.5),
        p.text_muted,
    );
    let mut right = rect.right() - 12.0;
    let mut plus_clicked = false;
    if let Some(plus) = plus {
        let r = Rect::from_center_size(egui::pos2(right - 8.0, y), egui::vec2(20.0, 20.0));
        let enabled = plus.disabled.is_none();
        let pr = ui.interact(r, resp.id.with("plus"), if enabled { Sense::click() } else { Sense::hover() });
        pr.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, plus.tooltip));
        let color = match (enabled, pr.hovered()) {
            (false, _) => p.text_faint,
            (true, true) => p.text,
            (true, false) => p.text_muted,
        };
        ui.painter()
            .text(r.center(), Align2::CENTER_CENTER, icon::PLUS, FontId::proportional(13.0), color);
        plus_clicked = enabled && pr.clicked();
        pr.on_hover_text(plus.disabled.unwrap_or(plus.tooltip));
        right = r.left() - 6.0;
    }
    ui.painter().text(
        egui::pos2(right, y),
        Align2::RIGHT_CENTER,
        count.to_string(),
        FontId::proportional(11.5),
        p.text_faint,
    );
    if resp.clicked() && !plus_clicked {
        open = !open;
        set_open(ui.ctx(), &key, open);
    }
    (open, plus_clicked)
}

/// A muted one-liner ("No tags").
fn empty_line(ui: &mut Ui, p: &Palette, text: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::hover());
    ui.painter().text(
        egui::pos2(rect.left() + 30.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(12.0),
        p.text_faint,
    );
}

/// Something painted at the right edge of a row (badge, icon with tooltip).
struct Trailing {
    text: String,
    color: Color32,
    tooltip: Option<String>,
}

struct RowSpec<'a> {
    depth: usize,
    glyph: &'a str,
    glyph_color: Color32,
    /// Shown text (last path component for branches in folders).
    text: &'a str,
    /// Accessible label + tooltip (full name).
    label: &'a str,
    active: bool,
    muted: bool,
    trailing: Vec<Trailing>,
}

fn row(ui: &mut Ui, p: &Palette, spec: RowSpec<'_>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, spec.active, spec.label));
    if spec.active {
        ui.painter().rect_filled(rect, 0, p.bg_selected);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 0, p.bg_hover);
    }
    let x = rect.left() + 30.0 + spec.depth as f32 * INDENT;
    let y = rect.center().y;
    ui.painter().text(
        egui::pos2(x, y),
        Align2::LEFT_CENTER,
        spec.glyph,
        FontId::proportional(13.0),
        spec.glyph_color,
    );

    // Trailing items, right to left.
    let mut right = rect.right() - 10.0;
    for (i, t) in spec.trailing.iter().enumerate().rev() {
        let r = ui.painter().text(
            egui::pos2(right, y),
            Align2::RIGHT_CENTER,
            &t.text,
            FontId::proportional(11.0),
            t.color,
        );
        if let Some(tip) = &t.tooltip {
            ui.interact(r, resp.id.with(("trailing", i)), Sense::hover()).on_hover_text(tip);
        }
        right = r.left() - 6.0;
    }
    let text_color = if spec.muted {
        p.text_faint
    } else if spec.active {
        p.text
    } else {
        p.text.gamma_multiply(0.85)
    };
    crate::ui::paint_truncated(
        ui.painter(),
        egui::pos2(x + 20.0, y),
        spec.text,
        FontId::proportional(13.0),
        text_color,
        right - (x + 20.0) - 4.0,
    );
    resp.on_hover_text(spec.label)
}

/// Icon buttons over the right end of a hovered row (`(glyph, tooltip)`; the tooltip is the accessible name).
/// Returns the index of the clicked one.
fn hover_actions(ui: &mut Ui, p: &Palette, row: &egui::Response, buttons: &[(&str, String)]) -> Option<usize> {
    let rect = row.rect;
    if !ui.rect_contains_pointer(rect) {
        return None;
    }
    let size = egui::vec2(24.0, rect.height() - 4.0);
    let strip = Rect::from_min_max(
        egui::pos2(rect.right() - 8.0 - buttons.len() as f32 * size.x, rect.top()),
        rect.right_bottom(),
    );
    // Covers the trailing badges.
    ui.painter().rect_filled(strip, 0, p.bg_hover);
    let mut clicked = None;
    for (i, (glyph, tooltip)) in buttons.iter().enumerate() {
        let r = Rect::from_min_size(egui::pos2(strip.left() + i as f32 * size.x, rect.top() + 2.0), size);
        let resp = ui.interact(r, row.id.with(("action", i)), Sense::click());
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, tooltip));
        if resp.hovered() {
            ui.painter().rect_filled(r, 4, p.bg_selected);
        }
        let color = if resp.hovered() { p.text } else { p.text_muted };
        ui.painter()
            .text(r.center(), Align2::CENTER_CENTER, *glyph, FontId::proportional(13.0), color);
        if resp.on_hover_text(tooltip).clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// A folder row (`feature/`): click toggles it. `key` is its persisted state key.
/// `(key, label)`: persisted state key and accessible label.
fn folder_row(ui: &mut Ui, p: &Palette, depth: usize, name: &str, open: bool, count: usize, (key, label): (&str, &str)) {
    let resp = row(
        ui,
        p,
        RowSpec {
            depth,
            glyph: if open { icon::FOLDER_OPEN } else { icon::FOLDER },
            glyph_color: p.text_muted,
            text: name,
            label,
            active: false,
            muted: false,
            trailing: vec![Trailing {
                text: count.to_string(),
                color: p.text_faint,
                tooltip: None,
            }],
        },
    );
    if resp.clicked() {
        set_open(ui.ctx(), key, !open);
    }
}

fn local_section(ui: &mut Ui, p: &Palette, repo: &RepoData, tab: &HistoryTab, ctx: &MenuCtx, out: &mut Vec<Intent>) {
    let branches: Vec<&Branch> = repo
        .local_branches
        .iter()
        .filter(|b| filter_matches(&b.name, &tab.filter))
        .collect();
    let (open, _) = section(ui, p, icon::DESKTOP, "LOCAL", branches.len(), None);
    if !open {
        return;
    }
    if branches.is_empty() {
        let text = if !tab.filter.trim().is_empty() {
            "No matches"
        } else if !repo.loaded.local_branches {
            "Loading…"
        } else {
            "No local branches"
        };
        return empty_line(ui, p, text);
    }
    let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
    let egui_ctx = ui.ctx().clone();
    let rows = folder_rows(
        &names,
        |path| is_open(&egui_ctx, &format!("local:{path}")),
        !tab.filter.trim().is_empty(),
    );
    for r in rows {
        match r {
            TreeRow::Folder {
                depth,
                path,
                name,
                open,
                count,
            } => folder_row(ui, p, depth, &name, open, count, (&format!("local:{path}"), &format!("{path}/"))),
            TreeRow::Leaf { depth, name, index } => {
                let Some(branch) = branches.get(index) else { continue };
                local_branch_row(ui, p, depth, &name, branch, ctx, out);
            }
        }
    }
}

fn local_branch_row(ui: &mut Ui, p: &Palette, depth: usize, text: &str, branch: &Branch, ctx: &MenuCtx, out: &mut Vec<Intent>) {
    let mut trailing = Vec::new();
    if branch.upstream_gone {
        let upstream = branch.upstream.as_deref().unwrap_or("upstream");
        trailing.push(Trailing {
            text: icon::CLOUD_SLASH.to_owned(),
            color: p.text_faint,
            tooltip: Some(format!("Upstream {upstream} is gone")),
        });
    }
    let badge = ahead_behind(branch.ahead, branch.behind);
    if !badge.is_empty() {
        let upstream = branch.upstream.as_deref().unwrap_or("upstream");
        trailing.push(Trailing {
            text: badge,
            color: p.text_muted,
            tooltip: Some(format!("{} ahead, {} behind {upstream}", branch.ahead, branch.behind)),
        });
    }
    let resp = row(
        ui,
        p,
        RowSpec {
            depth,
            glyph: if branch.is_head { icon::CHECK } else { icon::GIT_BRANCH },
            glyph_color: if branch.is_head { p.green } else { p.text_muted },
            text,
            label: &branch.name,
            active: branch.is_head,
            muted: false,
            trailing,
        },
    );
    if resp.double_clicked() {
        if !branch.is_head {
            out.push(Intent::CheckoutBranch {
                refname: branch.refname.clone(),
                label: branch.name.clone(),
            });
        }
    } else if resp.clicked()
        && let Some(target) = &branch.target
    {
        out.push(Intent::Reveal(target.clone()));
    }
    resp.context_menu(|ui| menus::local_branch(ui, branch, ctx, out));
}

fn remote_section(ui: &mut Ui, p: &Palette, repo: &RepoData, tab: &HistoryTab, ctx: &MenuCtx, out: &mut Vec<Intent>) {
    let branches: Vec<&Branch> = repo
        .remote_branches
        .iter()
        .filter(|b| !b.is_remote_head() && filter_matches(&b.name, &tab.filter))
        .collect();
    let (open, _) = section(ui, p, icon::CLOUD, "REMOTE", branches.len(), None);
    if !open {
        return;
    }
    let filtering = !tab.filter.trim().is_empty();
    // Remotes in the store's order, plus any remote only known from a branch.
    let mut remotes: Vec<String> = repo.remotes.clone();
    for b in &branches {
        if let Some(r) = &b.remote_name
            && !remotes.contains(r)
        {
            remotes.push(r.clone());
        }
    }
    if remotes.is_empty() {
        return empty_line(ui, p, if repo.loaded.remotes { "No remotes" } else { "Loading…" });
    }
    if filtering && branches.is_empty() {
        return empty_line(ui, p, "No matches");
    }
    let egui_ctx = ui.ctx().clone();
    for remote in &remotes {
        let of_remote: Vec<&Branch> = branches
            .iter()
            .copied()
            .filter(|b| b.remote_name.as_ref() == Some(remote))
            .collect();
        if filtering && of_remote.is_empty() {
            continue;
        }
        let key = format!("remote:{remote}");
        let open = filtering || is_open(&egui_ctx, &key);
        let resp = row(
            ui,
            p,
            RowSpec {
                depth: 0,
                glyph: icon::CLOUD,
                glyph_color: p.text_muted,
                text: remote,
                label: &format!("remote {remote}"),
                active: false,
                muted: false,
                trailing: vec![Trailing {
                    text: of_remote.len().to_string(),
                    color: p.text_faint,
                    tooltip: None,
                }],
            },
        );
        if resp.clicked() {
            set_open(&egui_ctx, &key, !open);
        }
        if !open {
            continue;
        }
        let names: Vec<&str> = of_remote.iter().map(|b| b.name_without_remote()).collect();
        let rows = folder_rows(&names, |path| is_open(&egui_ctx, &format!("{key}/{path}")), filtering);
        for r in rows {
            match r {
                TreeRow::Folder {
                    depth,
                    path,
                    name,
                    open,
                    count,
                } => folder_row(
                    ui,
                    p,
                    depth + 1,
                    &name,
                    open,
                    count,
                    (&format!("{key}/{path}"), &format!("{remote}/{path}/")),
                ),
                TreeRow::Leaf { depth, name, index } => {
                    let Some(branch) = of_remote.get(index) else { continue };
                    let resp = row(
                        ui,
                        p,
                        RowSpec {
                            depth: depth + 1,
                            glyph: icon::GIT_BRANCH,
                            glyph_color: p.text_muted,
                            text: &name,
                            label: &branch.name,
                            active: false,
                            muted: false,
                            trailing: Vec::new(),
                        },
                    );
                    if resp.double_clicked() {
                        out.push(Intent::CheckoutBranch {
                            refname: branch.refname.clone(),
                            label: branch.name.clone(),
                        });
                    } else if resp.clicked()
                        && let Some(target) = &branch.target
                    {
                        out.push(Intent::Reveal(target.clone()));
                    }
                    resp.context_menu(|ui| menus::remote_branch(ui, branch, ctx, out));
                }
            }
        }
    }
}

/// TAGS: a collapsible section in the sidebar (`header`), or the bare rows (the context bar's Tags popup).
#[allow(clippy::too_many_arguments)]
fn tags_section(
    ui: &mut Ui,
    p: &Palette,
    repo: &RepoData,
    tab: &HistoryTab,
    filter: &str,
    ctx: &MenuCtx,
    header: bool,
    out: &mut Vec<Intent>,
) {
    let remote = match &tab.remote_tags {
        RemoteTags::Loaded(map) => Some(map),
        _ => None,
    };
    let tags = &repo.commits.tags;
    let rows: Vec<TagRow> = tag_rows(tags, remote)
        .into_iter()
        .filter(|r| filter_matches(&r.name, filter))
        .collect();
    if header {
        let plus = Plus {
            tooltip: "Create tag on HEAD…",
            disabled: ctx.unborn.then_some("No commits yet"),
        };
        let (open, plus_clicked) = section(ui, p, icon::TAG, "TAGS", rows.len(), Some(plus));
        if plus_clicked {
            out.push(Intent::CreateTag(None));
        }
        if !open {
            return;
        }
    }
    if let RemoteTags::Failed(err) = &tab.remote_tags {
        let resp = row(
            ui,
            p,
            RowSpec {
                depth: 0,
                glyph: icon::WARNING,
                glyph_color: p.orange,
                text: "Couldn't list remote tags (click to retry)",
                label: err,
                active: false,
                muted: true,
                trailing: Vec::new(),
            },
        );
        if resp.clicked() {
            out.push(Intent::LoadRemoteTags);
        }
    }
    if rows.is_empty() {
        let text = if filter.trim().is_empty() { "No tags" } else { "No matches" };
        return empty_line(ui, p, text);
    }
    for tag_row in &rows {
        let tag = tag_row.local.and_then(|i| tags.get(i));
        let mut trailing = Vec::new();
        if let Some(t) = tag
            && let Some(annotation) = &t.annotation
        {
            trailing.push(Trailing {
                text: icon::NOTE.to_owned(),
                color: p.text_muted,
                tooltip: Some(if annotation.message.trim().is_empty() {
                    "Annotated tag".to_owned()
                } else {
                    annotation.message.trim().to_owned()
                }),
            });
        }
        let remote_only = tag.is_none();
        if remote_only {
            trailing.push(Trailing {
                text: icon::CLOUD.to_owned(),
                color: p.text_faint,
                tooltip: Some(format!("Only on {}", tag_row.on_remotes.join(", "))),
            });
        } else if remote.is_some() && tag_row.on_remotes.is_empty() {
            trailing.push(Trailing {
                text: icon::DESKTOP.to_owned(),
                color: p.text_faint,
                tooltip: Some("Only in this repository (not pushed)".to_owned()),
            });
        }
        let resp = row(
            ui,
            p,
            RowSpec {
                depth: 0,
                glyph: icon::TAG,
                glyph_color: if remote_only { p.text_faint } else { p.text_muted },
                text: &tag_row.name,
                label: &tag_row.name,
                active: false,
                muted: remote_only,
                trailing,
            },
        );
        // A popup can't host a context menu (opening one closes the popup): its rows get hover buttons instead.
        if !header && tag.is_some() {
            let remote = ctx.remotes.iter().find(|r| r.as_str() == "origin").or(ctx.remotes.first());
            let mut buttons = Vec::new();
            if let Some(remote) = remote {
                buttons.push((icon::CLOUD_ARROW_UP, format!("Push tag to {remote}")));
            }
            buttons.push((icon::TRASH, "Delete tag".to_owned()));
            match hover_actions(ui, p, &resp, &buttons) {
                Some(0) if let Some(remote) = remote => out.push(Intent::PushTag {
                    remote: remote.clone(),
                    name: tag_row.name.clone(),
                }),
                Some(_) => out.push(Intent::DeleteTag(tag_row.name.clone())),
                None => {}
            }
        }
        if resp.clicked()
            && let Some(t) = tag
        {
            out.push(Intent::Reveal(t.target.clone()));
        }
        if header {
            resp.context_menu(|ui| menus::tag(ui, tag_row, tag, ctx, &tab.remote_tags, out));
        }
    }
}

fn stash_counts(p: &Palette, stash: &StashDetails) -> Vec<Trailing> {
    let c = stash.stash.counts;
    [(c.added, "+", p.green), (c.modified, "~", p.orange), (c.deleted, "−", p.red)]
        .into_iter()
        .filter(|(n, _, _)| *n > 0)
        .map(|(n, sign, color)| Trailing {
            text: format!("{sign}{n}"),
            color,
            tooltip: None,
        })
        .collect()
}

/// STASHES: a collapsible section in the sidebar (`header`), or the bare rows (the context bar's Stashes popup).
fn stash_section(ui: &mut Ui, p: &Palette, repo: &RepoData, filter: &str, selection: &Selection, header: bool, out: &mut Vec<Intent>) {
    let stashes: Vec<&StashDetails> = repo.stashes.iter().filter(|s| filter_matches(&s.stash.message, filter)).collect();
    if header {
        let plus = Plus {
            tooltip: "Stash all changes…",
            disabled: (!repo.has_wip()).then_some("Nothing to stash"),
        };
        let (open, plus_clicked) = section(ui, p, icon::STACK, "STASHES", stashes.len(), Some(plus));
        if plus_clicked {
            out.push(Intent::StashAll);
        }
        if !open {
            return;
        }
    }
    if stashes.is_empty() {
        let text = if !filter.trim().is_empty() {
            "No matches"
        } else if !repo.loaded.stashes {
            "Loading…"
        } else {
            "No stashes"
        };
        return empty_line(ui, p, text);
    }
    for stash in stashes {
        let id = &stash.stash.id;
        let label = format!("{} {}", stash.stash.revspec(), stash.stash.message);
        let resp = row(
            ui,
            p,
            RowSpec {
                depth: 0,
                glyph: icon::STACK,
                glyph_color: p.text_muted,
                text: &stash.stash.message,
                label: &label,
                active: selection == &Selection::Stash(id.clone()),
                muted: false,
                trailing: stash_counts(p, stash),
            },
        );
        if !header {
            let buttons = [
                (icon::ARROW_SQUARE_IN, "Apply stash".to_owned()),
                (icon::TRAY_ARROW_UP, "Pop stash".to_owned()),
                (icon::TRASH, "Delete stash".to_owned()),
            ];
            match hover_actions(ui, p, &resp, &buttons) {
                Some(0) => out.push(Intent::ApplyStash(id.clone())),
                Some(1) => out.push(Intent::PopStash(id.clone())),
                Some(_) => out.push(Intent::DeleteStash(id.clone())),
                None => {}
            }
        }
        if resp.clicked() {
            out.push(Intent::Select(Selection::Stash(id.clone())));
        }
        if !header {
            continue;
        }
        resp.context_menu(|ui| {
            if menus::item(ui, "Apply", None) {
                out.push(Intent::ApplyStash(id.clone()));
            }
            if menus::item(ui, "Pop", None) {
                out.push(Intent::PopStash(id.clone()));
            }
            ui.separator();
            if menus::item(ui, "Delete…", None) {
                out.push(Intent::DeleteStash(id.clone()));
            }
        });
    }
}
