//! Non-blocking connection notice shared by every client screen.

use super::ui_text;
use egui::{RichText, Ui};
use jellyfin_api::ErrorClass;
use kattri_i18n::ids;

use crate::app::KattriApp;
use crate::theme::Palette;

pub(super) fn show(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let Some(class) = app.network_notice else {
        return;
    };
    ui.horizontal(|ui| {
        ui.add_space(20.0);
        ui.label(RichText::new(message(class)).color(colors.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let action = if class == ErrorClass::AuthExpired {
                ui_text(ids::STATUS_ACTION_SIGN_IN_AGAIN)
            } else {
                ui_text(ids::STATUS_ACTION_RETRY)
            };
            if ui.button(action).clicked() {
                if class == ErrorClass::AuthExpired {
                    app.navigate_to(crate::app::Page::Home);
                    app.network_notice = None;
                } else {
                    app.retry_current_network_request();
                }
            }
        });
        ui.add_space(20.0);
    });
}

pub(crate) fn message(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Offline | ErrorClass::Timeout => ui_text(ids::STATUS_ERROR_UNREACHABLE),
        ErrorClass::ServerUnavailable => ui_text(ids::STATUS_ERROR_UNAVAILABLE),
        ErrorClass::AuthExpired => ui_text(ids::STATUS_ERROR_AUTH_EXPIRED),
        ErrorClass::NotFound => ui_text(ids::STATUS_ERROR_NOT_FOUND),
        ErrorClass::Forbidden => ui_text(ids::STATUS_ERROR_FORBIDDEN),
        ErrorClass::RefusedPlaintext | ErrorClass::TlsFailure => {
            ui_text(ids::STATUS_ERROR_UNVERIFIED)
        }
        ErrorClass::Unexpected => ui_text(ids::STATUS_ERROR_UNEXPECTED),
    }
}

#[cfg(test)]
mod tests {
    use super::message;
    use jellyfin_api::ErrorClass;

    #[test]
    fn each_network_class_has_a_safe_user_message() {
        for class in [
            ErrorClass::Offline,
            ErrorClass::Timeout,
            ErrorClass::ServerUnavailable,
            ErrorClass::AuthExpired,
            ErrorClass::NotFound,
            ErrorClass::Forbidden,
            ErrorClass::RefusedPlaintext,
            ErrorClass::TlsFailure,
            ErrorClass::Unexpected,
        ] {
            let text = message(class);
            assert!(!text.is_empty());
            assert!(!text.contains("token"));
            assert!(!text.contains("api_key"));
        }
    }
}
