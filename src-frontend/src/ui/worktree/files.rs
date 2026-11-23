//! File sections of the WIP panel: Conflicted / Unstaged / Staged, rows, multi-select, context menu, and the
//! stage / unstage / discard / stash actions they trigger.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use egui::{Align2, FontId, RichText, Sense, Ui};
use egui_phosphor::regular as icon;
use gityas_core::diff::DiffTarget;
use gityas_core::models::{Delta, FileState};

use super::model::{self, ClickMods, Section, TreeRow};
use super::{TASK_FILES, WorktreeTab};
use crate::app::{DiffRequest, GitYasApp};
use crate::dialogs::Confirm;
use crate::{platform_ui, theme, ui};

const ROW_HEIGHT: f32 = 26.0;
const SECTION_GAP: f32 = 10.0;

/// What the user asked for this frame (applied after drawing, when the borrows of store and tab are released).
pub(super) enum Action {
    Stage(Vec<Delta>),
    Unstage(Vec<Delta>),
    Discard(Vec<Delta>),
    Stash(Vec<Delta>),
    OpenDiff {
        file: Delta,
        staged: bool,
    },
    OpenMerge(PathBuf),
    /// Stage conflicted files as they are in the working tree.
    MarkResolved(Vec<PathBuf>),
    Reveal(PathBuf),
    /// Drag these files (relative to the working tree) out of the window into another app.
    DragOut(Vec<PathBuf>),
    ToggleDir(PathBuf),
}

pub(super) fn show(ui: &mut Ui, app: &mut GitYasApp) {
    let Some(id) = app.store.active.clone() else { return };
    let Some(repo) = app.store.repo.as_ref() else { return };
    let busy = app.bridge.is_running(TASK_FILES);
    let tab = &mut app.tabs.entry(id).or_default().worktree;
    let files = &repo.files;
    let mut actions = Vec::new();

    let conflicted = model::section_files(files, Section::Conflicted);
    let unstaged = model::section_files(files, Section::Unstaged);
    let staged = model::section_files(files, Section::Staged);

    let sections: Vec<(Section, &Vec<&Delta>)> = [
        (Section::Conflicted, &conflicted),
        (Section::Unstaged, &unstaged),
        (Section::Staged, &staged),
    ]
    .into_iter()
    .filter(|(s, list)| *s != Section::Conflicted || !list.is_empty())
    .collect();
    // Every section keeps an equal share of the height, empty or not, and the last one reaches the commit box. The
    // header height is measured (all headers are alike), so the shares add up exactly.
    let gap = SECTION_GAP + ui.spacing().item_spacing.y;
    let count = sections.len();
    for (i, (section, list)) in sections.into_iter().enumerate() {
        let top = ui.cursor().top();
        section_header(ui, tab, section, list, busy, &mut actions);
        let header = ui.cursor().top() - top;
        let after = (count - i - 1) as f32;
        let height = ((ui.available_height() - after * (header + gap)) / (after + 1.0)).max(ROW_HEIGHT * 2.0);
        section_list(ui, tab, section, list, height, &mut actions);
        if i + 1 < count {
            ui.add_space(SECTION_GAP);
        }
    }

    let workdir = app.store.active_repository().map(|r| r.path.clone());
    for action in actions {
        apply(app, action, workdir.as_deref());
    }
}

fn section_header(ui: &mut Ui, tab: &WorktreeTab, section: Section, list: &[&Delta], busy: bool, actions: &mut Vec<Action>) {
    let p = theme::pal(ui);
    let (title, color) = match section {
        Section::Conflicted => (format!("{} Conflicted files ({})", icon::WARNING, list.len()), p.orange),
        Section::Unstaged => (format!("Unstaged files ({})", list.len()), p.text_muted),
        Section::Staged => (format!("Staged files ({})", list.len()), p.text_muted),
    };
    ui::trailing_row(
        ui,
        |ui| {
            let selected = tab.selection.count_in(section);
            let label = model::section_button_label(section, selected);
            let color = match section {
                Section::Conflicted => p.orange,
                Section::Unstaged => p.green,
                Section::Staged => p.red,
            };
            let enabled = !list.is_empty() && !busy;
            let resp = ui::tinted_button(ui, &label, color, enabled);
            let resp = if list.is_empty() {
                resp.on_disabled_hover_text("No files")
            } else {
                resp.on_disabled_hover_text("Updating files…")
            };
            let resp = if section == Section::Conflicted {
                resp.on_hover_text("Stage the files as they are now in the working tree")
            } else {
                resp
            };
            if resp.clicked() {
                let files = owned(&tab.selection.files_or_all(section, list));
                actions.push(match section {
                    Section::Conflicted => Action::MarkResolved(files.into_iter().map(|d| d.path).collect()),
                    Section::Staged => Action::Unstage(files),
                    Section::Unstaged => Action::Stage(files),
                });
            }
        },
        |ui| {
            ui.add(egui::Label::new(RichText::new(title).size(12.5).strong().color(color)).truncate());
        },
    );
    ui.add_space(2.0);
}

fn owned(files: &[&Delta]) -> Vec<Delta> {
    files.iter().map(|d| (*d).clone()).collect()
}

/// Rows in on-screen order (flat list, or the directory tree).
fn visible_rows(tab: &WorktreeTab, list: &[&Delta]) -> Vec<TreeRow> {
    if tab.tree {
        model::build_tree(list, &tab.collapsed)
    } else {
        (0..list.len()).map(|index| TreeRow::File { depth: 0, index }).collect()
    }
}

/// Paths of the file rows, in on-screen order.
fn row_order<'a>(rows: &[TreeRow], list: &[&'a Delta]) -> Vec<&'a Path> {
    rows.iter()
        .filter_map(|r| match r {
            TreeRow::File { index, .. } => list.get(*index).map(|d| d.path.as_path()),
            TreeRow::Dir { .. } => None,
        })
        .collect()
}

/// The row to show after `moving` leaves `section` while `shown` is open (see [`model::next_remaining`]).
pub(super) fn next_after_move(tab: &WorktreeTab, list: &[&Delta], shown: &Path, moving: &[Delta]) -> Option<PathBuf> {
    let rows = visible_rows(tab, list);
    let leaving: HashSet<&Path> = moving.iter().map(|d| d.path.as_path()).collect();
    model::next_remaining(&row_order(&rows, list), shown, &leaving)
}

/// The section's file list, exactly `height` tall (scrolls when the rows need more).
fn section_list(ui: &mut Ui, tab: &mut WorktreeTab, section: Section, list: &[&Delta], height: f32, actions: &mut Vec<Action>) {
    let p = theme::pal(ui);
    let card = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), height));
    ui.painter().rect_filled(card, 8, p.bg_graph);
    ui.painter()
        .rect_stroke(card, 8, egui::Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    if list.is_empty() {
        let text = match section {
            Section::Staged => "No staged files",
            Section::Unstaged | Section::Conflicted => "No unstaged changes",
        };
        // `put` centres the label in the rect and allocates the whole rect.
        ui.put(card, egui::Label::new(RichText::new(text).size(12.5).color(p.text_faint)));
        return;
    }
    let rows = visible_rows(tab, list);
    let order = row_order(&rows, list);
    egui::ScrollArea::vertical()
        .id_salt(("wip_section", section))
        .max_height(height)
        .min_scrolled_height(height)
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
            for row in rows.get(range).unwrap_or_default() {
                match row {
                    TreeRow::Dir { depth, name, path } => {
                        let open = !tab.collapsed.contains(path);
                        if dir_row(ui, *depth, name, open).clicked() {
                            actions.push(Action::ToggleDir(path.clone()));
                        }
                    }
                    TreeRow::File { depth, index } => {
                        if let Some(delta) = list.get(*index) {
                            file_row(ui, tab, section, list, &order, delta, *depth, actions);
                        }
                    }
                }
            }
        });
    // Directory toggles only touch this tab's UI state: apply them right away.
    actions.retain(|a| match a {
        Action::ToggleDir(path) => {
            if !tab.collapsed.remove(path) {
                tab.collapsed.insert(path.clone());
            }
            false
        }
        _ => true,
    });
}

fn dir_row(ui: &mut Ui, depth: usize, name: &str, open: bool) -> egui::Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect.shrink2(egui::vec2(4.0, 1.0)), 6, p.bg_hover);
    }
    let x = rect.left() + 12.0 + depth as f32 * 14.0;
    let caret = if open { icon::CARET_DOWN } else { icon::CARET_RIGHT };
    ui.painter().text(
        egui::pos2(x, rect.center().y),
        Align2::LEFT_CENTER,
        format!("{caret} {} {name}", icon::FOLDER),
        FontId::proportional(13.0),
        p.text_muted,
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("{name}/")));
    resp
}

#[allow(clippy::too_many_arguments)]
fn file_row(
    ui: &mut Ui,
    tab: &mut WorktreeTab,
    section: Section,
    list: &[&Delta],
    order: &[&Path],
    delta: &Delta,
    depth: usize,
    actions: &mut Vec<Action>,
) {
    let p = theme::pal(ui);
    let selected = tab.selection.contains(section, &delta.path);
    let sense = if platform_ui::CAN_DRAG_FILES_OUT {
        Sense::click_and_drag()
    } else {
        Sense::click()
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), sense);
    let row_hovered = ui.rect_contains_pointer(rect);
    let highlight = rect.shrink2(egui::vec2(4.0, 1.0));
    if selected {
        ui.painter().rect_filled(highlight, 6, p.bg_selected);
    } else if row_hovered {
        ui.painter().rect_filled(highlight, 6, p.bg_hover);
    }
    let (glyph, color) = ui::change_icon(p, delta.change);
    let x = rect.left() + 12.0 + depth as f32 * 14.0;
    ui.painter().text(
        egui::pos2(x, rect.center().y),
        Align2::LEFT_CENTER,
        glyph,
        FontId::proportional(13.0),
        color,
    );
    let (name, dir) = model::split_path(delta);
    let lossy = model::is_lossy(delta);
    let button_w = if row_hovered { 64.0 } else { 0.0 };
    let text_left = x + 20.0;
    let max_w = (rect.right() - text_left - button_w - 6.0).max(0.0);
    let name_color = if lossy { p.orange } else { p.text };
    let name_rect = ui::paint_truncated(
        ui.painter(),
        egui::pos2(text_left, rect.center().y),
        &name,
        FontId::proportional(13.0),
        name_color,
        max_w,
    );
    if !tab.tree && !dir.is_empty() {
        ui::paint_truncated(
            ui.painter(),
            egui::pos2(name_rect.right() + 6.0, rect.center().y),
            &dir,
            FontId::proportional(11.5),
            p.text_faint,
            (max_w - name_rect.width() - 6.0).max(0.0),
        );
    }
    let label = delta.display_label();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &label));

    // Hover mini-button: acts on this file only.
    if row_hovered {
        let btn_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - button_w - 4.0, rect.top() + 3.0),
            egui::vec2(button_w - 4.0, ROW_HEIGHT - 6.0),
        );
        let text = match section {
            Section::Conflicted => "Resolve…",
            Section::Unstaged => "Stage",
            Section::Staged => "Unstage",
        };
        let btn = ui.put(btn_rect, egui::Button::new(RichText::new(text).small()).corner_radius(3));
        if btn.clicked() {
            actions.push(match section {
                Section::Conflicted => Action::OpenMerge(delta.path.clone()),
                Section::Unstaged => Action::Stage(vec![delta.clone()]),
                Section::Staged => Action::Unstage(vec![delta.clone()]),
            });
        }
    }

    if resp.clicked() {
        let mods = ClickMods::from_egui(ui.input(|i| i.modifiers));
        tab.selection.click(section, order, &delta.path, mods);
        if mods.plain() {
            actions.push(match section {
                Section::Conflicted => Action::OpenMerge(delta.path.clone()),
                _ => Action::OpenDiff {
                    file: delta.clone(),
                    staged: section.staged(),
                },
            });
        }
    }
    if resp.secondary_clicked() {
        tab.selection.context_click(section, &delta.path);
    }
    // Dragging a selected row takes the whole selection along; any other row goes alone.
    if resp.drag_started_by(egui::PointerButton::Primary) {
        let paths = if selected {
            tab.selection.files_or_all(section, list).iter().map(|d| d.path.clone()).collect()
        } else {
            vec![delta.path.clone()]
        };
        actions.push(Action::DragOut(paths));
    }
    let resp = if lossy {
        resp.on_hover_text(format!("{label}\nThis path is not valid UTF-8; it is shown approximately."))
    } else {
        resp.on_hover_text(&label)
    };
    resp.context_menu(|ui| context_menu(ui, tab, section, list, delta, actions));
}

fn context_menu(ui: &mut Ui, tab: &WorktreeTab, section: Section, list: &[&Delta], clicked: &Delta, actions: &mut Vec<Action>) {
    let mut chosen = owned(&tab.selection.files_or_all(section, list));
    if !tab.selection.contains(section, &clicked.path) || chosen.is_empty() {
        chosen = vec![clicked.clone()];
    }
    let n = chosen.len();
    let what = if n == 1 { "file".to_string() } else { format!("{n} files") };
    match section {
        Section::Conflicted => {
            if ui.button("Resolve…").clicked() {
                actions.push(Action::OpenMerge(clicked.path.clone()));
            }
            if ui.button(format!("Mark {what} resolved")).clicked() {
                actions.push(Action::MarkResolved(chosen.iter().map(|d| d.path.clone()).collect()));
            }
        }
        Section::Unstaged => {
            if ui.button(format!("Stage {what}")).clicked() {
                actions.push(Action::Stage(chosen.clone()));
            }
            if ui.button("Discard changes…").clicked() {
                actions.push(Action::Discard(chosen.clone()));
            }
            if ui.button("Stash selected…").clicked() {
                actions.push(Action::Stash(chosen.clone()));
            }
        }
        Section::Staged => {
            if ui.button(format!("Unstage {what}")).clicked() {
                actions.push(Action::Unstage(chosen.clone()));
            }
            if ui.button("Stash selected…").clicked() {
                actions.push(Action::Stash(chosen.clone()));
            }
        }
    }
    ui.separator();
    let copy_label = if n == 1 { "Copy path" } else { "Copy paths" };
    if ui.button(copy_label).clicked() {
        let text = chosen
            .iter()
            .map(|d| d.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        ui.ctx().copy_text(text);
    }
    if ui.button(platform_ui::REVEAL_LABEL).clicked() {
        actions.push(Action::Reveal(clicked.path.clone()));
    }
}

pub(super) fn apply(app: &mut GitYasApp, action: Action, workdir: Option<&Path>) {
    match action {
        Action::Stage(files) => {
            let moved = super::shown_move(app, Section::Unstaged, &files);
            app.bridge.run(
                TASK_FILES,
                move |e| async move { e.stage_files(files).await },
                move |app, result| match result {
                    Ok(()) => super::after_move(app, moved),
                    Err(err) => app.report_error("Could not stage files", &err),
                },
            );
        }
        Action::Unstage(files) => {
            let moved = super::shown_move(app, Section::Staged, &files);
            app.bridge.run(
                TASK_FILES,
                move |e| async move { e.unstage_files(files).await },
                move |app, result| match result {
                    Ok(()) => super::after_move(app, moved),
                    Err(err) => app.report_error("Could not unstage files", &err),
                },
            );
        }
        Action::Discard(files) => confirm_discard(app, files),
        Action::Stash(files) => super::open_stash_dialog(app, Some(files)),
        Action::OpenDiff { file, staged } => ui::diff::toggle(
            app,
            DiffRequest {
                target: DiffTarget::WorkingTree { staged },
                file,
            },
        ),
        Action::OpenMerge(path) => ui::merge::open(app, path),
        Action::MarkResolved(paths) => {
            let n = paths.len();
            app.bridge.run(
                TASK_FILES,
                move |e| async move { e.mark_resolved(paths).await },
                move |app, result| match result {
                    Ok(()) => app.notify.success(format!("Marked {n} {} resolved", model::files_word(n))),
                    Err(err) => app.report_error("Could not mark the conflicts resolved", &err),
                },
            );
        }
        Action::Reveal(rel) => reveal(app, workdir, &rel),
        Action::DragOut(rels) => {
            if let Some(workdir) = workdir {
                app.file_drag.request(rels.iter().map(|rel| workdir.join(rel)));
            }
        }
        Action::ToggleDir(_) => {}
    }
}

/// "Discard changes to 3 files? This can't be undone." / "Delete 2 untracked files?".
pub(super) fn confirm_discard(app: &mut GitYasApp, files: Vec<Delta>) {
    if files.is_empty() {
        return;
    }
    let refs: Vec<&Delta> = files.iter().collect();
    let (title, body, button) = model::discard_confirm_text(&refs);
    app.confirm(Confirm::new(title, body, button, move |app| {
        app.bridge
            .run_reporting(TASK_FILES, "Could not discard changes", move |e| async move {
                e.discard_files(files).await
            });
    }));
}

fn reveal(app: &mut GitYasApp, workdir: Option<&Path>, rel: &Path) {
    let Some(workdir) = workdir else { return };
    let path = workdir.join(rel);
    // A deleted file no longer exists: show its directory instead.
    let target = if path.exists() {
        path
    } else {
        path.parent().map_or_else(|| workdir.to_path_buf(), Path::to_path_buf)
    };
    if let Err(err) = gityas_platform::reveal_in_file_manager(&target) {
        app.report_error("Could not open the file manager", &err);
    }
}

/// Files the stash dialog / tests use: everything listed in the panel (conflicts excluded).
pub(super) fn all_changes(files: &FileState) -> Vec<&Delta> {
    let mut all = model::section_files(files, Section::Unstaged);
    all.extend(model::section_files(files, Section::Staged));
    all
}
