//! Validation applied by every [`crate::CredentialStore::set`]. Error messages never contain the token.

use gityas_core::{Credential, GitYasError, Provider, Result};
use secrecy::{ExposeSecret, SecretString};

use crate::provider_help;

/// Sanity cap; real tokens are well under 300 characters (Azure DevOps PATs are 84).
pub const MAX_TOKEN_LEN: usize = 4096;
pub const MAX_USERNAME_LEN: usize = 256;

/// Trims surrounding whitespace and checks the credential:
/// * token: non-empty after trimming, no inner whitespace or control characters (a paste that picked up a second
///   line or a label), at most [`MAX_TOKEN_LEN`] bytes;
/// * username: blank becomes `None`; otherwise no `:` (it would split the HTTP Basic `user:password` pair), no
///   whitespace or control characters, at most [`MAX_USERNAME_LEN`] bytes; required only if
///   [`crate::ProviderHelp::username_required`].
pub fn normalize_credential(provider: Provider, credential: Credential) -> Result<Credential> {
    let Credential { username, token } = credential;

    let token = {
        let raw = token.expose_secret();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(invalid(format!("The {provider} token is empty.")));
        }
        if trimmed.len() > MAX_TOKEN_LEN {
            return Err(invalid(format!(
                "The {provider} token is too long (more than {MAX_TOKEN_LEN} characters)."
            )));
        }
        if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid(format!(
                "The {provider} token contains spaces, line breaks or control characters. Paste only the token."
            )));
        }
        if trimmed.len() == raw.len() {
            token
        } else {
            SecretString::from(trimmed)
        }
    };

    let username = match username.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(name) => {
            if name.len() > MAX_USERNAME_LEN {
                return Err(invalid(format!(
                    "The {provider} username is too long (more than {MAX_USERNAME_LEN} characters)."
                )));
            }
            if name.contains(':') {
                return Err(invalid(format!("The {provider} username must not contain ':'.")));
            }
            if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(invalid(format!(
                    "The {provider} username must not contain spaces or control characters."
                )));
            }
            Some(name.to_owned())
        }
    };

    if username.is_none() && provider_help(provider).username_required {
        return Err(invalid(format!("{provider} needs your account username together with the token.")));
    }

    Ok(Credential { username, token })
}

fn invalid(message: String) -> GitYasError {
    GitYasError::InvalidInput(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "glpat-SuperSecretValue123";

    fn cred(username: Option<&str>, token: &str) -> Credential {
        Credential {
            username: username.map(str::to_owned),
            token: SecretString::from(token),
        }
    }

    fn err_text(r: Result<Credential>) -> String {
        match r {
            Err(GitYasError::InvalidInput(m)) => m,
            Err(other) => panic!("expected InvalidInput, got {other:?}"),
            Ok(_) => panic!("expected an error"),
        }
    }

    #[test]
    fn trims_token_and_username() {
        let c = normalize_credential(Provider::Gitlab, cred(Some("  alice \t"), &format!("\n {TOKEN}  \r\n"))).unwrap();
        assert_eq!(c.username.as_deref(), Some("alice"));
        assert_eq!(c.token.expose_secret(), TOKEN);
    }

    #[test]
    fn blank_username_becomes_none() {
        for name in [None, Some(""), Some("   ")] {
            let c = normalize_credential(Provider::Github, cred(name, TOKEN)).unwrap();
            assert_eq!(c.username, None);
        }
    }

    #[test]
    fn rejects_empty_token() {
        for t in ["", "   ", "\n\t"] {
            let msg = err_text(normalize_credential(Provider::Github, cred(None, t)));
            assert!(msg.contains("empty"), "{msg}");
        }
    }

    #[test]
    fn rejects_inner_whitespace_and_control_chars_without_echoing_token() {
        for t in [format!("{TOKEN} extra"), format!("{TOKEN}\nsecond-line"), format!("{TOKEN}\u{7}")] {
            let msg = err_text(normalize_credential(Provider::Gitlab, cred(None, &t)));
            assert!(!msg.contains(TOKEN), "error message leaked the token: {msg}");
        }
    }

    #[test]
    fn rejects_overlong_token() {
        let long = "a".repeat(MAX_TOKEN_LEN + 1);
        let msg = err_text(normalize_credential(Provider::Github, cred(None, &long)));
        assert!(msg.contains("too long"));
        assert!(normalize_credential(Provider::Github, cred(None, &"a".repeat(MAX_TOKEN_LEN))).is_ok());
    }

    #[test]
    fn rejects_bad_usernames() {
        for name in ["al:ice", "al ice", "al\u{0}ice"] {
            err_text(normalize_credential(Provider::Bitbucket, cred(Some(name), TOKEN)));
        }
        err_text(normalize_credential(
            Provider::Bitbucket,
            cred(Some(&"u".repeat(MAX_USERNAME_LEN + 1)), TOKEN),
        ));
    }

    #[test]
    fn no_provider_currently_requires_a_username() {
        for p in Provider::ALL {
            assert!(normalize_credential(p, cred(None, TOKEN)).is_ok(), "{p}");
        }
    }
}
