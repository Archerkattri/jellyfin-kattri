//! Remote control for this client: the capability value it advertises (M4-12) and the
//! parser for inbound session messages (M4-13).
//!
//! Advertising uses `POST /Sessions/Capabilities/Full` with a `ClientCapabilitiesDto` body
//! (`PostFullCapabilities`). The OpenAPI document at api.jellyfin.org (version 12.2.0,
//! checked 2026-10-21) confirms the route and the PascalCase field names. No live server
//! has been checked.
//!
//! Inbound commands arrive as `{"MessageType": ..., "Data": ...}` envelopes. The OpenAPI
//! confirms the `GeneralCommand` and `Playstate` message types and their `Data` shapes.
//! The WebSocket that carries them is not in the OpenAPI, so the transport stays
//! unconfirmed. [`parse_session_message`] only reads the envelope.

use serde::Deserialize;
use serde_json::Value;

use crate::capabilities::{ClientCapabilities, GeneralCommandType, PlayableMediaType};
use crate::remote_command::{PlaystateCommand, RemoteCommand, RemoteCommandError};

/// Media types this client plays. Photos and books are not remote-playable here.
pub const PLAYABLE_MEDIA_TYPES: [PlayableMediaType; 2] =
    [PlayableMediaType::Video, PlayableMediaType::Audio];

/// General commands this client executes. Each must have a branch in
/// [`RemoteCommand::from_general_command_json`]; advertising anything else is a bug.
pub const SUPPORTED_GENERAL_COMMANDS: [GeneralCommandType; 1] = [GeneralCommandType::SetVolume];

/// Playstate commands this client executes. They are not listed in `SupportedCommands`
/// because the server sends them as `Playstate` messages once media control is on.
pub const SUPPORTED_PLAYSTATE_COMMANDS: [PlaystateCommand; 4] = [
    PlaystateCommand::Unpause,
    PlaystateCommand::Pause,
    PlaystateCommand::Stop,
    PlaystateCommand::Seek,
];

/// The capability body this client advertises for its session. Media control is on, and
/// the command list is exactly [`SUPPORTED_GENERAL_COMMANDS`].
pub fn remote_control_capabilities() -> ClientCapabilities {
    ClientCapabilities::remote_controllable(
        PLAYABLE_MEDIA_TYPES.to_vec(),
        SUPPORTED_GENERAL_COMMANDS.to_vec(),
    )
}

/// A session message envelope as the server sends it.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SessionMessage {
    message_type: String,
    data: Value,
}

/// Parse one inbound session message into a [`RemoteCommand`].
///
/// `GeneralCommand` and `Playstate` are the only message types read. Any other type, and
/// any command the client does not implement, returns
/// [`RemoteCommandError::UnsupportedCommand`]. A command is never guessed from a
/// different one.
pub fn parse_session_message(message: &str) -> Result<RemoteCommand, RemoteCommandError> {
    let envelope: SessionMessage =
        serde_json::from_str(message).map_err(|_| RemoteCommandError::InvalidPayload)?;
    let data =
        serde_json::to_string(&envelope.data).map_err(|_| RemoteCommandError::InvalidPayload)?;
    match envelope.message_type.as_str() {
        "GeneralCommand" => RemoteCommand::from_general_command_json(&data),
        "Playstate" => RemoteCommand::from_playstate_json(&data),
        _ => Err(RemoteCommandError::UnsupportedCommand),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use mock_jellyfin::{MockConfig, RunningServer, spawn as spawn_mock_jellyfin};
    use reqwest::header::AUTHORIZATION;
    use serde_json::{Value, json};

    use super::{
        SUPPORTED_GENERAL_COMMANDS, SUPPORTED_PLAYSTATE_COMMANDS, parse_session_message,
        remote_control_capabilities,
    };
    use crate::capabilities::GeneralCommandType;
    use crate::remote_command::{RemoteCommand, RemoteCommandError};
    use crate::{ClientConfig, JellyfinClient, Session, UserDto};

    const MOCK_TOKEN: &str = "mock-access-token-0001";
    /// Per-command bound for command receipt to parsed command, in process. The mock is
    /// loopback HTTP, so this bound is generous for a healthy run.
    const LATENCY_BOUND: Duration = Duration::from_secs(1);
    const SAMPLES_PER_TYPE: usize = 50;

    fn client(base_url: &str) -> JellyfinClient {
        let config = ClientConfig::new(
            base_url,
            "Kattri test",
            "test device",
            "device-id",
            "test-version",
        )
        .expect("valid config");
        JellyfinClient::new(config).expect("client")
    }

    fn session() -> Session {
        Session {
            access_token: MOCK_TOKEN.into(),
            user: UserDto {
                id: "user-id".into(),
                name: "Test user".into(),
                server_id: None,
            },
            server_id: None,
            session_info: None,
        }
    }

    async fn start() -> RunningServer {
        spawn_mock_jellyfin(MockConfig::default())
            .await
            .expect("spawn mock")
    }

    #[test]
    fn advertised_commands_are_exactly_the_implemented_set() {
        let body = serde_json::to_value(remote_control_capabilities()).expect("serialize");
        assert_eq!(
            body,
            json!({
                "PlayableMediaTypes": ["Video", "Audio"],
                "SupportedCommands": ["SetVolume"],
                "SupportsMediaControl": true,
                "SupportsPersistentIdentifier": false
            })
        );
        assert_eq!(SUPPORTED_GENERAL_COMMANDS, [GeneralCommandType::SetVolume]);
    }

    #[test]
    fn advertised_playstate_commands_parse_and_the_rest_are_refused_by_the_parser() {
        for (api_name, expected) in [
            ("Unpause", RemoteCommand::Play),
            ("Pause", RemoteCommand::Pause),
            ("Stop", RemoteCommand::Stop),
            (
                "Seek",
                RemoteCommand::Seek {
                    position_ticks: 10_000,
                },
            ),
        ] {
            assert!(
                SUPPORTED_PLAYSTATE_COMMANDS
                    .iter()
                    .any(|command| { command.as_api_value() == api_name })
            );
            let data = if api_name == "Seek" {
                json!({"Command": api_name, "SeekPositionTicks": 10_000})
            } else {
                json!({"Command": api_name})
            };
            let message = json!({"MessageType": "Playstate", "Data": data});
            assert_eq!(parse_session_message(&message.to_string()), Ok(expected));
        }
        // NextTrack and PreviousTrack parse to Next and Previous; client-core refuses them
        // (see `client-core` remote tests), so the parser only refuses the rest.
        for unadvertised in ["Rewind", "FastForward", "PlayPause"] {
            let message = json!({"MessageType": "Playstate", "Data": {"Command": unadvertised}});
            assert_eq!(
                parse_session_message(&message.to_string()),
                Err(RemoteCommandError::UnsupportedCommand),
                "{unadvertised}"
            );
        }
    }

    #[test]
    fn general_commands_not_advertised_are_refused() {
        for name in [
            "Mute",
            "Unmute",
            "ToggleMute",
            "VolumeUp",
            "VolumeDown",
            "Play",
            "PlayState",
            "DisplayMessage",
        ] {
            let message =
                json!({"MessageType": "GeneralCommand", "Data": {"Name": name, "Arguments": {}}});
            assert_eq!(
                parse_session_message(&message.to_string()),
                Err(RemoteCommandError::UnsupportedCommand),
                "{name}"
            );
        }
    }

    #[test]
    fn envelope_errors_are_distinct_from_unsupported_commands() {
        assert_eq!(
            parse_session_message("not json"),
            Err(RemoteCommandError::InvalidPayload)
        );
        assert_eq!(
            parse_session_message(r#"{"MessageType":"Playstate"}"#),
            Err(RemoteCommandError::InvalidPayload)
        );
        assert_eq!(
            parse_session_message(r#"{"MessageType":"Sessions","Data":{}}"#),
            Err(RemoteCommandError::UnsupportedCommand)
        );
    }

    #[test]
    fn seek_and_volume_envelopes_parse_to_their_commands() {
        let seek = json!({"MessageType": "Playstate", "Data": {"Command": "Seek", "SeekPositionTicks": 600000000_i64}});
        assert_eq!(
            parse_session_message(&seek.to_string()),
            Ok(RemoteCommand::Seek {
                position_ticks: 600_000_000
            })
        );
        let volume = json!({"MessageType": "GeneralCommand", "Data": {"Name": "SetVolume", "Arguments": {"Volume": "40"}}});
        assert_eq!(
            parse_session_message(&volume.to_string()),
            Ok(RemoteCommand::SetVolume { volume: 40 })
        );
    }

    #[tokio::test]
    async fn client_posts_the_expected_capability_set() {
        let server = start().await;
        let base = server.base_url();
        let api = client(&base);

        api.advertise_capabilities(&session(), &remote_control_capabilities())
            .await
            .expect("advertise capabilities");

        let posted: Value = reqwest::get(format!("{base}/__mock/capabilities"))
            .await
            .expect("capability log")
            .json()
            .await
            .expect("capability log json");
        assert_eq!(
            posted,
            json!([{
                "PlayableMediaTypes": ["Video", "Audio"],
                "SupportedCommands": ["SetVolume"],
                "SupportsMediaControl": true,
                "SupportsPersistentIdentifier": false
            }])
        );
        server.shutdown().await;
    }

    /// M4-13 latency check, in process. For each of the five command types the test posts
    /// [`SAMPLES_PER_TYPE`] envelopes to the mock, polls for the oldest one, and parses it.
    /// Each sample must finish within [`LATENCY_BOUND`] (1 s), and the 95th percentile of
    /// all samples must too. The timed span starts before the POST and ends after parsing,
    /// so it includes both HTTP round trips. It does not include the local player action,
    /// which needs `kattri-client-core` and cannot be linked from this crate's dev-dependencies.
    #[tokio::test]
    async fn posted_command_is_parsed_within_the_latency_bound() {
        let server = start().await;
        let base = server.base_url();
        let http = reqwest::Client::new();
        let auth = format!(
            "MediaBrowser Client=\"tests\", Device=\"ci\", DeviceId=\"ci-1\", Version=\"0\", Token=\"{MOCK_TOKEN}\""
        );
        let cases = [
            (
                json!({"MessageType": "Playstate", "Data": {"Command": "Unpause"}}),
                RemoteCommand::Play,
            ),
            (
                json!({"MessageType": "Playstate", "Data": {"Command": "Pause"}}),
                RemoteCommand::Pause,
            ),
            (
                json!({"MessageType": "Playstate", "Data": {"Command": "Stop"}}),
                RemoteCommand::Stop,
            ),
            (
                json!({"MessageType": "Playstate", "Data": {"Command": "Seek", "SeekPositionTicks": 300000000_i64}}),
                RemoteCommand::Seek {
                    position_ticks: 300_000_000,
                },
            ),
            (
                json!({"MessageType": "GeneralCommand", "Data": {"Name": "SetVolume", "Arguments": {"Volume": "65"}}}),
                RemoteCommand::SetVolume { volume: 65 },
            ),
        ];

        let mut samples = Vec::with_capacity(cases.len() * SAMPLES_PER_TYPE);
        for (envelope, expected) in &cases {
            for _ in 0..SAMPLES_PER_TYPE {
                let started = Instant::now();
                let posted = http
                    .post(format!("{base}/__mock/remote/commands"))
                    .json(envelope)
                    .send()
                    .await
                    .expect("post command");
                assert_eq!(posted.status(), 204);

                let received = http
                    .get(format!("{base}/__mock/remote/next"))
                    .header(AUTHORIZATION, auth.as_str())
                    .send()
                    .await
                    .expect("poll command");
                assert_eq!(received.status(), 200);
                let body = received.text().await.expect("command body");
                let parsed = parse_session_message(&body).expect("parse command");
                let elapsed = started.elapsed();

                assert_eq!(&parsed, expected);
                assert!(
                    elapsed < LATENCY_BOUND,
                    "command took {elapsed:?}, bound is {LATENCY_BOUND:?}"
                );
                samples.push(elapsed);
            }
        }
        server.shutdown().await;

        samples.sort();
        let p95_index = (samples.len() * 95).div_ceil(100) - 1;
        let p95 = samples[p95_index];
        let max = samples[samples.len() - 1];
        eprintln!(
            "remote command latency (in process, {} samples): p95 {:?}, max {:?}",
            samples.len(),
            p95,
            max
        );
        assert!(p95 < LATENCY_BOUND, "p95 {p95:?} exceeds {LATENCY_BOUND:?}");
    }
}
