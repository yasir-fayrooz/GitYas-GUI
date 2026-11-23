use serde::{Deserialize, Serialize};

use crate::GitYasError;

/// Author/committer identity used for commits, merges and annotated tags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSignature {
    pub name: String,
    pub email: String,
}

impl CommitSignature {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }

    /// libgit2 rejects empty names; callers must check this instead of panicking.
    /// Equivalent to `self.validate().is_ok()`.
    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    /// Checks what libgit2's `git_signature_new` would reject (empty name, `<`/`>`), plus what would corrupt the
    /// commit header (newlines) and an empty email (GitYas requires one). Error messages are user-facing.
    pub fn validate(&self) -> Result<(), GitYasError> {
        let invalid = |msg: &str| Err(GitYasError::InvalidInput(msg.to_owned()));
        if self.name.trim().is_empty() {
            return invalid("Commit signature name is empty");
        }
        if self.email.trim().is_empty() {
            return invalid("Commit signature email is empty");
        }
        let fields = [self.name.as_str(), self.email.as_str()];
        if fields.iter().any(|f| f.contains(['<', '>'])) {
            return invalid("Commit signature name and email must not contain '<' or '>'");
        }
        if fields.iter().any(|f| f.contains(['\n', '\r', '\0'])) {
            return invalid("Commit signature name and email must be a single line");
        }
        Ok(())
    }

    /// Copy with surrounding whitespace removed (what should be stored after the user edits it).
    pub fn trimmed(&self) -> Self {
        Self::new(self.name.trim(), self.email.trim())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// Diff viewer mode. Default is `Hunk`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffViewMode {
    #[default]
    Hunk,
    Inline,
    Split,
}

/// Default of [`GraphOptions::max_commits`].
pub const DEFAULT_MAX_COMMITS: usize = 2000;

/// Commit-graph display options. Extended by the frontend WPs; unknown fields default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphOptions {
    pub show_remote_branches: bool,
    pub show_tags: bool,
    pub show_stashes: bool,
    /// Most commits the history lists (the newest ones, in display order); the graph stops there.
    pub max_commits: usize,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            show_remote_branches: true,
            show_tags: true,
            show_stashes: true,
            max_commits: DEFAULT_MAX_COMMITS,
        }
    }
}

/// Git's `core.autocrlf` (line-ending conversion), set in the user's global git config (Windows only in the UI).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoCrlf {
    /// LF in the repository, CRLF in the working tree.
    True,
    /// No conversion (git's default when unset).
    #[default]
    False,
    /// CRLF → LF on commit, no conversion on checkout.
    Input,
}

impl AutoCrlf {
    pub const ALL: [AutoCrlf; 3] = [AutoCrlf::True, AutoCrlf::False, AutoCrlf::Input];

    /// The config value (`true` / `false` / `input`).
    pub fn as_str(self) -> &'static str {
        match self {
            AutoCrlf::True => "true",
            AutoCrlf::False => "false",
            AutoCrlf::Input => "input",
        }
    }

    /// Parses a config value the way git does: `input`, or a git boolean (anything unrecognised is `false`).
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "input" => AutoCrlf::Input,
            "true" | "yes" | "on" | "1" => AutoCrlf::True,
            _ => AutoCrlf::False,
        }
    }
}

/// Where the icon rail (page icons and git actions) is pinned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RailPosition {
    #[default]
    Left,
    Top,
    Right,
    Bottom,
}

impl RailPosition {
    pub const ALL: [RailPosition; 4] = [RailPosition::Left, RailPosition::Top, RailPosition::Right, RailPosition::Bottom];
}

/// Shortest background fetch interval, in seconds.
pub const MIN_FETCH_INTERVAL_SECS: u64 = 15;
/// Default of [`Settings::fetch_interval_secs`].
pub const DEFAULT_FETCH_INTERVAL_SECS: u64 = 60;

/// Speed mode's shortcuts (frontend `ui::speed`). Each is text such as `"Alt+Shift+G"` or `"P"`: modifiers (`Ctrl`,
/// `Alt`, `Shift`, `Cmd` = ⌘ on macOS) joined by `+`, then one key name. The frontend parses and validates them; an
/// empty string means unbound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpeedSettings {
    /// Whether [`SpeedSettings::summon`] is registered system-wide (it then works while other apps are focused).
    pub global_shortcut: bool,
    /// Opens speed mode and focuses it from anywhere; pressed again while the panel has the focus, collapses it.
    pub summon: String,
    pub changes: String,
    pub commit: String,
    pub push: String,
    pub pull: String,
    pub fetch: String,
    pub branches: String,
    pub new_branch: String,
    pub stash: String,
    pub pop: String,
    pub tag: String,
    pub exit: String,
}

impl Default for SpeedSettings {
    fn default() -> Self {
        Self {
            global_shortcut: true,
            summon: "Alt+Shift+G".into(),
            changes: "C".into(),
            commit: "M".into(),
            push: "P".into(),
            pull: "L".into(),
            fetch: "F".into(),
            branches: "B".into(),
            new_branch: "N".into(),
            stash: "S".into(),
            pop: "O".into(),
            tag: "T".into(),
            exit: "X".into(),
        }
    }
}

/// User settings (each field is one JSON value in the user DB `settings` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `None` until the user sets it; commit/merge must fail with a clear error rather than use an empty signature.
    pub commit_signature: Option<CommitSignature>,
    pub theme: Theme,
    pub diff_view_mode: DiffViewMode,
    pub graph: GraphOptions,
    /// Newest app version whose release notes the user has seen (the frontend's What's new dialog). `None` until
    /// the first start of a build that records it.
    pub last_seen_version: Option<String>,
    pub rail_position: RailPosition,
    /// Background fetch of the active repository, every this many seconds (see [`Settings::fetch_interval`]).
    pub fetch_interval_secs: u64,
    pub speed: SpeedSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            commit_signature: None,
            theme: Theme::default(),
            diff_view_mode: DiffViewMode::default(),
            graph: GraphOptions::default(),
            last_seen_version: None,
            rail_position: RailPosition::default(),
            fetch_interval_secs: DEFAULT_FETCH_INTERVAL_SECS,
            speed: SpeedSettings::default(),
        }
    }
}

impl Settings {
    /// The background fetch interval, never shorter than [`MIN_FETCH_INTERVAL_SECS`] (a hand-edited or older value
    /// below it is raised).
    pub fn fetch_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.fetch_interval_secs.max(MIN_FETCH_INTERVAL_SECS))
    }

    /// The signature to commit/merge/tag with, or a user-facing error if it is unset or invalid.
    pub fn require_signature(&self) -> Result<&CommitSignature, GitYasError> {
        let signature = self
            .commit_signature
            .as_ref()
            .ok_or_else(|| GitYasError::SignatureRequired("Set your commit signature (name and email) first".into()))?;
        signature.validate().map_err(|err| match err {
            GitYasError::InvalidInput(message) => GitYasError::SignatureRequired(message),
            other => other,
        })?;
        Ok(signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::round_trip;

    #[test]
    fn signature_validation() {
        assert!(CommitSignature::new("Ada Lovelace", "ada@example.com").is_valid());
        for (name, email) in [
            ("", "a@b"),
            ("   ", "a@b"),
            ("Ada", ""),
            ("Ada <x>", "a@b"),
            ("Ada", "<a@b>"),
            ("Ada\nEvil", "a@b"),
            ("Ada", "a@b\r"),
        ] {
            let sig = CommitSignature::new(name, email);
            assert!(!sig.is_valid(), "{sig:?} should be invalid");
            assert!(matches!(sig.validate(), Err(GitYasError::InvalidInput(_))));
        }
        assert_eq!(
            CommitSignature::new("  Ada ", " a@b\t").trimmed(),
            CommitSignature::new("Ada", "a@b")
        );
    }

    #[test]
    fn require_signature() {
        let mut settings = Settings::default();
        assert!(settings.require_signature().is_err());
        settings.commit_signature = Some(CommitSignature::new("", ""));
        assert!(settings.require_signature().is_err());
        settings.commit_signature = Some(CommitSignature::new("Ada", "ada@example.com"));
        assert_eq!(settings.require_signature().map(|s| s.name.as_str()), Ok("Ada"));
    }

    #[test]
    fn serde_round_trip() {
        round_trip(&Settings::default());
        round_trip(&Settings {
            commit_signature: Some(CommitSignature::new("Ada", "ada@example.com")),
            theme: Theme::Dark,
            diff_view_mode: DiffViewMode::Split,
            graph: GraphOptions {
                show_remote_branches: false,
                show_tags: true,
                show_stashes: false,
                max_commits: 500,
            },
            last_seen_version: Some("1.2.0".into()),
            rail_position: RailPosition::Bottom,
            fetch_interval_secs: 300,
            speed: SpeedSettings {
                global_shortcut: false,
                summon: "Ctrl+Alt+Up".into(),
                ..SpeedSettings::default()
            },
        });
        for position in RailPosition::ALL {
            round_trip(&position);
        }
        for theme in [Theme::System, Theme::Light, Theme::Dark] {
            round_trip(&theme);
        }
        for mode in [DiffViewMode::Hunk, DiffViewMode::Inline, DiffViewMode::Split] {
            round_trip(&mode);
        }
        assert_eq!(round_trip(&DiffViewMode::Split), r#""split""#);
        for value in AutoCrlf::ALL {
            assert_eq!(AutoCrlf::parse(value.as_str()), value);
        }
        assert_eq!(AutoCrlf::parse(" Yes"), AutoCrlf::True);
        assert_eq!(AutoCrlf::parse("nonsense"), AutoCrlf::False);
    }

    /// `#[serde(default)]`: settings written by an older build (missing keys) still load.
    #[test]
    fn missing_fields_default() -> Result<(), serde_json::Error> {
        let settings: Settings = serde_json::from_str(r#"{"theme":"light"}"#)?;
        assert_eq!(settings.theme, Theme::Light);
        assert_eq!(settings.commit_signature, None);
        assert_eq!(settings.graph, GraphOptions::default());
        let graph: GraphOptions = serde_json::from_str(r#"{"show_tags":false}"#)?;
        assert!(graph.show_remote_branches && !graph.show_tags && graph.show_stashes);
        assert_eq!(graph.max_commits, DEFAULT_MAX_COMMITS);
        assert_eq!(serde_json::from_str::<Settings>("{}")?, Settings::default());
        assert_eq!(settings.fetch_interval_secs, DEFAULT_FETCH_INTERVAL_SECS);
        assert_eq!(settings.rail_position, RailPosition::Left);
        assert_eq!(settings.speed, SpeedSettings::default());
        let speed: SpeedSettings = serde_json::from_str(r#"{"push":"Shift+P"}"#)?;
        assert_eq!((speed.push.as_str(), speed.pull.as_str()), ("Shift+P", "L"));
        Ok(())
    }

    #[test]
    fn fetch_interval_has_a_minimum() {
        let mut settings = Settings::default();
        assert_eq!(settings.fetch_interval().as_secs(), DEFAULT_FETCH_INTERVAL_SECS);
        settings.fetch_interval_secs = 1;
        assert_eq!(settings.fetch_interval().as_secs(), MIN_FETCH_INTERVAL_SECS);
    }
}
