//! UI-free logic of the repositories module: tab bookkeeping, reorder indices, URL / folder validation, error
//! messages and path display. Unit-tested here; the screens only draw what these return.

use std::path::{Path, PathBuf};

use gityas_core::models::OpenTabs;
use gityas_core::{GitYasError, RepoId};

// ---- Tabs -------------------------------------------------------------------------------------------------------

/// What the session has to do after the tab bar changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterClose {
    /// An inactive tab was closed (or nothing changed): only persist the tabs.
    Nothing,
    /// The active tab was closed; its neighbour becomes active and must be opened.
    Open(RepoId),
    /// The last tab was closed: end the session.
    CloseSession,
}

/// Closes `id`'s tab. Closing the active tab activates its neighbour (the next one, else the previous one).
pub fn close_tab(tabs: &mut OpenTabs, id: &str) -> AfterClose {
    let was_active = tabs.active.as_deref() == Some(id);
    if !tabs.close(id) || !was_active {
        return AfterClose::Nothing;
    }
    match tabs.active.clone() {
        Some(next) => AfterClose::Open(next),
        None => AfterClose::CloseSession,
    }
}

/// Closes every tab but `keep` (which becomes active). Nothing happens if `keep` isn't open.
pub fn close_others(tabs: &mut OpenTabs, keep: &str) -> AfterClose {
    if !tabs.is_open(keep) {
        return AfterClose::Nothing;
    }
    tabs.open.retain(|id| id == keep);
    if tabs.active.as_deref() == Some(keep) {
        AfterClose::Nothing
    } else {
        tabs.active = Some(keep.to_owned());
        AfterClose::Open(keep.to_owned())
    }
}

/// Index `delta` steps from `current` in a ring of `len` tabs (Ctrl+Tab). `None` without tabs; without a current
/// tab, forward starts at the first and backward at the last.
pub fn relative_index(len: usize, current: Option<usize>, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let len_i = isize::try_from(len).ok()?;
    let start = match current {
        Some(i) => isize::try_from(i).ok()?,
        None if delta > 0 => -1,
        None => len_i,
    };
    usize::try_from((start + delta).rem_euclid(len_i)).ok()
}

/// Final index of an item dragged from `from` and dropped on `target` (before it, or after it when `after`), in the
/// list *after* removing the dragged item (what [`OpenTabs::move_tab`] / [`move_item`] expect).
pub fn drop_index(from: usize, target: usize, after: bool) -> usize {
    let insert = if after { target + 1 } else { target };
    if insert > from { insert - 1 } else { insert }
}

/// Moves `items[from]` to index `to` (after removal, clamped). Returns false if `from` is out of range.
pub fn move_item<T>(items: &mut Vec<T>, from: usize, to: usize) -> bool {
    if from >= items.len() {
        return false;
    }
    let item = items.remove(from);
    let to = to.min(items.len());
    items.insert(to, item);
    true
}

// ---- Add local --------------------------------------------------------------------------------------------------

/// Problem with the "Folder" field before asking the engine (`registered` = paths already in GitYas, canonical).
/// Only what the frontend can tell without git: "Not a git repository" comes from the engine.
pub fn local_folder_error(folder: &str, registered: &[PathBuf]) -> Option<String> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Some("Choose a folder".into());
    }
    let path = expand_home(folder);
    if !path.is_absolute() {
        return Some("Enter the full path of the folder".into());
    }
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Some("Not a folder".into()),
        Err(_) => return Some("Folder doesn't exist".into()),
    }
    let canonical = canonicalize(&path).unwrap_or(path);
    registered.contains(&canonical).then(|| "Already added".into())
}

/// `fs::canonicalize` without the `\\?\` prefix Windows adds: the engine registers (and so `registered` holds) the plain
/// `C:\...` form, which a verbatim path would never equal.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let full = std::fs::canonicalize(path)?;
    Ok(match full.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => full,
    })
}

/// Field message for an `add_repository` error (`None`: not a field error; show it above the buttons).
pub fn add_error_message(err: &GitYasError) -> Option<String> {
    match err {
        GitYasError::AlreadyExists(_) => Some("Already added".into()),
        GitYasError::NotFound(_) => Some("Not a git repository".into()),
        GitYasError::InvalidInput(message) => Some(message.clone()),
        _ => None,
    }
}

/// Error text for the line above the dialog buttons: the engine's message without the "Invalid input:" style
/// prefix where the message already explains itself.
pub fn error_text(err: &GitYasError) -> String {
    match err {
        GitYasError::InvalidInput(message) | GitYasError::Other(message) => capitalize(message),
        other => other.to_string(),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ---- Clone ------------------------------------------------------------------------------------------------------

/// What kind of clone source the user typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlKind {
    Https,
    /// `ssh://`, `git+ssh://`, scp-like `git@host:owner/repo`; signs in with ssh-agent or `~/.ssh` keys.
    Ssh,
    /// `file://` or an absolute local path (e.g. a bare repository on disk); no credentials involved.
    Local,
    /// `http://`, `git://`, relative paths, garbage.
    Other,
}

pub fn classify_url(url: &str) -> UrlKind {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if let Some((scheme, _)) = lower.split_once("://") {
        return match scheme {
            "https" => UrlKind::Https,
            "ssh" | "git+ssh" | "ssh+git" => UrlKind::Ssh,
            "file" => UrlKind::Local,
            _ => UrlKind::Other,
        };
    }
    if is_absolute_local(url) {
        return UrlKind::Local;
    }
    // scp-like `user@host:path` / `host:path` (a `/` before the `:` makes it a path, a one-letter host a drive).
    match url.split_once(':') {
        Some((host, _)) if !host.contains('/') && !host.contains('\\') && host.len() > 1 => UrlKind::Ssh,
        _ => UrlKind::Other,
    }
}

fn is_absolute_local(url: &str) -> bool {
    let bytes = url.as_bytes();
    let windows_drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
    url.starts_with('/') || url.starts_with("\\\\") || windows_drive
}

/// Problem with the URL field; `None` when it may be cloned. Empty input is not an error message (the button is
/// disabled with "Enter an https URL" as its reason).
pub fn url_error(url: &str) -> Option<&'static str> {
    if url.trim().is_empty() {
        return None;
    }
    match classify_url(url) {
        UrlKind::Https | UrlKind::Ssh | UrlKind::Local => None,
        UrlKind::Other => Some("Enter an https URL"),
    }
}

/// Problem with the optional "Name" override (the folder created inside "Into").
pub fn name_error(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if name.contains(['/', '\\', ':']) {
        return Some("A name can't contain / \\ or :");
    }
    if name == "." || name == ".." {
        return Some("Enter a folder name");
    }
    None
}

/// Problem with the "Into" field.
pub fn into_error(into: &str) -> Option<&'static str> {
    let into = into.trim();
    if into.is_empty() {
        return Some("Choose a folder");
    }
    if !expand_home(into).is_absolute() {
        return Some("Enter the full path of the folder");
    }
    None
}

/// Problem with the final clone folder (the engine refuses these too; telling the user before is nicer).
pub fn destination_error(dest: &Path) -> Option<String> {
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() => match std::fs::read_dir(dest) {
            Ok(mut entries) => entries.next().is_some().then(|| "Folder already exists and isn't empty".into()),
            Err(err) => Some(format!("Cannot read the folder: {err}")),
        },
        Ok(_) => Some("A file with that name already exists".into()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => Some(format!("Cannot check the folder: {err}")),
    }
}

/// `owner/repo` for the success toast: the last two path segments of the URL without `.git` (one segment if that is
/// all there is).
pub fn short_repo_name(url: &str) -> String {
    let trimmed = url.trim().split(['?', '#']).next().unwrap_or_default();
    let path = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    let path = match path.split_once(':') {
        Some((host, rest)) if !host.contains('/') && host.len() > 1 => rest,
        _ => path,
    };
    let mut segments: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".git").collect();
    if trimmed.contains("://") && !trimmed.to_ascii_lowercase().starts_with("file://") && !segments.is_empty() {
        segments.remove(0); // host
    }
    let tail: Vec<String> = segments
        .iter()
        .rev()
        .take(2)
        .rev()
        .map(|s| s.strip_suffix(".git").unwrap_or(s).to_owned())
        .collect();
    if tail.is_empty() { url.trim().to_owned() } else { tail.join("/") }
}

// ---- Paths ------------------------------------------------------------------------------------------------------

/// The user's home folder (`HOME`, or `USERPROFILE` on Windows).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// `~/…` → the home folder; anything else unchanged.
pub fn expand_home(text: &str) -> PathBuf {
    let text = text.trim();
    match (text.strip_prefix('~'), home_dir()) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) if rest.starts_with(['/', '\\']) => home.join(&rest[1..]),
        _ => PathBuf::from(text),
    }
}

/// Path for display: the home folder abbreviated to `~`. Non-UTF-8 parts are shown lossily.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(rest) = home.and_then(|home| path.strip_prefix(home).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        let sep = std::path::MAIN_SEPARATOR;
        return format!("~{sep}{}", rest.display());
    }
    path.display().to_string()
}

/// Keeps the end of `text` ("…/src/project"), dropping characters from the left until `measure` fits `max_width`.
/// `measure` returns the rendered width of a string (galley width in the UI, char count in tests).
pub fn truncate_left(text: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(text) <= max_width {
        return text.to_owned();
    }
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).skip(1).collect();
    // Widths shrink as the start moves right: binary search the first start that fits.
    let (mut lo, mut hi) = (0, starts.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        if measure(&format!("…{}", &text[starts[mid]..])) <= max_width {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    match starts.get(lo) {
        Some(&start) => format!("…{}", &text[start..]),
        None => "…".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(open: &[&str], active: Option<&str>) -> OpenTabs {
        OpenTabs {
            open: open.iter().map(|s| s.to_string()).collect(),
            active: active.map(Into::into),
        }
    }

    #[test]
    fn closing_tabs_picks_the_neighbour() {
        let mut t = tabs(&["a", "b", "c"], Some("b"));
        assert_eq!(close_tab(&mut t, "a"), AfterClose::Nothing);
        assert_eq!(t, tabs(&["b", "c"], Some("b")));
        assert_eq!(close_tab(&mut t, "b"), AfterClose::Open("c".into()));
        assert_eq!(t, tabs(&["c"], Some("c")));
        assert_eq!(close_tab(&mut t, "zzz"), AfterClose::Nothing);
        assert_eq!(close_tab(&mut t, "c"), AfterClose::CloseSession);
        assert_eq!(t, OpenTabs::default());

        let mut t = tabs(&["a", "b", "c"], Some("c"));
        assert_eq!(close_tab(&mut t, "c"), AfterClose::Open("b".into()), "last tab: previous one");
    }

    #[test]
    fn close_others_keeps_one_and_activates_it() {
        let mut t = tabs(&["a", "b", "c"], Some("a"));
        assert_eq!(close_others(&mut t, "b"), AfterClose::Open("b".into()));
        assert_eq!(t, tabs(&["b"], Some("b")));
        assert_eq!(close_others(&mut t, "b"), AfterClose::Nothing);
        assert_eq!(close_others(&mut t, "x"), AfterClose::Nothing);
        assert_eq!(t, tabs(&["b"], Some("b")));
    }

    #[test]
    fn relative_index_wraps() {
        assert_eq!(relative_index(0, None, 1), None);
        assert_eq!(relative_index(3, Some(2), 1), Some(0));
        assert_eq!(relative_index(3, Some(0), -1), Some(2));
        assert_eq!(relative_index(3, None, 1), Some(0));
        assert_eq!(relative_index(3, None, -1), Some(2));
    }

    #[test]
    fn drop_indices_account_for_the_removed_item() {
        // [a b c d]: drag a after c → [b c a d]
        let mut v = vec!['a', 'b', 'c', 'd'];
        assert!(move_item(&mut v, 0, drop_index(0, 2, true)));
        assert_eq!(v, ['b', 'c', 'a', 'd']);
        // drag d before b → [b d c a]
        assert!(move_item(&mut v, 3, drop_index(3, 0, true)));
        assert_eq!(v, ['b', 'd', 'c', 'a']);
        // onto itself: unchanged
        assert_eq!(drop_index(1, 1, false), 1);
        assert_eq!(drop_index(1, 1, true), 1);
        assert!(!move_item(&mut v, 9, 0));
        // OpenTabs::move_tab uses the same convention.
        let mut t = tabs(&["a", "b", "c"], Some("a"));
        assert!(t.move_tab(0, drop_index(0, 2, true)));
        assert_eq!(t.open, ["b", "c", "a"]);
    }

    #[test]
    fn local_folder_validation() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().to_string_lossy().into_owned();
        assert_eq!(local_folder_error("", &[]).as_deref(), Some("Choose a folder"));
        assert_eq!(
            local_folder_error("relative/x", &[]).as_deref(),
            Some("Enter the full path of the folder")
        );
        let missing = dir.path().join("missing").to_string_lossy().into_owned();
        assert_eq!(local_folder_error(&missing, &[]).as_deref(), Some("Folder doesn't exist"));
        std::fs::write(dir.path().join("file"), "x").unwrap_or_else(|e| panic!("{e}"));
        let file = dir.path().join("file").to_string_lossy().into_owned();
        assert_eq!(local_folder_error(&file, &[]).as_deref(), Some("Not a folder"));
        assert_eq!(local_folder_error(&path, &[]), None);
        let canonical = canonicalize(dir.path()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(local_folder_error(&path, &[canonical]).as_deref(), Some("Already added"));
    }

    #[test]
    fn engine_errors_become_field_messages() {
        let msg = |e: GitYasError| add_error_message(&e);
        assert_eq!(msg(GitYasError::AlreadyExists("x".into())).as_deref(), Some("Already added"));
        assert_eq!(msg(GitYasError::NotFound("x".into())).as_deref(), Some("Not a git repository"));
        assert_eq!(msg(GitYasError::InvalidInput("bad".into())).as_deref(), Some("bad"));
        assert_eq!(msg(GitYasError::Storage("disk".into())), None);
        assert_eq!(error_text(&GitYasError::InvalidInput("folder exists".into())), "Folder exists");
        assert_eq!(
            error_text(&GitYasError::Auth("bad token".into())),
            "Authentication failed: bad token"
        );
    }

    #[test]
    fn url_classification() {
        assert_eq!(classify_url("https://github.com/o/r.git"), UrlKind::Https);
        assert_eq!(classify_url(" HTTPS://x/y "), UrlKind::Https);
        assert_eq!(classify_url("git@github.com:o/r.git"), UrlKind::Ssh);
        assert_eq!(classify_url("ssh://git@host/o/r"), UrlKind::Ssh);
        assert_eq!(classify_url("git+ssh://host/o/r"), UrlKind::Ssh);
        assert_eq!(classify_url("file:///srv/r.git"), UrlKind::Local);
        assert_eq!(classify_url("/srv/r.git"), UrlKind::Local);
        assert_eq!(classify_url("C:\\repos\\r.git"), UrlKind::Local);
        assert_eq!(classify_url("http://x/y"), UrlKind::Other);
        assert_eq!(classify_url("git://x/y"), UrlKind::Other);
        assert_eq!(classify_url("relative/path"), UrlKind::Other);

        assert_eq!(url_error(""), None);
        assert_eq!(url_error("https://github.com/o/r"), None);
        assert_eq!(url_error("/srv/r.git"), None);
        assert_eq!(url_error("git@github.com:o/r.git"), None);
        assert_eq!(url_error("http://github.com/o/r"), Some("Enter an https URL"));
        assert_eq!(url_error("github.com/o/r"), Some("Enter an https URL"));
    }

    #[test]
    fn clone_fields() {
        assert_eq!(name_error(""), None);
        assert_eq!(name_error("repo"), None);
        assert!(name_error("a/b").is_some() && name_error("a\\b").is_some() && name_error("..").is_some());
        assert_eq!(into_error(""), Some("Choose a folder"));
        assert_eq!(into_error("rel"), Some("Enter the full path of the folder"));
        // `/abs` is not absolute on Windows (no drive letter).
        let abs = if cfg!(windows) { "C:\\abs" } else { "/abs" };
        assert_eq!(into_error(abs), None);

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(destination_error(&dir.path().join("new")), None);
        assert_eq!(destination_error(dir.path()), None, "an empty folder is fine");
        std::fs::write(dir.path().join("f"), "x").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            destination_error(dir.path()).as_deref(),
            Some("Folder already exists and isn't empty")
        );
        assert_eq!(
            destination_error(&dir.path().join("f")).as_deref(),
            Some("A file with that name already exists")
        );
    }

    #[test]
    fn short_names_for_the_toast() {
        assert_eq!(short_repo_name("https://github.com/owner/repo.git"), "owner/repo");
        assert_eq!(short_repo_name("https://github.com/owner/repo/"), "owner/repo");
        assert_eq!(short_repo_name("https://dev.azure.com/org/proj/_git/repo"), "_git/repo");
        assert_eq!(short_repo_name("https://host/repo"), "repo");
        assert_eq!(short_repo_name("git@github.com:owner/repo.git"), "owner/repo");
        assert_eq!(short_repo_name("/srv/git/remote.git"), "git/remote");
        assert_eq!(short_repo_name("file:///srv/git/remote.git"), "git/remote");
    }

    #[test]
    fn paths_are_abbreviated_and_truncated_from_the_left() {
        let home = Path::new("/Users/me");
        assert_eq!(display_path(Path::new("/Users/me"), Some(home)), "~");
        let sep = std::path::MAIN_SEPARATOR;
        // Built with `join`: `strip_prefix` keeps the input's separators, so a literal with `/` differs on Windows.
        let nested = home.join("src").join("x");
        assert_eq!(display_path(&nested, Some(home)), format!("~{sep}src{sep}x"));
        assert_eq!(display_path(Path::new("/Users/meow/x"), Some(home)), "/Users/meow/x");
        assert_eq!(display_path(Path::new("/srv/x"), None), "/srv/x");

        let chars = |s: &str| s.chars().count() as f32;
        assert_eq!(truncate_left("~/src/project", 20.0, chars), "~/src/project");
        assert_eq!(truncate_left("~/src/project", 8.0, chars), "…project");
        assert_eq!(truncate_left("~/src/prøject", 8.0, chars), "…prøject");
        assert_eq!(truncate_left("abc", 0.0, chars), "…");
    }

    #[test]
    fn home_expansion() {
        if let Some(home) = home_dir() {
            assert_eq!(expand_home("~"), home);
            assert_eq!(expand_home("~/src"), home.join("src"));
        }
        assert_eq!(expand_home("/srv"), PathBuf::from("/srv"));
        assert_eq!(expand_home("~other"), PathBuf::from("~other"));
    }
}
