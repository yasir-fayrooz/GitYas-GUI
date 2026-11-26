//! Diff & file viewer: Hunk / Inline / Split from one `FileDiff`.
//!
//! * [`diff_model`]: pure `FileDiff` + mode → rows (unit-tested).
//! * [`word_diff`]: in-house intra-line highlights, computed off the UI thread when a diff loads (with the
//!   [`DiffSyntax`] highlights of both sides, see [`crate::syntax`]).
//! * [`rows`]: reusable painter primitives (gutters, virtualised monospace rows, horizontal offset).
//! * this file: per-tab state ([`DiffTab`]), data flow (fetch on request change, re-fetch on `GitFilesChanged`
//!   for working-tree targets), header, guards, hunk / file actions, keyboard.

pub(crate) mod diff_model;
pub(crate) mod rows;
pub(crate) mod word_diff;

use std::collections::BTreeSet;
use std::sync::Arc;

use egui::{Align, Color32, Layout, RichText, Sense, Ui, UiBuilder, Vec2};
use egui_phosphor::regular as icon;
use gityas_core::diff::{DiffKind, DiffTarget, FileDiff, HunkRef, LineOrigin};
use gityas_core::models::{Delta, DiffViewMode};
use gityas_core::{AppEvent, GitYasError, RepoId};

use self::diff_model::{CellKind, Folding, Row, RowKind, Rows, SplitRow, TextRef};
use self::rows::{LineSpec, Metrics};
use self::word_diff::IntraLine;
use crate::app::{Center, DiffRequest, GitYasApp};
use crate::bridge::TaskKey;
use crate::dialogs::Confirm;
use crate::syntax::HighlightedText;
use crate::theme::{self, Palette};
use crate::ui::worktree::{self, model::Section};

pub const TASK_LOAD: TaskKey = "diff-load";
pub const TASK_HUNK: TaskKey = "diff-hunk";
pub const TASK_FILE: TaskKey = "diff-file";
pub const TASK_SETTINGS: TaskKey = "diff-settings";

/// Gaps of at least this many unchanged lines fold when "Collapse unchanged" is on (Inline / Split).
const FOLD_MIN_LINES: usize = 8;
const HEADER_HEIGHT: f32 = 34.0;
const SCROLLBAR_HEIGHT: f32 = 8.0;

/// Which text column a selection is in (Split mode has one per side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Unified,
    Left,
    Right,
}

/// Which lines of a selection a copy keeps. Context lines are always kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// The new version: context + added lines (the default, Cmd+C).
    WithChanges,
    /// Context + added + removed lines, in diff order.
    ChangesAndRemovals,
    /// The old version: context + removed lines.
    RemovalsOnly,
    /// Context lines only.
    Unchanged,
    /// Every line with its ` ` / `+` / `-` prefix.
    Patch,
}

impl CopyMode {
    const MENU: [(CopyMode, &'static str); 5] = [
        (CopyMode::WithChanges, "Copy with changes"),
        (CopyMode::ChangesAndRemovals, "Copy with changes + removals"),
        (CopyMode::RemovalsOnly, "Copy with removals only"),
        (CopyMode::Unchanged, "Copy without changes or removals"),
        (CopyMode::Patch, "Copy as patch"),
    ];

    fn keeps(self, kind: LineKind) -> bool {
        match (self, kind) {
            (_, LineKind::Context) => true,
            (CopyMode::WithChanges, k) => k == LineKind::Add,
            (CopyMode::RemovalsOnly, k) => k == LineKind::Del,
            (CopyMode::Unchanged, _) => false,
            (CopyMode::ChangesAndRemovals | CopyMode::Patch, _) => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Context,
    Add,
    Del,
}

/// A caret position: row of the built rows and char index into that row's text (clamped to the line's length
/// wherever it is used, so `usize::MAX` means "end of line").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    pub row: usize,
    pub col: usize,
}

impl TextPos {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// Selected text in one column (press and drag, Shift+click, double-click a word, triple-click a line, Cmd+A),
/// read-only like a text view; copied with Cmd+C or the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub column: Column,
    pub anchor: TextPos,
    pub cursor: TextPos,
}

impl TextSelection {
    fn caret(column: Column, pos: TextPos) -> Self {
        Self {
            column,
            anchor: pos,
            cursor: pos,
        }
    }

    fn ordered(&self) -> (TextPos, TextPos) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    /// The selected chars of row `row` (a line `len` chars long), and whether the selection runs on past its end.
    fn cols_in(&self, column: Column, row: usize, len: usize) -> Option<(std::ops::Range<usize>, bool)> {
        let (s, e) = self.ordered();
        if self.column != column || row < s.row || row > e.row {
            return None;
        }
        let start = if row == s.row { s.col.min(len) } else { 0 };
        let end = if row == e.row { e.col.min(len) } else { len };
        let past_end = row < e.row;
        (end > start || past_end).then_some((start..end.max(start), past_end))
    }

    fn contains(&self, column: Column, pos: TextPos) -> bool {
        let (s, e) = self.ordered();
        self.column == column && !self.is_empty() && s <= pos && pos <= e
    }
}

/// Char range of the word (or run of other non-space chars, or of spaces) around char `col` of `text`.
fn word_at(text: &str, col: usize) -> std::ops::Range<usize> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return 0..0;
    }
    let col = col.min(chars.len() - 1);
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let k = class(chars[col]);
    let start = chars[..col].iter().rposition(|c| class(*c) != k).map_or(0, |i| i + 1);
    let end = chars[col..].iter().position(|c| class(*c) != k).map_or(chars.len(), |i| col + i);
    start..end
}

/// Rows built for the current diff / mode / folding.
enum Built {
    Unified(Rows<Row>),
    Split(Rows<SplitRow>),
}

impl Built {
    fn len(&self) -> usize {
        match self {
            Built::Unified(r) => r.rows.len(),
            Built::Split(r) => r.rows.len(),
        }
    }

    fn hunk_starts(&self) -> &[usize] {
        match self {
            Built::Unified(r) => &r.hunk_starts,
            Built::Split(r) => &r.hunk_starts,
        }
    }

    fn max_chars(&self) -> usize {
        match self {
            Built::Unified(r) => r.max_chars,
            Built::Split(r) => r.max_chars,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct BuiltKey {
    diff: usize,
    mode: DiffViewMode,
    folding: Folding,
}

/// A loaded diff with its intra-line and syntax highlights (computed with it, off the UI thread).
pub struct Loaded {
    pub diff: Arc<FileDiff>,
    pub intraline: Arc<IntraLine>,
    pub syntax: Arc<DiffSyntax>,
}

/// Syntax highlights of both sides of a diff, each parsed whole so multi-line strings and comments are right.
#[derive(Debug, Default)]
pub struct DiffSyntax {
    pub old: HighlightedText,
    pub new: HighlightedText,
}

impl DiffSyntax {
    pub fn compute(file: &Delta, diff: &FileDiff) -> Self {
        if !diff.kind.is_text() {
            return Self::default();
        }
        Self {
            old: HighlightedText::new(file.old_path.as_deref().unwrap_or(&file.path), &diff.old_text),
            new: HighlightedText::new(&file.path, &diff.new_text),
        }
    }
}

/// Per-tab state: the loaded diff for the current request and view state. Survives tab switches (the diff of a
/// working-tree target is re-fetched by the `GitFilesChanged` the engine re-emits).
#[derive(Default)]
pub struct DiffTab {
    /// The request `loaded` / `loading` belong to; a different request in the centre resets the state.
    request: Option<DiffRequest>,
    pub loaded: Option<Loaded>,
    pub loading: bool,
    /// Load error (shown in the body with Retry).
    pub error: Option<GitYasError>,
    /// The file is no longer in the working-tree list (or the engine has no change for it): "No file changes".
    pub gone: bool,
    /// Bumped per fetch; a completion for an older generation is dropped.
    generation: u64,
    /// "Show anyway" cap for `TooLarge`.
    pub limit: Option<u64>,
    /// Mode chosen in this tab until the settings write completes (or if it failed).
    mode_override: Option<DiffViewMode>,
    /// NonUtf8: the user asked to see the (lossy, read-only) hunks.
    pub show_non_utf8: bool,
    /// "Collapse unchanged" (Inline / Split) and the gaps expanded by clicking their fold row.
    pub fold: bool,
    expanded: BTreeSet<usize>,
    built: Option<(BuiltKey, Built)>,
    /// First visible row last frame (J / K start from here).
    top_row: usize,
    scroll_to: Option<usize>,
    /// Hunk last jumped to with J / K (painted with an accent bar).
    pub focused_hunk: Option<usize>,
    h_offset: f32,
    pub selection: Option<TextSelection>,
    /// A drag-select is in progress (primary button held after pressing on a line).
    dragging: bool,
}

impl DiffTab {
    fn reset_for(&mut self, request: DiffRequest) {
        *self = DiffTab {
            request: Some(request),
            generation: self.generation,
            mode_override: self.mode_override,
            fold: self.fold,
            ..DiffTab::default()
        };
    }

    pub fn diff(&self) -> Option<&Arc<FileDiff>> {
        self.loaded.as_ref().map(|l| &l.diff)
    }

    fn set_loaded(&mut self, loaded: Loaded) {
        self.loaded = Some(loaded);
        self.error = None;
        self.gone = false;
        self.built = None;
        self.expanded.clear();
        self.selection = None;
        if self.focused_hunk.is_some_and(|h| self.diff().is_none_or(|d| h >= d.hunks.len())) {
            self.focused_hunk = None;
        }
    }

    /// The mode this tab shows (`NonUtf8` diffs have no full sides: always Hunk).
    fn effective_mode(&self, settings: DiffViewMode) -> DiffViewMode {
        if self.diff().is_some_and(|d| d.kind == DiffKind::NonUtf8) {
            return DiffViewMode::Hunk;
        }
        self.mode_override.unwrap_or(settings)
    }

    fn folding(&self) -> Folding {
        Folding {
            min_lines: self.fold.then_some(FOLD_MIN_LINES),
            expanded: self.expanded.clone(),
        }
    }

    /// Rows for the current diff / mode / folding (rebuilt when any of them changed).
    fn ensure_built(&mut self, mode: DiffViewMode) -> Option<&Built> {
        let diff = self.diff()?.clone();
        let key = BuiltKey {
            diff: Arc::as_ptr(&diff) as usize,
            mode,
            folding: self.folding(),
        };
        if self.built.as_ref().is_none_or(|(k, _)| *k != key) {
            let built = match mode {
                DiffViewMode::Hunk => Built::Unified(diff_model::hunk_rows(&diff)),
                DiffViewMode::Inline => Built::Unified(diff_model::inline_rows(&diff, &key.folding)),
                DiffViewMode::Split => Built::Split(diff_model::split_rows(&diff, &key.folding)),
            };
            self.selection = None;
            self.built = Some((key, built));
        }
        self.built.as_ref().map(|(_, b)| b)
    }
}

// ---- Cross-module API --------------------------------------------------------------------------------------

/// Shows `request` in the centre (the history and working-tree file lists call this).
pub fn open(app: &mut GitYasApp, request: DiffRequest) {
    if let Some(tab) = app.tab_mut() {
        tab.center = Center::Diff(request);
    }
}

/// A file list click: shows `request`, or closes the diff when it already shows that file.
pub fn toggle(app: &mut GitYasApp, request: DiffRequest) {
    let showing = app.tab().is_some_and(
        |tab| matches!(&tab.center, Center::Diff(current) if current.target == request.target && current.file.path == request.file.path),
    );
    if showing {
        close(app);
    } else {
        open(app, request);
    }
}

/// Back to the graph (Esc, ✕); the file rows it was opened from are deselected.
pub fn close(app: &mut GitYasApp) {
    if let Some(tab) = app.tab_mut() {
        tab.center = Center::Graph;
        tab.diff.selection = None;
        tab.worktree.selection.clear();
    }
}

/// Working-tree diffs follow the file lists: re-fetch when they change, show "No file changes" when the file
/// left its list but is still in the other one (staged / unstaged), and close when it is in neither (committed,
/// discarded, reverted).
pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    let AppEvent::GitFilesChanged { repo, files } = event else { return };
    if app.store.active.as_ref() != Some(repo) {
        return;
    }
    let Some(tab) = app.tab_mut() else { return };
    let Center::Diff(request) = &tab.center else { return };
    let DiffTarget::WorkingTree { staged } = request.target else {
        return;
    };
    let (list, other) = if staged {
        (&files.staged, &files.unstaged)
    } else {
        (&files.unstaged, &files.staged)
    };
    match list.iter().find(|d| d.path == request.file.path) {
        None if !other.iter().any(|d| d.path == request.file.path) => close(app),
        None => {
            let d = &mut tab.diff;
            d.generation += 1;
            d.loading = false;
            d.loaded = None;
            d.built = None;
            d.error = None;
            d.gone = true;
            d.selection = None;
        }
        Some(delta) if *delta != request.file => {
            // Same path, different change (e.g. modified → deleted): a new request, fetched by `view`.
            tab.center = Center::Diff(DiffRequest {
                target: request.target.clone(),
                file: delta.clone(),
            });
        }
        Some(_) => {
            let request = request.clone();
            fetch(app, request);
        }
    }
}

/// Starts loading `request` into the active tab (the current diff stays visible until the result arrives).
fn fetch(app: &mut GitYasApp, request: DiffRequest) {
    let Some(repo) = app.store.active.clone() else { return };
    let tab = app.tabs.entry(repo.clone()).or_default();
    let d = &mut tab.diff;
    d.generation += 1;
    d.loading = true;
    let generation = d.generation;
    let limit = d.limit;
    let DiffRequest { target, file } = request;
    let delta = file.clone();
    app.bridge.run(
        TASK_LOAD,
        move |engine| async move {
            let diff = match limit {
                Some(max) => engine.file_diff_with_limit(target, file, max).await?,
                None => engine.file_diff(target, file).await?,
            };
            let intraline = word_diff::compute(&diff);
            let syntax = DiffSyntax::compute(&delta, &diff);
            Ok(Loaded {
                diff: Arc::new(diff),
                intraline: Arc::new(intraline),
                syntax: Arc::new(syntax),
            })
        },
        move |app, result| loaded(app, &repo, generation, result),
    );
}

fn loaded(app: &mut GitYasApp, repo: &RepoId, generation: u64, result: gityas_core::Result<Loaded>) {
    let active = app.store.active.as_ref() == Some(repo);
    let Some(tab) = app.tabs.get_mut(repo) else { return };
    let d = &mut tab.diff;
    if d.generation != generation {
        return;
    }
    d.loading = false;
    if !active {
        // The engine ran it against whatever repository was active by then; fetch again when shown.
        d.request = None;
        return;
    }
    match result {
        Ok(loaded) => d.set_loaded(loaded),
        Err(GitYasError::NotFound(_)) => {
            d.loaded = None;
            d.built = None;
            d.gone = true;
        }
        Err(err) => {
            d.loaded = None;
            d.built = None;
            d.error = Some(err);
        }
    }
}

// ---- Actions -----------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Act {
    Close,
    Mode(DiffViewMode),
    StageFile,
    UnstageFile,
    StageHunk(HunkRef),
    UnstageHunk(HunkRef),
    DiscardHunk(HunkRef),
    ShowAnyway(u64),
    ShowNonUtf8,
    Retry,
    OpenMerge,
    ToggleFold,
    Expand(usize),
    /// Primary press on a line: place the caret (or extend with Shift) and start drag-selecting.
    Press {
        column: Column,
        pos: TextPos,
        extend: bool,
    },
    DragTo(TextPos),
    SelectWord {
        column: Column,
        pos: TextPos,
    },
    SelectLine {
        column: Column,
        row: usize,
    },
    SelectAll,
    /// Right-click: outside the selection, the clicked line becomes the selection.
    SelectIfOutside {
        column: Column,
        pos: TextPos,
    },
    Copy(CopyMode),
    JumpHunk {
        forward: bool,
    },
}

fn persist_mode(app: &mut GitYasApp, mode: DiffViewMode) {
    if let Some(tab) = app.tab_mut() {
        tab.diff.mode_override = Some(mode);
    }
    if app.store.settings.diff_view_mode == mode {
        return;
    }
    app.modify_settings(
        TASK_SETTINGS,
        move |settings| settings.diff_view_mode = mode,
        move |app, result| match result {
            // There is no settings event: `modify_settings` already copied the saved mode into the store.
            Ok(_) => {
                // The setting now says it; drop the per-tab overrides so later changes (settings window) apply.
                for tab in app.tabs.values_mut() {
                    if tab.diff.mode_override == Some(mode) {
                        tab.diff.mode_override = None;
                    }
                }
            }
            Err(err) => app.report_error("Could not save the diff view mode", &err),
        },
    );
}

fn hunk_command(app: &mut GitYasApp, request: &DiffRequest, hunk: HunkRef, op: HunkOp) {
    let file = request.file.clone();
    let request = request.clone();
    app.bridge.run(
        TASK_HUNK,
        move |engine| async move {
            match op {
                HunkOp::Stage => engine.stage_hunk(file, hunk).await,
                HunkOp::Unstage => engine.unstage_hunk(file, hunk).await,
                HunkOp::Discard => engine.discard_hunk(file, hunk).await,
            }
        },
        move |app, result| {
            // Success: the engine's GitFilesChanged re-fetches (on_event).
            if let Err(err) = result {
                app.report_error(op.failure(), &err);
                if err == GitYasError::StaleHunk {
                    let still_shown = app.tab().is_some_and(|t| t.center == Center::Diff(request.clone()));
                    if still_shown {
                        fetch(app, request);
                    }
                }
            }
        },
    );
}

#[derive(Debug, Clone, Copy)]
enum HunkOp {
    Stage,
    Unstage,
    Discard,
}

impl HunkOp {
    fn failure(self) -> &'static str {
        match self {
            HunkOp::Stage => "Could not stage hunk",
            HunkOp::Unstage => "Could not unstage hunk",
            HunkOp::Discard => "Could not discard hunk",
        }
    }
}

/// Stage / unstage the whole file; afterwards the viewer moves on to the next file of the same list, or closes
/// when it was the last one.
fn file_command(app: &mut GitYasApp, request: &DiffRequest, stage: bool) {
    let file = request.file.clone();
    let from = if stage { Section::Unstaged } else { Section::Staged };
    let moved = worktree::shown_move(app, from, std::slice::from_ref(&file));
    app.bridge.run(
        TASK_FILE,
        move |engine| async move {
            if stage {
                engine.stage_files(vec![file]).await
            } else {
                engine.unstage_files(vec![file]).await
            }
        },
        move |app, result| match result {
            Ok(()) => worktree::after_move(app, moved),
            Err(err) => app.report_error(if stage { "Could not stage file" } else { "Could not unstage file" }, &err),
        },
    );
}

fn apply(app: &mut GitYasApp, ctx: &egui::Context, request: &DiffRequest, act: Act) {
    match act {
        Act::Close => close(app),
        Act::Mode(mode) => persist_mode(app, mode),
        Act::StageFile => file_command(app, request, true),
        Act::UnstageFile => file_command(app, request, false),
        Act::StageHunk(h) => hunk_command(app, request, h, HunkOp::Stage),
        Act::UnstageHunk(h) => hunk_command(app, request, h, HunkOp::Unstage),
        Act::DiscardHunk(h) => {
            let request = request.clone();
            let body = format!(
                "Discard this hunk's changes to {}? This can't be undone.",
                request.file.path.display()
            );
            app.confirm(Confirm::new("Discard hunk?", body, "Discard hunk", move |app| {
                hunk_command(app, &request, h, HunkOp::Discard)
            }));
        }
        Act::ShowAnyway(bytes) => {
            if let Some(tab) = app.tab_mut() {
                tab.diff.limit = Some(bytes.saturating_mul(2).max(bytes.saturating_add(1)));
            }
            fetch(app, request.clone());
        }
        Act::ShowNonUtf8 => {
            if let Some(tab) = app.tab_mut() {
                tab.diff.show_non_utf8 = true;
            }
        }
        Act::Retry => fetch(app, request.clone()),
        Act::OpenMerge => crate::ui::merge::open(app, request.file.path.clone()),
        Act::ToggleFold => {
            if let Some(tab) = app.tab_mut() {
                tab.diff.fold = !tab.diff.fold;
                tab.diff.expanded.clear();
            }
        }
        Act::Expand(gap) => {
            if let Some(tab) = app.tab_mut() {
                tab.diff.expanded.insert(gap);
            }
        }
        Act::Press { column, pos, extend } => {
            if let Some(tab) = app.tab_mut() {
                let d = &mut tab.diff;
                d.selection = Some(match d.selection {
                    Some(sel) if extend && sel.column == column => TextSelection { cursor: pos, ..sel },
                    _ => TextSelection::caret(column, pos),
                });
                d.dragging = true;
            }
        }
        Act::DragTo(pos) => {
            if let Some(tab) = app.tab_mut()
                && tab.diff.dragging
                && let Some(sel) = tab.diff.selection.as_mut()
            {
                sel.cursor = pos;
            }
        }
        Act::SelectWord { column, pos } => {
            if let Some(tab) = app.tab_mut() {
                let d = &mut tab.diff;
                let word = cell_text(d, column, pos.row).map(|t| word_at(t, pos.col));
                if let Some(word) = word {
                    d.selection = Some(TextSelection {
                        column,
                        anchor: TextPos::new(pos.row, word.start),
                        cursor: TextPos::new(pos.row, word.end),
                    });
                }
            }
        }
        Act::SelectLine { column, row } => {
            if let Some(tab) = app.tab_mut() {
                tab.diff.selection = Some(line_selection(column, row));
            }
        }
        Act::SelectAll => {
            if let Some(tab) = app.tab_mut() {
                let d = &mut tab.diff;
                let column = d.selection.map(|s| s.column);
                if let Some((_, built)) = d.built.as_ref()
                    && built.len() > 0
                {
                    let column = column.unwrap_or(match built {
                        Built::Unified(_) => Column::Unified,
                        Built::Split(_) => Column::Right,
                    });
                    d.selection = Some(TextSelection {
                        column,
                        anchor: TextPos::new(0, 0),
                        cursor: TextPos::new(built.len() - 1, usize::MAX),
                    });
                }
            }
        }
        Act::SelectIfOutside { column, pos } => {
            if let Some(tab) = app.tab_mut() {
                let d = &mut tab.diff;
                if !d.selection.is_some_and(|s| s.contains(column, pos)) {
                    d.selection = Some(line_selection(column, pos.row));
                }
            }
        }
        Act::Copy(mode) => {
            let text = app.tab().and_then(|t| {
                let d = &t.diff;
                // Cmd+C on removed lines only ("with changes" keeps none of them) copies what is highlighted.
                selection_text(d, mode).or_else(|| (mode == CopyMode::WithChanges).then(|| exact_text(d)).flatten())
            });
            if let Some(text) = text {
                ctx.copy_text(text);
            }
        }
        Act::JumpHunk { forward } => {
            if let Some(tab) = app.tab_mut() {
                let d = &mut tab.diff;
                let starts = d.built.as_ref().map(|(_, b)| b.hunk_starts().to_vec()).unwrap_or_default();
                if let Some(h) = diff_model::next_hunk(&starts, d.top_row, d.focused_hunk, forward) {
                    d.focused_hunk = Some(h);
                    d.scroll_to = starts.get(h).copied();
                }
            }
        }
    }
}

fn line_selection(column: Column, row: usize) -> TextSelection {
    TextSelection {
        column,
        anchor: TextPos::new(row, 0),
        cursor: TextPos::new(row, usize::MAX),
    }
}

fn unified_kind(kind: RowKind) -> Option<LineKind> {
    match kind {
        RowKind::Context => Some(LineKind::Context),
        RowKind::Add => Some(LineKind::Add),
        RowKind::Del => Some(LineKind::Del),
        _ => None,
    }
}

fn cell_kind(kind: CellKind) -> Option<LineKind> {
    match kind {
        CellKind::Context => Some(LineKind::Context),
        CellKind::Add => Some(LineKind::Add),
        CellKind::Del => Some(LineKind::Del),
        _ => None,
    }
}

/// The text of a selectable line (context / added / removed) in `column` at `row`.
fn cell_text(d: &DiffTab, column: Column, row: usize) -> Option<&str> {
    let diff = d.diff()?;
    let (_, built) = d.built.as_ref()?;
    let text = match built {
        Built::Unified(rows) => rows.rows.get(row).filter(|r| unified_kind(r.kind).is_some()).map(|r| &r.text),
        Built::Split(rows) => rows.rows.get(row).and_then(|r| {
            let cell = if column == Column::Left { &r.left } else { &r.right };
            cell_kind(cell.kind).map(|_| &cell.text)
        }),
    }?;
    Some(diff_model::text(diff, text))
}

/// The selected chars of a line `text` in `column` at `row`; a line outside the selection's column (the other
/// side of a Split row) is taken whole.
fn clip(sel: &TextSelection, column: Column, row: usize, text: &str) -> String {
    if column != sel.column {
        return text.to_owned();
    }
    let len = text.chars().count();
    match sel.cols_in(column, row, len) {
        Some((range, _)) => text.chars().skip(range.start).take(range.len()).collect(),
        None => String::new(),
    }
}

/// The selected lines in diff order, the first and last cut to the selection; headers, folds, fillers and
/// `\ No newline` markers are skipped. In Split mode `exact` keeps only the selected side; otherwise both sides
/// of the selected rows are used (each run of removals before its additions, as in a unified diff).
fn selected_lines(d: &DiffTab, exact: bool) -> Option<Vec<(LineKind, String)>> {
    let sel = d.selection.filter(|s| !s.is_empty())?;
    let diff = d.diff()?;
    let (_, built) = d.built.as_ref()?;
    let (s, e) = sel.ordered();
    let mut lines = Vec::new();
    match built {
        Built::Unified(rows) => {
            for (i, row) in rows.rows.iter().enumerate().take(e.row + 1).skip(s.row) {
                if let Some(kind) = unified_kind(row.kind) {
                    lines.push((kind, clip(&sel, Column::Unified, i, diff_model::text(diff, &row.text))));
                }
            }
        }
        Built::Split(rows) => {
            let (mut dels, mut adds) = (Vec::new(), Vec::new());
            for (i, row) in rows.rows.iter().enumerate().take(e.row + 1).skip(s.row) {
                for (column, cell) in [(Column::Left, &row.left), (Column::Right, &row.right)] {
                    if exact && column != sel.column {
                        continue;
                    }
                    let Some(kind) = cell_kind(cell.kind) else { continue };
                    let text = clip(&sel, column, i, diff_model::text(diff, &cell.text));
                    match kind {
                        LineKind::Del => dels.push((kind, text)),
                        LineKind::Add => adds.push((kind, text)),
                        // Both sides hold the same context line: take it once, from the selected side.
                        LineKind::Context if column == sel.column => {
                            lines.append(&mut dels);
                            lines.append(&mut adds);
                            lines.push((kind, text));
                        }
                        LineKind::Context => {}
                    }
                }
            }
            lines.append(&mut dels);
            lines.append(&mut adds);
        }
    }
    Some(lines)
}

/// Exactly the highlighted text.
fn exact_text(d: &DiffTab) -> Option<String> {
    let lines = selected_lines(d, true)?;
    let text = lines.into_iter().map(|(_, t)| t).collect::<Vec<_>>().join("\n");
    (!text.is_empty()).then_some(text)
}

/// The selected text for `mode`, `\n`-joined (no numbers; signs only for [`CopyMode::Patch`]).
fn selection_text(d: &DiffTab, mode: CopyMode) -> Option<String> {
    let lines: Vec<String> = selected_lines(d, false)?
        .into_iter()
        .filter(|(kind, _)| mode.keeps(*kind))
        .map(|(kind, text)| match mode {
            CopyMode::Patch => {
                let sign = match kind {
                    LineKind::Context => ' ',
                    LineKind::Add => '+',
                    LineKind::Del => '-',
                };
                format!("{sign}{text}")
            }
            _ => text,
        })
        .collect();
    let text = lines.join("\n");
    (!text.is_empty()).then_some(text)
}

// ---- View --------------------------------------------------------------------------------------------------

/// Centre area while a diff is open.
pub fn view(ui: &mut Ui, app: &mut GitYasApp, request: &DiffRequest) {
    let Some(repo) = app.store.active.clone() else { return };
    let needs_fetch = {
        let tab = app.tabs.entry(repo.clone()).or_default();
        if tab.diff.request.as_ref() != Some(request) {
            tab.diff.reset_for(request.clone());
            true
        } else {
            false
        }
    };
    if needs_fetch {
        fetch(app, request.clone());
    }

    let p = theme::pal(ui);
    let settings_mode = app.store.settings.diff_view_mode;
    let busy_hunk = app.bridge.is_running(TASK_HUNK);
    let busy_file = app.bridge.is_running(TASK_FILE);
    let keys_free = app.confirm.is_none() && ui.ctx().memory(|m| m.top_modal_layer().is_none()) && !ui.ctx().egui_wants_keyboard_input();
    let mut acts: Vec<Act> = Vec::new();

    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.bg_graph))
        .show(ui, |ui| {
            let Some(tab) = app.tabs.get_mut(&repo) else { return };
            let d = &mut tab.diff;
            let mode = d.effective_mode(settings_mode);
            header(ui, p, request, d, mode, busy_file, &mut acts);
            body(ui, p, request, d, mode, busy_hunk, &mut acts);
        });

    if keys_free {
        let ctx = ui.ctx().clone();
        let shift_f7 = egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::F7);
        ctx.input_mut(|i| {
            if i.consume_shortcut(&shift_f7) {
                acts.push(Act::JumpHunk { forward: false });
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::J) || i.consume_key(egui::Modifiers::NONE, egui::Key::F7) {
                acts.push(Act::JumpHunk { forward: true });
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::K) {
                acts.push(Act::JumpHunk { forward: false });
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                acts.push(Act::Close);
            }
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::A) {
                acts.push(Act::SelectAll);
            }
            if i.events.iter().any(|e| matches!(e, egui::Event::Copy)) {
                acts.push(Act::Copy(CopyMode::WithChanges));
            }
        });
    }

    let ctx = ui.ctx().clone();
    for act in acts {
        apply(app, &ctx, request, act);
    }
}

fn header(ui: &mut Ui, p: &Palette, request: &DiffRequest, d: &DiffTab, mode: DiffViewMode, busy_file: bool, acts: &mut Vec<Act>) {
    let file = &request.file;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), HEADER_HEIGHT), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.bg_toolbar);
    ui.painter()
        .hline(rect.x_range(), rect.bottom() - 0.5, egui::Stroke::new(1.0, p.border));
    let inner = rect.shrink2(Vec2::new(10.0, 4.0));
    ui.scope_builder(
        UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)),
        |ui| {
            let (glyph, color) = crate::ui::change_icon(p, file.change);
            ui.label(RichText::new(glyph).color(color).size(15.0));

            // Controls first (right to left), so the path gets only the width that is left.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if crate::ui::icon_button(ui, icon::X, "Close (Esc)").clicked() {
                    acts.push(Act::Close);
                }
                if let DiffTarget::WorkingTree { staged } = request.target
                    && !file.change.is_conflict()
                {
                    let (label, act) = if staged {
                        ("Unstage file", Act::UnstageFile)
                    } else {
                        ("Stage file", Act::StageFile)
                    };
                    let fill = if staged { p.red } else { p.green };
                    let resp = crate::ui::solid_button(ui, label, fill, !busy_file);
                    if resp.clicked() {
                        acts.push(act);
                    }
                }
                change_badge(ui, p, file);
                if let Some(diff) = d.diff() {
                    let (add, del) = diff.line_stats();
                    if add + del > 0 {
                        ui.label(RichText::new(format!("−{del}")).color(p.red).small());
                        ui.label(RichText::new(format!("+{add}")).color(p.green).small());
                    }
                }
                ui.add_space(8.0);
                mode_toggle(ui, d, mode, acts);
                if matches!(mode, DiffViewMode::Inline | DiffViewMode::Split) && d.diff().is_some_and(|f| f.kind.is_text()) {
                    let mut fold = d.fold;
                    if ui
                        .checkbox(&mut fold, "Collapse unchanged")
                        .on_hover_text("Fold long runs of unchanged lines")
                        .changed()
                    {
                        acts.push(Act::ToggleFold);
                    }
                }
                if d.loading && d.loaded.is_some() {
                    ui.add(egui::Spinner::new().size(12.0));
                }
                ui.add_space(8.0);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| path_label(ui, p, file));
            });
        },
    );
}

/// The file's path in the available width: directories in muted text, the file name in bold. When it does not fit,
/// the directories are cut from the left ("…rc/ui/diff/mod.rs") so the file name always shows in full.
fn path_label(ui: &mut Ui, p: &Palette, file: &Delta) {
    let full = file.display_label();
    let body = egui::TextStyle::Body.resolve(ui.style());
    let mut prefix = String::new();
    if let Some(old) = file.old_path.as_ref().filter(|o| **o != file.path) {
        prefix.push_str(&format!("{} → ", old.display()));
    }
    if let Some(dir) = file.directory() {
        prefix.push_str(&format!("{}/", dir.display()));
    }
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.path.display().to_string());
    let job = |prefix: &str| {
        let mut job = egui::text::LayoutJob::default();
        job.append(prefix, 0.0, egui::TextFormat::simple(body.clone(), p.text_muted));
        job.append(&name, 0.0, egui::TextFormat::simple(body.clone(), p.text));
        job
    };
    let max = ui.available_width();
    let fits = |prefix: &str| ui.painter().layout_job(job(prefix)).size().x <= max;
    let shown = if fits(&prefix) {
        prefix
    } else {
        // The fewest leading characters to drop (binary search); all of them when even "…name" is too wide.
        let chars: Vec<char> = prefix.chars().collect();
        let cut = |drop: usize| format!("…{}", chars[drop..].iter().collect::<String>());
        let (mut lo, mut hi) = (0, chars.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if fits(&cut(mid)) { hi = mid } else { lo = mid + 1 }
        }
        cut(lo)
    };
    let mut job = job(&shown);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max.max(0.0));
    ui.add(egui::Label::new(job).selectable(false)).on_hover_text(full);
}

fn change_badge(ui: &mut Ui, p: &Palette, file: &Delta) {
    let (_, color) = crate::ui::change_icon(p, file.change);
    egui::Frame::new()
        .fill(color.gamma_multiply(0.25))
        .corner_radius(3)
        .inner_margin(egui::Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(file.change.label().to_uppercase()).small().strong().color(color));
        });
}

fn mode_toggle(ui: &mut Ui, d: &DiffTab, mode: DiffViewMode, acts: &mut Vec<Act>) {
    let non_utf8 = d.diff().is_some_and(|f| f.kind == DiffKind::NonUtf8);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        // Right-to-left layout: add in reverse so they read Hunk | Inline | Split.
        for (m, label, tip) in [
            (DiffViewMode::Split, "Split", "Side-by-side view"),
            (DiffViewMode::Inline, "Inline", "Whole file, unified"),
            (DiffViewMode::Hunk, "Hunk", "Changed hunks only"),
        ] {
            let enabled = !non_utf8 || m == DiffViewMode::Hunk;
            let resp = ui.add_enabled(enabled, egui::Button::selectable(mode == m, label));
            let resp = if enabled {
                resp.on_hover_text(tip)
            } else {
                resp.on_disabled_hover_text("Not valid UTF-8: only the hunks can be shown")
            };
            if resp.clicked() && mode != m {
                acts.push(Act::Mode(m));
            }
        }
    });
}

/// Centred explanation panel (guards, empty state, errors).
fn panel(ui: &mut Ui, p: &Palette, glyph: &str, title: &str, detail: Option<&str>, add: impl FnOnce(&mut Ui)) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.3).max(16.0));
        ui.label(RichText::new(glyph).size(36.0).color(p.text_faint));
        ui.add_space(6.0);
        ui.label(RichText::new(title).color(p.text_muted));
        if let Some(detail) = detail {
            ui.add(egui::Label::new(RichText::new(detail).small().color(p.text_faint)).wrap());
        }
        ui.add_space(6.0);
        add(ui);
    });
}

fn human_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

fn body(ui: &mut Ui, p: &Palette, request: &DiffRequest, d: &mut DiffTab, mode: DiffViewMode, busy_hunk: bool, acts: &mut Vec<Act>) {
    if d.gone {
        panel(ui, p, icon::CHECK_CIRCLE, "No file changes", None, |_| {});
        return;
    }
    if let Some(err) = &d.error {
        let conflicted = matches!(err, GitYasError::InvalidInput(m) if m.contains("conflict"));
        let message = err.to_string();
        panel(ui, p, icon::WARNING, "Could not load the diff", Some(&message), |ui| {
            ui.horizontal(|ui| {
                if ui.button("Retry").clicked() {
                    acts.push(Act::Retry);
                }
                if conflicted && ui.button("Open merge editor").clicked() {
                    acts.push(Act::OpenMerge);
                }
            });
        });
        return;
    }
    let Some(diff) = d.diff().cloned() else {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.add(egui::Spinner::new());
            ui.label(RichText::new("Loading diff…").color(p.text_muted));
        });
        return;
    };
    match diff.kind {
        DiffKind::Binary => {
            panel(
                ui,
                p,
                icon::FILE,
                "Binary file not shown",
                Some("Git treats this file as binary, so there are no lines to compare."),
                |_| {},
            );
            return;
        }
        DiffKind::TooLarge { bytes } => {
            let title = format!("File too large to display ({})", human_bytes(bytes));
            panel(
                ui,
                p,
                icon::FILE_TEXT,
                &title,
                Some("Large files are not loaded by default to keep the app responsive."),
                |ui| {
                    if ui.button("Show anyway").clicked() {
                        acts.push(Act::ShowAnyway(bytes));
                    }
                },
            );
            return;
        }
        DiffKind::NonUtf8 if !d.show_non_utf8 => {
            panel(
                ui,
                p,
                icon::TEXT_AA,
                "File is not valid UTF-8",
                Some("Its lines can only be shown approximately (invalid bytes replaced), so they are read-only."),
                |ui| {
                    if ui.button("Show hunks (read-only)").clicked() {
                        acts.push(Act::ShowNonUtf8);
                    }
                },
            );
            return;
        }
        DiffKind::NonUtf8 => {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!(
                        "{} Not valid UTF-8: invalid bytes are shown as boxes; hunk actions are disabled.",
                        icon::WARNING
                    ))
                    .small()
                    .color(p.orange),
                );
            });
        }
        DiffKind::Text => {}
    }
    if let Some(label) = diff_model::mode_change_label(&diff) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(RichText::new(label).color(p.text_muted));
        });
        ui.add_space(4.0);
    }
    if !diff.has_hunks() {
        if diff_model::mode_change_label(&diff).is_none() {
            panel(ui, p, icon::CHECK_CIRCLE, "No file changes", None, |_| {});
        }
        return;
    }

    let mutable = !request.target.is_read_only() && diff.kind.is_text() && !request.file.change.is_conflict();
    let staged = matches!(request.target, DiffTarget::WorkingTree { staged: true });
    let intraline = d.loaded.as_ref().map(|l| l.intraline.clone()).unwrap_or_default();
    let syntax = d.loaded.as_ref().map(|l| l.syntax.clone()).unwrap_or_default();
    let Some(built) = d.ensure_built(mode) else { return };
    let total = built.len();
    let max_chars = built.max_chars();
    let max_lineno = diff
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .flat_map(|l| [l.old_lineno, l.new_lineno])
        .flatten()
        .max()
        .unwrap_or(1)
        .max(diff_model::line_ranges(&diff.old_text).len() as u32);
    let m = Metrics::new(ui, max_lineno);

    // Layout: rows, then the horizontal scroll bar.
    let area = ui.available_rect_before_wrap();
    let rows_rect = egui::Rect::from_min_max(area.min, egui::pos2(area.max.x, area.max.y - SCROLLBAR_HEIGHT));
    let bar_rect = egui::Rect::from_min_max(egui::pos2(area.min.x, rows_rect.max.y), area.max);
    let split = mode == DiffViewMode::Split;
    let text_view = if split {
        rows_rect.width() / 2.0 - m.gutter_width(1)
    } else {
        rows_rect.width() - m.gutter_width(2)
    };
    let content = m.text_width(max_chars);
    let max_offset = (content - text_view + 20.0).max(0.0);
    rows::scroll_horizontally(ui, rows_rect, &mut d.h_offset, max_offset);
    let h_offset = d.h_offset;
    let mut scroll_to = d.scroll_to.take();
    if d.dragging && !ui.input(|i| i.pointer.primary_down()) {
        d.dragging = false;
    }
    // While drag-selecting: the pointer moves the caret end of the selection; above / below the rows it
    // auto-scrolls.
    let drag_pos = d.dragging.then(|| ui.input(|i| i.pointer.latest_pos())).flatten();
    let drag_y = drag_pos.map(|pos| pos.y);
    let drag_edge = drag_y.and_then(|y| {
        if y < rows_rect.top() {
            Some(false)
        } else if y > rows_rect.bottom() {
            Some(true)
        } else {
            None
        }
    });
    if let Some(down) = drag_edge {
        scroll_to = Some(if down { d.top_row + 1 } else { d.top_row.saturating_sub(1) });
        ui.ctx().request_repaint();
    }
    let selection = d.selection;
    let focused = d.focused_hunk;
    let Some((_, built)) = d.built.as_ref() else { return };
    let focus_row = focused.and_then(|h| built.hunk_starts().get(h).copied());

    let painter_ctx = RowCtx {
        p,
        m: &m,
        diff: &diff,
        intraline: &intraline,
        syntax: &syntax,
        h_offset,
        selection,
        drag: drag_pos.filter(|_| drag_edge.is_none()),
        mutable,
        staged,
        busy_hunk,
    };
    let mut top = 0;
    let mut painted: Option<(usize, usize)> = None;
    ui.scope_builder(UiBuilder::new().max_rect(rows_rect), |ui| {
        top = rows::virtual_rows(ui, ("diff_rows", mode_salt(mode)), total, scroll_to, |ui, i, rect| {
            let visible = rect.intersect(rows_rect);
            if visible.height() > 0.0 {
                painted = Some(painted.map_or((i, i), |(first, _)| (first, i)));
            }
            if drag_edge.is_none() && drag_y.is_some_and(|y| visible.y_range().contains(y)) {
                // Refined to the char under the pointer when the row has text in the selection's column.
                acts.push(Act::DragTo(TextPos::new(i, 0)));
            }
            match built {
                Built::Unified(r) => {
                    if let Some(row) = r.rows.get(i) {
                        painter_ctx.unified_row(ui, i, rect, row, acts);
                    }
                }
                Built::Split(r) => {
                    if let Some(row) = r.rows.get(i) {
                        painter_ctx.split_row(ui, i, rect, row, acts);
                    }
                }
            }
            if focus_row == Some(i) {
                // The hunk last reached with J / K.
                ui.painter()
                    .rect_filled(egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())), 0.0, p.accent);
            }
        });
    });
    d.top_row = top;
    if let (Some(down), Some((first, last))) = (drag_edge, painted) {
        acts.push(Act::DragTo(if down {
            TextPos::new(last, usize::MAX)
        } else {
            TextPos::new(first, 0)
        }));
    }
    rows::h_scrollbar(
        ui,
        bar_rect.shrink2(Vec2::new(4.0, 1.0)),
        &mut d.h_offset,
        content + 20.0,
        text_view,
        p.bg_graph_alt,
        p.border,
    );
    ui.advance_cursor_after_rect(area);
}

fn mode_salt(mode: DiffViewMode) -> u8 {
    match mode {
        DiffViewMode::Hunk => 0,
        DiffViewMode::Inline => 1,
        DiffViewMode::Split => 2,
    }
}

struct RowCtx<'a> {
    p: &'a Palette,
    m: &'a Metrics,
    diff: &'a FileDiff,
    intraline: &'a IntraLine,
    syntax: &'a DiffSyntax,
    h_offset: f32,
    selection: Option<TextSelection>,
    /// Pointer position while drag-selecting inside the rows.
    drag: Option<egui::Pos2>,
    mutable: bool,
    staged: bool,
    busy_hunk: bool,
}

impl RowCtx<'_> {
    fn highlights(&self, t: &TextRef) -> &[std::ops::Range<usize>] {
        match t {
            TextRef::Hunk { hunk, line } => self.intraline.get(&(*hunk, *line)).map_or(&[], Vec::as_slice),
            _ => &[],
        }
    }

    /// Syntax colours of a row's text: deleted lines from the old side, the rest from the new side.
    fn syntax(&self, t: &TextRef) -> Vec<(std::ops::Range<usize>, Color32)> {
        match t {
            TextRef::Hunk { hunk, line } => {
                let Some(l) = self.diff.hunks.get(*hunk).and_then(|h| h.lines.get(*line)) else {
                    return Vec::new();
                };
                let (side, text, number) = match l.origin {
                    LineOrigin::Del => (&self.syntax.old, &*self.diff.old_text, l.old_lineno),
                    LineOrigin::Add | LineOrigin::Context => (&self.syntax.new, &*self.diff.new_text, l.new_lineno),
                    LineOrigin::NoNewlineAtEof => return Vec::new(),
                };
                match number.and_then(|n| (n as usize).checked_sub(1)) {
                    Some(index) => side.line_colors(text, index, &l.text, self.p),
                    None => Vec::new(),
                }
            }
            TextRef::Old(range) => self.syntax.old.colors(range.clone(), self.p),
            TextRef::None | TextRef::Header(_) => Vec::new(),
        }
    }

    /// The selected chars of a line of `text` in `column` at row `i` (none for non-text cells).
    fn selected(&self, column: Column, i: usize, text: &str, is_text: bool) -> Option<(std::ops::Range<usize>, bool)> {
        let sel = self.selection.filter(|_| is_text)?;
        sel.cols_in(column, i, text.chars().count())
    }

    /// Text-view interaction of a painted line (read-only): press / drag / Shift+click to select, double-click a
    /// word, triple-click the line, right-click for the copy menu.
    #[allow(clippy::too_many_arguments)]
    fn text_cell(
        &self,
        ui: &mut Ui,
        rect: egui::Rect,
        column: Column,
        i: usize,
        copyable: bool,
        painted: &rows::Painted,
        acts: &mut Vec<Act>,
    ) {
        let resp = ui.interact(rect, ui.id().with(("row", column as u8, i)), Sense::click_and_drag());
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }
        let pos_at = |x: f32| TextPos::new(i, painted.col_at(x));
        if let Some(pointer) = resp.interact_pointer_pos().or(resp.hover_pos()) {
            let pos = pos_at(pointer.x);
            let (pressed, shift) = ui.input(|inp| (inp.pointer.primary_pressed(), inp.modifiers.shift));
            if pressed && resp.hovered() {
                acts.push(Act::Press {
                    column,
                    pos,
                    extend: shift,
                });
            }
            if resp.triple_clicked() {
                acts.push(Act::SelectLine { column, row: i });
            } else if resp.double_clicked() {
                acts.push(Act::SelectWord { column, pos });
            }
            if copyable && resp.secondary_clicked() {
                acts.push(Act::SelectIfOutside { column, pos });
            }
        }
        if let Some(drag) = self.drag
            && self.selection.is_some_and(|s| s.column == column)
            && rect.y_range().contains(drag.y)
        {
            acts.push(Act::DragTo(pos_at(drag.x)));
        }
        if copyable {
            resp.context_menu(|ui| {
                let shortcut = ui
                    .ctx()
                    .format_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::C));
                for (mode, label) in CopyMode::MENU {
                    let mut button = egui::Button::new(label);
                    if mode == CopyMode::WithChanges {
                        button = button.shortcut_text(shortcut.clone());
                    }
                    if ui.add(button).clicked() {
                        acts.push(Act::Copy(mode));
                        ui.close();
                    }
                }
            });
        }
    }

    fn unified_row(&self, ui: &mut Ui, i: usize, rect: egui::Rect, row: &Row, acts: &mut Vec<Act>) {
        let p = self.p;
        match row.kind {
            RowKind::HunkHeader => {
                ui.painter().rect_filled(rect, 0.0, p.bg_panel);
                let h = row.hunk.unwrap_or(0);
                let text = diff_model::text(self.diff, &row.text);
                crate::ui::paint_truncated(
                    ui.painter(),
                    egui::pos2(rect.left() + 10.0, rect.center().y),
                    text,
                    egui::FontId::monospace(rows::FONT_SIZE),
                    p.text_muted,
                    rect.width() * 0.6,
                );
                if self.mutable
                    && let Some(hunk) = self.diff.hunks.get(h)
                {
                    self.hunk_buttons(ui, rect, hunk.id, acts);
                }
            }
            RowKind::Spacer => {}
            RowKind::Fold { gap, lines } => {
                let resp = ui.interact(rect, ui.id().with(("fold", gap)), Sense::click());
                let bg = if resp.hovered() { p.bg_hover } else { p.bg_graph_alt };
                ui.painter().rect_filled(rect, 0.0, bg);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{} {lines} unchanged lines", icon::DOTS_THREE),
                    egui::FontId::monospace(rows::FONT_SIZE),
                    p.text_muted,
                );
                if resp.on_hover_text("Click to expand").clicked() {
                    acts.push(Act::Expand(gap));
                }
            }
            kind => {
                let copyable = kind != RowKind::NoNewline;
                let (bg, sign_color, hl) = match kind {
                    RowKind::Add => (p.add_bg, p.green, p.add_hl),
                    RowKind::Del => (p.del_bg, p.red, p.del_hl),
                    _ => (Color32::TRANSPARENT, p.text_muted, Color32::TRANSPARENT),
                };
                let text = diff_model::text(self.diff, &row.text);
                let syntax = self.syntax(&row.text);
                let spec = LineSpec {
                    bg,
                    numbers: &[row.old_no, row.new_no],
                    number_color: p.text_faint,
                    sign: kind.sign(),
                    sign_color,
                    text,
                    text_color: if kind == RowKind::NoNewline { p.text_faint } else { p.text },
                    syntax: &syntax,
                    highlights: self.highlights(&row.text),
                    highlight_bg: hl,
                    italic: kind == RowKind::NoNewline,
                    selected: self.selected(Column::Unified, i, text, copyable),
                    selection_bg: ui.visuals().selection.bg_fill,
                };
                let painted = rows::paint_line(ui.painter(), rect, self.m, &spec, self.h_offset);
                self.text_cell(ui, rect, Column::Unified, i, copyable, &painted, acts);
            }
        }
    }

    fn hunk_buttons(&self, ui: &mut Ui, rect: egui::Rect, id: HunkRef, acts: &mut Vec<Act>) {
        let p = self.p;
        let area = rect.shrink2(Vec2::new(8.0, 1.0));
        ui.scope_builder(UiBuilder::new().max_rect(area).layout(Layout::right_to_left(Align::Center)), |ui| {
            ui.spacing_mut().button_padding = Vec2::new(6.0, 0.0);
            ui.spacing_mut().item_spacing.x = 6.0;
            let enabled = !self.busy_hunk;
            let small = |text: &str, color: Color32| egui::Button::new(RichText::new(text).small().color(color)).corner_radius(3);
            if self.staged {
                if ui.add_enabled(enabled, small("Unstage hunk", p.text)).clicked() {
                    acts.push(Act::UnstageHunk(id));
                }
            } else {
                if ui.add_enabled(enabled, small("Discard hunk…", p.red)).clicked() {
                    acts.push(Act::DiscardHunk(id));
                }
                if ui.add_enabled(enabled, small("Stage hunk", p.text)).clicked() {
                    acts.push(Act::StageHunk(id));
                }
            }
        });
    }

    fn split_row(&self, ui: &mut Ui, i: usize, rect: egui::Rect, row: &SplitRow, acts: &mut Vec<Act>) {
        let p = self.p;
        if let CellKind::Fold { gap, lines } = row.left.kind {
            let resp = ui.interact(rect, ui.id().with(("fold", gap)), Sense::click());
            let bg = if resp.hovered() { p.bg_hover } else { p.bg_graph_alt };
            ui.painter().rect_filled(rect, 0.0, bg);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("{} {lines} unchanged lines", icon::DOTS_THREE),
                egui::FontId::monospace(rows::FONT_SIZE),
                p.text_muted,
            );
            if resp.on_hover_text("Click to expand").clicked() {
                acts.push(Act::Expand(gap));
            }
            return;
        }
        let half = rect.width() / 2.0;
        let left = egui::Rect::from_min_size(rect.min, Vec2::new(half, rect.height()));
        let right = egui::Rect::from_min_size(egui::pos2(rect.left() + half, rect.top()), Vec2::new(half, rect.height()));
        for (cell, r, column) in [(&row.left, left, Column::Left), (&row.right, right, Column::Right)] {
            let copyable = cell_kind(cell.kind).is_some();
            let text = diff_model::text(self.diff, &cell.text);
            let (bg, sign, sign_color, hl) = match cell.kind {
                CellKind::Add => (p.add_bg, "+", p.green, p.add_hl),
                CellKind::Del => (p.del_bg, "−", p.red, p.del_hl),
                CellKind::Filler => (p.bg_graph_alt, "", p.text_faint, Color32::TRANSPARENT),
                _ => (Color32::TRANSPARENT, "", p.text_faint, Color32::TRANSPARENT),
            };
            let syntax = self.syntax(&cell.text);
            let spec = LineSpec {
                bg,
                numbers: &[cell.no],
                number_color: p.text_faint,
                sign,
                sign_color,
                text,
                text_color: if cell.kind == CellKind::NoNewline { p.text_faint } else { p.text },
                syntax: &syntax,
                highlights: self.highlights(&cell.text),
                highlight_bg: hl,
                italic: cell.kind == CellKind::NoNewline,
                selected: self.selected(column, i, text, copyable),
                selection_bg: ui.visuals().selection.bg_fill,
            };
            let painted = rows::paint_line(&ui.painter().with_clip_rect(r), r, self.m, &spec, self.h_offset);
            self.text_cell(ui, r, column, i, copyable, &painted, acts);
        }
        ui.painter()
            .vline(rect.left() + half, rect.y_range(), egui::Stroke::new(1.0, p.border));
    }
}

#[cfg(test)]
mod tests;
