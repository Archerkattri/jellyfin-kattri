//! Manual update install, its status line and the rollback button (M4-16).
//!
//! Kattri does not download updates (owner decision 2026-10-22). The user names a signed
//! envelope and a build that is already on disk, and the verified apply runs on that.
//! Rollback is offered after a successful apply and runs only when the user clicks the button.
//! Nothing here rolls back on its own (ADR 0004 section 11).

use super::{ui_text, ui_text_args};
use kattri_i18n::ids;
use std::io::Read;
use std::path::{Path, PathBuf};

use egui::{Id, RichText, Ui};
use kattri_updater::verify::MAX_ENVELOPE_BYTES;
use kattri_updater::{Channel, Keyring};

use crate::app::KattriApp;
use crate::theme::Palette;
use crate::worker::update::{ApplyRequest, UpdateStatus};

const INSTALL_FORM_ID: &str = "kattri_update_install_form";

/// The paths the install form keeps between frames.
#[derive(Clone, Default)]
struct InstallForm {
    envelope_path: String,
    staged_path: String,
    store_dir: String,
}

/// The sentence the panel shows for `status`, or `None` when there is nothing to report.
pub(crate) fn status_line(status: &UpdateStatus) -> Option<String> {
    let line = match status {
        UpdateStatus::Idle => return None,
        UpdateStatus::Deferred { version } => ui_text_args(
            ids::UPDATE_STATUS_DEFERRED,
            &[("version", version.as_str())],
        ),
        UpdateStatus::Refused {
            version: Some(version),
            reason,
        } => ui_text_args(
            ids::UPDATE_STATUS_REFUSED_VERSION,
            &[("version", version.as_str()), ("reason", reason.as_str())],
        ),
        UpdateStatus::Refused {
            version: None,
            reason,
        } => ui_text_args(ids::UPDATE_STATUS_REFUSED, &[("reason", reason.as_str())]),
        UpdateStatus::Applying { version, .. } => ui_text_args(
            ids::UPDATE_STATUS_APPLYING,
            &[("version", version.as_str())],
        ),
        UpdateStatus::Applied { version, .. } => {
            ui_text_args(ids::UPDATE_STATUS_APPLIED, &[("version", version.as_str())])
        }
        UpdateStatus::Failed {
            version: Some(version),
            reason,
        } => ui_text_args(
            ids::UPDATE_STATUS_FAILED_VERSION,
            &[("version", version.as_str()), ("reason", reason.as_str())],
        ),
        UpdateStatus::Failed {
            version: None,
            reason,
        } => ui_text_args(ids::UPDATE_STATUS_FAILED, &[("reason", reason.as_str())]),
        UpdateStatus::RollingBack { version, .. } => ui_text_args(
            ids::UPDATE_STATUS_ROLLING_BACK,
            &[("version", version.as_str())],
        ),
        UpdateStatus::RolledBack { version } => ui_text_args(
            ids::UPDATE_STATUS_ROLLED_BACK,
            &[("version", version.as_str())],
        ),
        UpdateStatus::RollbackFailed { reason } => ui_text_args(
            ids::UPDATE_STATUS_ROLLBACK_FAILED,
            &[("reason", reason.as_str())],
        ),
    };
    Some(line)
}

/// Draw the last update result and, after an apply, the rollback button.
pub(crate) fn show(ui: &mut Ui, app: &mut KattriApp) {
    let Some(line) = status_line(&app.update.status) else {
        return;
    };
    ui.label(line);
    if let UpdateStatus::Applied { .. } = app.update.status
        && ui.button(ui_text(ids::UPDATE_BUTTON_ROLL_BACK)).clicked()
    {
        app.rollback_update();
    }
}

/// The manual install form (M4-16): a signed envelope file, a staged build, and the update
/// folder. Paths are typed; no file dialog is in the crate set. Only the Settings page calls it.
pub(crate) fn show_install(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let id = Id::unique(INSTALL_FORM_ID);
    let mut form: InstallForm = ui.data(|data| data.get_temp(id)).unwrap_or_default();
    ui.label(
        RichText::new(ui_text(ids::UPDATE_HEADING_TITLE))
            .size(16.0)
            .strong()
            .color(colors.text),
    );
    ui.label(
        RichText::new(ui_text(ids::UPDATE_HELP_NO_DOWNLOAD))
            .size(12.0)
            .color(colors.muted),
    );
    let folder_hint = default_store_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();
    path_field(
        ui,
        ui_text(ids::UPDATE_FIELD_ENVELOPE),
        &mut form.envelope_path,
        "",
    );
    path_field(
        ui,
        ui_text(ids::UPDATE_FIELD_STAGED),
        &mut form.staged_path,
        "",
    );
    path_field(
        ui,
        ui_text(ids::UPDATE_FIELD_STORE_DIR),
        &mut form.store_dir,
        &folder_hint,
    );
    if ui
        .button(ui_text(ids::UPDATE_BUTTON_INSTALL_FILE))
        .clicked()
    {
        install_from_form(app, &form);
    }
    show(ui, app);
    ui.data_mut(|data| data.insert_temp(id, form));
}

fn path_field(ui: &mut Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(RichText::new(label).size(14.0));
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    );
}

/// Verify and start the apply from the form's paths. Nothing is written here. The envelope is
/// read with a cap one byte over the verifier's limit, so an oversized file is refused by the
/// verifier and never read whole.
fn install_from_form(app: &mut KattriApp, form: &InstallForm) {
    let staged = form.staged_path.trim();
    if staged.is_empty() {
        return refuse(app, ui_text(ids::UPDATE_ERROR_CHOOSE_STAGED).to_owned());
    }
    let envelope = match read_envelope(Path::new(form.envelope_path.trim())) {
        Ok(text) => text,
        Err(reason) => return refuse(app, reason),
    };
    let keyring = match Keyring::release() {
        Ok(keyring) => keyring,
        Err(error) => {
            return refuse(
                app,
                ui_text_args(
                    ids::UPDATE_ERROR_KEYS_LOAD,
                    &[("error", &error.to_string())],
                ),
            );
        }
    };
    let typed_folder = form.store_dir.trim();
    let store_dir = if typed_folder.is_empty() {
        default_store_dir()
    } else {
        Some(PathBuf::from(typed_folder))
    };
    let Some(store_dir) = store_dir else {
        return refuse(app, ui_text(ids::UPDATE_ERROR_NO_FOLDER).to_owned());
    };
    let playback = app.update_playback_state();
    app.apply_update(ApplyRequest {
        envelope: &envelope,
        keyring: &keyring,
        channel: Channel::Stable,
        installed_version: env!("CARGO_PKG_VERSION"),
        staged_path: PathBuf::from(staged),
        store_dir,
        playback,
    });
}

fn refuse(app: &mut KattriApp, reason: String) {
    app.update.status = UpdateStatus::Refused {
        version: None,
        reason,
    };
}

fn read_envelope(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path)
        .map_err(|_| ui_text(ids::UPDATE_ERROR_ENVELOPE_OPEN).to_owned())?;
    let limit = u64::try_from(MAX_ENVELOPE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut text = String::new();
    file.take(limit)
        .read_to_string(&mut text)
        .map_err(|_| ui_text(ids::UPDATE_ERROR_ENVELOPE_TEXT).to_owned())?;
    Ok(text)
}

/// The per-user update folder, next to the app's other local data.
fn default_store_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "Jellyfin", "Kattri")
        .map(|dirs| dirs.data_local_dir().join("update-store"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::status_line;
    use crate::worker::update::UpdateStatus;

    #[test]
    fn idle_panel_shows_nothing() {
        assert_eq!(status_line(&UpdateStatus::Idle), None);
    }

    #[test]
    fn deferred_line_names_the_version_and_playback() {
        let line = status_line(&UpdateStatus::Deferred {
            version: "1.1.0".to_owned(),
        })
        .expect("line");
        assert!(line.contains("1.1.0"));
        assert!(line.contains("media is open"));
    }

    #[test]
    fn refusal_and_failure_lines_carry_the_reason() {
        let refused = status_line(&UpdateStatus::Refused {
            version: Some("1.1.0".to_owned()),
            reason: "Free disk space could not be checked.".to_owned(),
        })
        .expect("line");
        assert!(refused.contains("refused") && refused.contains("1.1.0"));

        let failed = status_line(&UpdateStatus::Failed {
            version: None,
            reason: "store is busy".to_owned(),
        })
        .expect("line");
        assert!(failed.contains("store is busy"));
    }

    #[test]
    fn applied_line_does_not_claim_the_app_was_replaced() {
        let line = status_line(&UpdateStatus::Applied {
            version: "1.1.0".to_owned(),
            store_dir: PathBuf::from("store"),
        })
        .expect("line");
        assert!(line.contains("update store"));
        assert!(!line.contains("installed"));
    }

    #[test]
    fn install_with_a_missing_envelope_is_refused_before_any_write() {
        use super::{InstallForm, install_from_form};
        use crate::app::KattriApp;

        let mut app = KattriApp::new(egui::Context::default());
        let form = InstallForm {
            envelope_path: "no-such-envelope-for-test.json".to_owned(),
            staged_path: "no-such-build-for-test.exe".to_owned(),
            store_dir: String::new(),
        };
        install_from_form(&mut app, &form);
        assert!(matches!(
            &app.update.status,
            UpdateStatus::Refused { version: None, reason }
                if reason == "The envelope file could not be opened."
        ));
    }

    #[test]
    fn install_without_a_staged_build_is_refused() {
        use super::{InstallForm, install_from_form};
        use crate::app::KattriApp;

        let mut app = KattriApp::new(egui::Context::default());
        let form = InstallForm {
            envelope_path: "anything.json".to_owned(),
            staged_path: "   ".to_owned(),
            store_dir: String::new(),
        };
        install_from_form(&mut app, &form);
        assert!(matches!(
            &app.update.status,
            UpdateStatus::Refused { version: None, reason }
                if reason == "Choose the staged build file first."
        ));
    }
}
