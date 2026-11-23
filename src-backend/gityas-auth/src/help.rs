//! Per-provider PAT rules and help text (verified against provider docs in October 2026). Pure data: no IO.
//!
//! None of the four providers needs the account's real username for HTTPS git with a token, but each needs a
//! **non-empty** username in HTTP Basic auth, so every provider has a `default_username` used when the user leaves
//! the field blank. WP4's credential callback uses [`effective_username`].

use gityas_core::{Credential, Provider};

/// What the UI (WP15) shows next to a provider's token form, and what the git credential callback (WP4) needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderHelp {
    pub provider: Provider,
    /// The provider's own name for the secret ("personal access token", "API token").
    pub token_name: &'static str,
    /// When `true` the user must enter their account username; `set` rejects a credential without one.
    pub username_required: bool,
    /// Username sent with the token when the user did not enter one. Never empty.
    pub default_username: &'static str,
    /// Where the user creates a token.
    pub token_page_url: &'static str,
    /// Minimum scopes / permissions for fetch and push.
    pub required_scopes: &'static str,
    /// One short paragraph for the UI.
    pub help_text: &'static str,
}

pub fn provider_help(provider: Provider) -> ProviderHelp {
    match provider {
        Provider::Github => ProviderHelp {
            provider,
            token_name: "personal access token",
            username_required: false,
            default_username: "x-access-token",
            token_page_url: "https://github.com/settings/personal-access-tokens/new",
            required_scopes: "Fine-grained token: Repository permissions > Contents: Read and write \
                              (plus Workflows: Read and write to push changes to .github/workflows). \
                              Classic token: the \"repo\" scope.",
            help_text: "Create a fine-grained personal access token for the repositories you use. GitHub ignores the \
                        username when a token is used, so you can leave it blank.",
        },
        Provider::Gitlab => ProviderHelp {
            provider,
            token_name: "personal access token",
            username_required: false,
            default_username: "oauth2",
            token_page_url: "https://gitlab.com/-/user_settings/personal_access_tokens?name=GitYas&scopes=read_repository,write_repository",
            required_scopes: "read_repository and write_repository.",
            help_text: "Create a personal access token with the read_repository and write_repository scopes. GitLab does \
                        not check the username for token logins, so you can leave it blank.",
        },
        Provider::Bitbucket => ProviderHelp {
            provider,
            token_name: "API token",
            username_required: false,
            default_username: "x-bitbucket-api-token-auth",
            token_page_url: "https://id.atlassian.com/manage-profile/security/api-tokens",
            required_scopes: "Create an \"API token with scopes\", choose Bitbucket as the app, and select \
                              read:repository:bitbucket and write:repository:bitbucket.",
            help_text: "Bitbucket app passwords no longer work (retired June 2026); use an Atlassian API token with \
                        Bitbucket scopes. Leave the username blank, or enter your Bitbucket username exactly as shown in \
                        Bitbucket's personal settings (case sensitive; not your email address).",
        },
        Provider::AzureDevops => ProviderHelp {
            provider,
            token_name: "personal access token",
            username_required: false,
            default_username: "pat",
            token_page_url: "https://dev.azure.com/",
            required_scopes: "Code: Read & write.",
            help_text: "Sign in to your organization, open User settings > Personal access tokens, and create an \
                        organization-scoped token with the Code (Read & write) scope. Global tokens stop working on \
                        1 December 2026. Azure DevOps ignores the username, so you can leave it blank.",
        },
    }
}

/// The username to send with the token: the stored one if the user entered one, otherwise the provider default.
pub fn effective_username(provider: Provider, credential: &Credential) -> &str {
    credential.username.as_deref().unwrap_or(provider_help(provider).default_username)
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;

    #[test]
    fn every_provider_has_usable_help() {
        for p in Provider::ALL {
            let h = provider_help(p);
            assert_eq!(h.provider, p);
            assert!(!h.default_username.is_empty(), "{p}");
            assert!(
                !h.default_username.contains(':') && !h.default_username.contains(char::is_whitespace),
                "{p}"
            );
            assert!(h.token_page_url.starts_with("https://"), "{p}");
            assert!(
                !h.help_text.is_empty() && !h.required_scopes.is_empty() && !h.token_name.is_empty(),
                "{p}"
            );
        }
    }

    #[test]
    fn effective_username_prefers_stored_value() {
        let with = Credential {
            username: Some("alice".into()),
            token: SecretString::from("t"),
        };
        let without = Credential {
            username: None,
            token: SecretString::from("t"),
        };
        assert_eq!(effective_username(Provider::Gitlab, &with), "alice");
        assert_eq!(effective_username(Provider::Gitlab, &without), "oauth2");
        assert_eq!(effective_username(Provider::Bitbucket, &without), "x-bitbucket-api-token-auth");
        assert_eq!(effective_username(Provider::Github, &without), "x-access-token");
        assert_eq!(effective_username(Provider::AzureDevops, &without), "pat");
    }
}
