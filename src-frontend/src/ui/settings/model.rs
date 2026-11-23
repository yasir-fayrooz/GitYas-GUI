//! UI-free state and rules of the settings window: form drafts, validation, button enablement. Unit-tested here;
//! the egui code in the sibling modules only draws these.

use gityas_core::models::{CommitSignature, Repository};
use gityas_core::{GitYasError, Provider};

/// The user-facing text of an error, without the variant prefix for validation errors (`InvalidInput("…")` is
/// shown as "…", not "Invalid input: …").
pub fn user_message(err: &GitYasError) -> String {
    match err {
        GitYasError::InvalidInput(message) => message.clone(),
        other => other.to_string(),
    }
}

// ---- Token draft ------------------------------------------------------------------------------------------------

/// The token being typed into a password field. egui edits a `String`, so the draft is one; it is wiped (every
/// byte of its buffer overwritten) when cleared or dropped, and never printed by `Debug`.
#[derive(Default)]
pub struct TokenDraft(String);

impl TokenDraft {
    /// The buffer for `TextEdit::password`.
    pub fn buffer(&mut self) -> &mut String {
        &mut self.0
    }

    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// A copy for `set_credential` (the copy lives in a `SecretString`, which zeroizes itself).
    pub fn to_secret(&self) -> gityas_engine::SecretString {
        gityas_engine::SecretString::from(self.0.clone())
    }

    /// Overwrites the whole buffer (its capacity, not just the current text) with zeros, then empties it.
    pub fn wipe(&mut self) {
        let capacity = self.0.capacity();
        self.0.clear();
        self.0.extend(std::iter::repeat_n('\0', capacity));
        std::hint::black_box(&self.0);
        self.0.clear();
    }
}

impl Drop for TokenDraft {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl std::fmt::Debug for TokenDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "TokenDraft(empty)"
        } else {
            "TokenDraft(<redacted>)"
        })
    }
}

// ---- Credential card --------------------------------------------------------------------------------------------

/// Result of the last "Test connection" of a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOutcome {
    /// Shown in green, e.g. "Connected to origin".
    Connected(String),
    Failed(String),
}

/// Form state of one provider card.
#[derive(Debug, Default)]
pub struct CredentialCard {
    /// Optional username; blank = the provider's default (placeholder).
    pub username: String,
    pub token: TokenDraft,
    /// A token is saved and the user clicked "Replace": the form is shown again.
    pub replacing: bool,
    /// Validation error from `set_credential` (`InvalidInput`), shown under the fields.
    pub field_error: Option<String>,
    /// Credential-store error (keychain unavailable, locked, …): red card-level message.
    pub store_error: Option<String>,
    pub test: Option<TestOutcome>,
    /// The token field's egui state (undo history holds copies of the text) must be dropped on the next frame.
    pub scrub: bool,
}

impl CredentialCard {
    /// Whether the token form (username + token + Save) is shown: no token saved yet, or replacing it.
    pub fn editing(&self, connected: bool) -> bool {
        !connected || self.replacing
    }

    /// Clears the draft after a successful save or a cancel.
    pub fn reset_form(&mut self) {
        self.token.wipe();
        self.username.clear();
        self.replacing = false;
        self.field_error = None;
        self.scrub = true;
    }
}

/// Why "Save" is disabled, if it is.
pub fn save_disabled_reason(card: &CredentialCard, saving: bool) -> Option<&'static str> {
    if saving {
        Some("Saving…")
    } else if card.token.is_blank() {
        Some("Paste a token first")
    } else {
        None
    }
}

/// Why "Test connection" is disabled for `provider`, if it is. The test connects to the active repository's
/// remotes on that provider with the saved token, so it needs a saved token and an open repository on `provider`.
pub fn test_disabled_reason(provider: Provider, connected: bool, active: Option<&Repository>, running: bool) -> Option<String> {
    if running {
        return Some("Testing…".into());
    }
    if !connected {
        return Some(format!("Save a {provider} token first"));
    }
    let Some(repo) = active else {
        return Some("Open a repository first".into());
    };
    match repo.provider {
        Some(p) if p == provider => None,
        Some(other) => Some(format!("The open repository ({}) is on {other}, not {provider}", repo.name)),
        None => Some(format!("The open repository ({}) is not on {provider}", repo.name)),
    }
}

/// Success text of a connection test. The engine tests every HTTPS remote on the provider (origin first); the
/// store only knows remote *names*, not their hosts, so a remote is named only when it is the repository's only
/// one. The fetch-direction connect can succeed anonymously for a public repository, so this says "connected",
/// never "token valid".
pub fn connected_message(provider: Provider, remotes: &[String]) -> String {
    match remotes {
        [only] => format!("Connected to {only}"),
        _ => format!("Connected to the {provider} remotes"),
    }
}

/// `test_connection` result → what the card shows (`None`: cancelled, show nothing).
pub fn test_outcome(provider: Provider, result: Result<(), GitYasError>, remotes: &[String]) -> Option<TestOutcome> {
    match result {
        Ok(()) => Some(TestOutcome::Connected(connected_message(provider, remotes))),
        Err(GitYasError::Cancelled) => None,
        Err(err) => Some(TestOutcome::Failed(user_message(&err))),
    }
}

/// Where a `set_credential` / `delete_credential` error goes: validation errors under the fields, anything else
/// (keychain unavailable or locked, storage) as the card-level error.
pub fn apply_credential_error(card: &mut CredentialCard, err: &GitYasError) {
    match err {
        GitYasError::InvalidInput(message) => {
            card.field_error = Some(message.clone());
            card.store_error = None;
        }
        other => card.store_error = Some(other.to_string()),
    }
}

// ---- Signature --------------------------------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct SignatureForm {
    pub name: String,
    pub email: String,
    /// The user edited a field (validation messages are shown only then, not on an untouched empty form).
    pub dirty: bool,
    /// Error returned by the engine on save.
    pub save_error: Option<String>,
}

impl SignatureForm {
    pub fn from_signature(signature: Option<&CommitSignature>) -> Self {
        Self {
            name: signature.map(|s| s.name.clone()).unwrap_or_default(),
            email: signature.map(|s| s.email.clone()).unwrap_or_default(),
            dirty: false,
            save_error: None,
        }
    }

    /// The signature to save (trimmed).
    pub fn signature(&self) -> CommitSignature {
        CommitSignature::new(&self.name, &self.email).trimmed()
    }

    /// `CommitSignature::validate` message (any Unicode name is fine; empty, `<`/`>` and newlines are not).
    pub fn validation_error(&self) -> Option<String> {
        self.signature().validate().err().map(|e| user_message(&e))
    }

    /// The inline message to show: the engine's save error, else the validation error once the form is dirty.
    pub fn shown_error(&self) -> Option<String> {
        self.save_error
            .clone()
            .or_else(|| if self.dirty { self.validation_error() } else { None })
    }

    /// Why "Save" is disabled, if it is.
    pub fn save_disabled_reason(&self, saved: Option<&CommitSignature>, saving: bool) -> Option<String> {
        if saving {
            return Some("Saving…".into());
        }
        if let Some(err) = self.validation_error() {
            return Some(err);
        }
        (saved == Some(&self.signature())).then(|| "No changes".into())
    }
}

// ---- Appearance -------------------------------------------------------------------------------------------------

/// Zoom factor as shown on the reset button ("100 %").
pub fn zoom_label(factor: f32) -> String {
    format!("{:.0} %", factor * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(provider: Option<Provider>) -> Repository {
        Repository {
            id: "r".into(),
            name: "git-yas".into(),
            path: "/tmp/git-yas".into(),
            provider,
            sort_order: 0,
        }
    }

    #[test]
    fn token_draft_is_redacted_and_wiped() {
        let mut draft = TokenDraft::default();
        assert_eq!(format!("{draft:?}"), "TokenDraft(empty)");
        draft.buffer().push_str("ghp_secret");
        let card = CredentialCard {
            token: TokenDraft::default(),
            ..Default::default()
        };
        assert!(!format!("{draft:?}{card:?}").contains("ghp_secret"));
        assert!(!draft.is_blank());
        let capacity = draft.buffer().capacity();
        draft.wipe();
        assert!(draft.buffer().is_empty() && draft.is_blank());
        assert_eq!(draft.buffer().capacity(), capacity, "wiped in place, no reallocation");
        draft.buffer().push_str("  \n");
        assert!(draft.is_blank());
    }

    #[test]
    fn save_needs_a_token_and_not_running() {
        let mut card = CredentialCard::default();
        assert_eq!(save_disabled_reason(&card, false), Some("Paste a token first"));
        card.token.buffer().push_str("tok");
        assert_eq!(save_disabled_reason(&card, false), None);
        assert_eq!(save_disabled_reason(&card, true), Some("Saving…"));
    }

    #[test]
    fn editing_and_reset() {
        let mut card = CredentialCard::default();
        assert!(card.editing(false));
        assert!(!card.editing(true));
        card.replacing = true;
        assert!(card.editing(true));
        card.username = "alice".into();
        card.token.buffer().push_str("tok");
        card.field_error = Some("bad".into());
        card.reset_form();
        assert!(!card.replacing && card.username.is_empty() && card.token.is_blank() && card.field_error.is_none());
        assert!(card.scrub);
    }

    #[test]
    fn test_connection_enablement() {
        let gh = Provider::Github;
        assert_eq!(
            test_disabled_reason(gh, false, None, false).as_deref(),
            Some("Save a GitHub token first")
        );
        assert_eq!(
            test_disabled_reason(gh, true, None, false).as_deref(),
            Some("Open a repository first")
        );
        let on_gitlab = repo(Some(Provider::Gitlab));
        assert_eq!(
            test_disabled_reason(gh, true, Some(&on_gitlab), false).as_deref(),
            Some("The open repository (git-yas) is on GitLab, not GitHub")
        );
        let local = repo(None);
        assert_eq!(
            test_disabled_reason(gh, true, Some(&local), false).as_deref(),
            Some("The open repository (git-yas) is not on GitHub")
        );
        let on_github = repo(Some(gh));
        assert_eq!(test_disabled_reason(gh, true, Some(&on_github), false), None);
        assert_eq!(test_disabled_reason(gh, true, Some(&on_github), true).as_deref(), Some("Testing…"));
    }

    #[test]
    fn test_outcome_never_claims_the_token_is_valid() {
        let gh = Provider::Github;
        let origin = vec!["origin".to_owned()];
        assert_eq!(
            test_outcome(gh, Ok(()), &origin),
            Some(TestOutcome::Connected("Connected to origin".into()))
        );
        let two = vec!["upstream".to_owned(), "origin".to_owned()];
        assert_eq!(connected_message(gh, &two), "Connected to the GitHub remotes");
        assert_eq!(test_outcome(gh, Err(GitYasError::Cancelled), &origin), None);
        assert_eq!(
            test_outcome(gh, Err(GitYasError::Auth("rejected".into())), &origin),
            Some(TestOutcome::Failed("Authentication failed: rejected".into()))
        );
        for text in [connected_message(gh, &origin), connected_message(gh, &two)] {
            assert!(!text.to_lowercase().contains("valid"));
        }
    }

    #[test]
    fn credential_errors_go_inline_or_to_the_card() {
        let mut card = CredentialCard::default();
        apply_credential_error(&mut card, &GitYasError::InvalidInput("The GitHub token is empty.".into()));
        assert_eq!(card.field_error.as_deref(), Some("The GitHub token is empty."));
        assert!(card.store_error.is_none());
        apply_credential_error(&mut card, &GitYasError::Auth("the system keychain is locked".into()));
        assert_eq!(
            card.store_error.as_deref(),
            Some("Authentication failed: the system keychain is locked")
        );
    }

    #[test]
    fn signature_validation_accepts_unicode_names() {
        let mut form = SignatureForm::from_signature(None);
        assert_eq!(form.shown_error(), None, "untouched empty form shows no error");
        assert_eq!(
            form.save_disabled_reason(None, false).as_deref(),
            Some("Commit signature name is empty")
        );
        form.dirty = true;
        assert_eq!(form.shown_error().as_deref(), Some("Commit signature name is empty"));
        // The old React form only allowed [a-zA-Z -] and lowercase emails.
        form.name = "  José Ñúñez-Øster 山田  ".into();
        form.email = "José@Example.COM".into();
        assert_eq!(form.validation_error(), None);
        assert_eq!(form.signature().name, "José Ñúñez-Øster 山田");
        assert_eq!(form.save_disabled_reason(None, false), None);
        assert_eq!(
            form.save_disabled_reason(Some(&form.signature()), false).as_deref(),
            Some("No changes")
        );
        form.name = "Ada <x>".into();
        assert!(form.validation_error().is_some_and(|e| e.contains('<')));
        form.name = "Ada\nB".into();
        assert!(form.validation_error().is_some_and(|e| e.contains("single line")));
        form.name = "Ada".into();
        form.save_error = Some("disk full".into());
        assert_eq!(form.shown_error().as_deref(), Some("disk full"));
    }

    #[test]
    fn zoom_labels() {
        assert_eq!(zoom_label(1.0), "100 %");
        assert_eq!(zoom_label(1.1), "110 %");
        assert_eq!(zoom_label(0.9), "90 %");
    }

    #[test]
    fn user_message_strips_the_validation_prefix() {
        assert_eq!(user_message(&GitYasError::InvalidInput("x".into())), "x");
        assert_eq!(user_message(&GitYasError::NoActiveRepository), "No repository is open");
    }
}
