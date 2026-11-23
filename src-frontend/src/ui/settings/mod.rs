//! WP15 — settings: credentials (PATs), commit signature, appearance (reference UI §12, §15).
//!
//! * Settings are the drawer's Settings page, with its sub-pages as a row of tabs ([`open`]: ‹GEAR› at the top
//!   right, Cmd+, , `[Open credentials]` toast actions). Leaving the page forgets every draft ([`sync`]).
//! * [`request_signature`] shows the signature form as a dialog of its own (commit / annotated tag / merge without a
//!   valid signature) and runs the caller's `then` once a signature was saved.
//! * State lives in [`SettingsUi`]; rules and drafts are UI-free in [`model`].
//! * Nothing is optimistic: credential status comes from `AuthStatusUpdated`, the signature from
//!   `CommitsSignatureUpdated`. Appearance settings have no event, so their command's completion writes the saved
//!   values into `store.settings` (and `GitYasApp` applies the theme from there).

mod appearance;
mod credentials;
mod git;
pub mod model;
mod signature;
#[cfg(test)]
mod tests;

use gityas_core::models::AutoCrlf;
use gityas_core::{AppEvent, Provider};

use crate::app::{Deferred, GitYasApp, SidePage};
use model::{CredentialCard, SignatureForm};

/// Settings page state.
#[derive(Default)]
pub struct SettingsUi {
    /// The open sub-page; `None` = the Settings page is not shown.
    page: Option<Page>,
    /// One card per provider, in `Provider::ALL` order.
    cards: [CredentialCard; 4],
    /// The Signature page's form.
    signature: SignatureForm,
    /// The stand-alone signature dialog ([`request_signature`]).
    signature_dialog: Option<SignatureDialog>,
    /// Identifies the current signature dialog, so a late save completion of a replaced dialog does nothing.
    next_dialog: u64,
    /// "Max commits shown" while it is being dragged / typed (saved once the edit ends).
    max_commits: Option<usize>,
    /// "Fetch every" while it is being dragged / typed (saved once the edit ends).
    fetch_interval: Option<u64>,
    /// Global `core.autocrlf` as last read / saved (Git page, Windows); `None` until read.
    autocrlf: Option<AutoCrlf>,
}

struct SignatureDialog {
    id: u64,
    form: SignatureForm,
    then: Option<Deferred>,
}

impl SettingsUi {
    /// The sub-page shown, or `None` when the Settings page is not shown.
    #[cfg(test)]
    pub fn page(&self) -> Option<Page> {
        self.page
    }

    #[cfg(test)]
    pub fn signature_dialog_open(&self) -> bool {
        self.signature_dialog.is_some()
    }

    #[cfg(test)]
    pub(crate) fn card(&self, provider: Provider) -> &CredentialCard {
        &self.cards[card_index(provider)]
    }

    pub(crate) fn card_mut(&mut self, provider: Provider) -> &mut CredentialCard {
        &mut self.cards[card_index(provider)]
    }
}

/// `cards` index; total over `Provider` (a new provider fails to compile here rather than at runtime).
fn card_index(provider: Provider) -> usize {
    match provider {
        Provider::Github => 0,
        Provider::Gitlab => 1,
        Provider::Bitbucket => 2,
        Provider::AzureDevops => 3,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Credentials,
    Signature,
    Appearance,
    /// Background fetch interval, and global git config (`core.autocrlf`, Windows only).
    Git,
}

impl Page {
    /// The sub-pages listed as tabs, in order.
    const ALL: &[Page] = &[Page::Credentials, Page::Signature, Page::Appearance, Page::Git];

    fn label(self) -> &'static str {
        match self {
            Page::Credentials => "Credentials",
            Page::Signature => "Signature",
            Page::Appearance => "Appearance",
            Page::Git => "Git",
        }
    }
}

pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    let ui = &mut app.settings_ui;
    match event {
        AppEvent::AuthStatusUpdated { provider, connected } => {
            let card = ui.card_mut(*provider);
            // The saved token changed: an earlier test result no longer describes it.
            card.test = None;
            if *connected {
                card.store_error = None;
            }
        }
        // Test results describe the previously active repository.
        AppEvent::SelectedRepositoryUpdated(_) => ui.cards.iter_mut().for_each(|c| c.test = None),
        AppEvent::CommitsSignatureUpdated(signature) if !ui.signature.dirty => {
            ui.signature = SignatureForm::from_signature(Some(signature));
        }
        _ => {}
    }
}

pub fn show_dialogs(ctx: &egui::Context, app: &mut GitYasApp) {
    credentials::scrub_text_state(ctx, app);
    if app.settings_ui.signature_dialog.is_some() {
        signature::dialog(ctx, app);
    }
}

/// Shows the Settings page on `page` (‹GEAR›, Cmd+, , `[Open credentials]` toast actions, `[Add token]` hints).
pub fn open(app: &mut GitYasApp, page: Page) {
    if app.settings_ui.page.is_none() {
        app.settings_ui.signature = SignatureForm::from_signature(app.store.settings.commit_signature.as_ref());
    }
    app.settings_ui.page = Some(page);
    app.layout.show(SidePage::Settings);
}

/// Follows the drawer (before the side panels are drawn): showing the Settings page opens it on Credentials,
/// leaving it (another page, panel hidden) closes it.
pub fn sync(ctx: &egui::Context, app: &mut GitYasApp) {
    match (app.layout.showing(SidePage::Settings), app.settings_ui.page) {
        (true, None) => open(app, Page::Credentials),
        (false, Some(_)) => close(ctx, app),
        _ => {}
    }
}

/// Opens the signature dialog on its own (commit / annotated tag / merge without a valid signature); `then` runs
/// once a valid signature was saved (e.g. retry the commit). Dropped if the user cancels. A second request replaces
/// the first (its `then` is dropped).
pub fn request_signature(app: &mut GitYasApp, then: Option<Deferred>) {
    let ui = &mut app.settings_ui;
    ui.next_dialog += 1;
    ui.signature_dialog = Some(SignatureDialog {
        id: ui.next_dialog,
        form: SignatureForm::from_signature(app.store.settings.commit_signature.as_ref()),
        then,
    });
}

/// The Settings page of the side panel: sub-page tabs, then the sub-page.
pub fn page(ui: &mut egui::Ui, app: &mut GitYasApp) {
    // Still drawn while the panel slides closed, after `sync` has already closed (and reset) the page.
    let page = app.settings_ui.page.unwrap_or(Page::Credentials);
    crate::ui::side::page_header(ui, "Settings", |_| {});
    ui.horizontal_wrapped(|ui| {
        for &candidate in Page::ALL {
            if ui.selectable_label(candidate == page, candidate.label()).clicked() {
                app.settings_ui.page = Some(candidate);
            }
        }
    });
    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt(("settings-page", page))
        .auto_shrink([false, false])
        .show(ui, |ui| match page {
            Page::Credentials => credentials::page(ui, app),
            Page::Signature => signature::page(ui, app),
            Page::Appearance => appearance::page(ui, app),
            Page::Git => git::page(ui, app),
        });
}

/// Closes the Settings page and forgets every draft (tokens are wiped, their text-field state dropped).
fn close(ctx: &egui::Context, app: &mut GitYasApp) {
    let ui = &mut app.settings_ui;
    ui.page = None;
    // Re-read next time: the git config may be edited outside GitYas.
    ui.autocrlf = None;
    ui.max_commits = None;
    ui.fetch_interval = None;
    for card in &mut ui.cards {
        card.reset_form();
        card.store_error = None;
        card.test = None;
    }
    credentials::scrub_text_state(ctx, app);
}
