//! UI-free logic of the history screens: ref pills, commit search, branch folder trees, sidebar filter, tag rows,
//! ref-name validation and error classification. Everything here is unit-tested.

use std::collections::{BTreeMap, HashMap};

use gityas_core::Oid;
use gityas_core::models::{BranchKind, Commit, GraphOptions, Head, Reference, ReferenceKind, Tag, split_remote_branch};

// ---- Ref pills -----------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillKind {
    /// A local branch (maybe also on a remote: `remote`).
    Local,
    /// A remote-tracking branch with no local branch of the same name at this commit.
    Remote,
    Tag,
    /// `HEAD` when detached at this commit.
    DetachedHead,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pill {
    pub kind: PillKind,
    /// Text on the pill (`main`, `origin/feature/x`, `v1.0`, `HEAD`).
    pub label: String,
    /// Full ref name (`refs/heads/main`); `None` for the detached `HEAD` pill.
    pub refname: Option<String>,
    /// HEAD is on this branch (‹CHECK›).
    pub head: bool,
    /// A local branch exists (‹DESKTOP›).
    pub local: bool,
    /// The branch exists on a remote at this commit (‹CLOUD›); for a local pill: a remote branch of the same name.
    pub remote: bool,
    /// Remote-tracking ref names merged into this pill (tooltip).
    pub remotes: Vec<String>,
}

impl Pill {
    pub fn is_branch(&self) -> bool {
        matches!(self.kind, PillKind::Local | PillKind::Remote)
    }
}

/// Pills for one commit, in display order: detached `HEAD`, HEAD's branch, other local branches, remote-only
/// branches, tags. A local branch and a remote branch with the same branch name at the same commit collapse into one
/// pill with both icons (in sync). `refs` is `RepoData::refs_by_commit[commit]` (no `<remote>/HEAD`).
pub fn build_pills(commit: &Oid, refs: &[Reference], head: Option<&Head>, remotes: &[String], opts: &GraphOptions) -> Vec<Pill> {
    let mut out = Vec::new();
    if let Some(Head::Detached { target }) = head
        && target == commit
    {
        out.push(Pill {
            kind: PillKind::DetachedHead,
            label: "HEAD".into(),
            refname: None,
            head: true,
            local: false,
            remote: false,
            remotes: Vec::new(),
        });
    }
    let head_ref = head.and_then(Head::refname);
    let remote_branch_name = |r: &Reference| -> String {
        split_remote_branch(&r.short_name, remotes)
            .map(|(_, b)| b.to_owned())
            .unwrap_or_else(|| r.short_name.split_once('/').map_or(r.short_name.clone(), |(_, b)| b.to_owned()))
    };
    let remote_refs: Vec<&Reference> = refs
        .iter()
        .filter(|r| r.kind == ReferenceKind::Branch(BranchKind::Remote) && !r.is_remote_head())
        .collect();
    let mut merged_remote = vec![false; remote_refs.len()];

    let mut locals: Vec<Pill> = Vec::new();
    for r in refs.iter().filter(|r| r.kind == ReferenceKind::Branch(BranchKind::Local)) {
        let mut names = Vec::new();
        for (i, rr) in remote_refs.iter().enumerate() {
            if remote_branch_name(rr) == r.short_name {
                merged_remote[i] = true;
                names.push(rr.short_name.clone());
            }
        }
        locals.push(Pill {
            kind: PillKind::Local,
            label: r.short_name.clone(),
            refname: Some(r.name.clone()),
            head: head_ref == Some(r.name.as_str()),
            local: true,
            remote: !names.is_empty(),
            remotes: names,
        });
    }
    // HEAD's branch first, keeping the store's order otherwise.
    locals.sort_by_key(|p| !p.head);
    out.extend(locals);
    if opts.show_remote_branches {
        for (i, r) in remote_refs.iter().enumerate() {
            if !merged_remote[i] {
                out.push(Pill {
                    kind: PillKind::Remote,
                    label: r.short_name.clone(),
                    refname: Some(r.name.clone()),
                    head: false,
                    local: false,
                    remote: true,
                    remotes: vec![r.short_name.clone()],
                });
            }
        }
    }
    if opts.show_tags {
        for r in refs.iter().filter(|r| r.is_tag()) {
            out.push(Pill {
                kind: PillKind::Tag,
                label: r.short_name.clone(),
                refname: Some(r.name.clone()),
                head: false,
                local: false,
                remote: false,
                remotes: Vec::new(),
            });
        }
    }
    out
}

/// Tooltip listing every ref of a pill row.
pub fn pills_tooltip(pills: &[Pill]) -> String {
    pills
        .iter()
        .map(|p| match p.kind {
            PillKind::Local if p.head => format!("{} (checked out)", p.label),
            PillKind::Local if p.remote => format!("{} (local, on {})", p.label, p.remotes.join(", ")),
            PillKind::Local => format!("{} (local)", p.label),
            PillKind::Remote => format!("{} (remote)", p.label),
            PillKind::Tag => format!("tag {}", p.label),
            PillKind::DetachedHead => "HEAD (detached)".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- Commit search (Cmd+F) -----------------------------------------------------------------------------------------

/// A prepared search query: case-insensitive substring of the summary / author name / email, or an id prefix.
#[derive(Debug, Clone, Default)]
pub struct Search {
    lower: String,
    hex_prefix: Option<String>,
}

impl Search {
    pub fn new(query: &str) -> Self {
        let q = query.trim();
        let lower = q.to_lowercase();
        let hex_prefix = (!lower.is_empty() && lower.len() <= 40 && lower.chars().all(|c| c.is_ascii_hexdigit())).then(|| lower.clone());
        Self { lower, hex_prefix }
    }

    pub fn is_empty(&self) -> bool {
        self.lower.is_empty()
    }

    pub fn matches(&self, commit: &Commit) -> bool {
        if self.lower.is_empty() {
            return false;
        }
        if self.hex_prefix.as_deref().is_some_and(|p| commit.id.starts_with_hex(p)) {
            return true;
        }
        contains_ci(&commit.summary, &self.lower)
            || contains_ci(&commit.author_name, &self.lower)
            || contains_ci(&commit.author_email, &self.lower)
    }
}

fn contains_ci(haystack: &str, lower_needle: &str) -> bool {
    haystack.to_lowercase().contains(lower_needle)
}

/// Sidebar filter: case-insensitive substring of the full name. Empty filter matches everything.
pub fn filter_matches(name: &str, filter: &str) -> bool {
    let f = filter.trim();
    f.is_empty() || contains_ci(name, &f.to_lowercase())
}

/// Next search hit after `from` (wrapping), for Enter in the search field.
pub fn next_match(commits: &[Commit], search: &Search, from: Option<usize>, backwards: bool) -> Option<usize> {
    let n = commits.len();
    if n == 0 || search.is_empty() {
        return None;
    }
    let start = from.unwrap_or(if backwards { 0 } else { n - 1 });
    (1..=n)
        .map(|step| {
            if backwards {
                (start + n - step % n) % n
            } else {
                (start + step) % n
            }
        })
        .find(|&i| search.matches(&commits[i]))
}

// ---- Branch folders ------------------------------------------------------------------------------------------------

/// One visible sidebar row of a folder tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    Folder {
        depth: usize,
        /// `feature/ui` (the folder's full path; the key of its open state).
        path: String,
        name: String,
        open: bool,
        /// Leaves below this folder.
        count: usize,
    },
    Leaf {
        depth: usize,
        /// Last path component (`x` for `feature/x`).
        name: String,
        /// Index into the caller's item list.
        index: usize,
    },
}

#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    leaves: Vec<(String, usize)>,
}

impl Dir {
    fn count(&self) -> usize {
        self.leaves.len() + self.dirs.values().map(Dir::count).sum::<usize>()
    }
}

/// Builds the visible rows of a `/`-folder tree of `names` (index = position in `names`). Folders come first,
/// sorted by name; leaves keep the caller's order. `is_open(path)` gives a folder's state; `force_open` (an active
/// filter) opens every folder. Empty path components (`a//b`, a trailing `/`) are kept as part of the leaf name.
pub fn folder_rows<S: AsRef<str>>(names: &[S], is_open: impl Fn(&str) -> bool, force_open: bool) -> Vec<TreeRow> {
    let mut root = Dir::default();
    for (index, name) in names.iter().enumerate() {
        let name = name.as_ref();
        let parts: Vec<&str> = name.split('/').collect();
        let well_formed = parts.len() > 1 && parts.iter().all(|p| !p.is_empty());
        if !well_formed {
            root.leaves.push((name.to_owned(), index));
            continue;
        }
        let (leaf, folders) = match parts.split_last() {
            Some(split) => split,
            None => continue,
        };
        let mut dir = &mut root;
        for part in folders {
            dir = dir.dirs.entry((*part).to_owned()).or_default();
        }
        dir.leaves.push(((*leaf).to_owned(), index));
    }
    let mut out = Vec::new();
    emit(&root, "", 0, &is_open, force_open, &mut out);
    out
}

fn emit(dir: &Dir, prefix: &str, depth: usize, is_open: &impl Fn(&str) -> bool, force_open: bool, out: &mut Vec<TreeRow>) {
    for (name, child) in &dir.dirs {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let open = force_open || is_open(&path);
        out.push(TreeRow::Folder {
            depth,
            path: path.clone(),
            name: name.clone(),
            open,
            count: child.count(),
        });
        if open {
            emit(child, &path, depth + 1, is_open, force_open, out);
        }
    }
    for (name, index) in &dir.leaves {
        out.push(TreeRow::Leaf {
            depth,
            name: name.clone(),
            index: *index,
        });
    }
}

// ---- Tags ----------------------------------------------------------------------------------------------------------

/// Where a tag exists, once the remotes' tag lists are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRow {
    pub name: String,
    /// Index into `CommitWrapper::tags`; `None` for a tag that is only on a remote.
    pub local: Option<usize>,
    /// Remotes known to have this tag (empty until `remote_tag_names` was fetched).
    pub on_remotes: Vec<String>,
}

/// Local tags plus (when known) remote-only tags, sorted by name. `remote_tags` maps a remote to its tag names.
pub fn tag_rows(tags: &[Tag], remote_tags: Option<&HashMap<String, Vec<String>>>) -> Vec<TagRow> {
    let mut rows: BTreeMap<String, TagRow> = BTreeMap::new();
    for (i, tag) in tags.iter().enumerate() {
        rows.insert(
            tag.name.clone(),
            TagRow {
                name: tag.name.clone(),
                local: Some(i),
                on_remotes: Vec::new(),
            },
        );
    }
    if let Some(remote_tags) = remote_tags {
        let mut remotes: Vec<&String> = remote_tags.keys().collect();
        remotes.sort();
        for remote in remotes {
            for name in remote_tags.get(remote).into_iter().flatten() {
                rows.entry(name.clone())
                    .or_insert_with(|| TagRow {
                        name: name.clone(),
                        local: None,
                        on_remotes: Vec::new(),
                    })
                    .on_remotes
                    .push(remote.clone());
            }
        }
    }
    rows.into_values().collect()
}

// ---- Names and errors ----------------------------------------------------------------------------------------------

/// Why `name` is not a valid branch / tag name (git's `check-ref-format` rules for one ref component path), or
/// `None` if it is valid. The engine validates again; this drives the dialog's live error.
pub fn ref_name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("Enter a name");
    }
    if name.chars().any(|c| c.is_whitespace()) {
        return Some("Names can't contain spaces");
    }
    let bad_char = |c: char| c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\');
    if name.chars().any(bad_char) {
        return Some("Names can't contain ~ ^ : ? * [ \\ or control characters");
    }
    if name.starts_with('-') {
        return Some("Names can't start with '-'");
    }
    if name == "@" || name == "HEAD" {
        return Some("This name is reserved by git");
    }
    if name.contains("..") || name.contains("@{") {
        return Some("Names can't contain '..' or '@{'");
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Some("Invalid use of '/'");
    }
    if name.ends_with('.') {
        return Some("Names can't end with '.'");
    }
    if name.split('/').any(|part| part.starts_with('.') || part.ends_with(".lock")) {
        return Some("Path parts can't start with '.' or end with '.lock'");
    }
    None
}

/// `ahead` / `behind` badge text: `2↑ 1↓`, empty when both are zero.
pub fn ahead_behind(ahead: usize, behind: usize) -> String {
    let mut parts = Vec::new();
    if ahead > 0 {
        parts.push(format!("{ahead}↑"));
    }
    if behind > 0 {
        parts.push(format!("{behind}↓"));
    }
    parts.join(" ")
}

/// The commit message without its first line (the body shown below the summary), trimmed.
pub fn message_body(message: &str) -> &str {
    match message.split_once('\n') {
        Some((_, rest)) => rest.trim(),
        None => "",
    }
}

/// Whether `target` is `head` or one of its ancestors, walking parent links through `commits` (display order, so
/// children always precede parents: nothing listed after `target` can lead to it). Commits whose message can be
/// edited are exactly these.
pub fn is_head_or_ancestor(commits: &[Commit], index: &HashMap<Oid, usize>, head: &Oid, target: &Oid) -> bool {
    let Some(&goal) = index.get(target) else { return false };
    let mut seen = std::collections::HashSet::new();
    let mut frontier = vec![head];
    while let Some(id) = frontier.pop() {
        let Some(&i) = index.get(id) else { continue };
        if i == goal {
            return true;
        }
        if i > goal || !seen.insert(i) {
            continue;
        }
        if let Some(commit) = commits.get(i) {
            frontier.extend(&commit.parents);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use gityas_core::models::TagAnnotation;

    use super::*;

    fn oid(b: u8) -> Oid {
        Oid::from_bytes(&[b; 20]).unwrap_or_else(|e| panic!("{e}"))
    }

    fn r(name: &str, target: u8) -> Reference {
        Reference::from_refname(name, oid(target))
    }

    fn commit(summary: &str, author: &str, id: u8) -> Commit {
        Commit {
            id: oid(id),
            parents: vec![],
            author_name: author.into(),
            author_email: format!("{}@example.com", author.to_lowercase()),
            author_time: 0,
            committer_time: 0,
            summary: summary.into(),
            description: String::new(),
            lossy: false,
        }
    }

    fn head_on(name: &str, target: u8) -> Head {
        Head::Branch {
            name: name.into(),
            refname: format!("refs/heads/{name}"),
            target: oid(target),
        }
    }

    #[test]
    fn pills_merge_local_and_remote_and_order_head_first() {
        let refs = [
            r("refs/heads/dev", 1),
            r("refs/heads/main", 1),
            r("refs/remotes/origin/main", 1),
            r("refs/remotes/origin/other", 1),
            r("refs/tags/v1", 1),
        ];
        let remotes = ["origin".to_owned()];
        let pills = build_pills(&oid(1), &refs, Some(&head_on("main", 1)), &remotes, &GraphOptions::default());
        let labels: Vec<&str> = pills.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["main", "dev", "origin/other", "v1"]);
        assert!(pills[0].head && pills[0].local && pills[0].remote);
        assert_eq!(pills[0].remotes, ["origin/main"]);
        assert!(!pills[1].head && !pills[1].remote);
        assert_eq!(pills[2].kind, PillKind::Remote);
        assert_eq!(pills[3].kind, PillKind::Tag);
        assert!(pills_tooltip(&pills).contains("main (checked out)"));
    }

    #[test]
    fn pills_respect_graph_options_and_detached_head() {
        let refs = [r("refs/remotes/origin/x", 2), r("refs/tags/v2", 2)];
        let head = Head::Detached { target: oid(2) };
        let opts = GraphOptions {
            show_remote_branches: false,
            show_tags: false,
            show_stashes: true,
            ..GraphOptions::default()
        };
        let pills = build_pills(&oid(2), &refs, Some(&head), &[], &opts);
        assert_eq!(pills.len(), 1);
        assert_eq!(pills[0].kind, PillKind::DetachedHead);
        assert_eq!(pills[0].refname, None);
        // Detached elsewhere: no HEAD pill.
        assert!(build_pills(&oid(3), &[], Some(&head), &[], &opts).is_empty());
    }

    #[test]
    fn pills_split_remote_names_with_slashes() {
        // Remote "up/stream": `up/stream/main` is branch `main` of that remote.
        let refs = [r("refs/heads/main", 1), r("refs/remotes/up/stream/main", 1)];
        let remotes = ["up/stream".to_owned()];
        let pills = build_pills(&oid(1), &refs, None, &remotes, &GraphOptions::default());
        assert_eq!(pills.len(), 1);
        assert!(pills[0].remote);
    }

    #[test]
    fn search_matches_message_author_and_sha_prefix() {
        let c = commit("Fix the Graph", "Ada", 0xab);
        assert!(Search::new("graph").matches(&c));
        assert!(Search::new("ADA").matches(&c));
        assert!(Search::new("ada@example").matches(&c));
        assert!(Search::new("abab").matches(&c));
        assert!(!Search::new("abac").matches(&c));
        assert!(!Search::new("").matches(&c));
        assert!(!Search::new("zzz").matches(&c));
        assert!(Search::new("  graph ").matches(&c));
    }

    #[test]
    fn next_match_wraps_in_both_directions() {
        let commits = [commit("a fix", "X", 1), commit("other", "X", 2), commit("fix b", "X", 3)];
        let s = Search::new("fix");
        assert_eq!(next_match(&commits, &s, None, false), Some(0));
        assert_eq!(next_match(&commits, &s, Some(0), false), Some(2));
        assert_eq!(next_match(&commits, &s, Some(2), false), Some(0));
        assert_eq!(next_match(&commits, &s, Some(0), true), Some(2));
        assert_eq!(next_match(&commits, &Search::new("none"), None, false), None);
        assert_eq!(next_match(&[], &s, None, false), None);
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        assert!(filter_matches("feature/Login", "login"));
        assert!(filter_matches("anything", "  "));
        assert!(!filter_matches("main", "dev"));
    }

    #[test]
    fn folder_rows_group_by_path_and_honour_state() {
        let names = ["main", "feature/a", "feature/ui/b", "feature/c", "fix/x", "weird/", "a//b"];
        let all_open = folder_rows(&names, |_| true, false);
        let shape: Vec<String> = all_open
            .iter()
            .map(|r| match r {
                TreeRow::Folder { depth, path, count, .. } => format!("{depth}:{path}/({count})"),
                TreeRow::Leaf { depth, name, index } => format!("{depth}:{name}#{index}"),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "0:feature/(3)",
                "1:feature/ui/(1)",
                "2:b#2",
                "1:a#1",
                "1:c#3",
                "0:fix/(1)",
                "1:x#4",
                "0:main#0",
                "0:weird/#5",
                "0:a//b#6",
            ]
        );
        // Closed `feature`: its children are hidden; the state is keyed by path, so `feature/ui` stays open.
        let closed = folder_rows(&names, |p| p != "feature", false);
        assert!(closed.iter().all(|r| !matches!(r, TreeRow::Leaf { index: 1..=3, .. })));
        // A filter forces every folder open.
        let forced = folder_rows(&names, |_| false, true);
        assert_eq!(forced.len(), all_open.len());
    }

    #[test]
    fn tag_rows_merge_local_and_remote() {
        let tags = [
            Tag {
                name: "v2".into(),
                target: oid(1),
                annotation: None,
            },
            Tag {
                name: "v1".into(),
                target: oid(1),
                annotation: Some(TagAnnotation {
                    message: "m".into(),
                    tagger: None,
                }),
            },
        ];
        let rows = tag_rows(&tags, None);
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["v1", "v2"]);
        assert!(rows.iter().all(|r| r.on_remotes.is_empty()));
        let remote = HashMap::from([("origin".to_owned(), vec!["v1".to_owned(), "v3".to_owned()])]);
        let rows = tag_rows(&tags, Some(&remote));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].on_remotes, ["origin"]);
        assert_eq!(rows[0].local, Some(1));
        assert!(rows[1].on_remotes.is_empty());
        assert_eq!((rows[2].name.as_str(), rows[2].local), ("v3", None));
    }

    #[test]
    fn ref_names_follow_git_rules() {
        for ok in ["main", "feature/x", "v1.0", "fix-1", "a.b/c_d", "ünï"] {
            assert_eq!(ref_name_error(ok), None, "{ok}");
        }
        for bad in [
            "",
            "a b",
            "-x",
            "a..b",
            "a@{b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[b",
            "a\\b",
            "/a",
            "a/",
            "a//b",
            "a.",
            ".a",
            "a/.b",
            "a.lock",
            "a/b.lock/c",
            "@",
            "HEAD",
            "a\u{7}",
        ] {
            assert!(ref_name_error(bad).is_some(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn small_formatters() {
        assert_eq!(ahead_behind(0, 0), "");
        assert_eq!(ahead_behind(2, 0), "2↑");
        assert_eq!(ahead_behind(2, 1), "2↑ 1↓");
        assert_eq!(message_body("summary"), "");
        assert_eq!(message_body("summary\n\nbody line\n"), "body line");
    }

    #[test]
    fn head_ancestry_follows_every_parent() {
        // 5 (merge of 4 and 3) → 4 → 2 → 1; 3 → 1; 6 is on another branch.
        let mut commits: Vec<Commit> = [6, 5, 4, 3, 2, 1].map(|id| commit("c", "a", id)).into();
        let parents: [&[u8]; 6] = [&[1], &[4, 3], &[2], &[1], &[1], &[]];
        for (c, ps) in commits.iter_mut().zip(parents) {
            c.parents = ps.iter().map(|&b| oid(b)).collect();
        }
        let index: HashMap<Oid, usize> = commits.iter().enumerate().map(|(i, c)| (c.id.clone(), i)).collect();
        let head = oid(5);
        for id in [5, 4, 3, 2, 1] {
            assert!(is_head_or_ancestor(&commits, &index, &head, &oid(id)), "{id}");
        }
        assert!(!is_head_or_ancestor(&commits, &index, &head, &oid(6)));
        assert!(!is_head_or_ancestor(&commits, &index, &head, &oid(9)));
    }
}
