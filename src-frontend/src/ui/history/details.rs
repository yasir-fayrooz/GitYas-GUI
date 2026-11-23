//! §7 Commit / stash details (the Changes panel while a commit / stash is selected). The message and file list of a commit are fetched lazily and cached
//! per commit id ([`super::HistoryUi`]); stash files come with `StashDetails`.

use egui::{Margin, RichText, Sense, Ui, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;
use gityas_core::Oid;
use gityas_core::diff::DiffTarget;
use gityas_core::models::{Commit, Delta, StashDetails};

use super::actions::{TASK_REWORD, TASK_STASH, ensure_commit_data};
use super::model::{PillKind, build_pills, is_head_or_ancestor, message_body};
use super::{HistoryUi, Intent, Loadable, MessageEdit};
use crate::app::{DiffRequest, GitYasApp, Selection};
use crate::store::RepoData;
use crate::theme::{self, Palette};
use crate::ui::{avatar, change_icon, format_time, icon_button, paint_truncated};

pub(super) fn show(ui: &mut Ui, app: &mut GitYasApp) -> Vec<Intent> {
    let mut out = Vec::new();
    let selection = app.tab().map(|t| t.selection.clone()).unwrap_or_default();
    if let Selection::Commit(id) = &selection {
        ensure_commit_data(app, id);
    }
    let p = theme::pal(ui);
    let stash_busy = app.bridge.is_running(TASK_STASH);
    let reword = Reword {
        allowed: matches!(&selection, Selection::Commit(id) if rewordable(app, id)),
        busy: app.bridge.is_running(TASK_REWORD),
    };
    // A draft belongs to the commit it was opened on; selecting anything else discards it.
    if app
        .history_ui
        .message_edit
        .as_ref()
        .is_some_and(|e| selection != Selection::Commit(e.commit.clone()))
    {
        app.history_ui.message_edit = None;
    }
    let GitYasApp { store, history_ui, .. } = app;
    let Some(repo) = store.repo.as_ref() else { return out };
    let opts = &store.settings.graph;
    match &selection {
        Selection::Commit(id) => commit_panel(ui, p, repo, opts, history_ui, id, reword, &mut out),
        Selection::Stash(id) => stash_panel(ui, p, repo, id, stash_busy, &mut out),
        Selection::Wip => {}
    }
    out
}

/// Whether the selected commit's message can be edited, and whether a save is running.
#[derive(Clone, Copy)]
struct Reword {
    allowed: bool,
    busy: bool,
}

/// HEAD or one of its ancestors (only those can be reworded); cached per (HEAD, commit) since the walk can be long.
fn rewordable(app: &mut GitYasApp, id: &Oid) -> bool {
    let Some(repo) = app.store.repo.as_ref() else { return false };
    let Some(head) = repo.head_target() else { return false };
    if let Some((h, c, answer)) = &app.history_ui.rewordable
        && h == head
        && c == id
    {
        return *answer;
    }
    let answer = is_head_or_ancestor(&repo.commits.commits, &repo.index, head, id);
    app.history_ui.rewordable = Some((head.clone(), id.clone(), answer));
    answer
}

/// [`icon_button`] whose accessible label is its tooltip (the glyph alone says nothing to a screen reader / test).
fn labeled_icon_button(ui: &mut Ui, glyph: &str, tooltip: &str) -> egui::Response {
    let resp = icon_button(ui, glyph, tooltip);
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, tooltip));
    resp
}

/// `commit: a1b2c3d ‹COPY›  …  ✕`
fn header(ui: &mut Ui, p: &Palette, kind: &str, short: &str, full: &str, out: &mut Vec<Intent>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(kind).color(p.text_muted));
        ui.label(RichText::new(short).monospace().color(p.accent));
        if labeled_icon_button(ui, icon::COPY, "Copy full SHA").clicked() {
            ui.ctx().copy_text(full.to_owned());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if labeled_icon_button(ui, icon::X, "Back to the working tree").clicked() {
                out.push(Intent::Select(Selection::Wip));
            }
        });
    });
    ui.add_space(6.0);
}

#[allow(clippy::too_many_arguments)]
fn commit_panel(
    ui: &mut Ui,
    p: &Palette,
    repo: &RepoData,
    opts: &gityas_core::models::GraphOptions,
    cache: &mut HistoryUi,
    id: &Oid,
    reword: Reword,
    out: &mut Vec<Intent>,
) {
    header(ui, p, "commit:", &id.short(7), &id.to_hex(), out);
    let Some(commit) = repo.commit_index(id).and_then(|i| repo.commits.commits.get(i)) else {
        ui.label(RichText::new("This commit is not in the loaded history.").color(p.text_muted));
        return;
    };

    // Everything below the header scrolls together (a long message can fill the panel on its own).
    scroll_body(ui, id, |ui| {
        if let Some(edit) = cache.message_edit.as_mut().filter(|e| &e.commit == id) {
            if message_editor(ui, p, edit, reword.busy, repo.head_branch().map(|b| b.name.as_str()), out) {
                cache.message_edit = None;
            }
        } else {
            message_view(ui, p, commit, cache, id, reword.allowed);
        }
        ui.add_space(10.0);
        author(ui, p, commit);
        ui.add_space(6.0);
        parents(ui, p, repo, commit, out);
        refs(ui, p, repo, opts, commit);

        ui.add_space(8.0);
        ui.separator();
        match cache.files.get(id) {
            Some(Loadable::Ready(files)) => {
                ui.horizontal_wrapped(|ui| {
                    let n = files.len();
                    ui.label(
                        RichText::new(if n == 1 {
                            "1 file changed".to_owned()
                        } else {
                            format!("{n} files changed")
                        })
                        .strong(),
                    );
                    if commit.is_merge() {
                        ui.label(RichText::new("(merge commit, compared with first parent)").color(p.text_muted));
                    }
                });
                ui.add_space(4.0);
                file_list(
                    ui,
                    p,
                    files,
                    |file| DiffRequest {
                        target: DiffTarget::Commit(id.clone()),
                        file: file.clone(),
                    },
                    out,
                );
            }
            Some(Loadable::Loading) => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(RichText::new("Loading files…").color(p.text_muted));
                });
            }
            Some(Loadable::Failed(err)) => {
                ui.add(egui::Label::new(RichText::new(format!("Could not load the changed files: {err}")).color(p.text_faint)).wrap());
            }
            None => {
                ui.label(RichText::new("Changed files are not available").color(p.text_faint));
            }
        }
    });
}

/// The commit message editor: multi-line field, then [Save] [Cancel]. Cmd+Enter saves, Escape cancels. Returns
/// whether the editor should close (cancelled); after a save it stays open until the result arrives.
fn message_editor(ui: &mut Ui, p: &Palette, edit: &mut MessageEdit, busy: bool, branch: Option<&str>, out: &mut Vec<Intent>) -> bool {
    let field_id = egui::Id::new(("commit_message_edit", &edit.commit));
    let focused = ui.memory(|m| m.has_focus(field_id));
    let mut save = focused && ui.input_mut(|i| i.consume_shortcut(&crate::ui::cmd(egui::Key::Enter)));
    let mut cancel = focused && ui.input(|i| i.key_pressed(egui::Key::Escape));

    let resp = ui.add_enabled(
        !busy,
        egui::TextEdit::multiline(&mut edit.text)
            .id(field_id)
            .hint_text("Commit message")
            .desired_rows(4)
            .desired_width(f32::INFINITY),
    );
    if edit.focus {
        resp.request_focus();
        edit.focus = false;
    }

    let blocker = if busy {
        Some("Saving…")
    } else if edit.text.trim().is_empty() {
        Some("The message cannot be empty")
    } else if edit.text.trim_end() == edit.original.trim_end() {
        Some("The message is unchanged")
    } else {
        None
    };
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let resp = crate::ui::solid_button(ui, "Save", p.green.gamma_multiply(0.85), blocker.is_none());
        let resp = match blocker {
            Some(reason) => resp.on_disabled_hover_text(reason),
            None => resp.on_hover_text("Rewrite the commit with this message (Ctrl/Cmd+Enter)"),
        };
        save |= resp.clicked();
        cancel |= ui
            .add_enabled(!busy, egui::Button::new("Cancel"))
            .on_hover_text("Discard the edit (Esc)")
            .clicked();
        if busy {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    let scope = branch.map_or_else(|| "HEAD".to_owned(), |b| format!("on {b}"));
    ui.label(
        RichText::new(format!(
            "Rewrites this commit and every later commit {scope}; they all get new ids."
        ))
        .small()
        .color(p.text_muted),
    );

    if save && blocker.is_none() {
        out.push(Intent::RewordCommit {
            commit: edit.commit.clone(),
            message: edit.text.clone(),
        });
    }
    cancel && !busy
}

/// Summary and body; when `editable`, a click on the box opens the editor (§7).
fn message_view(ui: &mut Ui, p: &Palette, commit: &Commit, cache: &mut HistoryUi, id: &Oid, rewordable: bool) {
    let editable = rewordable && !commit.lossy && matches!(cache.bodies.get(id), Some(Loadable::Ready(body)) if !body.lossy);
    let frame = egui::Frame::new()
        .fill(p.bg_input)
        .corner_radius(4)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Editable text is not selectable: a click on it opens the editor instead (copy from there).
            ui.add(
                egui::Label::new(RichText::new(&commit.summary).size(14.5).strong())
                    .wrap()
                    .selectable(!editable),
            );
            if commit.lossy {
                ui.label(
                    RichText::new(format!("{} Commit text is not valid UTF-8", icon::WARNING))
                        .small()
                        .color(p.orange),
                );
            }
            match cache.bodies.get(id) {
                Some(Loadable::Ready(body)) => {
                    let rest = message_body(&body.message);
                    if !rest.is_empty() {
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(RichText::new(rest).color(p.text_muted))
                                .wrap()
                                .selectable(!editable),
                        );
                    }
                    if body.lossy {
                        let encoding = body.encoding.as_deref().unwrap_or("none");
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!(
                                "{} Message is not valid UTF-8 (declared encoding: {encoding})",
                                icon::WARNING
                            ))
                            .small()
                            .color(p.orange),
                        );
                    }
                }
                Some(Loadable::Loading) => {
                    ui.add(egui::Spinner::new().size(12.0));
                }
                Some(Loadable::Failed(err)) => {
                    ui.label(
                        RichText::new(format!("Could not load the message: {err}"))
                            .small()
                            .color(p.text_faint),
                    );
                }
                None => {}
            }
        });
    if !editable {
        return;
    }
    let resp = ui
        .interact(frame.response.rect, ui.id().with(("commit_message_view", id)), Sense::click())
        .on_hover_text("Click to edit the message")
        .on_hover_cursor(egui::CursorIcon::Text);
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Edit commit message"));
    if resp.hovered() {
        ui.painter()
            .rect_stroke(frame.response.rect, 4, egui::Stroke::new(1.0, p.accent), egui::StrokeKind::Inside);
    }
    if resp.clicked()
        && let Some(Loadable::Ready(body)) = cache.bodies.get(id)
    {
        let message = body.message.trim_end().to_owned();
        cache.message_edit = Some(MessageEdit {
            commit: id.clone(),
            original: message.clone(),
            text: message,
            focus: true,
        });
    }
}

/// The scrolling part of a details panel; the offset is kept per commit / stash.
fn scroll_body(ui: &mut Ui, id: &Oid, add: impl FnOnce(&mut Ui)) {
    egui::ScrollArea::vertical()
        .id_salt(("details_body", id))
        .auto_shrink(false)
        .show(ui, add);
}

fn author(ui: &mut Ui, p: &Palette, commit: &Commit) {
    ui.horizontal(|ui| {
        avatar(ui, &commit.author_name, 32.0).on_hover_text(&commit.author_email);
        ui.vertical(|ui| {
            ui.label(RichText::new(&commit.author_name).strong())
                .on_hover_text(&commit.author_email);
            ui.label(
                RichText::new(format!("authored {}", format_time(commit.author_time)))
                    .small()
                    .color(p.text_muted),
            );
            // The history carries only the committer's time (not name); show it when it differs.
            if commit.committer_time != commit.author_time {
                ui.label(
                    RichText::new(format!("committed {}", format_time(commit.committer_time)))
                        .small()
                        .color(p.text_muted),
                );
            }
        });
    });
}

fn parents(ui: &mut Ui, p: &Palette, repo: &RepoData, commit: &Commit, out: &mut Vec<Intent>) {
    if commit.parents.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        let label = if commit.is_merge() { "parents:" } else { "parent:" };
        ui.label(RichText::new(label).color(p.text_muted));
        for parent in &commit.parents {
            let text = RichText::new(parent.short(7)).monospace();
            if repo.commit_index(parent).is_some() {
                if ui.link(text.color(p.accent)).on_hover_text("Select this commit").clicked() {
                    out.push(Intent::Reveal(parent.clone()));
                }
            } else {
                ui.label(text.color(p.text_faint))
                    .on_hover_text("Not in the loaded history (shallow clone?)");
            }
        }
    });
}

fn refs(ui: &mut Ui, p: &Palette, repo: &RepoData, opts: &gityas_core::models::GraphOptions, commit: &Commit) {
    let refs = repo.refs_by_commit.get(&commit.id).map_or(&[][..], Vec::as_slice);
    let pills = build_pills(&commit.id, refs, repo.head.as_ref(), &repo.remotes, opts);
    if pills.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for pill in &pills {
            let glyph = match pill.kind {
                PillKind::Local if pill.head => icon::CHECK,
                PillKind::Local => icon::DESKTOP,
                PillKind::Remote => icon::CLOUD,
                PillKind::Tag => icon::TAG,
                PillKind::DetachedHead => icon::CROSSHAIR,
            };
            ui.label(RichText::new(format!("{glyph} {}", pill.label)).small().color(p.text_muted));
        }
    });
}

/// File rows with change icons; a click opens the read-only diff built by `request`.
fn file_list(ui: &mut Ui, p: &Palette, files: &[Delta], request: impl Fn(&Delta) -> DiffRequest, out: &mut Vec<Intent>) {
    if files.is_empty() {
        ui.label(RichText::new("No file changes").color(p.text_faint));
        return;
    }
    // Scrolled by the panel's `scroll_body`; rows outside the viewport are only allocated, not painted.
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for file in files {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), Sense::click());
            if !ui.is_rect_visible(rect) {
                continue;
            }
            let label = file.display_label();
            resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &label));
            if resp.hovered() {
                ui.painter().rect_filled(rect, 3, p.bg_hover);
            }
            let (glyph, color) = change_icon(p, file.change);
            let y = rect.center().y;
            ui.painter().text(
                egui::pos2(rect.left() + 6.0, y),
                egui::Align2::LEFT_CENTER,
                glyph,
                egui::FontId::proportional(12.0),
                color,
            );
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| label.clone());
            let r = paint_truncated(
                ui.painter(),
                egui::pos2(rect.left() + 26.0, y),
                &name,
                egui::FontId::proportional(13.0),
                p.text,
                rect.width() - 30.0,
            );
            if let Some(dir) = file.directory().map(|d| d.display().to_string()).filter(|d| !d.is_empty()) {
                paint_truncated(
                    ui.painter(),
                    egui::pos2(r.right() + 8.0, y),
                    &dir,
                    egui::FontId::proportional(12.0),
                    p.text_faint,
                    rect.right() - r.right() - 12.0,
                );
            }
            if resp.on_hover_text(&label).clicked() {
                out.push(Intent::OpenDiff(request(file)));
            }
        }
    });
}

fn stash_panel(ui: &mut Ui, p: &Palette, repo: &RepoData, id: &Oid, busy: bool, out: &mut Vec<Intent>) {
    let Some(details) = repo.stashes.iter().find(|s| &s.stash.id == id) else {
        header(ui, p, "stash:", &id.short(7), &id.to_hex(), out);
        ui.label(RichText::new("This stash no longer exists.").color(p.text_muted));
        return;
    };
    let StashDetails { stash, files } = details;
    header(ui, p, "stash:", &stash.revspec(), &stash.id.to_hex(), out);
    scroll_body(ui, id, |ui| {
        egui::Frame::new()
            .fill(p.bg_input)
            .corner_radius(4)
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(
                    egui::Label::new(RichText::new(&stash.message).size(14.5).strong())
                        .wrap()
                        .selectable(true),
                );
            });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            avatar(ui, &stash.author.name, 32.0).on_hover_text(&stash.author.email);
            ui.vertical(|ui| {
                ui.label(RichText::new(&stash.author.name).strong())
                    .on_hover_text(&stash.author.email);
                ui.label(
                    RichText::new(format!("stashed {}", format_time(stash.time)))
                        .small()
                        .color(p.text_muted),
                );
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let reason = "A stash operation is running";
            let apply = ui.add_enabled(!busy, egui::Button::new("Apply")).on_disabled_hover_text(reason);
            if apply.on_hover_text("Apply the stash and keep it").clicked() {
                out.push(Intent::ApplyStash(id.clone()));
            }
            let pop = ui.add_enabled(!busy, egui::Button::new("Pop")).on_disabled_hover_text(reason);
            if pop.on_hover_text("Apply the stash and delete it").clicked() {
                out.push(Intent::PopStash(id.clone()));
            }
            let delete = crate::ui::solid_button(ui, "Delete…", p.red, !busy).on_disabled_hover_text(reason);
            if delete.clicked() {
                out.push(Intent::DeleteStash(id.clone()));
            }
        });
        ui.add_space(8.0);
        ui.separator();
        let c = stash.counts;
        ui.horizontal(|ui| {
            let n = files.len();
            ui.label(
                RichText::new(if n == 1 {
                    "1 file changed".to_owned()
                } else {
                    format!("{n} files changed")
                })
                .strong(),
            );
            for (count, sign, color) in [(c.added, "+", p.green), (c.modified, "~", p.orange), (c.deleted, "−", p.red)] {
                if count > 0 {
                    ui.label(RichText::new(format!("{sign}{count}")).color(color));
                }
            }
        });
        ui.add_space(4.0);
        file_list(
            ui,
            p,
            files,
            |file| DiffRequest {
                target: DiffTarget::Stash(id.clone()),
                file: file.clone(),
            },
            out,
        );
    });
}
