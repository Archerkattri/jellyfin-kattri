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

/// What the dialog body asked for this frame.
#[derive(Debug, Default)]
struct DialogAction {
    submit: bool,
    cancelled: bool,
}

/// Draws the dialog fields. Kept separate from the floating window so the accessible names can be
/// asserted headlessly: a floating area's subtree is not published by `Context::run_ui`'s
/// AccessKit pass, but the widgets themselves are unchanged.
fn dialog_body(ui: &mut egui::Ui, open: &mut OpenUrlState) -> DialogAction {
    let mut action = DialogAction::default();
    ui.set_min_width(420.0);
    ui.label(ui_text(ids::OPEN_URL_NOTE));
    ui.add_space(8.0);
    let field = ui.add(
        egui::TextEdit::singleline(&mut open.input)
            .hint_text(ui_text(ids::OPEN_URL_HINT))
            .desired_width(f32::INFINITY),
    );
    field.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::Role::TextInput,
            true,
            ui_text(ids::OPEN_URL_TITLE).to_owned(),
        )
    });
    if !field.has_focus() && open.input.is_empty() {
        field.request_focus();
    }
    if field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
        action.submit = true;
    }
    if let Some(error) = open.error {
        ui.add_space(6.0);
        ui.label(RichText::new(error.to_string()).color(crate::theme::active_palette().danger));
    }
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if ui.button(ui_text(ids::OPEN_URL_PLAY)).clicked() {
            action.submit = true;
        }
        if ui.button(ui_text(ids::OPEN_URL_CANCEL)).clicked() {
            action.cancelled = true;
        }
    });
    action
}

/// Draws the dialog while `state` is `Some`. Returns the accepted address on the frame the viewer
/// confirms it; the dialog closes then, and also when it is cancelled.
pub(crate) fn show(ctx: &Context, state: &mut Option<OpenUrlState>) -> Option<StreamUrl> {
    let open = state.as_mut()?;
    let mut accepted = None;
    let mut action = DialogAction::default();
    egui::Window::new(ui_text(ids::OPEN_URL_TITLE))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            action = dialog_body(ui, open);
        });
    if ctx.input(|input| input.key_pressed(Key::Escape)) {
        action.cancelled = true;
    }
    if action.submit
        && let Ok(url) = open.submit()
    {
        accepted = Some(url);
    }
    if action.cancelled || accepted.is_some() {
        *state = None;
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_url_field_has_an_accessible_name() {
        let context = Context::default();
        context.enable_accesskit();
        let mut open = OpenUrlState::default();
        // The dialog body is rendered on the root layer here: egui does not publish a floating
        // window's subtree through `run_ui`'s AccessKit pass, but the widgets are identical.
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |ui| {
                let _ = dialog_body(ui, &mut open);
            },
        );
        output.textures_delta.clear();
        let update = output
            .platform_output
            .accesskit_update
            .expect("the open-address dialog produces an AccessKit tree");
        let all = update
            .nodes
            .iter()
            .map(|(_, node)| {
                (
                    node.role(),
                    node.label().map(str::to_owned),
                    node.value().map(str::to_owned),
                )
            })
            .collect::<Vec<_>>();
        let field = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::TextInput
                    && (node.label() == Some(ui_text(ids::OPEN_URL_TITLE))
                        || node.value() == Some(ui_text(ids::OPEN_URL_TITLE)))
            })
            .unwrap_or_else(|| {
                panic!("the stream-address field has a meaningful accessible name; nodes={all:?}")
            });
        assert!(
            field
                .1
                .label()
                .or_else(|| field.1.value())
                .is_some_and(|name| !name.trim().is_empty())
        );
    }

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
