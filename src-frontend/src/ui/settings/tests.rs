//! Headless UI tests of the settings window (in-memory engine: the credential store is in memory, so saving and
//! removing tokens really works; nothing touches the keychain or the network; `open_url` is never clicked).

use egui::{Key, Modifiers};
use egui_kittest::kittest::{NodeT, Queryable};
use gityas_core::Provider;
use gityas_core::models::{DiffViewMode, Theme};

use super::model::TestOutcome;
use super::*;
use crate::testutil::{TestApp, temp_repo};
use crate::theme;

const GH: Provider = Provider::Github;

fn type_into(t: &mut TestApp, label: &str, text: &str) {
    t.harness.get_by_label(label).focus();
    t.harness.step();
    t.harness.get_by_label(label).type_text(text);
    t.harness.step();
}

fn opened(page: Page) -> TestApp {
    let mut t = TestApp::engine();
    open(t.app(), page);
    t.harness.step();
    t.harness.step();
    t
}

fn engine(t: &mut TestApp) -> gityas_engine::Engine {
    t.app().bridge.engine().clone()
}

fn is_disabled(t: &TestApp, label: &str) -> bool {
    t.harness.get_by_label(label).accesskit_node().is_disabled()
}

/// The card's status line ("GitHub: Connected"); its accessible name is set by hand, which kittest's label
/// queries don't see for `Label` nodes (they match the text), so read the node data.
fn shows_status(t: &TestApp, provider: Provider, connected: bool) -> bool {
    let wanted = format!("{provider}: {}", if connected { "Connected" } else { "Not connected" });
    t.harness
        .query_all_by_label_contains("onnected")
        .any(|n| n.accesskit_node().data().label() == Some(wanted.as_str()))
}

fn save_token(t: &mut TestApp, provider: Provider, token: &str) {
    let e = engine(t);
    t.block_on(e.set_credential(provider, None, gityas_engine::SecretString::from(token)))
        .unwrap_or_else(|e| panic!("{e}"));
    t.settle_until("connected", |app| app.store.is_connected(provider));
}

#[test]
fn cmd_comma_opens_settings_and_the_gear_closes_it() {
    let mut t = TestApp::engine();
    t.harness.key_press_modifiers(Modifiers::COMMAND, Key::Comma);
    t.harness.step();
    t.harness.step();
    assert_eq!(t.app().settings_ui.page(), Some(Page::Credentials));
    for provider in Provider::ALL {
        assert!(shows_status(&t, provider, false));
    }
    t.harness.get_by_label("Signature").click();
    t.harness.step();
    t.harness.step();
    assert_eq!(t.app().settings_ui.page(), Some(Page::Signature));
    t.harness.get_by_role_and_label(egui::accesskit::Role::Button, "Settings").click();
    t.harness.step();
    t.harness.step();
    assert_eq!(t.app().settings_ui.page(), None);
}

#[test]
fn saving_a_token_marks_the_provider_connected_and_forgets_the_draft() {
    let mut t = opened(Page::Credentials);
    assert!(is_disabled(&t, "Save GitHub token"), "nothing typed yet");
    type_into(&mut t, "GitHub token", "ghp_example123");
    let field = t.harness.get_by_label("GitHub token");
    assert!(
        field.value().is_some_and(|v| !v.contains("ghp_")),
        "password field is masked: {:?}",
        field.value()
    );
    assert!(!is_disabled(&t, "Save GitHub token"));
    t.harness.get_by_label("Save GitHub token").click();
    t.settle_until("GitHub connected", |app| app.store.is_connected(GH));
    t.settle();

    let card = t.app().settings_ui.card(GH);
    assert!(card.token.is_blank() && !card.replacing && card.field_error.is_none());
    assert!(!format!("{card:?}").contains("ghp_"));
    // The saved token is never shown again: only Replace / Remove.
    t.harness.get_by_label("Replace GitHub token");
    t.harness.get_by_label("Remove GitHub token");
    assert!(shows_status(&t, Provider::Github, true));
    assert!(t.harness.query_by_label("GitHub token").is_none());
    // The text field's state (egui keeps copies of the text for undo) was dropped.
    let id = egui::Id::new(("settings-token", GH.key()));
    assert!(egui::text_edit::TextEditState::load(&t.harness.ctx, id).is_none());
    // Other providers are untouched.
    assert!(shows_status(&t, Provider::Gitlab, false));
    let e = engine(&mut t);
    let status = t.block_on(e.auth_status()).unwrap_or_else(|e| panic!("{e}"));
    assert!(status.contains(&(GH, true)) && status.contains(&(Provider::Gitlab, false)));
}

#[test]
fn replacing_a_token_shows_the_form_again_and_cancel_restores_the_saved_view() {
    let mut t = opened(Page::Credentials);
    save_token(&mut t, GH, "old_token");
    t.harness.get_by_label("Replace GitHub token").click();
    t.harness.step();
    t.harness.step();
    type_into(&mut t, "GitHub token", "draft");
    t.harness.get_by_label("Cancel replacing the GitHub token").click();
    t.harness.step();
    t.harness.step();
    assert!(t.app().settings_ui.card(GH).token.is_blank());
    assert!(t.harness.query_by_label("GitHub token").is_none());

    t.harness.get_by_label("Replace GitHub token").click();
    t.harness.step();
    t.harness.step();
    type_into(&mut t, "GitHub username", "alice");
    type_into(&mut t, "GitHub token", "new_token");
    t.harness.get_by_label("Save GitHub token").click();
    t.settle();
    let card = t.app().settings_ui.card(GH);
    assert!(!card.replacing && card.token.is_blank() && card.username.is_empty());
    assert!(t.app().store.is_connected(GH));
    t.harness.get_by_label("Replace GitHub token");
}

#[test]
fn remove_asks_for_confirmation() {
    let mut t = opened(Page::Credentials);
    save_token(&mut t, GH, "tok");
    t.harness.get_by_label("Remove GitHub token").click();
    t.harness.step();
    t.harness.step();
    assert!(t.app().confirm.as_ref().is_some_and(|c| c.title == "Remove GitHub token?"));
    t.harness.get_by_label("Cancel").click();
    t.harness.step();
    t.settle();
    assert!(t.app().confirm.is_none() && t.app().store.is_connected(GH));

    t.harness.get_by_label("Remove GitHub token").click();
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Remove token").click();
    t.settle_until("GitHub disconnected", |app| !app.store.is_connected(GH));
    t.settle();
    assert!(shows_status(&t, Provider::Github, false));
    t.harness.get_by_label("GitHub token");
    let e = engine(&mut t);
    let status = t.block_on(e.auth_status()).unwrap_or_else(|e| panic!("{e}"));
    assert!(status.contains(&(GH, false)));
}

#[test]
fn invalid_token_and_username_are_rejected_inline() {
    let mut t = opened(Page::Credentials);
    type_into(&mut t, "GitLab token", "two words");
    t.harness.get_by_label("Save GitLab token").click();
    t.settle();
    let card = t.app().settings_ui.card(Provider::Gitlab);
    assert!(
        card.field_error.as_deref().is_some_and(|e| e.contains("spaces")),
        "{:?}",
        card.field_error
    );
    assert!(card.store_error.is_none());
    assert!(!card.token.is_blank(), "a rejected token stays in the field to be fixed");
    assert!(!t.app().store.is_connected(Provider::Gitlab));
    t.harness.get_by_label_contains("contains spaces");
    assert!(t.app().notify.toasts().next().is_none(), "inline, not a toast");

    // Editing clears the error; a bad username is rejected too.
    type_into(&mut t, "GitLab username", "a:b");
    let card = t.app().settings_ui.card_mut(Provider::Gitlab);
    card.token.wipe();
    card.token.buffer().push_str("valid_token");
    assert!(t.app().settings_ui.card(Provider::Gitlab).field_error.is_none());
    t.harness.get_by_label("Save GitLab token").click();
    t.settle();
    let card = t.app().settings_ui.card(Provider::Gitlab);
    assert!(
        card.field_error.as_deref().is_some_and(|e| e.contains("':'")),
        "{:?}",
        card.field_error
    );
}

#[test]
fn card_level_store_error_is_shown_in_the_card() {
    // The in-memory store cannot fail like a locked keychain; the routing is unit-tested in `model`, this checks
    // that the card shows the backend's message.
    let mut t = opened(Page::Credentials);
    t.app().settings_ui.card_mut(Provider::Bitbucket).store_error =
        Some("Authentication failed: the system keychain is locked or access was denied".into());
    t.harness.step();
    t.harness.get_by_label_contains("keychain is locked");
}

#[test]
fn test_connection_is_disabled_without_a_matching_repository() {
    let mut t = opened(Page::Credentials);
    save_token(&mut t, GH, "tok");
    t.harness.step();
    assert!(is_disabled(&t, "Test GitHub connection"), "no repository open");

    // A repository that is not on GitHub.
    let (dir, _repo) = temp_repo();
    t.add_and_open(dir.path());
    assert_eq!(t.app().store.active_repository().and_then(|r| r.provider), None);
    t.harness.step();
    assert!(is_disabled(&t, "Test GitHub connection"), "repository has no provider");
}

#[test]
fn test_connection_against_a_local_bare_remote() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    let bare = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    git2::Repository::init_bare(bare.path()).unwrap_or_else(|e| panic!("{e}"));
    let url = bare.path().to_str().unwrap_or_else(|| panic!("utf-8 temp path"));
    repo.remote("origin", url).unwrap_or_else(|e| panic!("{e}"));
    let e = engine(&mut t);
    let added = t
        .block_on(e.add_repository(dir.path().to_path_buf(), Some(GH)))
        .unwrap_or_else(|e| panic!("{e}"));
    let id = added.id.clone();
    t.settle_until("registered", |app| app.store.repository(&id).is_some());
    crate::ui::repos::switch_to(t.app(), id.clone());
    t.settle_until("opened", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.id == id && r.loaded.remotes)
    });
    save_token(&mut t, GH, "tok");
    open(t.app(), Page::Credentials);
    t.harness.step();
    t.harness.step();
    assert!(!is_disabled(&t, "Test GitHub connection"));
    t.harness.get_by_label("Test GitHub connection").click();
    t.settle();
    assert_eq!(
        t.app().settings_ui.card(GH).test,
        Some(TestOutcome::Connected("Connected to origin".into()))
    );
    t.harness.get_by_label_contains("Connected to origin");

    // Removing the token clears the result.
    let e = engine(&mut t);
    t.block_on(e.delete_credential(GH)).unwrap_or_else(|e| panic!("{e}"));
    t.settle_until("disconnected", |app| !app.store.is_connected(GH));
    assert_eq!(t.app().settings_ui.card(GH).test, None);
}

#[test]
fn signature_page_saves_unicode_names() {
    let mut t = opened(Page::Signature);
    assert!(is_disabled(&t, "Save"), "empty signature cannot be saved");
    type_into(&mut t, "Name", "José Ñúñez 山田");
    type_into(&mut t, "Email", "jose@example.com");
    t.harness.get_by_label("Save").click();
    t.settle_until("signature stored", |app| app.store.signature().is_some());
    let e = engine(&mut t);
    let settings = t.block_on(e.settings()).unwrap_or_else(|e| panic!("{e}"));
    let saved = settings.commit_signature.unwrap_or_else(|| panic!("saved"));
    assert_eq!((saved.name.as_str(), saved.email.as_str()), ("José Ñúñez 山田", "jose@example.com"));
    t.settle();
    assert!(is_disabled(&t, "Save"), "no changes after saving");
}

#[test]
fn signature_page_shows_validation_errors_inline() {
    let mut t = opened(Page::Signature);
    type_into(&mut t, "Name", "Ada <x>");
    type_into(&mut t, "Email", "ada@example.com");
    t.harness.get_by_label_contains("must not contain '<' or '>'");
    assert!(is_disabled(&t, "Save"));
}

#[test]
fn request_signature_runs_then_after_saving() {
    let mut t = TestApp::engine();
    request_signature(t.app(), Some(Box::new(|app: &mut GitYasApp| app.warnings.push("then ran".into()))));
    t.harness.step();
    t.harness.step();
    assert!(t.app().settings_ui.signature_dialog_open());
    type_into(&mut t, "Name", "Ada Lovelace");
    type_into(&mut t, "Email", "ada@example.com");
    t.harness.get_by_label("Save").click();
    t.settle();
    assert!(!t.app().settings_ui.signature_dialog_open());
    assert_eq!(t.app().warnings, ["then ran"]);
    assert_eq!(t.app().store.signature().map(|s| s.name.as_str()), Some("Ada Lovelace"));
}

#[test]
fn request_signature_cancel_drops_then() {
    let mut t = TestApp::engine();
    request_signature(t.app(), Some(Box::new(|app: &mut GitYasApp| app.warnings.push("then ran".into()))));
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Cancel").click();
    t.harness.step();
    t.settle();
    assert!(!t.app().settings_ui.signature_dialog_open());
    assert!(t.app().warnings.is_empty());
}

#[test]
fn request_signature_shows_engine_errors_and_keeps_the_dialog() {
    let mut t = TestApp::engine();
    request_signature(t.app(), Some(Box::new(|app: &mut GitYasApp| app.warnings.push("then ran".into()))));
    t.harness.step();
    // Bypass the UI-side validation to check the engine's error path.
    if let Some(dialog) = t.app().settings_ui.signature_dialog.as_mut() {
        dialog.form.name = "Ada".into();
        dialog.form.email = "ada@example.com".into();
    }
    t.harness.step();
    if let Some(dialog) = t.app().settings_ui.signature_dialog.as_mut() {
        dialog.form.email = "ada\n@example.com".into();
    }
    super::signature::save_dialog(t.app());
    t.settle();
    assert!(t.app().settings_ui.signature_dialog_open());
    assert!(t.app().warnings.is_empty());
    let error = t
        .app()
        .settings_ui
        .signature_dialog
        .as_ref()
        .and_then(|d| d.form.save_error.clone());
    assert!(error.is_some_and(|e| e.contains("single line")));
}

#[test]
fn theme_change_is_persisted_and_applied() {
    let mut t = opened(Page::Appearance);
    t.harness.get_by_label("Light").click();
    t.settle_until("light saved", |app| app.store.settings.theme == Theme::Light);
    t.harness.step();
    t.harness.step();
    assert!(!theme::palette(&t.harness.ctx).dark);
    let e = engine(&mut t);
    assert_eq!(t.block_on(e.settings()).map(|s| s.theme), Ok(Theme::Light));

    t.harness.get_by_label("Dark").click();
    t.settle_until("dark saved", |app| app.store.settings.theme == Theme::Dark);
    t.harness.step();
    t.harness.step();
    assert!(theme::palette(&t.harness.ctx).dark);
}

#[test]
fn graph_and_diff_options_are_persisted_without_touching_the_signature() {
    let mut t = opened(Page::Appearance);
    let e = engine(&mut t);
    t.block_on(e.update_commit_signature(gityas_core::models::CommitSignature::new("Ada", "ada@example.com")))
        .unwrap_or_else(|e| panic!("{e}"));
    t.harness.get_by_label("Show tags").click();
    t.harness.step();
    // A second change before the first finished must not undo it.
    super::appearance::save(t.app(), super::appearance::Change::DiffView(DiffViewMode::Split));
    t.settle();
    let settings = t.block_on(e.settings()).unwrap_or_else(|e| panic!("{e}"));
    assert!(!settings.graph.show_tags && settings.graph.show_remote_branches && settings.graph.show_stashes);
    assert_eq!(settings.diff_view_mode, DiffViewMode::Split);
    assert_eq!(settings.commit_signature.map(|s| s.name), Some("Ada".into()));
    let store = &t.app().store.settings;
    assert!(!store.graph.show_tags && store.diff_view_mode == DiffViewMode::Split);
}

#[test]
fn zoom_buttons_change_the_zoom_factor() {
    let mut t = opened(Page::Appearance);
    t.harness.get_by_label("+").click();
    t.harness.step();
    t.harness.step();
    assert!((t.harness.ctx.zoom_factor() - 1.1).abs() < 1e-4, "{}", t.harness.ctx.zoom_factor());
    t.harness.get_by_label("110 %").click();
    t.harness.step();
    t.harness.step();
    assert!((t.harness.ctx.zoom_factor() - 1.0).abs() < 1e-4);
    t.harness.get_by_label("−").click();
    t.harness.step();
    t.harness.step();
    assert!((t.harness.ctx.zoom_factor() - 0.9).abs() < 1e-4);
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_settings_pages_dark_and_light() {
    let mut t = TestApp::mock_gpu();
    for (page, name) in [
        (Page::Credentials, "settings_credentials"),
        (Page::Signature, "settings_signature"),
        (Page::Appearance, "settings_appearance"),
        (Page::Git, "settings_git"),
    ] {
        for light in [false, true] {
            t.app().store.settings.theme = if light { Theme::Light } else { Theme::Dark };
            open(t.app(), page);
            t.harness.step();
            t.harness.step();
            t.snapshot(if light { format!("{name}_light") } else { name.to_owned() });
        }
    }
    request_signature(t.app(), None);
    t.harness.step();
    t.harness.step();
    t.snapshot("settings_signature_dialog");
}

#[test]
fn git_page_saves_the_fetch_interval() {
    let mut t = opened(Page::Git);
    t.harness.get_by_label("Fetch every");
    appearance::save(t.app(), appearance::Change::FetchInterval(45));
    t.settle_until("saved", |app| app.store.settings.fetch_interval_secs == 45);
    let e = engine(&mut t);
    let stored = t.block_on(e.settings()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(stored.fetch_interval_secs, 45);
}
