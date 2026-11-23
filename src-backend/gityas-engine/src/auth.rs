//! Credential commands (PATs only).
//!
//! * Auth state is global: none of these commands needs (or looks at) the active session, except
//!   [`Engine::test_connection`], which by definition connects to the active repository's remote.
//! * Every credential-store call runs on the blocking pool: the OS keychain can block, and on macOS it may show an
//!   access prompt.
//! * Writes are serialised by [`crate::Inner::auth_lock`] (not the command lock, so saving a token never waits for a
//!   fetch), which keeps each `AuthStatusUpdated` in step with the store.
//! * Tokens stay in `SecretString` end to end; nothing here formats or logs one.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gityas_auth::{ProviderHelp, normalize_credential};
use gityas_core::{AppEvent, Credential, CredentialProvider, GitYasError, Provider, Result};
use gityas_git::remote::{RemoteContext, detect_provider, is_ssh_url};
use gityas_git::repository::RemoteInfo;
use secrecy::SecretString;

use crate::session::SessionCtx;
use crate::{Engine, Inner};

impl Engine {
    /// Connection state for every provider in [`Provider::ALL`] order ("connected" = a credential is stored).
    /// Keychain errors count as "not connected" (and are logged by the store).
    pub async fn auth_status(&self) -> Result<Vec<(Provider, bool)>> {
        let credentials = Arc::clone(&self.inner.credentials);
        self.inner.blocking(move || Ok(credentials.status())).await
    }

    /// Validates (trims; rejects an empty, overlong or multi-line token, or a bad username), stores the credential,
    /// replacing any previous one, and emits `AuthStatusUpdated { connected: true }`. Nothing is emitted on error.
    pub async fn set_credential(&self, provider: Provider, username: Option<String>, token: SecretString) -> Result<()> {
        // Validate before touching the keychain; the store validates again (idempotent).
        let credential = normalize_credential(provider, Credential::new(username, token))?;
        let _guard = self.inner.auth_lock.lock().await;
        let credentials = Arc::clone(&self.inner.credentials);
        self.inner.blocking(move || credentials.set(provider, credential)).await?;
        tracing::info!(%provider, "credential saved");
        self.inner.emit(AppEvent::AuthStatusUpdated { provider, connected: true });
        Ok(())
    }

    /// Removes the provider's credential (a missing one is fine) and emits `AuthStatusUpdated { connected: false }`.
    pub async fn delete_credential(&self, provider: Provider) -> Result<()> {
        let _guard = self.inner.auth_lock.lock().await;
        let credentials = Arc::clone(&self.inner.credentials);
        self.inner.blocking(move || credentials.delete(provider)).await?;
        tracing::info!(%provider, "credential deleted");
        self.inner.emit(AppEvent::AuthStatusUpdated {
            provider,
            connected: false,
        });
        Ok(())
    }

    /// Connects (fetch direction, ref advertisement only, no data transfer) to every HTTPS remote of the active
    /// repository that belongs to `provider`, offering only the stored token for `provider`: the git credential
    /// helper is disabled, since the point is to test the saved token.
    ///
    /// A remote belongs to `provider` if its URL host is one of the provider's hosts, or if the host is not a known
    /// provider host (self-hosted, local) and the repository's stored provider is `provider`. A remote on another
    /// provider's host is never sent this token.
    ///
    /// Errors: `NoActiveRepository`; `Auth` when no token is saved, the keychain is unusable, the only matching
    /// remotes are SSH, or the remote rejects the token; `NotFound` when no remote belongs to `provider`;
    /// `Cancelled` on `cancel_remote_operation` or a repository switch; other errors as `gityas-git` maps them.
    ///
    /// Does **not** take the command lock: it only reads the remote configuration and talks to the network (no
    /// repository or engine state changes, no repo-scoped events), and holding the lock would block every git
    /// command for as long as a slow server takes. A tab switch still aborts it (`cancel_active` signals the
    /// registered remote operation).
    ///
    /// Limitation: a server that serves public repositories anonymously (e.g. a public GitHub repository) may not
    /// ask for credentials at all on the fetch side, so success there does not prove the token is valid.
    pub async fn test_connection(&self, provider: Provider) -> Result<()> {
        let ctx = self.inner.sessions.require()?;
        // Moved into the blocking closure so the operation stays registered (and cancellable) even if this future
        // is dropped while the connection attempt is still running.
        let op = self.inner.sessions.begin_remote_op(Some(&ctx));
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || test_connection_blocking(&inner, &ctx, provider, op.flag()))
            .await
    }

    /// Token page URL, scopes and username guidance for the credentials dialog (the frontend does not depend on
    /// `gityas-auth`).
    pub fn provider_help(&self, provider: Provider) -> ProviderHelp {
        gityas_auth::provider_help(provider)
    }
}

/// Serves exactly one provider's credential (the one `test_connection` looked up), so the connection test can
/// never pick up another provider's token and the keychain is read once.
#[derive(Debug)]
struct SingleCredential {
    provider: Provider,
    credential: Credential,
}

impl CredentialProvider for SingleCredential {
    fn credential(&self, provider: Provider) -> Option<Credential> {
        (provider == self.provider).then(|| self.credential.clone())
    }
}

fn test_connection_blocking(inner: &Inner, ctx: &SessionCtx, provider: Provider, cancel: &AtomicBool) -> Result<()> {
    let credential = inner.credentials.try_credential(provider)?.ok_or_else(|| {
        GitYasError::Auth(format!(
            "no {} {} is saved; add one before testing the connection",
            provider.display_name(),
            gityas_auth::provider_help(provider).token_name
        ))
    })?;

    // The stored provider can change after the session opened (manual override), so read it fresh.
    let repo_provider = match inner.storage.user().repository(&ctx.repo_id) {
        Ok(Some(repo)) => repo.provider,
        Ok(None) => ctx.provider,
        Err(err) => {
            tracing::warn!(repo = %ctx.repo_id, "could not read the repository's provider, using the session's: {err}");
            ctx.provider
        }
    };

    let repo = gityas_git::repository::open(&ctx.path)?;
    let remotes = gityas_git::repository::list_remotes(&repo)?;
    let names = select_remotes(&remotes, provider, repo_provider)?;

    let source = SingleCredential { provider, credential };
    let remote_ctx = RemoteContext::new(&source)
        .with_provider(Some(provider))
        .with_credential_helper(false)
        .with_cancel(cancel);
    for name in names {
        gityas_git::remote::test_connection(&repo, &name, &remote_ctx).map_err(|err| for_remote(&name, err))?;
        tracing::info!(%provider, remote = %name, "connection test succeeded");
    }
    Ok(())
}

/// The HTTPS remotes `test_connection` connects to, `origin` first, then config order. Uses the fetch URL, since
/// the test connects in the fetch direction.
fn select_remotes(remotes: &[RemoteInfo], provider: Provider, repo_provider: Option<Provider>) -> Result<Vec<String>> {
    let matching: Vec<(&str, &str)> = remotes
        .iter()
        .filter_map(|r| Some((r.name.as_str(), r.url.as_deref()?)))
        .filter(|(_, url)| match detect_provider(url) {
            Some(host_provider) => host_provider == provider,
            None => repo_provider == Some(provider),
        })
        .collect();
    if matching.is_empty() {
        return Err(GitYasError::NotFound(format!(
            "this repository has no remote on {}; set the repository's provider to {} if it is self-hosted",
            provider.display_name(),
            provider.display_name()
        )));
    }
    let mut https: Vec<String> = matching
        .iter()
        .filter(|(_, url)| !is_ssh_url(url))
        .map(|(name, _)| (*name).to_owned())
        .collect();
    if https.is_empty() {
        let names: Vec<&str> = matching.iter().map(|(name, _)| *name).collect();
        return Err(GitYasError::Auth(format!(
            "the {} remote ({}) uses SSH, which signs in with SSH keys rather than a personal access token; change the \
             remote URL to HTTPS to test the token",
            provider.display_name(),
            names.join(", ")
        )));
    }
    if let Some(pos) = https.iter().position(|n| n == "origin") {
        let origin = https.remove(pos);
        https.insert(0, origin);
    }
    Ok(https)
}

/// Prefixes the remote's name so the user knows which remote failed when several were tested.
fn for_remote(name: &str, err: GitYasError) -> GitYasError {
    match err {
        GitYasError::Auth(m) => GitYasError::Auth(format!("remote \"{name}\": {m}")),
        GitYasError::Git(m) => GitYasError::Git(format!("remote \"{name}\": {m}")),
        GitYasError::NotFound(m) => GitYasError::NotFound(format!("remote \"{name}\": {m}")),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use gityas_auth::{MAX_TOKEN_LEN, effective_username};
    use secrecy::ExposeSecret;

    use super::*;
    use crate::testutil::{TestEngine, test_engine};

    const TOKEN: &str = "ghp-VerySecretTokenValue42";

    fn secret(s: &str) -> SecretString {
        SecretString::from(s)
    }

    fn status_of(status: &[(Provider, bool)], p: Provider) -> bool {
        status.iter().find(|(q, _)| *q == p).is_some_and(|(_, c)| *c)
    }

    fn init_repo(dir: &Path, remotes: &[(&str, &str)]) -> git2::Repository {
        let repo = git2::Repository::init(dir).unwrap();
        for (name, url) in remotes {
            repo.remote(name, url).unwrap();
        }
        repo
    }

    fn set_repo_provider(t: &TestEngine, ctx: &SessionCtx, provider: Option<Provider>) {
        t.engine
            .inner
            .storage
            .user()
            .update_repository_provider(&ctx.repo_id, provider)
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_status_delete_round_trip_with_events() {
        let t = test_engine();
        let status = t.engine.auth_status().await.unwrap();
        assert_eq!(status.iter().map(|(p, _)| *p).collect::<Vec<_>>(), Provider::ALL.to_vec());
        assert!(status.iter().all(|(_, c)| !c));

        t.engine.set_credential(Provider::Gitlab, None, secret(TOKEN)).await.unwrap();
        assert_eq!(
            t.sink.take(),
            vec![AppEvent::AuthStatusUpdated {
                provider: Provider::Gitlab,
                connected: true
            }]
        );
        let status = t.engine.auth_status().await.unwrap();
        assert!(status_of(&status, Provider::Gitlab));
        assert!(!status_of(&status, Provider::Github));
        assert_eq!(t.credentials.credential(Provider::Gitlab).unwrap().token.expose_secret(), TOKEN);

        // Replace.
        t.engine.set_credential(Provider::Gitlab, None, secret("second")).await.unwrap();
        assert_eq!(t.sink.take().len(), 1);
        assert_eq!(t.credentials.credential(Provider::Gitlab).unwrap().token.expose_secret(), "second");

        t.engine.delete_credential(Provider::Gitlab).await.unwrap();
        assert_eq!(
            t.sink.take(),
            vec![AppEvent::AuthStatusUpdated {
                provider: Provider::Gitlab,
                connected: false
            }]
        );
        assert!(t.engine.auth_status().await.unwrap().iter().all(|(_, c)| !c));

        // Deleting a missing credential succeeds (and still reports the state).
        t.engine.delete_credential(Provider::Bitbucket).await.unwrap();
        assert_eq!(
            t.sink.take(),
            vec![AppEvent::AuthStatusUpdated {
                provider: Provider::Bitbucket,
                connected: false
            }]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_tokens_are_rejected_without_event_and_keep_previous() {
        let t = test_engine();
        t.engine.set_credential(Provider::Github, None, secret(TOKEN)).await.unwrap();
        t.sink.take();
        let long = "a".repeat(MAX_TOKEN_LEN + 1);
        for bad in ["", "   ", "\n\t", "two words", "line\nbreak", long.as_str()] {
            let err = t.engine.set_credential(Provider::Github, None, secret(bad)).await.unwrap_err();
            assert!(matches!(err, GitYasError::InvalidInput(_)), "{bad:?}: {err:?}");
            if !bad.trim().is_empty() {
                assert!(!err.to_string().contains(bad.trim()), "error echoes the token: {err}");
            }
        }
        for bad_user in ["al:ice", "al ice"] {
            let err = t
                .engine
                .set_credential(Provider::Github, Some(bad_user.into()), secret(TOKEN))
                .await
                .unwrap_err();
            assert!(matches!(err, GitYasError::InvalidInput(_)), "{err:?}");
        }
        assert!(t.sink.take().is_empty());
        assert_eq!(t.credentials.credential(Provider::Github).unwrap().token.expose_secret(), TOKEN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn usernames_and_tokens_are_normalised_per_provider() {
        let t = test_engine();
        for p in Provider::ALL {
            t.engine
                .set_credential(p, Some("  alice\t".into()), secret(&format!("  {TOKEN}\n")))
                .await
                .unwrap();
            let c = t.credentials.credential(p).unwrap();
            assert_eq!(c.username.as_deref(), Some("alice"), "{p}");
            assert_eq!(c.token.expose_secret(), TOKEN);
            assert_eq!(effective_username(p, &c), "alice");

            t.engine.set_credential(p, Some("   ".into()), secret(TOKEN)).await.unwrap();
            let c = t.credentials.credential(p).unwrap();
            assert_eq!(c.username, None, "{p}");
            assert_eq!(effective_username(p, &c), t.engine.provider_help(p).default_username);
        }
        let c = t.credentials.credential(Provider::Bitbucket).unwrap();
        assert_eq!(effective_username(Provider::Bitbucket, &c), "x-bitbucket-api-token-auth");
        let c = t.credentials.credential(Provider::Gitlab).unwrap();
        assert_eq!(effective_username(Provider::Gitlab, &c), "oauth2");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auth_does_not_depend_on_the_session_or_the_command_lock() {
        let t = test_engine();
        // Hold the command lock as a long fetch would: credential commands must not wait for it.
        let _lock = t.engine.inner.sessions.command_lock.lock().await;
        let work = async {
            t.engine.set_credential(Provider::AzureDevops, None, secret(TOKEN)).await.unwrap();
            assert!(status_of(&t.engine.auth_status().await.unwrap(), Provider::AzureDevops));
            t.engine.delete_credential(Provider::AzureDevops).await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(10), work).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), &[]);
        t.install_session(dir.path());
        t.engine.set_credential(Provider::Github, None, secret(TOKEN)).await.unwrap();
        assert!(status_of(&t.engine.auth_status().await.unwrap(), Provider::Github));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn debug_output_never_contains_the_token() {
        let t = test_engine();
        t.engine
            .set_credential(Provider::Github, Some("alice".into()), secret(TOKEN))
            .await
            .unwrap();
        let credential = t.credentials.credential(Provider::Github).unwrap();
        let single = SingleCredential {
            provider: Provider::Github,
            credential: credential.clone(),
        };
        let ctx = RemoteContext::new(&single);
        let err = t
            .engine
            .set_credential(Provider::Github, None, secret(&format!("{TOKEN} {TOKEN}")))
            .await
            .unwrap_err();
        let shown = format!(
            "{credential:?} {single:?} {ctx:?} {:?} {:?} {err:?} {err}",
            t.credentials,
            t.sink.take()
        );
        assert!(!shown.contains(TOKEN) && !shown.contains("VerySecret"), "{shown}");
        assert!(shown.contains("alice"));
    }

    #[test]
    fn single_credential_serves_only_its_provider() {
        let single = SingleCredential {
            provider: Provider::Gitlab,
            credential: Credential::new(None, TOKEN),
        };
        assert!(single.credential(Provider::Gitlab).is_some());
        for p in [Provider::Github, Provider::Bitbucket, Provider::AzureDevops] {
            assert!(single.credential(p).is_none());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn provider_help_matches_gityas_auth() {
        let t = test_engine();
        for p in Provider::ALL {
            assert_eq!(t.engine.provider_help(p), gityas_auth::provider_help(p));
            // gityas-git keeps its own copy of the defaults (it can't depend on gityas-auth).
            assert_eq!(t.engine.provider_help(p).default_username, gityas_git::remote::default_username(p));
        }
    }

    fn remote(name: &str, url: Option<&str>) -> RemoteInfo {
        RemoteInfo {
            name: name.into(),
            url: url.map(str::to_owned),
            push_url: None,
            provider: url.and_then(detect_provider),
            is_ssh: url.is_some_and(is_ssh_url),
        }
    }

    #[test]
    fn remote_selection() {
        let gh = Provider::Github;
        let remotes = [
            remote("upstream", Some("https://github.com/up/r.git")),
            remote("lab", Some("https://gitlab.com/g/r.git")),
            remote("origin", Some("https://github.com/me/r.git")),
            remote("ssh", Some("git@github.com:me/r.git")),
            remote("selfhosted", Some("https://git.example.com/r.git")),
            remote("broken", None),
        ];
        assert_eq!(select_remotes(&remotes, gh, None).unwrap(), ["origin", "upstream"]);
        assert_eq!(
            select_remotes(&remotes, gh, Some(gh)).unwrap(),
            ["origin", "upstream", "selfhosted"]
        );
        // The repository's provider never sends a token to another known provider's host.
        assert_eq!(select_remotes(&remotes, Provider::Gitlab, Some(gh)).unwrap(), ["lab"]);
        assert!(matches!(
            select_remotes(&remotes, Provider::Bitbucket, Some(gh)),
            Err(GitYasError::NotFound(_))
        ));
        assert!(matches!(
            select_remotes(&remotes, Provider::Bitbucket, Some(Provider::Bitbucket)),
            Ok(v) if v == ["selfhosted"]
        ));
        assert!(matches!(select_remotes(&[], gh, Some(gh)), Err(GitYasError::NotFound(_))));
        let only_ssh = [remote("origin", Some("ssh://git@github.com/me/r.git"))];
        match select_remotes(&only_ssh, gh, None) {
            Err(GitYasError::Auth(m)) => assert!(m.contains("SSH") && m.contains("origin"), "{m}"),
            other => panic!("expected SSH error, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_connection_without_a_session() {
        let t = test_engine();
        t.engine.set_credential(Provider::Github, None, secret(TOKEN)).await.unwrap();
        assert_eq!(
            t.engine.test_connection(Provider::Github).await,
            Err(GitYasError::NoActiveRepository)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_connection_without_a_credential() {
        let t = test_engine();
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), &[("origin", "https://github.com/me/r.git")]);
        t.install_session(dir.path());
        match t.engine.test_connection(Provider::Github).await {
            Err(GitYasError::Auth(m)) => assert!(m.contains("no GitHub personal access token is saved"), "{m}"),
            other => panic!("expected Auth error, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_connection_without_a_matching_remote() {
        let t = test_engine();
        t.engine.set_credential(Provider::Github, None, secret(TOKEN)).await.unwrap();
        // No remotes at all (also an empty repository with unborn HEAD).
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), &[]);
        let ctx = t.install_session(dir.path());
        assert!(matches!(
            t.engine.test_connection(Provider::Github).await,
            Err(GitYasError::NotFound(_))
        ));
        // Only a GitLab remote, even with the repository's provider set to GitHub: the GitHub token is not sent.
        let dir2 = tempfile::tempdir().unwrap();
        init_repo(dir2.path(), &[("origin", "https://gitlab.com/g/r.git")]);
        let ctx2 = t.install_session(dir2.path());
        set_repo_provider(&t, &ctx2, Some(Provider::Github));
        assert!(matches!(
            t.engine.test_connection(Provider::Github).await,
            Err(GitYasError::NotFound(_))
        ));
        drop(ctx);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_connection_refuses_ssh_remotes() {
        let t = test_engine();
        t.engine.set_credential(Provider::Github, None, secret(TOKEN)).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), &[("origin", "git@github.com:me/r.git")]);
        t.install_session(dir.path());
        match t.engine.test_connection(Provider::Github).await {
            Err(GitYasError::Auth(m)) => assert!(m.contains("SSH"), "{m}"),
            other => panic!("expected SSH error, got {other:?}"),
        }
    }

    /// Local transports never invoke the credential callback, so this proves remote selection (via the stored
    /// repository provider, read fresh from storage), opening the repository and the connection plumbing, but not
    /// that a token is accepted; the HTTPS path needs a real or fake HTTP server.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_connection_to_a_local_bare_remote() {
        let t = test_engine();
        t.engine.set_credential(Provider::Gitlab, None, secret(TOKEN)).await.unwrap();
        let remote_dir = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote_dir.path()).unwrap();
        let url = remote_dir.path().to_str().unwrap().to_owned();
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), &[("origin", &url)]);
        let ctx = t.install_session(dir.path());

        // Without a stored provider a local remote belongs to no provider.
        assert!(matches!(
            t.engine.test_connection(Provider::Gitlab).await,
            Err(GitYasError::NotFound(_))
        ));
        set_repo_provider(&t, &ctx, Some(Provider::Gitlab));
        // The command lock is not needed (a long fetch holding it does not block the test).
        let lock = t.engine.inner.sessions.command_lock.lock().await;
        tokio::time::timeout(Duration::from_secs(30), t.engine.test_connection(Provider::Gitlab))
            .await
            .unwrap()
            .unwrap();
        drop(lock);
        // Nothing stays registered afterwards, and no events are emitted.
        assert_eq!(t.engine.cancel_remote_operation(), 0);
        assert_eq!(t.sink.take().len(), 1); // only the AuthStatusUpdated from set_credential

        // A bare repository as the active repository works the same way.
        let bare_dir = tempfile::tempdir().unwrap();
        let bare = git2::Repository::init_bare(bare_dir.path()).unwrap();
        bare.remote("origin", &url).unwrap();
        let ctx = t.install_session(bare_dir.path());
        set_repo_provider(&t, &ctx, Some(Provider::Gitlab));
        t.engine.test_connection(Provider::Gitlab).await.unwrap();

        // A missing remote repository is reported as a git error naming the remote.
        let dir3 = tempfile::tempdir().unwrap();
        let gone = remote_dir.path().join("does-not-exist");
        init_repo(dir3.path(), &[("origin", gone.to_str().unwrap())]);
        let ctx = t.install_session(dir3.path());
        set_repo_provider(&t, &ctx, Some(Provider::Gitlab));
        let err = t.engine.test_connection(Provider::Gitlab).await.unwrap_err();
        assert!(err.to_string().contains("remote \"origin\""), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_connection_is_cancelled_by_a_session_switch() {
        let t = test_engine();
        t.engine.set_credential(Provider::Github, None, secret(TOKEN)).await.unwrap();
        let remote_dir = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote_dir.path()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), &[("origin", remote_dir.path().to_str().unwrap())]);
        let ctx = t.install_session(dir.path());
        set_repo_provider(&t, &ctx, Some(Provider::Github));
        // What open_repository does before taking the lock: the op registers on a cancelled session and fails fast.
        t.engine.inner.sessions.cancel_active();
        assert_eq!(t.engine.test_connection(Provider::Github).await, Err(GitYasError::Cancelled));
        assert_eq!(t.engine.cancel_remote_operation(), 0);
    }
}
