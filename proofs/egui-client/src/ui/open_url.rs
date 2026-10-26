//! "Open stream address" dialog (M6-15). The text is checked by `StreamUrl::parse`, which
//! refuses anything but https (or http to a private host), credentials in the address, and
//! credential-like query values. The dialog only collects the text and shows why it was refused;
//! the player host plays the accepted address directly.

use egui::{Context, Key, RichText};
use kattri_client_core::stream_url::{StreamUrl, StreamUrlError};
use kattri_i18n::ids;

use super::ui_text;

/// What the dialog holds while it is open.
#[derive(Debug, Default)]
pub(crate) struct OpenUrlState {
    input: String,
    error: Option<StreamUrlError>,
}

impl OpenUrlState {
    /// Checks the typed text. On success the text is dropped from the state and the checked
    /// address is returned; on failure the reason stays for the dialog to show.
    pub(crate) fn submit(&mut self) -> Result<StreamUrl, StreamUrlError> {
        match StreamUrl::parse(&self.input) {
            Ok(url) => {
                self.input.clear();
                self.error = None;
                Ok(url)
            }
            Err(error) => {
                self.error = Some(error);
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_input(input: &str) -> Self {
        Self {
            input: input.to_owned(),
            error: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn error(&self) -> Option<StreamUrlError> {
        self.error
    }
}

/// Draws the dialog while `state` is `Some`. Returns the accepted address on the frame the viewer
/// confirms it; the dialog closes then, and also when it is cancelled.
pub(crate) fn show(ctx: &Context, state: &mut Option<OpenUrlState>) -> Option<StreamUrl> {
    let open = state.as_mut()?;
    let mut accepted = None;
    let mut cancelled = false;
    let mut submit = false;
    egui::Window::new(ui_text(ids::OPEN_URL_TITLE))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.set_min_width(420.0);
            ui.label(ui_text(ids::OPEN_URL_NOTE));
            ui.add_space(8.0);
            let field = ui.add(
                egui::TextEdit::singleline(&mut open.input)
                    .hint_text(ui_text(ids::OPEN_URL_HINT))
                    .desired_width(f32::INFINITY),
            );
            if !field.has_focus() && open.input.is_empty() {
                field.request_focus();
            }
            if field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                submit = true;
            }
            if let Some(error) = open.error {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(error.to_string())
                        .color(egui::Color32::from_rgb(0xF0, 0xA0, 0x99)),
                );
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(ui_text(ids::OPEN_URL_PLAY)).clicked() {
                    submit = true;
                }
                if ui.button(ui_text(ids::OPEN_URL_CANCEL)).clicked() {
                    cancelled = true;
                }
            });
        });
    if ctx.input(|input| input.key_pressed(Key::Escape)) {
        cancelled = true;
    }
    if submit && let Ok(url) = open.submit() {
        accepted = Some(url);
    }
    if cancelled || accepted.is_some() {
        *state = None;
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_good_address_is_accepted_and_the_text_is_dropped() {
        let mut state = OpenUrlState::with_input("  https://media.example.org/live/stream.m3u8 ");
        let url = state.submit().expect("accepted");
        assert_eq!(url.as_str(), "https://media.example.org/live/stream.m3u8");
        assert!(state.input.is_empty());
        assert_eq!(state.error(), None);
    }

    #[test]
    fn a_refused_address_keeps_the_text_and_says_why() {
        for (text, expected) in [
            ("", StreamUrlError::Empty),
            ("file:///etc/passwd", StreamUrlError::UnsupportedScheme),
            (
                "rtsp://cam.example.org/1",
                StreamUrlError::UnsupportedScheme,
            ),
            (
                "https://user:pw@media.example.org/a",
                StreamUrlError::Userinfo,
            ),
            (
                "http://media.example.org/a",
                StreamUrlError::InsecureRemoteHttp,
            ),
            (
                "https://media.example.org/a?api_key=abc",
                StreamUrlError::CredentialParameter,
            ),
        ] {
            let mut state = OpenUrlState::with_input(text);
            assert_eq!(state.submit().map(|_| ()), Err(expected), "{text:?}");
            assert_eq!(state.error(), Some(expected));
            assert_eq!(state.input, text, "the text stays so it can be corrected");
        }
    }

    #[test]
    fn a_private_http_address_is_accepted() {
        let mut state = OpenUrlState::with_input("http://192.168.1.20:8096/stream.mkv");
        assert!(state.submit().is_ok());
    }
}
