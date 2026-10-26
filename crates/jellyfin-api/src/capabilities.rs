//! Client capability advertisement for remote control.
//!
//! The OpenAPI operation is `PostFullCapabilities`: `POST /Sessions/Capabilities/Full`
//! with a `ClientCapabilitiesDto` body. It tells the server which media types
//! this client can play and which general commands it can execute, so the
//! server can offer remote control of this session. The simpler
//! `POST /Sessions/Capabilities` (`PostCapabilities`) takes the same values as
//! query parameters and is not used here.

use reqwest::{Method, StatusCode};
use serde::Serialize;

use crate::{ApiError, JellyfinClient, Session};

/// A playable media kind (`MediaType` in the OpenAPI, without `Unknown`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum PlayableMediaType {
    Video,
    Audio,
    Photo,
    Book,
}

/// A general remote-control command (`GeneralCommandType` in the OpenAPI).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum GeneralCommandType {
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    PageUp,
    PageDown,
    PreviousLetter,
    NextLetter,
    ToggleOsd,
    ToggleContextMenu,
    Select,
    Back,
    TakeScreenshot,
    SendKey,
    SendString,
    GoHome,
    GoToSettings,
    VolumeUp,
    VolumeDown,
    Mute,
    Unmute,
    ToggleMute,
    SetVolume,
    SetAudioStreamIndex,
    SetSubtitleStreamIndex,
    ToggleFullscreen,
    DisplayContent,
    GoToSearch,
    DisplayMessage,
    SetRepeatMode,
    ChannelUp,
    ChannelDown,
    Guide,
    ToggleStats,
    PlayMediaSource,
    PlayTrailers,
    SetShuffleQueue,
    PlayState,
    PlayNext,
    ToggleOsdMenu,
    Play,
    SetMaxStreamingBitrate,
    SetPlaybackOrder,
}

/// The `ClientCapabilitiesDto` body. Field names serialize in the PascalCase
/// form Jellyfin expects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ClientCapabilities {
    pub playable_media_types: Vec<PlayableMediaType>,
    pub supported_commands: Vec<GeneralCommandType>,
    pub supports_media_control: bool,
    pub supports_persistent_identifier: bool,
}

impl ClientCapabilities {
    /// Capabilities for a player that can be controlled remotely. The list of
    /// commands is what the client can execute, not everything the server knows.
    pub fn remote_controllable(
        playable_media_types: Vec<PlayableMediaType>,
        supported_commands: Vec<GeneralCommandType>,
    ) -> Self {
        Self {
            playable_media_types,
            supported_commands,
            supports_media_control: true,
            supports_persistent_identifier: false,
        }
    }
}

impl JellyfinClient {
    /// Advertise this client's playback capabilities for the signed-in session.
    ///
    /// Jellyfin answers with `204 No Content` on success.
    pub async fn advertise_capabilities(
        &self,
        session: &Session,
        capabilities: &ClientCapabilities,
    ) -> Result<(), ApiError> {
        let url = self.endpoint(&["Sessions", "Capabilities", "Full"]);
        let request = self.http.request(Method::POST, url).json(capabilities);
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ApiError::ReauthenticationRequired);
        }
        if status == StatusCode::FORBIDDEN {
            return Err(ApiError::Forbidden);
        }
        if !status.is_success() {
            return Err(crate::status_error(status, response.headers()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ClientCapabilities, GeneralCommandType, PlayableMediaType};

    #[test]
    fn body_uses_the_openapi_field_names_and_enum_strings() {
        let capabilities = ClientCapabilities::remote_controllable(
            vec![PlayableMediaType::Video, PlayableMediaType::Audio],
            vec![GeneralCommandType::SetVolume, GeneralCommandType::Mute],
        );
        let body = serde_json::to_value(&capabilities).expect("serialize");
        assert_eq!(
            body,
            json!({
                "PlayableMediaTypes": ["Video", "Audio"],
                "SupportedCommands": ["SetVolume", "Mute"],
                "SupportsMediaControl": true,
                "SupportsPersistentIdentifier": false
            })
        );
    }

    #[test]
    fn empty_capabilities_still_send_the_media_control_flag() {
        let capabilities = ClientCapabilities {
            playable_media_types: Vec::new(),
            supported_commands: Vec::new(),
            supports_media_control: false,
            supports_persistent_identifier: false,
        };
        let body = serde_json::to_value(&capabilities).expect("serialize");
        assert_eq!(body["SupportsMediaControl"], json!(false));
        assert_eq!(body["PlayableMediaTypes"], json!([]));
    }
}
