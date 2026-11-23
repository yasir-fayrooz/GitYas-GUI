//! `clone_repository` (blocking part): destination checks, progress, cancellation and cleanup.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use gityas_core::models::RemoteOp;
use gityas_core::{AppEvent, CredentialProvider, GitYasError, Provider, Result};
use gityas_git::remote::{RemoteContext, TransferProgress};

use super::register::require_utf8;
use crate::Inner;
use crate::progress::ProgressReporter;

/// What existed at the destination before the clone, so a failed or cancelled clone removes only what it created.
#[derive(Debug, PartialEq, Eq)]
enum Cleanup {
    /// The destination (and possibly some parents) did not exist: remove this, the topmost created ancestor.
    RemoveTree(PathBuf),
    /// The destination was an existing empty directory: empty it again but keep it.
    EmptyDir(PathBuf),
}

/// Clones `url` into `dest` and returns the destination. Fails with `Cancelled` if `cancel` is set meanwhile.
pub(super) fn clone_blocking(inner: &Inner, url: &str, dest: &Path, provider: Option<Provider>, cancel: &AtomicBool) -> Result<PathBuf> {
    let url = url.trim();
    if url.is_empty() {
        return Err(GitYasError::InvalidInput("Enter a repository URL".into()));
    }
    if dest.as_os_str().is_empty() {
        return Err(GitYasError::InvalidInput("Choose a destination folder".into()));
    }
    if !dest.is_absolute() {
        return Err(GitYasError::InvalidInput(format!(
            "the clone destination must be an absolute path: {}",
            dest.display()
        )));
    }
    require_utf8(dest)?;
    let cleanup = plan_cleanup(dest)?;

    let credentials: &dyn CredentialProvider = &*inner.credentials;
    let reporter = ProgressReporter::new(RemoteOp::Clone, |progress| {
        inner.emit(AppEvent::RemoteProgress { repo: None, progress });
    });
    let on_progress = |progress: &TransferProgress| reporter.report(progress);
    let ctx = RemoteContext::new(credentials)
        .with_provider(provider.or_else(|| gityas_git::remote::detect_provider(url)))
        .with_cancel(cancel)
        .with_progress(&on_progress);

    match gityas_git::repository::clone(url, dest, &ctx) {
        Ok(_repo) => {
            reporter.finish();
            Ok(dest.to_path_buf())
        }
        Err(err) => {
            run_cleanup(&cleanup);
            Err(err)
        }
    }
}

fn plan_cleanup(dest: &Path) -> Result<Cleanup> {
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() => {
            let mut entries = std::fs::read_dir(dest).map_err(|err| GitYasError::Io(format!("cannot read {}: {err}", dest.display())))?;
            if entries.next().is_some() {
                return Err(GitYasError::InvalidInput(format!(
                    "{} already exists and isn't empty",
                    dest.display()
                )));
            }
            Ok(Cleanup::EmptyDir(dest.to_path_buf()))
        }
        Ok(_) => Err(GitYasError::InvalidInput(format!(
            "{} already exists and isn't a folder",
            dest.display()
        ))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // libgit2 creates missing parents too; remember the topmost one we are about to create.
            let mut topmost = dest;
            while let Some(parent) = topmost.parent() {
                if parent.as_os_str().is_empty() || parent.exists() {
                    break;
                }
                topmost = parent;
            }
            Ok(Cleanup::RemoveTree(topmost.to_path_buf()))
        }
        Err(err) => Err(GitYasError::Io(format!("cannot inspect {}: {err}", dest.display()))),
    }
}

/// Best effort: libgit2 usually removes a directory it created itself; anything left is removed here. Failures are
/// logged (the clone error is what the user needs to see).
fn run_cleanup(cleanup: &Cleanup) {
    match cleanup {
        Cleanup::RemoveTree(path) => match std::fs::remove_dir_all(path) {
            Ok(()) => tracing::debug!(path = %path.display(), "removed partial clone"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(path = %path.display(), "cannot remove partial clone: {err}"),
        },
        Cleanup::EmptyDir(dir) => {
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
                Err(err) => {
                    tracing::warn!(path = %dir.display(), "cannot clean up partial clone: {err}");
                    return;
                }
            };
            for entry in entries {
                let result = entry.and_then(|entry| {
                    let path = entry.path();
                    if entry.file_type()?.is_dir() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    }
                });
                if let Err(err) = result {
                    tracing::warn!(path = %dir.display(), "cannot clean up partial clone: {err}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_plan() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("a").join("b");
        assert_eq!(plan_cleanup(&fresh).unwrap(), Cleanup::RemoveTree(dir.path().join("a")));
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(plan_cleanup(&empty).unwrap(), Cleanup::EmptyDir(empty.clone()));
        std::fs::write(empty.join("f"), "x").unwrap();
        assert!(matches!(plan_cleanup(&empty), Err(GitYasError::InvalidInput(_))));
        assert!(matches!(plan_cleanup(&empty.join("f")), Err(GitYasError::InvalidInput(_))));
        run_cleanup(&Cleanup::EmptyDir(empty.clone()));
        assert!(empty.is_dir() && std::fs::read_dir(&empty).unwrap().next().is_none());
    }
}
