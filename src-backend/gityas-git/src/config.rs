//! The user's global git config (`~/.gitconfig`): `core.autocrlf`, and `user.name` / `user.email` as a suggested
//! commit signature.

use std::path::PathBuf;

use gityas_core::models::{AutoCrlf, CommitSignature};
use gityas_core::{GitYasError, Result};

use crate::git_error;

const AUTOCRLF: &str = "core.autocrlf";
const USER_NAME: &str = "user.name";
const USER_EMAIL: &str = "user.email";

/// The effective `core.autocrlf` outside any repository (system, global and XDG config; `false` when unset).
pub fn autocrlf() -> Result<AutoCrlf> {
    let config = git2::Config::open_default().map_err(git_error)?;
    Ok(read(&config))
}

/// Writes `core.autocrlf` to the global config file, creating `~/.gitconfig` if there is none.
pub fn set_global_autocrlf(value: AutoCrlf) -> Result<()> {
    let mut config = git2::Config::open(&global_path()?).map_err(git_error)?;
    config.set_str(AUTOCRLF, value.as_str()).map_err(git_error)
}

/// `user.name` / `user.email` outside any repository (system, global and XDG config), trimmed. `None` when either is
/// missing or the pair is not a valid signature.
pub fn signature() -> Result<Option<CommitSignature>> {
    let config = git2::Config::open_default().map_err(git_error)?;
    Ok(read_signature(&config))
}

fn read_signature(config: &git2::Config) -> Option<CommitSignature> {
    let name = config.get_string(USER_NAME).ok()?;
    let email = config.get_string(USER_EMAIL).ok()?;
    let signature = CommitSignature::new(name, email).trimmed();
    signature.is_valid().then_some(signature)
}

fn read(config: &git2::Config) -> AutoCrlf {
    config.get_string(AUTOCRLF).map_or(AutoCrlf::False, |v| AutoCrlf::parse(&v))
}

/// The existing global config file, else where git would create it (`$HOME/.gitconfig`; `%USERPROFILE%` on Windows).
fn global_path() -> Result<PathBuf> {
    if let Ok(path) = git2::Config::find_global() {
        return Ok(path);
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".gitconfig"))
        .ok_or_else(|| GitYasError::Other("cannot find the home directory for the global git config".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_autocrlf_in_a_config_file() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut config = git2::Config::open(&dir.path().join(".gitconfig"))?;
        assert_eq!(read(&config), AutoCrlf::False);
        for value in AutoCrlf::ALL {
            config.set_str(AUTOCRLF, value.as_str())?;
            assert_eq!(read(&config.snapshot()?), value);
        }
        Ok(())
    }

    #[test]
    fn reads_the_signature_only_when_complete_and_valid() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut config = git2::Config::open(&dir.path().join(".gitconfig"))?;
        assert_eq!(read_signature(&config.snapshot()?), None);
        config.set_str(USER_NAME, " Ada Lovelace ")?;
        assert_eq!(read_signature(&config.snapshot()?), None, "no email");
        config.set_str(USER_EMAIL, "ada@example.com\t")?;
        assert_eq!(
            read_signature(&config.snapshot()?),
            Some(CommitSignature::new("Ada Lovelace", "ada@example.com"))
        );
        config.set_str(USER_NAME, "<Ada>")?;
        assert_eq!(read_signature(&config.snapshot()?), None, "rejected by libgit2");
        Ok(())
    }
}
