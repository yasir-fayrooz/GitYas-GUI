use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Provider, RepoId};

/// A repository registered in GitYas (row of `repositories` in the user DB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub id: RepoId,
    pub name: String,
    /// Working-directory root (or the git dir for bare repositories).
    pub path: PathBuf,
    /// Detected from the remote URL at add/clone time; user-overridable. `None` = unknown / local only.
    pub provider: Option<Provider>,
    pub sort_order: i64,
}

/// Open tabs and the active repository, persisted in the user DB so they survive a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTabs {
    /// Ordered as shown in the tab bar.
    pub open: Vec<RepoId>,
    pub active: Option<RepoId>,
}

impl Repository {
    /// Default display name for a repository at `path`: the directory name, without a `.git` suffix for bare
    /// repositories, and the parent's name when `path` is a `.git` directory itself. `None` if no usable name
    /// (e.g. `/`) or the name is not valid UTF-8 (the UI then asks for a name instead of showing a mangled one).
    pub fn default_name(path: &Path) -> Option<String> {
        let mut components = path.components().rev().filter_map(|c| match c {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        });
        let last = components.next()?;
        let name = if last == ".git" {
            components.next()?
        } else {
            last.strip_suffix(".git").filter(|n| !n.is_empty()).unwrap_or(last)
        };
        Some(name.to_owned())
    }
}

impl OpenTabs {
    pub fn is_open(&self, id: &str) -> bool {
        self.open.iter().any(|open| open == id)
    }

    /// Opens `id` (appended if not already open) and makes it active.
    pub fn open(&mut self, id: RepoId) {
        if !self.is_open(&id) {
            self.open.push(id.clone());
        }
        self.active = Some(id);
    }

    /// Closes `id`. If it was active, the tab that takes its place (the next one, else the previous one) becomes
    /// active; `None` when no tabs remain. Returns whether a tab was closed.
    pub fn close(&mut self, id: &str) -> bool {
        let Some(pos) = self.open.iter().position(|open| open == id) else {
            return false;
        };
        self.open.remove(pos);
        if self.active.as_deref() == Some(id) {
            self.active = self.open.get(pos).or_else(|| self.open.last()).cloned();
        }
        true
    }

    /// Moves the tab at `from` to index `to` (clamped to the end). Returns false if `from` is out of range.
    pub fn move_tab(&mut self, from: usize, to: usize) -> bool {
        if from >= self.open.len() {
            return false;
        }
        let id = self.open.remove(from);
        let to = to.min(self.open.len());
        self.open.insert(to, id);
        true
    }

    /// Repairs a loaded value: drops ids not in `known` (deleted repositories) and duplicates, and makes sure
    /// `active` is one of the open tabs (falling back to the first). Returns whether anything changed.
    pub fn normalize<S: AsRef<str>>(&mut self, known: &[S]) -> bool {
        let before = self.clone();
        let mut seen = std::collections::HashSet::new();
        self.open
            .retain(|id| known.iter().any(|k| k.as_ref() == id.as_str()) && seen.insert(id.clone()));
        if !self.active.as_ref().is_some_and(|a| self.is_open(a)) {
            self.active = self.open.first().cloned();
        }
        *self != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::round_trip;

    fn tabs(open: &[&str], active: Option<&str>) -> OpenTabs {
        OpenTabs {
            open: open.iter().map(|s| s.to_string()).collect(),
            active: active.map(Into::into),
        }
    }

    #[test]
    fn default_names() {
        let name = |p: &str| Repository::default_name(Path::new(p));
        assert_eq!(name("/home/me/code/gityas").as_deref(), Some("gityas"));
        assert_eq!(name("/home/me/code/gityas/").as_deref(), Some("gityas"));
        assert_eq!(name("/srv/git/project.git").as_deref(), Some("project"));
        assert_eq!(name("/home/me/code/gityas/.git").as_deref(), Some("gityas"));
        assert_eq!(name("relative/dir").as_deref(), Some("dir"));
        assert_eq!(name("/srv/.git.git").as_deref(), Some(".git"));
        assert_eq!(name("/"), None);
        assert_eq!(name(""), None);
        assert_eq!(name(".git"), None);
    }

    #[test]
    fn open_and_close_tabs() {
        let mut t = OpenTabs::default();
        t.open("a".into());
        t.open("b".into());
        t.open("c".into());
        t.open("a".into());
        assert_eq!(t, tabs(&["a", "b", "c"], Some("a")));
        t.active = Some("b".into());
        assert!(t.close("b"));
        assert_eq!(t, tabs(&["a", "c"], Some("c")));
        assert!(t.close("c"));
        assert_eq!(t, tabs(&["a"], Some("a")));
        assert!(!t.close("zzz"));
        assert!(t.close("a"));
        assert_eq!(t, OpenTabs::default());
        // Closing an inactive tab leaves the active one alone.
        let mut t = tabs(&["a", "b"], Some("a"));
        t.close("b");
        assert_eq!(t, tabs(&["a"], Some("a")));
    }

    #[test]
    fn move_tabs() {
        let mut t = tabs(&["a", "b", "c"], None);
        assert!(t.move_tab(0, 2));
        assert_eq!(t.open, ["b", "c", "a"]);
        assert!(t.move_tab(2, 99));
        assert_eq!(t.open, ["b", "c", "a"]);
        assert!(t.move_tab(1, 0));
        assert_eq!(t.open, ["c", "b", "a"]);
        assert!(!t.move_tab(3, 0));
    }

    #[test]
    fn normalize_repairs_loaded_tabs() {
        let mut t = tabs(&["a", "gone", "b", "a"], Some("gone"));
        assert!(t.normalize(&["a", "b"]));
        assert_eq!(t, tabs(&["a", "b"], Some("a")));
        assert!(!t.normalize(&["a", "b"]));
        let mut t = tabs(&["x"], Some("x"));
        let none: [&str; 0] = [];
        assert!(t.normalize(&none));
        assert_eq!(t, OpenTabs::default());
    }

    #[test]
    fn serde_round_trip() {
        for provider in [None, Some(Provider::Github), Some(Provider::AzureDevops)] {
            round_trip(&Repository {
                id: "V1StGXR8_Z5jdHi6B-myT".into(),
                name: "GitYas".into(),
                path: PathBuf::from("/Users/me/Code/Git Yas"),
                provider,
                sort_order: -1,
            });
        }
        round_trip(&tabs(&["a", "b"], Some("b")));
        round_trip(&OpenTabs::default());
    }
}
