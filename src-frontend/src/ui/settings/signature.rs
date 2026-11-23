//! Signature page and the stand-alone signature dialog (shown on its own when a
//! commit needs a signature).

use egui::{RichText, Ui};

use super::model::{self, SignatureForm};
use crate::app::GitYasApp;
use crate::bridge::TaskKey;
use crate::{dialogs, theme};

pub const TASK_SIGNATURE: TaskKey = "settings.signature";

const EXPLANATION: &str = "The name and email GitYas writes into commits, merges and annotated tags.";

/// Name / Email fields and the inline error. Returns `true` if Enter was pressed in a field.
fn fields(ui: &mut Ui, form: &mut SignatureForm, id: &str) -> bool {
    let p = theme::pal(ui);
    let mut enter = false;
    egui::Grid::new((id, "grid")).num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
        for (label, value, hint) in [
            ("Name", &mut form.name, "Ada Lovelace"),
            ("Email", &mut form.email, "ada@example.com"),
        ] {
            let label_resp = ui.label(label);
            let resp = ui.add(
                egui::TextEdit::singleline(value)
                    .id(egui::Id::new((id, label)))
                    .hint_text(hint)
                    .desired_width(300.0),
            );
            let resp = resp.labelled_by(label_resp.id);
            if resp.changed() {
                form.dirty = true;
                form.save_error = None;
            }
            enter |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.end_row();
        }
    });
    if let Some(error) = form.shown_error() {
        ui.add(egui::Label::new(RichText::new(error).color(p.red)).wrap());
    }
    enter
}

pub(super) fn page(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    ui.label(RichText::new("Commit signature").strong().size(14.0));
    ui.add(egui::Label::new(RichText::new(EXPLANATION).color(p.text_muted)).wrap());
    ui.add_space(6.0);
    let saving = app.bridge.is_running(TASK_SIGNATURE);
    let saved = app.store.settings.commit_signature.clone();
    let form = &mut app.settings_ui.signature;
    let enter = fields(ui, form, "settings-signature");
    let disabled = form.save_disabled_reason(saved.as_ref(), saving);
    if saved.is_none() || saved.as_ref().is_some_and(|s| !s.is_valid()) {
        ui.label(RichText::new("No signature set yet: commits, merges and annotated tags need one.").color(p.orange));
    }
    ui.add_space(6.0);
    let save = crate::ui::solid_button(ui, if saving { "Saving…" } else { "Save" }, p.accent, disabled.is_none());
    let save = match &disabled {
        Some(reason) => save.on_disabled_hover_text(reason.as_str()),
        None => save,
    };
    if (save.clicked() || enter) && disabled.is_none() {
        save_page(app);
    }
}

fn save_page(app: &mut GitYasApp) {
    let signature = app.settings_ui.signature.signature();
    app.bridge.run(
        TASK_SIGNATURE,
        move |engine| async move { engine.update_commit_signature(signature).await },
        |app, result| match result {
            Ok(()) => {
                // The store already has it (`CommitsSignatureUpdated` is drained before completions).
                app.settings_ui.signature = SignatureForm::from_signature(app.store.settings.commit_signature.as_ref());
                app.notify.success("Signature saved");
            }
            Err(err) => app.settings_ui.signature.save_error = Some(model::user_message(&err)),
        },
    );
}

pub(super) fn dialog(ctx: &egui::Context, app: &mut GitYasApp) {
    let saving = app.bridge.is_running(TASK_SIGNATURE);
    let Some(dialog) = app.settings_ui.signature_dialog.as_mut() else {
        return;
    };
    let p = theme::palette(ctx);
    let outcome = dialogs::modal(ctx, "signature-dialog", "Set your commit signature", 400.0, |ui| {
        ui.add(egui::Label::new(RichText::new(EXPLANATION).color(p.text_muted)).wrap());
        ui.add_space(6.0);
        let enter = fields(ui, &mut dialog.form, "signature-dialog");
        // Saving the stored signature unchanged is fine here: the caller asked because it is missing or invalid.
        let disabled = dialog.form.save_disabled_reason(None, saving);
        let (cancel, ok) = dialogs::buttons(ui, "Save", false, disabled.is_none(), disabled.as_deref().unwrap_or(""));
        (cancel, (ok || enter) && disabled.is_none())
    });
    let (cancel, ok) = outcome.inner;
    if cancel || outcome.closed {
        app.settings_ui.signature_dialog = None;
    } else if ok {
        save_dialog(app);
    }
}

pub(super) fn save_dialog(app: &mut GitYasApp) {
    let Some(dialog) = app.settings_ui.signature_dialog.as_ref() else {
        return;
    };
    let id = dialog.id;
    let signature = dialog.form.signature();
    app.bridge.run(
        TASK_SIGNATURE,
        move |engine| async move { engine.update_commit_signature(signature).await },
        move |app, result| {
            // Cancelled or replaced while saving: the signature is saved, but nobody waits for it any more.
            if app.settings_ui.signature_dialog.as_ref().map(|d| d.id) != Some(id) {
                return;
            }
            match result {
                Ok(()) => {
                    let dialog = app.settings_ui.signature_dialog.take();
                    if let Some(then) = dialog.and_then(|d| d.then) {
                        then(app);
                    }
                }
                Err(err) => {
                    if let Some(dialog) = app.settings_ui.signature_dialog.as_mut() {
                        dialog.form.save_error = Some(model::user_message(&err));
                    }
                }
            }
        },
    );
}
