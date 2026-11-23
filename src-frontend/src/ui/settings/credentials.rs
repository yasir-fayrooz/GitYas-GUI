//! Credentials page: one card per provider with status, token form, Replace / Remove, Test connection and
//! the provider's help text.

use egui::{RichText, Ui};
use gityas_core::Provider;

use super::model::{self, TestOutcome};
use crate::app::GitYasApp;
use crate::bridge::TaskKey;
use crate::dialogs::Confirm;
use crate::theme;

/// Task keys per provider (several cards can save / test at the same time).
pub fn save_key(provider: Provider) -> TaskKey {
    match provider {
        Provider::Github => "settings.credential.github",
        Provider::Gitlab => "settings.credential.gitlab",
        Provider::Bitbucket => "settings.credential.bitbucket",
        Provider::AzureDevops => "settings.credential.azure_devops",
    }
}

pub fn test_key(provider: Provider) -> TaskKey {
    match provider {
        Provider::Github => "settings.test.github",
        Provider::Gitlab => "settings.test.gitlab",
        Provider::Bitbucket => "settings.test.bitbucket",
        Provider::AzureDevops => "settings.test.azure_devops",
    }
}

fn token_field_id(provider: Provider) -> egui::Id {
    egui::Id::new(("settings-token", provider.key()))
}

/// Sets the accessible name of a widget (cards repeat "Save", "Token", …; screen readers and tests need to know
/// which provider a control belongs to).
fn accessible_name(resp: &egui::Response, name: String) {
    resp.ctx.accesskit_node_builder(resp.id, |node| node.set_label(name));
}

/// Drops the egui state of token fields marked for scrubbing: egui's undo history keeps copies of the text, so a
/// saved / cancelled token could otherwise come back with Cmd+Z.
pub(super) fn scrub_text_state(ctx: &egui::Context, app: &mut GitYasApp) {
    for provider in Provider::ALL {
        let card = app.settings_ui.card_mut(provider);
        if card.scrub {
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(token_field_id(provider)));
            card.scrub = false;
        }
    }
}

enum Action {
    Save,
    Cancel,
    Replace,
    Remove,
    Test,
    OpenTokenPage(&'static str),
    DismissError,
}

pub(super) fn page(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    ui.add(
        egui::Label::new(
            RichText::new(
                "GitYas signs in to HTTPS remotes with personal access tokens, stored in the system keychain. When no \
                 token is linked for a host (or it is rejected), the login git has saved (its credential helper) is \
                 used instead. SSH remotes use ssh-agent or your keys in ~/.ssh.",
            )
            .color(p.text_muted),
        )
        .wrap(),
    );
    ui.add_space(6.0);
    for (i, provider) in Provider::ALL.into_iter().enumerate() {
        if i > 0 {
            ui.separator();
        }
        if let Some(action) = card(ui, app, provider) {
            run(app, provider, action);
        }
    }
}

fn card(ui: &mut Ui, app: &mut GitYasApp, provider: Provider) -> Option<Action> {
    let p = theme::pal(ui);
    let help = app.bridge.engine().provider_help(provider);
    let connected = app.store.is_connected(provider);
    let saving = app.bridge.is_running(save_key(provider));
    let testing = app.bridge.is_running(test_key(provider));
    let test_disabled = model::test_disabled_reason(provider, connected, app.store.active_repository(), testing);
    let card = app.settings_ui.card_mut(provider);
    let mut action = None;

    crate::ui::trailing_row(
        ui,
        |ui| {
            let (text, color) = if connected {
                ("● Connected", p.green)
            } else {
                ("○ Not connected", p.text_muted)
            };
            let status = ui.label(RichText::new(text).color(color));
            accessible_name(
                &status,
                format!("{provider}: {}", if connected { "Connected" } else { "Not connected" }),
            );
        },
        |ui| {
            ui.add(egui::Label::new(RichText::new(provider.display_name()).strong().size(14.0)).truncate());
        },
    );

    if let Some(error) = &card.store_error {
        egui::Frame::new()
            .fill(p.del_bg)
            .stroke(egui::Stroke::new(1.0, p.red))
            .corner_radius(4)
            .inner_margin(6)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(egui_phosphor::regular::WARNING_CIRCLE).color(p.red));
                    ui.add(egui::Label::new(RichText::new(error).color(p.red)).wrap());
                    if crate::ui::icon_button(ui, egui_phosphor::regular::X, "Dismiss").clicked() {
                        action = Some(Action::DismissError);
                    }
                });
            });
    }

    if card.editing(connected) {
        egui::Grid::new(("settings-cred-grid", provider.key()))
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Username");
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut card.username)
                            .id(egui::Id::new(("settings-username", provider.key())))
                            .hint_text(help.default_username)
                            .desired_width(field_width(ui, 70.0)),
                    );
                    accessible_name(&resp, format!("{provider} username"));
                    if resp.changed() {
                        card.field_error = None;
                    }
                    let note = if help.username_required { "(required)" } else { "(optional)" };
                    ui.label(RichText::new(note).small().color(p.text_muted));
                });
                ui.end_row();

                ui.label("Token");
                ui.horizontal(|ui| {
                    // Room for Save (and Cancel while replacing) on the same line.
                    let buttons = if card.replacing { 120.0 } else { 56.0 };
                    let resp = ui.add(
                        egui::TextEdit::singleline(card.token.buffer())
                            .id(token_field_id(provider))
                            .password(true)
                            .hint_text(format!("Paste your {}", help.token_name))
                            .desired_width(field_width(ui, buttons)),
                    );
                    accessible_name(&resp, format!("{provider} token"));
                    if resp.changed() {
                        card.field_error = None;
                    }
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let disabled = model::save_disabled_reason(card, saving);
                    let save = crate::ui::solid_button(ui, if saving { "Saving…" } else { "Save" }, p.accent, disabled.is_none());
                    let save = match disabled {
                        Some(reason) => save.on_disabled_hover_text(reason),
                        None => save.on_hover_text("Save the token in the system keychain"),
                    };
                    accessible_name(&save, format!("Save {provider} token"));
                    if (save.clicked() || enter) && disabled.is_none() {
                        action = Some(Action::Save);
                    }
                    if card.replacing {
                        let cancel = ui.button("Cancel");
                        accessible_name(&cancel, format!("Cancel replacing the {provider} token"));
                        if cancel.clicked() {
                            action = Some(Action::Cancel);
                        }
                    }
                });
                ui.end_row();
            });
        if let Some(error) = &card.field_error {
            ui.add(egui::Label::new(RichText::new(error).color(p.red)).wrap());
        }
    } else {
        crate::ui::trailing_row(
            ui,
            |ui| {
                let remove = crate::ui::solid_button(ui, "Remove", p.red, !saving);
                accessible_name(&remove, format!("Remove {provider} token"));
                if remove.clicked() {
                    action = Some(Action::Remove);
                }
                let replace = ui.add_enabled(!saving, egui::Button::new("Replace"));
                accessible_name(&replace, format!("Replace {provider} token"));
                if replace.clicked() {
                    action = Some(Action::Replace);
                }
            },
            |ui| {
                ui.label("Token");
                ui.add(egui::Label::new(RichText::new("••••••••  saved in the system keychain").color(p.text_muted)).truncate());
            },
        );
        ui.horizontal(|ui| {
            let button = ui.add_enabled(test_disabled.is_none(), egui::Button::new("Test connection"));
            let button = match &test_disabled {
                Some(reason) => button.on_disabled_hover_text(reason.as_str()),
                None => button.on_hover_text(
                    "Connects to the open repository's remotes on this provider with the saved token. A public \
                     repository may accept the connection without checking the token.",
                ),
            };
            accessible_name(&button, format!("Test {provider} connection"));
            if button.clicked() {
                action = Some(Action::Test);
            }
            if testing {
                ui.add(egui::Spinner::new());
            }
            match &card.test {
                Some(TestOutcome::Connected(text)) => {
                    ui.add(egui::Label::new(RichText::new(format!("{} {text}", egui_phosphor::regular::CHECK)).color(p.green)).wrap());
                }
                Some(TestOutcome::Failed(text)) => {
                    ui.add(egui::Label::new(RichText::new(text).color(p.red)).wrap());
                }
                None => {}
            }
        });
    }

    ui.add(egui::Label::new(RichText::new(help.help_text).small().color(p.text_muted)).wrap());
    ui.horizontal_wrapped(|ui| {
        let link = ui.link(format!("Create a token {}", egui_phosphor::regular::ARROW_SQUARE_OUT));
        let link = link.on_hover_text(help.token_page_url);
        accessible_name(&link, format!("Create a {provider} token"));
        if link.clicked() {
            action = Some(Action::OpenTokenPage(help.token_page_url));
        }
        ui.label(
            RichText::new(format!("Needs: {}", help.required_scopes))
                .small()
                .color(p.text_faint),
        );
    });
    ui.add_space(4.0);
    action
}

/// Width of a form field that shares its line with `reserved` px of other controls: up to 240 px, shrinking with a
/// narrow panel.
fn field_width(ui: &Ui, reserved: f32) -> f32 {
    (ui.available_width() - reserved).clamp(80.0, 240.0)
}

fn run(app: &mut GitYasApp, provider: Provider, action: Action) {
    match action {
        Action::Save => save(app, provider),
        Action::Cancel => app.settings_ui.card_mut(provider).reset_form(),
        Action::Replace => {
            let card = app.settings_ui.card_mut(provider);
            card.replacing = true;
            card.test = None;
        }
        Action::Remove => confirm_remove(app, provider),
        Action::Test => test(app, provider),
        Action::OpenTokenPage(url) => {
            if let Err(err) = gityas_platform::open_url(url) {
                app.report_error("Could not open the browser", &err);
            }
        }
        Action::DismissError => app.settings_ui.card_mut(provider).store_error = None,
    }
}

/// `set_credential` with the draft; the draft is wiped only once the token was stored (a rejected token stays in
/// the field so the user can fix it).
pub(super) fn save(app: &mut GitYasApp, provider: Provider) {
    let key = save_key(provider);
    if app.bridge.is_running(key) {
        return;
    }
    let card = app.settings_ui.card_mut(provider);
    if model::save_disabled_reason(card, false).is_some() {
        return;
    }
    let token = card.token.to_secret();
    let username = Some(card.username.trim().to_owned()).filter(|u| !u.is_empty());
    card.field_error = None;
    card.store_error = None;
    app.bridge.run(
        key,
        move |engine| async move { engine.set_credential(provider, username, token).await },
        move |app, result| {
            let card = app.settings_ui.card_mut(provider);
            match result {
                Ok(()) => {
                    card.reset_form();
                    card.test = None;
                    app.notify.success(format!("{provider} token saved"));
                }
                Err(err) => model::apply_credential_error(card, &err),
            }
        },
    );
}

fn confirm_remove(app: &mut GitYasApp, provider: Provider) {
    app.confirm(Confirm::new(
        format!("Remove {provider} token?"),
        format!(
            "Remove the {provider} token from the system keychain? Fetching from and pushing to {provider} over HTTPS \
             will need a new token."
        ),
        "Remove token",
        move |app| remove(app, provider),
    ));
}

fn remove(app: &mut GitYasApp, provider: Provider) {
    app.settings_ui.card_mut(provider).store_error = None;
    app.bridge.run(
        save_key(provider),
        move |engine| async move { engine.delete_credential(provider).await },
        move |app, result| {
            let card = app.settings_ui.card_mut(provider);
            match result {
                Ok(()) => {
                    card.reset_form();
                    card.test = None;
                    app.notify.success(format!("{provider} token removed"));
                }
                Err(err) => model::apply_credential_error(card, &err),
            }
        },
    );
}

fn test(app: &mut GitYasApp, provider: Provider) {
    let connected = app.store.is_connected(provider);
    let key = test_key(provider);
    if model::test_disabled_reason(provider, connected, app.store.active_repository(), app.bridge.is_running(key)).is_some() {
        return;
    }
    let remotes = app.store.repo.as_ref().map(|r| r.remotes.clone()).unwrap_or_default();
    app.settings_ui.card_mut(provider).test = None;
    app.bridge.run(
        key,
        move |engine| async move { engine.test_connection(provider).await },
        move |app, result| {
            app.settings_ui.card_mut(provider).test = model::test_outcome(provider, result, &remotes);
        },
    );
}
