//! OS integration: data/cache/log directories, tracing initialisation, opening URLs (WP7), single instance
//! ([`claim_instance`]). No deep links, no argv handling (D7).
//!
//! Directory policy:
//! * defaults come from `directories::ProjectDirs::from("com", "gityas", "GitYas")`;
//! * logs go to `~/Library/Logs/<project>` on macOS, `$XDG_STATE_HOME/gityas/logs` on Linux (falling back to the local
//!   data dir), and `<LocalAppData>\gityas\GitYas\data\logs` on Windows;
//! * `GITYAS_DATA_DIR` / `GITYAS_CACHE_DIR` / `GITYAS_LOG_DIR` override each directory independently. An empty value
//!   counts as unset; a relative value is rejected with [`GitYasError::InvalidInput`] (a GUI app's working directory is
//!   arbitrary, so a relative path would silently point somewhere different on every launch).

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use gityas_core::{GitYasError, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod instance;

pub use instance::{Instance, InstanceLock, claim_instance};

/// Overrides for tests and development.
pub const DATA_DIR_ENV: &str = "GITYAS_DATA_DIR";
pub const CACHE_DIR_ENV: &str = "GITYAS_CACHE_DIR";
pub const LOG_DIR_ENV: &str = "GITYAS_LOG_DIR";

/// Log file name parts: files are named `gityas.YYYY-MM-DD.log`.
pub const LOG_FILE_PREFIX: &str = "gityas";
pub const LOG_FILE_SUFFIX: &str = "log";
/// Daily log files kept on disk; older ones are deleted on rotation.
pub const MAX_LOG_FILES: usize = 14;

/// Default filter when `RUST_LOG` is unset, empty or invalid.
#[cfg(debug_assertions)]
pub const DEFAULT_LOG_FILTER: &str = "info,gityas=debug";
#[cfg(not(debug_assertions))]
pub const DEFAULT_LOG_FILTER: &str = "info";

/// Longest URL [`open_url`] accepts.
const MAX_URL_LEN: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Permanent user data (`gityas.db`).
    pub data_dir: PathBuf,
    /// Disposable caches (`cache.db`); the OS may reclaim it.
    pub cache_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl Paths {
    /// Resolves directories from `directories::ProjectDirs::from("com", "gityas", "GitYas")`, applying the env
    /// overrides, and creates them.
    pub fn resolve() -> Result<Paths> {
        Self::resolve_with(|key| std::env::var_os(key))
    }

    /// Like [`Paths::resolve`], but reads the overrides through `env` instead of the process environment (tests).
    pub fn resolve_with(env: impl Fn(&str) -> Option<OsString>) -> Result<Paths> {
        Self::resolve_from(env, platform_defaults)
    }

    /// Core of [`Paths::resolve`]: `defaults` is only consulted when at least one directory is not overridden.
    fn resolve_from(env: impl Fn(&str) -> Option<OsString>, defaults: impl FnOnce() -> Result<Paths>) -> Result<Paths> {
        let data = env_override(&env, DATA_DIR_ENV)?;
        let cache = env_override(&env, CACHE_DIR_ENV)?;
        let log = env_override(&env, LOG_DIR_ENV)?;

        let paths = match (data, cache, log) {
            (Some(data_dir), Some(cache_dir), Some(log_dir)) => Paths {
                data_dir,
                cache_dir,
                log_dir,
            },
            (data, cache, log) => {
                let default = defaults()?;
                Paths {
                    data_dir: data.unwrap_or(default.data_dir),
                    cache_dir: cache.unwrap_or(default.cache_dir),
                    log_dir: log.unwrap_or(default.log_dir),
                }
            }
        };
        paths.create_dirs()?;
        Ok(paths)
    }

    /// All three directories under one root (tests).
    pub fn under(root: impl Into<PathBuf>) -> Paths {
        let root = root.into();
        Paths {
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            log_dir: root.join("logs"),
        }
    }

    /// Creates all three directories (and their parents) if missing.
    pub fn create_dirs(&self) -> Result<()> {
        for (kind, dir) in [("data", &self.data_dir), ("cache", &self.cache_dir), ("log", &self.log_dir)] {
            std::fs::create_dir_all(dir)
                .map_err(|err| GitYasError::Io(format!("cannot create the {kind} directory {}: {err}", dir.display())))?;
        }
        Ok(())
    }
}

/// Reads one override: unset or empty means "no override"; a relative path is an error.
fn env_override(env: &impl Fn(&str) -> Option<OsString>, key: &str) -> Result<Option<PathBuf>> {
    let Some(value) = env(key).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(GitYasError::InvalidInput(format!(
            "{key} must be an absolute path, got {}",
            path.display()
        )));
    }
    Ok(Some(path))
}

/// The per-OS default directories (not created).
fn platform_defaults() -> Result<Paths> {
    let project = directories::ProjectDirs::from("com", "gityas", "GitYas").ok_or_else(|| {
        GitYasError::NotFound(format!(
            "cannot determine the user's home directory; set {DATA_DIR_ENV}, {CACHE_DIR_ENV} and {LOG_DIR_ENV}"
        ))
    })?;
    Ok(Paths {
        data_dir: project.data_dir().to_path_buf(),
        cache_dir: project.cache_dir().to_path_buf(),
        log_dir: default_log_dir(&project)?,
    })
}

#[cfg(target_os = "macos")]
fn default_log_dir(project: &directories::ProjectDirs) -> Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| GitYasError::NotFound(format!("cannot determine the user's home directory; set {LOG_DIR_ENV}")))?;
    Ok(base.home_dir().join("Library").join("Logs").join(project.project_path()))
}

#[cfg(not(target_os = "macos"))]
fn default_log_dir(project: &directories::ProjectDirs) -> Result<PathBuf> {
    // `state_dir` is `Some` on Linux only ($XDG_STATE_HOME/gityas); elsewhere use the non-roaming data dir.
    Ok(project.state_dir().unwrap_or_else(|| project.data_local_dir()).join("logs"))
}

/// Keeps the non-blocking log writer alive; drop it at shutdown to flush.
pub struct TracingGuard {
    _worker: WorkerGuard,
}

impl std::fmt::Debug for TracingGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracingGuard").finish_non_exhaustive()
    }
}

/// Initialises `tracing` with a rolling file appender in `paths.log_dir` (and stderr in debug builds).
///
/// The filter comes from `RUST_LOG`, falling back to [`DEFAULT_LOG_FILTER`] (with a warning if `RUST_LOG` is invalid).
/// Returns [`GitYasError::Other`] instead of panicking if a global subscriber is already installed.
pub fn init_tracing(paths: &Paths) -> Result<TracingGuard> {
    init_tracing_with(paths, std::env::var("RUST_LOG").ok())
}

fn init_tracing_with(paths: &Paths, rust_log: Option<String>) -> Result<TracingGuard> {
    let (filter, filter_warning) = build_filter(rust_log.as_deref())?;

    std::fs::create_dir_all(&paths.log_dir)
        .map_err(|err| GitYasError::Io(format!("cannot create the log directory {}: {err}", paths.log_dir.display())))?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix(LOG_FILE_SUFFIX)
        .max_log_files(MAX_LOG_FILES)
        .build(&paths.log_dir)
        .map_err(|err| GitYasError::Io(format!("cannot open a log file in {}: {err}", paths.log_dir.display())))?;
    let (writer, worker) = tracing_appender::non_blocking(appender);

    let file_layer = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(writer);
    let stderr_layer = cfg!(debug_assertions).then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(std::io::stderr().is_terminal())
            .with_writer(std::io::stderr)
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .map_err(|err| GitYasError::Other(format!("tracing is already initialised: {err}")))?;

    if let Some(warning) = filter_warning {
        tracing::warn!("{warning}");
    }
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        log_dir = %paths.log_dir.display(),
        "logging initialised"
    );
    Ok(TracingGuard { _worker: worker })
}

/// Parses `RUST_LOG`; on failure falls back to the default and returns a warning to log once tracing is up.
fn build_filter(rust_log: Option<&str>) -> Result<(EnvFilter, Option<String>)> {
    let default = || EnvFilter::try_new(DEFAULT_LOG_FILTER).map_err(|err| GitYasError::Other(format!("invalid default log filter: {err}")));
    match rust_log.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok((default()?, None)),
        Some(directives) => match EnvFilter::try_new(directives) {
            Ok(filter) => Ok((filter, None)),
            Err(err) => Ok((
                default()?,
                Some(format!(
                    "ignoring invalid RUST_LOG {directives:?} ({err}); using {DEFAULT_LOG_FILTER:?}"
                )),
            )),
        },
    }
}

/// Opens `url` in the default browser (used by the "Create token" buttons).
///
/// Only absolute `http`/`https` URLs with a host and without credentials, whitespace or control characters are
/// accepted; anything else is [`GitYasError::InvalidInput`] and nothing is launched.
pub fn open_url(url: &str) -> Result<()> {
    validate_url(url)?;
    open::that_detached(url).map_err(|err| GitYasError::Io(format!("cannot open {url} in the browser: {err}")))
}

/// Shows `path` in Finder / Explorer / the default file manager: a directory is opened, a file's parent directory
/// is opened (selecting the file itself needs per-OS tooling; not worth it yet). A missing path is
/// [`GitYasError::NotFound`] and nothing is launched.
pub fn reveal_in_file_manager(path: &Path) -> Result<()> {
    let dir = reveal_target(path)?;
    open::that_detached(&dir).map_err(|err| GitYasError::Io(format!("cannot open {} in the file manager: {err}", dir.display())))
}

fn reveal_target(path: &Path) -> Result<PathBuf> {
    let meta = std::fs::metadata(path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => GitYasError::NotFound(format!("{} does not exist", path.display())),
        _ => GitYasError::Io(format!("cannot read {}: {err}", path.display())),
    })?;
    if meta.is_dir() {
        return Ok(path.to_path_buf());
    }
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| GitYasError::InvalidInput(format!("{} has no parent directory", path.display())))
}

fn validate_url(url: &str) -> Result<()> {
    let invalid = |why: &str| GitYasError::InvalidInput(format!("cannot open URL {url:?}: {why}"));

    if url.len() > MAX_URL_LEN {
        return Err(invalid("too long"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid("contains whitespace or control characters"));
    }
    let (scheme, rest) = url.split_once("://").ok_or_else(|| invalid("not an absolute http(s) URL"))?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return Err(invalid("only http and https URLs are allowed"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(invalid("URLs with credentials are not allowed"));
    }
    let host = match authority.strip_prefix('[') {
        // IPv6 literal: `[::1]:8080`.
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    if host.is_empty() {
        return Err(invalid("missing host"));
    }
    Ok(())
}

/// Returns the files in `dir` whose names look like our log files (tests and diagnostics).
pub fn log_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(LOG_FILE_PREFIX) && name.ends_with(LOG_FILE_SUFFIX) && entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env_of(vars: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: HashMap<String, OsString> = vars.iter().map(|(k, v)| (k.to_string(), v.as_os_str().to_os_string())).collect();
        move |key| map.get(key).cloned()
    }

    fn no_defaults() -> Result<Paths> {
        Err(GitYasError::NotFound("no home directory".into()))
    }

    #[test]
    fn all_overrides_are_used_and_created_without_consulting_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("d/nested");
        let cache = tmp.path().join("c");
        let log = tmp.path().join("l");
        let env = env_of(&[(DATA_DIR_ENV, &data), (CACHE_DIR_ENV, &cache), (LOG_DIR_ENV, &log)]);

        let paths = Paths::resolve_from(env, no_defaults).unwrap();

        assert_eq!(
            paths,
            Paths {
                data_dir: data.clone(),
                cache_dir: cache.clone(),
                log_dir: log.clone()
            }
        );
        assert!(data.is_dir() && cache.is_dir() && log.is_dir());
    }

    #[test]
    fn partial_overrides_fall_back_to_defaults_per_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("override-data");
        let env = env_of(&[(DATA_DIR_ENV, &data)]);

        let paths = Paths::resolve_from(env, || Ok(Paths::under(tmp.path().join("default")))).unwrap();

        assert_eq!(paths.data_dir, data);
        assert_eq!(paths.cache_dir, tmp.path().join("default/cache"));
        assert_eq!(paths.log_dir, tmp.path().join("default/logs"));
        assert!(paths.data_dir.is_dir() && paths.cache_dir.is_dir() && paths.log_dir.is_dir());
    }

    #[test]
    fn empty_env_value_counts_as_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let env = |key: &str| (key == CACHE_DIR_ENV).then(OsString::new);

        let paths = Paths::resolve_from(env, || Ok(Paths::under(tmp.path()))).unwrap();

        assert_eq!(paths, Paths::under(tmp.path()));
    }

    #[test]
    fn relative_override_is_rejected_and_nothing_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let env = |key: &str| (key == LOG_DIR_ENV).then(|| OsString::from("relative/logs"));

        let err = Paths::resolve_from(env, || Ok(Paths::under(tmp.path().join("x")))).unwrap_err();

        assert!(
            matches!(&err, GitYasError::InvalidInput(msg) if msg.contains(LOG_DIR_ENV)),
            "{err:?}"
        );
        assert!(!tmp.path().join("x").exists());
    }

    #[test]
    fn missing_home_directory_is_an_error_not_a_panic() {
        let err = Paths::resolve_from(|_| None, no_defaults).unwrap_err();
        assert!(matches!(err, GitYasError::NotFound(_)), "{err:?}");
    }

    #[test]
    fn directory_creation_failure_names_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let mut paths = Paths::under(tmp.path());
        paths.cache_dir = file.join("cache");

        let err = paths.create_dirs().unwrap_err();

        assert!(matches!(&err, GitYasError::Io(msg) if msg.contains("cache directory")), "{err:?}");
    }

    #[test]
    fn real_platform_defaults_are_absolute_and_distinct() {
        // Only checks the computed paths; nothing is created in the user's home.
        let Ok(paths) = platform_defaults() else {
            return; // no home directory on this machine
        };
        assert!(paths.data_dir.is_absolute());
        assert!(paths.cache_dir.is_absolute());
        assert!(paths.log_dir.is_absolute());
        assert_ne!(paths.data_dir, paths.cache_dir);
        assert_ne!(paths.data_dir, paths.log_dir);
    }

    #[test]
    fn url_validation_accepts_http_and_https() {
        for url in [
            "https://github.com/settings/tokens/new?scopes=repo&description=GitYas",
            "http://localhost:8080/path",
            "HTTPS://gitlab.com/-/user_settings/personal_access_tokens",
            "https://[::1]:443/",
            "https://dev.azure.com/org/_usersSettings/tokens#frag",
        ] {
            assert_eq!(validate_url(url), Ok(()), "{url}");
        }
    }

    #[test]
    fn url_validation_rejects_other_schemes_and_malformed_urls() {
        for url in [
            "",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://example.com",
            "gityas://callback",
            "/usr/bin/calc",
            "C:\\Windows\\System32\\calc.exe",
            "https:example.com",
            "https://",
            "https:///path",
            "https://:443/",
            "https://user:pass@example.com/",
            "https://github.com@evil.example/",
            "https://example.com/ with space",
            "https://example.com/\nnewline",
            " https://example.com",
            "-https://example.com",
        ] {
            let err = validate_url(url).unwrap_err();
            assert!(matches!(err, GitYasError::InvalidInput(_)), "{url}: {err:?}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(validate_url(&long).is_err());
    }

    #[test]
    fn open_url_rejects_before_launching_anything() {
        let err = open_url("file:///etc/passwd").unwrap_err();
        assert!(matches!(err, GitYasError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn invalid_rust_log_falls_back_with_a_warning() {
        let (_, warning) = build_filter(Some("gityas=notalevel")).unwrap();
        assert!(warning.is_some_and(|w| w.contains("RUST_LOG")));
        let (_, warning) = build_filter(Some("  ")).unwrap();
        assert!(warning.is_none());
        let (_, warning) = build_filter(Some("warn,gityas_engine=trace")).unwrap();
        assert!(warning.is_none());
    }

    /// The only test that installs the global subscriber (there is one per process).
    #[test]
    fn init_tracing_writes_a_log_file_and_second_init_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());

        let guard = init_tracing_with(&paths, None).unwrap();
        tracing::info!("wp7 test marker");
        drop(guard); // flushes the non-blocking writer

        let files = log_files(&paths.log_dir).unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        let contents = std::fs::read_to_string(&files[0]).unwrap();
        assert!(contents.contains("wp7 test marker"), "{contents}");
        assert!(contents.contains("logging initialised"), "{contents}");
        assert!(!contents.contains('\u{1b}'), "file log must not contain ANSI escapes");

        let second = Paths::under(tmp.path().join("second"));
        let err = init_tracing(&second).unwrap_err();
        assert!(matches!(err, GitYasError::Other(_)), "{err:?}");
    }

    #[test]
    fn reveal_target_opens_dirs_and_parents_of_files() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(reveal_target(dir.path()).ok(), Some(dir.path().to_path_buf()));
        assert_eq!(reveal_target(&file).ok(), Some(dir.path().to_path_buf()));
        assert!(matches!(reveal_target(&dir.path().join("missing")), Err(GitYasError::NotFound(_))));
        assert!(matches!(
            reveal_in_file_manager(&dir.path().join("missing")),
            Err(GitYasError::NotFound(_))
        ));
    }
}
