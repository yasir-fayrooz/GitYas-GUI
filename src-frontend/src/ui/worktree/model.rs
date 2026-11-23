//! Pure, UI-free logic of the WIP panel: multi-selection, tree grouping, commit-box labels / validation, merge
//! prefill and confirmation texts. Everything here is unit tested; `panel.rs` / `commit_box.rs` only draw it.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use gityas_core::models::{Delta, DeltaChange, FileState, RepoOperation, RepoStatus};

/// Summary length git conventions recommend; the counter counts down from it.
pub const SUMMARY_LIMIT: i64 = 72;

// ---- Sections ---------------------------------------------------------------------------------------------------

/// The three file lists of the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Conflicted,
    Unstaged,
    Staged,
}

impl Section {
    /// `staged` flag of the working-tree diff target for files of this section.
    pub fn staged(self) -> bool {
        self == Section::Staged
    }
}

/// Files of `section`. Conflicted files are listed only in their own section (staging one would mark it resolved
/// with markers in it; the engine refuses that anyway), so "Stage all" never touches them.
pub fn section_files(files: &FileState, section: Section) -> Vec<&Delta> {
    match section {
        Section::Conflicted => files.conflicted().collect(),
        Section::Unstaged => files.unstaged.iter().filter(|d| !d.change.is_conflict()).collect(),
        Section::Staged => files.staged.iter().filter(|d| !d.change.is_conflict()).collect(),
    }
}

// ---- Selection --------------------------------------------------------------------------------------------------

/// How a row was clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClickMods {
    /// Shift: range from the anchor.
    pub shift: bool,
    /// Cmd on macOS, Ctrl elsewhere (`Modifiers::command`): toggle.
    pub command: bool,
}

impl ClickMods {
    pub fn from_egui(m: egui::Modifiers) -> Self {
        Self {
            shift: m.shift,
            command: m.command,
        }
    }

    pub fn plain(self) -> bool {
        !self.shift && !self.command
    }
}

/// Multi-selection inside one section, keyed by path (indices shift when the files change). `order` arguments are
/// the paths in on-screen order (list or tree), so Shift ranges follow what the user sees.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSelection {
    /// Section the selection belongs to (a plain click in another section moves it there).
    pub section: Option<Section>,
    pub paths: HashSet<PathBuf>,
    /// Last plainly clicked / toggled row: start of Shift ranges.
    pub anchor: Option<PathBuf>,
}

impl FileSelection {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn contains(&self, section: Section, path: &Path) -> bool {
        self.section == Some(section) && self.paths.contains(path)
    }

    /// Number of selected files in `section` (0 if the selection is elsewhere).
    pub fn count_in(&self, section: Section) -> usize {
        if self.section == Some(section) { self.paths.len() } else { 0 }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Applies a click on `path` in `section`. Plain: select only it (or deselect it when it is the only selected
    /// row). Cmd: toggle it. Shift: select the range from
    /// the anchor to it (Cmd+Shift adds the range to the selection). Clicking in another section starts over there.
    pub fn click(&mut self, section: Section, order: &[&Path], path: &Path, mods: ClickMods) {
        if self.section != Some(section) {
            self.clear();
            self.section = Some(section);
        }
        if mods.shift {
            let anchor = self.anchor.as_deref().and_then(|a| order.iter().position(|p| *p == a));
            let target = order.iter().position(|p| *p == path);
            if let (Some(a), Some(t)) = (anchor, target) {
                if !mods.command {
                    self.paths.clear();
                }
                let (lo, hi) = (a.min(t), a.max(t));
                self.paths.extend(order[lo..=hi].iter().map(|p| p.to_path_buf()));
                return;
            }
            // No (valid) anchor: behaves like a plain click.
        }
        if mods.command && !mods.shift {
            if !self.paths.remove(path) {
                self.paths.insert(path.to_path_buf());
            }
            self.anchor = Some(path.to_path_buf());
            return;
        }
        // Clicking the only selected row again deselects it (its diff closes too: `diff::toggle`).
        if self.paths.len() == 1 && self.paths.contains(path) {
            self.clear();
            return;
        }
        self.paths.clear();
        self.paths.insert(path.to_path_buf());
        self.anchor = Some(path.to_path_buf());
    }

    /// Makes `path` the only selected row (no toggling).
    pub fn select_only(&mut self, section: Section, path: &Path) {
        self.clear();
        self.section = Some(section);
        self.paths.insert(path.to_path_buf());
        self.anchor = Some(path.to_path_buf());
    }

    /// Right-click: a row outside the selection becomes the only selected row; inside, the selection is kept.
    pub fn context_click(&mut self, section: Section, path: &Path) {
        if !self.contains(section, path) {
            self.select_only(section, path);
        }
    }

    /// Drops paths that are no longer listed in the section (after `GitFilesChanged`).
    pub fn retain_existing(&mut self, files: &FileState) {
        let Some(section) = self.section else { return };
        let existing: HashSet<&Path> = section_files(files, section).into_iter().map(|d| d.path.as_path()).collect();
        self.paths.retain(|p| existing.contains(p.as_path()));
        if self.anchor.as_deref().is_some_and(|a| !existing.contains(a)) {
            self.anchor = None;
        }
        if self.is_empty() {
            self.clear();
        }
    }

    /// The files an action on `section` applies to: the selection if it is in this section, else `all`.
    pub fn files_or_all<'a>(&self, section: Section, all: &[&'a Delta]) -> Vec<&'a Delta> {
        if self.count_in(section) == 0 {
            return all.to_vec();
        }
        all.iter().copied().filter(|d| self.paths.contains(&d.path)).collect()
    }
}

/// The row to show once `current` and the `leaving` files move to the other section: the next remaining row in
/// on-screen order, else the closest one above it. `None` when nothing is left.
pub fn next_remaining(order: &[&Path], current: &Path, leaving: &HashSet<&Path>) -> Option<PathBuf> {
    let at = order.iter().position(|p| *p == current)?;
    let stays = |p: &&&Path| !leaving.contains(**p) && **p != current;
    order[at + 1..]
        .iter()
        .find(stays)
        .or_else(|| order[..at].iter().rev().find(stays))
        .map(|p| p.to_path_buf())
}

// ---- Tree view --------------------------------------------------------------------------------------------------

/// One visible row of the tree view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    /// A directory (`path` = its repository-relative path, used as the collapse key).
    Dir { depth: usize, name: String, path: PathBuf },
    /// `index` into the section's file list.
    File { depth: usize, index: usize },
}

#[derive(Default)]
struct DirNode {
    dirs: BTreeMap<String, DirNode>,
    files: Vec<(String, usize)>,
}

/// Groups `files` by directory: directories first (sorted), then files (sorted by name); children of a collapsed
/// directory are skipped. Works on path components, so Windows separators are fine.
pub fn build_tree(files: &[&Delta], collapsed: &HashSet<PathBuf>) -> Vec<TreeRow> {
    let mut root = DirNode::default();
    for (index, delta) in files.iter().enumerate() {
        let mut node = &mut root;
        let parts: Vec<String> = delta
            .path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let Some((name, dirs)) = parts.split_last() else { continue };
        for dir in dirs {
            node = node.dirs.entry(dir.clone()).or_default();
        }
        node.files.push((name.clone(), index));
    }
    let mut rows = Vec::new();
    push_rows(&root, 0, &PathBuf::new(), collapsed, &mut rows);
    rows
}

fn push_rows(node: &DirNode, depth: usize, prefix: &Path, collapsed: &HashSet<PathBuf>, rows: &mut Vec<TreeRow>) {
    for (name, child) in &node.dirs {
        let path = prefix.join(name);
        rows.push(TreeRow::Dir {
            depth,
            name: name.clone(),
            path: path.clone(),
        });
        if !collapsed.contains(&path) {
            push_rows(child, depth + 1, &path, collapsed, rows);
        }
    }
    let mut files = node.files.clone();
    files.sort();
    for (_, index) in files {
        rows.push(TreeRow::File { depth, index });
    }
}

// ---- Rows -------------------------------------------------------------------------------------------------------

/// File name and directory (with a trailing `/`) of a row; the full path if it has no file name.
pub fn split_path(delta: &Delta) -> (String, String) {
    let name = delta
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| delta.path.to_string_lossy().into_owned());
    let dir = delta
        .directory()
        // Git paths use `/` on every OS (the engine passes them through unchanged).
        .map(|d| format!("{}/", d.to_string_lossy()))
        .unwrap_or_default();
    (name, dir)
}

/// `true` if the path cannot be shown faithfully (non-UTF-8 bytes; the UI flags it).
pub fn is_lossy(delta: &Delta) -> bool {
    delta.path.to_str().is_none() || delta.old_path.as_ref().is_some_and(|p| p.to_str().is_none())
}

pub fn files_word(n: usize) -> &'static str {
    if n == 1 { "file" } else { "files" }
}

/// Section button: "Stage all changes" / "Stage 3 files" (with a selection in that section); conflicts get
/// "Mark all resolved" / "Mark 2 files resolved".
pub fn section_button_label(section: Section, selected: usize) -> String {
    let verb = match section {
        Section::Staged => "Unstage",
        Section::Unstaged => "Stage",
        Section::Conflicted if selected == 0 => return "Mark all resolved".into(),
        Section::Conflicted => return format!("Mark {selected} {} resolved", files_word(selected)),
    };
    if selected == 0 {
        format!("{verb} all changes")
    } else {
        format!("{verb} {selected} {}", files_word(selected))
    }
}

// ---- Commit box -------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterTone {
    Normal,
    /// Fewer than 10 characters left (orange).
    Warn,
    /// Over the limit (red).
    Over,
}

/// Characters left of [`SUMMARY_LIMIT`] (negative when over) and how to colour it.
pub fn summary_counter(summary: &str) -> (i64, CounterTone) {
    let count = i64::try_from(summary.chars().count()).unwrap_or(i64::MAX);
    let remaining = SUMMARY_LIMIT.saturating_sub(count);
    let tone = if remaining < 0 {
        CounterTone::Over
    } else if remaining < 10 {
        CounterTone::Warn
    } else {
        CounterTone::Normal
    };
    (remaining, tone)
}

/// What the commit button needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitContext<'a> {
    pub staged: usize,
    pub conflicts: usize,
    pub summary: &'a str,
    pub operation: RepoOperation,
    pub committing: bool,
}

impl<'a> CommitContext<'a> {
    pub fn new(files: &FileState, status: &RepoStatus, summary: &'a str, committing: bool) -> Self {
        Self {
            staged: section_files(files, Section::Staged).len(),
            conflicts: files.conflicted().count(),
            summary,
            operation: status.operation,
            committing,
        }
    }

    pub fn merging(&self) -> bool {
        self.operation == RepoOperation::Merge
    }

    /// "Commit merge" during a merge, else "Commit changes to N files".
    pub fn button_label(&self) -> String {
        if self.merging() {
            "Commit merge".into()
        } else {
            format!("Commit changes to {} {}", self.staged, files_word(self.staged))
        }
    }

    /// Why the button is disabled (its tooltip), or `None` if a commit can be attempted.
    pub fn blocker(&self) -> Option<String> {
        if self.committing {
            return Some("Committing…".into());
        }
        if matches!(self.operation, RepoOperation::Rebase | RepoOperation::ApplyMailbox) {
            let what = self.operation.describe().unwrap_or("an operation");
            return Some(format!("Finish or abort {what} first"));
        }
        if self.conflicts > 0 {
            return Some(format!(
                "Resolve {} conflicted {} first",
                self.conflicts,
                files_word(self.conflicts)
            ));
        }
        // A merge commit may have no changes of its own (e.g. resolved to "ours"); the engine allows it.
        if self.staged == 0 && !self.merging() {
            return Some("Stage files to commit".into());
        }
        if self.summary.trim().is_empty() {
            return Some("Enter a commit summary".into());
        }
        None
    }
}

/// The description as the engine wants it (`None` when blank).
pub fn description_arg(description: &str) -> Option<String> {
    let trimmed = description.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Commit draft of one tab with the merge prefill state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergePrefill {
    /// Summary we prefilled for the current merge (cleared again if untouched when the merge ends).
    pub prefilled: Option<String>,
}

impl MergePrefill {
    /// Keeps `summary` in sync with the repository state: during a merge, an empty draft gets the prepared summary
    /// (once per merge, so clearing it sticks); when the merge ends, an untouched prefill is removed.
    pub fn sync(&mut self, status: &RepoStatus, summary: &mut String) {
        if status.is_merging() {
            if self.prefilled.is_none() {
                if summary.trim().is_empty()
                    && let Some(prepared) = status.prepared_summary()
                {
                    *summary = prepared.to_string();
                    self.prefilled = Some(prepared.to_string());
                } else {
                    // The user already had a draft: never overwrite it, and don't retry later in this merge.
                    self.prefilled = Some(String::new());
                }
            }
        } else if let Some(prefilled) = self.prefilled.take()
            && !prefilled.is_empty()
            && *summary == prefilled
        {
            summary.clear();
        }
    }
}

// ---- Confirmations / stash --------------------------------------------------------------------------------------

/// Text of "Discard changes…": `(title, body, button)`.
pub fn discard_confirm_text(files: &[&Delta]) -> (String, String, String) {
    let untracked = files.iter().filter(|d| d.change == DeltaChange::Untracked).count();
    let tracked = files.len() - untracked;
    let single = (files.len() == 1).then(|| files.first().map(|d| d.display_label())).flatten();
    let body = match (tracked, untracked, single) {
        (0, 1, Some(name)) => format!("Delete untracked file {name}?"),
        (1, 0, Some(name)) => format!("Discard changes to {name}? This can't be undone."),
        (0, n, _) => format!("Delete {n} untracked files?"),
        (t, 0, _) => format!("Discard changes to {t} {}? This can't be undone.", files_word(t)),
        (t, n, _) => format!(
            "Discard changes to {t} {} and delete {n} untracked {}? This can't be undone.",
            files_word(t),
            files_word(n)
        ),
    };
    let (title, button) = if tracked == 0 {
        ("Delete untracked files?", "Delete")
    } else {
        ("Discard changes?", "Discard")
    };
    (title.into(), body, button.into())
}

/// True if a stash would have nothing to save (checked before calling the engine, which also refuses).
pub fn nothing_to_stash(files: &[&Delta], include_untracked: bool) -> bool {
    !files.iter().any(|d| include_untracked || d.change != DeltaChange::Untracked)
}

/// The stash message as the engine wants it (`None` when blank: git's "WIP on <branch>").
pub fn stash_message_arg(message: &str) -> Option<String> {
    description_arg(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(path: &str, change: DeltaChange) -> Delta {
        Delta::new(path, change)
    }

    fn paths(sel: &FileSelection) -> Vec<String> {
        let mut v: Vec<String> = sel.paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    const SHIFT: ClickMods = ClickMods {
        shift: true,
        command: false,
    };
    const CMD: ClickMods = ClickMods {
        shift: false,
        command: true,
    };

    #[test]
    fn plain_click_selects_one_and_cmd_toggles() {
        let order: Vec<&Path> = ["a", "b", "c", "d"].iter().map(Path::new).collect();
        let mut sel = FileSelection::default();
        sel.click(Section::Unstaged, &order, Path::new("b"), ClickMods::default());
        assert_eq!(paths(&sel), ["b"]);
        // Plain click on the only selected row deselects it.
        sel.click(Section::Unstaged, &order, Path::new("b"), ClickMods::default());
        assert!(sel.is_empty() && sel.count_in(Section::Unstaged) == 0);
        sel.click(Section::Unstaged, &order, Path::new("b"), ClickMods::default());
        assert_eq!(paths(&sel), ["b"]);
        sel.click(Section::Unstaged, &order, Path::new("d"), CMD);
        assert_eq!(paths(&sel), ["b", "d"]);
        sel.click(Section::Unstaged, &order, Path::new("b"), CMD);
        assert_eq!(paths(&sel), ["d"]);
        assert_eq!(sel.count_in(Section::Unstaged), 1);
        assert_eq!(sel.count_in(Section::Staged), 0);
    }

    #[test]
    fn shift_selects_ranges_from_the_anchor_in_both_directions() {
        let order: Vec<&Path> = ["a", "b", "c", "d", "e"].iter().map(Path::new).collect();
        let mut sel = FileSelection::default();
        sel.click(Section::Unstaged, &order, Path::new("b"), ClickMods::default());
        sel.click(Section::Unstaged, &order, Path::new("d"), SHIFT);
        assert_eq!(paths(&sel), ["b", "c", "d"]);
        // A second Shift-click re-ranges from the same anchor (shrinks / flips).
        sel.click(Section::Unstaged, &order, Path::new("a"), SHIFT);
        assert_eq!(paths(&sel), ["a", "b"]);
        // Cmd+Shift adds a range to the selection.
        sel.click(Section::Unstaged, &order, Path::new("e"), CMD);
        sel.click(
            Section::Unstaged,
            &order,
            Path::new("d"),
            ClickMods {
                shift: true,
                command: true,
            },
        );
        assert_eq!(paths(&sel), ["a", "b", "d", "e"]);
    }

    #[test]
    fn shift_without_anchor_and_other_sections() {
        let order: Vec<&Path> = ["a", "b"].iter().map(Path::new).collect();
        let mut sel = FileSelection::default();
        sel.click(Section::Staged, &order, Path::new("b"), SHIFT);
        assert_eq!(paths(&sel), ["b"]);
        sel.click(Section::Unstaged, &order, Path::new("a"), CMD);
        assert_eq!(sel.section, Some(Section::Unstaged));
        assert_eq!(paths(&sel), ["a"]);
        assert!(!sel.contains(Section::Staged, Path::new("b")));
    }

    #[test]
    fn context_click_keeps_or_replaces_the_selection() {
        let order: Vec<&Path> = ["a", "b", "c"].iter().map(Path::new).collect();
        let mut sel = FileSelection::default();
        sel.click(Section::Unstaged, &order, Path::new("a"), ClickMods::default());
        sel.click(Section::Unstaged, &order, Path::new("b"), CMD);
        sel.context_click(Section::Unstaged, Path::new("a"));
        assert_eq!(paths(&sel), ["a", "b"]);
        sel.context_click(Section::Unstaged, Path::new("c"));
        assert_eq!(paths(&sel), ["c"]);
    }

    #[test]
    fn next_remaining_prefers_the_row_below() {
        let order: Vec<&Path> = ["a", "b", "c", "d"].iter().map(Path::new).collect();
        let none = HashSet::new();
        let next = |current: &str, leaving: &HashSet<&Path>| next_remaining(&order, Path::new(current), leaving);
        assert_eq!(next("b", &none), Some(PathBuf::from("c")));
        assert_eq!(next("d", &none), Some(PathBuf::from("c")), "last row: the one above");
        let leaving: HashSet<&Path> = [Path::new("c"), Path::new("d")].into();
        assert_eq!(next("b", &leaving), Some(PathBuf::from("a")), "skips the rows moving with it");
        assert_eq!(next_remaining(&order[..1], Path::new("a"), &none), None);
        assert_eq!(next("x", &none), None, "not listed");
    }

    #[test]
    fn retain_existing_drops_vanished_files() {
        let order: Vec<&Path> = ["a", "b"].iter().map(Path::new).collect();
        let mut sel = FileSelection::default();
        sel.click(Section::Unstaged, &order, Path::new("a"), ClickMods::default());
        sel.click(Section::Unstaged, &order, Path::new("b"), CMD);
        let files = FileState {
            unstaged: vec![d("b", DeltaChange::Modified)],
            staged: vec![d("a", DeltaChange::Modified)],
        };
        sel.retain_existing(&files);
        assert_eq!(paths(&sel), ["b"]);
        sel.retain_existing(&FileState::default());
        assert_eq!(sel, FileSelection::default());
    }

    #[test]
    fn files_or_all_and_section_labels() {
        let a = d("a", DeltaChange::Modified);
        let b = d("b", DeltaChange::Untracked);
        let all = [&a, &b];
        let mut sel = FileSelection::default();
        assert_eq!(sel.files_or_all(Section::Unstaged, &all).len(), 2);
        sel.click(Section::Unstaged, &[Path::new("b")], Path::new("b"), ClickMods::default());
        assert_eq!(sel.files_or_all(Section::Unstaged, &all), [&b]);
        assert_eq!(sel.files_or_all(Section::Staged, &all).len(), 2);
        assert_eq!(section_button_label(Section::Unstaged, 0), "Stage all changes");
        assert_eq!(section_button_label(Section::Staged, 0), "Unstage all changes");
        assert_eq!(section_button_label(Section::Unstaged, 3), "Stage 3 files");
        assert_eq!(section_button_label(Section::Staged, 1), "Unstage 1 file");
        assert_eq!(section_button_label(Section::Conflicted, 0), "Mark all resolved");
        assert_eq!(section_button_label(Section::Conflicted, 1), "Mark 1 file resolved");
        assert_eq!(section_button_label(Section::Conflicted, 2), "Mark 2 files resolved");
    }

    #[test]
    fn sections_keep_conflicts_apart() {
        let files = FileState {
            unstaged: vec![d("c", DeltaChange::Conflicted), d("m", DeltaChange::Modified)],
            staged: vec![d("s", DeltaChange::Added)],
        };
        let names = |s| section_files(&files, s).iter().map(|d| d.path.clone()).collect::<Vec<_>>();
        assert_eq!(names(Section::Conflicted), [PathBuf::from("c")]);
        assert_eq!(names(Section::Unstaged), [PathBuf::from("m")]);
        assert_eq!(names(Section::Staged), [PathBuf::from("s")]);
    }

    #[test]
    fn tree_groups_by_directory_dirs_first() {
        let files = [
            d("src/ui/b.rs", DeltaChange::Modified),
            d("README.md", DeltaChange::Modified),
            d("src/a.rs", DeltaChange::Added),
            d("src/ui/a.rs", DeltaChange::Deleted),
            d("docs/x.md", DeltaChange::Untracked),
        ];
        let refs: Vec<&Delta> = files.iter().collect();
        let rows = build_tree(&refs, &HashSet::new());
        let dir = |depth, name: &str, path: &str| TreeRow::Dir {
            depth,
            name: name.into(),
            path: path.into(),
        };
        let file = |depth, index| TreeRow::File { depth, index };
        assert_eq!(
            rows,
            [
                dir(0, "docs", "docs"),
                file(1, 4),
                dir(0, "src", "src"),
                dir(1, "ui", "src/ui"),
                file(2, 3),
                file(2, 0),
                file(1, 2),
                file(0, 1),
            ]
        );
        let collapsed: HashSet<PathBuf> = [PathBuf::from("src")].into();
        let rows = build_tree(&refs, &collapsed);
        assert_eq!(rows, [dir(0, "docs", "docs"), file(1, 4), dir(0, "src", "src"), file(0, 1)]);
        assert!(build_tree(&[], &HashSet::new()).is_empty());
    }

    #[test]
    fn split_path_and_lossy() {
        let (name, dir) = split_path(&d("src/ui/app.rs", DeltaChange::Modified));
        assert_eq!(name, "app.rs");
        assert_eq!(dir, "src/ui/");
        let (name, dir) = split_path(&d("top.txt", DeltaChange::Modified));
        assert_eq!((name.as_str(), dir.as_str()), ("top.txt", ""));
        assert!(!is_lossy(&d("ok", DeltaChange::Added)));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bad = Delta::new(std::ffi::OsStr::from_bytes(b"bad\xff.txt"), DeltaChange::Added);
            assert!(is_lossy(&bad));
        }
    }

    #[test]
    fn counter_colours() {
        assert_eq!(summary_counter(""), (72, CounterTone::Normal));
        assert_eq!(summary_counter(&"x".repeat(62)), (10, CounterTone::Normal));
        assert_eq!(summary_counter(&"x".repeat(63)), (9, CounterTone::Warn));
        assert_eq!(summary_counter(&"x".repeat(72)), (0, CounterTone::Warn));
        assert_eq!(summary_counter(&"x".repeat(73)), (-1, CounterTone::Over));
        // Characters, not bytes.
        assert_eq!(summary_counter("ééé").0, 69);
    }

    fn ctx(staged: usize, conflicts: usize, summary: &str, operation: RepoOperation, committing: bool) -> CommitContext<'_> {
        CommitContext {
            staged,
            conflicts,
            summary,
            operation,
            committing,
        }
    }

    #[test]
    fn commit_button_labels_and_blockers() {
        use RepoOperation::*;
        assert_eq!(ctx(1, 0, "x", None, false).button_label(), "Commit changes to 1 file");
        assert_eq!(ctx(3, 0, "x", None, false).button_label(), "Commit changes to 3 files");
        assert_eq!(ctx(0, 0, "x", Merge, false).button_label(), "Commit merge");

        assert_eq!(ctx(1, 0, "fix", None, false).blocker(), Option::None);
        assert_eq!(ctx(0, 0, "fix", None, false).blocker().as_deref(), Some("Stage files to commit"));
        assert_eq!(ctx(1, 0, "  ", None, false).blocker().as_deref(), Some("Enter a commit summary"));
        assert_eq!(
            ctx(1, 2, "fix", Merge, false).blocker().as_deref(),
            Some("Resolve 2 conflicted files first")
        );
        assert_eq!(ctx(1, 0, "fix", None, true).blocker().as_deref(), Some("Committing…"));
        // A merge commit may have nothing staged.
        assert_eq!(ctx(0, 0, "Merge", Merge, false).blocker(), Option::None);
        assert_eq!(
            ctx(1, 0, "fix", Rebase, false).blocker().as_deref(),
            Some("Finish or abort a rebase first")
        );
        assert_eq!(ctx(1, 0, "fix", CherryPick, false).blocker(), Option::None);
        assert_eq!(description_arg("  \n"), Option::None);
        assert_eq!(description_arg(" body \n").as_deref(), Some("body"));
    }

    fn merging(message: Option<&str>) -> RepoStatus {
        RepoStatus {
            operation: RepoOperation::Merge,
            prepared_message: message.map(str::to_owned),
            ..RepoStatus::default()
        }
    }

    #[test]
    fn merge_prefill_only_into_an_empty_draft_and_once() {
        let mut prefill = MergePrefill::default();
        let mut summary = String::new();
        let status = merging(Some("Merge branch 'x' into main\n\n# Conflicts:\n#\ta.txt\n"));
        prefill.sync(&status, &mut summary);
        assert_eq!(summary, "Merge branch 'x' into main");
        // Cleared by the user: not refilled during the same merge.
        summary.clear();
        prefill.sync(&status, &mut summary);
        assert_eq!(summary, "");
        // Merge ends: nothing to remove; a new merge prefills again.
        prefill.sync(&RepoStatus::default(), &mut summary);
        prefill.sync(&status, &mut summary);
        assert_eq!(summary, "Merge branch 'x' into main");
        // Merge aborted with the prefill untouched: removed.
        prefill.sync(&RepoStatus::default(), &mut summary);
        assert_eq!(summary, "");

        // An existing draft is never overwritten and survives the merge.
        let mut summary = String::from("my draft");
        prefill.sync(&status, &mut summary);
        assert_eq!(summary, "my draft");
        prefill.sync(&RepoStatus::default(), &mut summary);
        assert_eq!(summary, "my draft");

        // No prepared message: nothing to prefill.
        let mut summary = String::new();
        let mut prefill = MergePrefill::default();
        prefill.sync(&merging(None), &mut summary);
        assert_eq!(summary, "");
    }

    #[test]
    fn discard_texts() {
        let m = d("src/app.rs", DeltaChange::Modified);
        let m2 = d("b.rs", DeltaChange::Deleted);
        let u = d("new.txt", DeltaChange::Untracked);
        let u2 = d("new2.txt", DeltaChange::Untracked);
        assert_eq!(
            discard_confirm_text(&[&m]),
            (
                "Discard changes?".into(),
                format!("Discard changes to {}? This can't be undone.", m.display_label()),
                "Discard".into()
            )
        );
        assert_eq!(
            discard_confirm_text(&[&m, &m2]).1,
            "Discard changes to 2 files? This can't be undone."
        );
        assert_eq!(
            discard_confirm_text(&[&u, &u2]),
            (
                "Delete untracked files?".into(),
                "Delete 2 untracked files?".into(),
                "Delete".into()
            )
        );
        assert_eq!(discard_confirm_text(&[&u]).1, "Delete untracked file new.txt?");
        assert_eq!(
            discard_confirm_text(&[&m, &u]).1,
            "Discard changes to 1 file and delete 1 untracked file? This can't be undone."
        );
    }

    #[test]
    fn stash_preconditions() {
        let m = d("a", DeltaChange::Modified);
        let u = d("b", DeltaChange::Untracked);
        assert!(nothing_to_stash(&[], true));
        assert!(nothing_to_stash(&[&u], false));
        assert!(!nothing_to_stash(&[&u], true));
        assert!(!nothing_to_stash(&[&m, &u], false));
        assert_eq!(stash_message_arg(""), Option::None);
        assert_eq!(stash_message_arg("wip").as_deref(), Some("wip"));
    }
}
