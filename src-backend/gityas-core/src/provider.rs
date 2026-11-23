use std::fmt;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// Hosted git providers. All authenticate with personal access tokens; self-hosted instances are out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Github,
    Gitlab,
    Bitbucket,
    AzureDevops,
}

impl Provider {
    pub const ALL: [Provider; 4] = [Provider::Github, Provider::Gitlab, Provider::Bitbucket, Provider::AzureDevops];

    /// Stable machine key, used in SQLite (`repositories.provider`) and the keychain account name (`pat/<key>`).
    pub fn key(self) -> &'static str {
        match self {
            Provider::Github => "github",
            Provider::Gitlab => "gitlab",
            Provider::Bitbucket => "bitbucket",
            Provider::AzureDevops => "azure_devops",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.key() == key)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Github => "GitHub",
            Provider::Gitlab => "GitLab",
            Provider::Bitbucket => "Bitbucket",
            Provider::AzureDevops => "Azure DevOps",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// A personal access token plus the optional username some providers require. `token` is a `SecretString`,
/// so it never appears in `Debug` output or logs.
#[derive(Debug, Clone)]
pub struct Credential {
    pub username: Option<String>,
    pub token: SecretString,
}

impl Credential {
    pub fn new(username: Option<String>, token: impl Into<SecretString>) -> Self {
        Self {
            username,
            token: token.into(),
        }
    }
}

/// Read-only credential lookup, used by `gityas-git` remote callbacks. `gityas-auth::CredentialStore` extends it.
pub trait CredentialProvider: Send + Sync {
    /// The stored credential, or `None` when there is none **or the credential backend (OS keychain) is
    /// unavailable**; `gityas_auth::CredentialStore::try_credential` distinguishes the two.
    fn credential(&self, provider: Provider) -> Option<Credential>;
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    #[test]
    fn key_round_trip() {
        for p in Provider::ALL {
            assert_eq!(Provider::from_key(p.key()), Some(p));
        }
        assert_eq!(Provider::from_key("nope"), None);
    }

    #[test]
    fn token_not_in_debug() {
        let cred = Credential::new(Some("me".into()), "hunter2");
        assert!(!format!("{cred:?}").contains("hunter2"));
        assert_eq!(cred.token.expose_secret(), "hunter2");
    }
}
