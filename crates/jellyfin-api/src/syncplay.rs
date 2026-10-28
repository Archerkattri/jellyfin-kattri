//! Typed SyncPlay group calls: list groups, create a group, join a group, and
//! leave the current group.
//!
//! Routes used here, all sent with the session token:
//!
//! - `GET /SyncPlay/List` returns an array of group objects.
//! - `POST /SyncPlay/New` with a `{"GroupName": ...}` body returns the new group.
//! - `POST /SyncPlay/Join` with a `{"GroupId": ...}` body returns no content.
//! - `POST /SyncPlay/Leave` has no body and returns no content.
//!
//! Group identifiers must be GUIDs, and group names are limited to
//! [`MAX_GROUP_NAME_LENGTH`] UTF-16 code units. Invalid values are refused with
//! [`ApiError::InvalidResponse`] before any request is sent.
//!
//! Group playback messages go out through [`JellyfinClient::sync_play_pause`],
//! [`JellyfinClient::sync_play_unpause`] and [`JellyfinClient::sync_play_seek`]:
//!
//! - `POST /SyncPlay/Pause` and `POST /SyncPlay/Unpause` have no body.
//! - `POST /SyncPlay/Seek` with `{"PositionTicks": ...}` takes a 64-bit signed value.
//! - `POST /SyncPlay/SetNewQueue` with a `PlayRequestDto` replaces the group's
//!   queue and starts at the selected item/position.
//! - `POST /SyncPlay/Ready` and `/SyncPlay/Buffering` report a member's position,
//!   play state, playlist item identifier, and RFC 3339 UTC timestamp.
//!
//! Each returns no content on success. [`PlaybackCommand`] holds the route and body for
//! these and for ping. Ping is described but not sent by this module yet.

use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiError, JellyfinClient, Session, decode_response};

/// Longest group name accepted, counted in UTF-16 code units.
pub const MAX_GROUP_NAME_LENGTH: usize = 200;
const MAX_SYNCPLAY_TIMESTAMP_LENGTH: usize = 64;

/// Playback state of a SyncPlay group.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub enum GroupState {
    Idle,
    Waiting,
    Paused,
    Playing,
    /// Any state name this client does not recognize.
    #[default]
    #[serde(other)]
    Unknown,
}

/// A SyncPlay group from `GET /SyncPlay/List` or `POST /SyncPlay/New`.
/// Missing fields take their defaults.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct SyncPlayGroup {
    pub group_id: String,
    pub group_name: String,
    pub state: GroupState,
    pub participants: Vec<String>,
    /// The server's timestamp string, passed through unparsed.
    pub last_updated_at: String,
}

impl JellyfinClient {
    /// List the SyncPlay groups (`GET /SyncPlay/List`).
    pub async fn list_sync_play_groups(
        &self,
        session: &Session,
    ) -> Result<Vec<SyncPlayGroup>, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["SyncPlay", "List"]));
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }

    /// Create a SyncPlay group with the given name (`POST /SyncPlay/New`) and
    /// return it.
    pub async fn create_sync_play_group(
        &self,
        session: &Session,
        group_name: &str,
    ) -> Result<SyncPlayGroup, ApiError> {
        validate_group_name(group_name)?;
        let response = self
            .sync_play_post(
                session,
                &["SyncPlay", "New"],
                Some(new_group_body(group_name)),
            )
            .await?;
        decode_response(response).await
    }

    /// Join an existing SyncPlay group (`POST /SyncPlay/Join`). The group ID
    /// must be a GUID; it is checked before the request is sent.
    pub async fn join_sync_play_group(
        &self,
        session: &Session,
        group_id: &str,
    ) -> Result<(), ApiError> {
        validate_group_id(group_id)?;
        let response = self
            .sync_play_post(
                session,
                &["SyncPlay", "Join"],
                Some(join_group_body(group_id)),
            )
            .await?;
        expect_no_content(response.status())
    }

    /// Leave the SyncPlay group the session is currently in
    /// (`POST /SyncPlay/Leave`).
    pub async fn leave_sync_play_group(&self, session: &Session) -> Result<(), ApiError> {
        let response = self
            .sync_play_post(session, &["SyncPlay", "Leave"], None)
            .await?;
        expect_no_content(response.status())
    }

    /// Send a group pause to the joined group (`POST /SyncPlay/Pause`).
    pub async fn sync_play_pause(&self, session: &Session) -> Result<(), ApiError> {
        self.send_playback_command(session, PlaybackCommand::Pause)
            .await
    }

    /// Send a group unpause to the joined group (`POST /SyncPlay/Unpause`).
    pub async fn sync_play_unpause(&self, session: &Session) -> Result<(), ApiError> {
        self.send_playback_command(session, PlaybackCommand::Unpause)
            .await
    }

    /// Send a group stop to the joined group (`POST /SyncPlay/Stop`).
    pub async fn sync_play_stop(&self, session: &Session) -> Result<(), ApiError> {
        self.send_playback_command(session, PlaybackCommand::Stop)
            .await
    }

    /// Send a group seek to the joined group (`POST /SyncPlay/Seek`). The position is
    /// in Jellyfin ticks (100-nanosecond units). Values above `i64::MAX` are refused
    /// before any request is sent.
    pub async fn sync_play_seek(
        &self,
        session: &Session,
        position_ticks: u64,
    ) -> Result<(), ApiError> {
        self.send_playback_command(session, PlaybackCommand::Seek { position_ticks })
            .await
    }

    /// Replace the joined SyncPlay group's queue and start the selected item
    /// (`POST /SyncPlay/SetNewQueue`). The queue must be nonempty, the item
    /// position must be an in-range zero-based index, and start ticks must be
    /// nonnegative. Invalid values are rejected before any request is sent.
    pub async fn set_sync_play_new_queue(
        &self,
        session: &Session,
        playing_queue: &[String],
        playing_item_position: i32,
        start_position_ticks: i64,
    ) -> Result<(), ApiError> {
        let request = PlayRequestDto {
            playing_queue: playing_queue.to_vec(),
            playing_item_position,
            start_position_ticks,
        };
        request.validate()?;
        let response = self
            .sync_play_post(session, &["SyncPlay", "SetNewQueue"], Some(request.body()))
            .await?;
        expect_no_content(response.status())
    }

    /// Report that the client can play the selected group item (`POST /SyncPlay/Ready`).
    /// `when` must be an RFC 3339 UTC timestamp representing the estimated server time.
    pub async fn sync_play_ready(
        &self,
        session: &Session,
        status: &SyncPlayPlaybackStatus,
    ) -> Result<(), ApiError> {
        self.send_sync_play_status(session, "Ready", status).await
    }

    /// Report that the client is buffering the selected group item
    /// (`POST /SyncPlay/Buffering`). `when` must be an RFC 3339 UTC timestamp
    /// representing the estimated server time.
    pub async fn sync_play_buffering(
        &self,
        session: &Session,
        status: &SyncPlayPlaybackStatus,
    ) -> Result<(), ApiError> {
        self.send_sync_play_status(session, "Buffering", status)
            .await
    }

    async fn send_sync_play_status(
        &self,
        session: &Session,
        route: &'static str,
        status: &SyncPlayPlaybackStatus,
    ) -> Result<(), ApiError> {
        let body = status.body()?;
        let response = self
            .sync_play_post(session, &["SyncPlay", route], Some(body))
            .await?;
        expect_no_content(response.status())
    }

    async fn send_playback_command(
        &self,
        session: &Session,
        command: PlaybackCommand,
    ) -> Result<(), ApiError> {
        // Build the body first, so an out-of-range value never reaches the network.
        let body = command.body()?;
        let response = self.sync_play_post(session, command.route(), body).await?;
        expect_no_content(response.status())
    }

    async fn sync_play_post(
        &self,
        session: &Session,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<reqwest::Response, ApiError> {
        let mut request = self.http.request(Method::POST, self.endpoint(segments));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        request.send().await.map_err(crate::transport_error)
    }
}

/// JSON body for `POST /SyncPlay/New`.
fn new_group_body(group_name: &str) -> Value {
    json!({ "GroupName": group_name })
}

/// JSON body for `POST /SyncPlay/Join`.
fn join_group_body(group_id: &str) -> Value {
    json!({ "GroupId": group_id })
}

/// Typed request body matching Jellyfin's `PlayRequestDto`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlayRequestDto {
    playing_queue: Vec<String>,
    playing_item_position: i32,
    start_position_ticks: i64,
}

impl PlayRequestDto {
    fn validate(&self) -> Result<(), ApiError> {
        let Ok(index) = usize::try_from(self.playing_item_position) else {
            return Err(ApiError::InvalidResponse);
        };
        if self.playing_queue.is_empty()
            || index >= self.playing_queue.len()
            || self.start_position_ticks < 0
            || self
                .playing_queue
                .iter()
                .any(|item_id| validate_group_id(item_id).is_err())
        {
            return Err(ApiError::InvalidResponse);
        }
        Ok(())
    }

    fn body(&self) -> Value {
        // This DTO only contains infallibly serializable JSON primitives.
        json!({
            "PlayingQueue": &self.playing_queue,
            "PlayingItemPosition": self.playing_item_position,
            "StartPositionTicks": self.start_position_ticks,
        })
    }
}

/// Playback state reported to the joined SyncPlay group when a member is ready
/// or buffering. `playlist_item_id` is Jellyfin's playlist-entry GUID, not the
/// media item GUID. `when` is an RFC 3339 UTC timestamp estimated from the
/// server-clock synchronizer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncPlayPlaybackStatus {
    pub when: String,
    pub position_ticks: u64,
    pub is_playing: bool,
    pub playlist_item_id: String,
}

impl SyncPlayPlaybackStatus {
    fn body(&self) -> Result<Value, ApiError> {
        if self.when.is_empty()
            || self.when.trim() != self.when
            || self.when.len() > MAX_SYNCPLAY_TIMESTAMP_LENGTH
        {
            return Err(ApiError::InvalidResponse);
        }
        validate_group_id(&self.playlist_item_id)?;
        let position_ticks =
            i64::try_from(self.position_ticks).map_err(|_| ApiError::InvalidResponse)?;
        Ok(json!({
            "When": self.when,
            "PositionTicks": position_ticks,
            "IsPlaying": self.is_playing,
            "PlaylistItemId": self.playlist_item_id,
        }))
    }
}

/// Which SyncPlay player-state report is being sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPlayStatusReportKind {
    Ready,
    Buffering,
}

/// Map a status from an endpoint with no response body. Any 2xx status is
/// success; the error mapping matches the shared response path.
fn expect_no_content(status: StatusCode) -> Result<(), ApiError> {
    if status == StatusCode::UNAUTHORIZED {
        return Err(ApiError::ReauthenticationRequired);
    }
    if status == StatusCode::FORBIDDEN {
        return Err(ApiError::Forbidden);
    }
    if !status.is_success() {
        return Err(ApiError::HttpStatus(status));
    }
    Ok(())
}

/// Accepts a GUID in hyphenated form (`8-4-4-4-12`) or simple form (32 hex
/// digits). Anything else is refused before a request is built.
fn validate_group_id(value: &str) -> Result<(), ApiError> {
    let bytes = value.as_bytes();
    let hyphenated = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    let simple = bytes.len() == 32 && bytes.iter().all(u8::is_ascii_hexdigit);
    if hyphenated || simple {
        Ok(())
    } else {
        Err(ApiError::InvalidResponse)
    }
}

fn validate_group_name(value: &str) -> Result<(), ApiError> {
    if value.encode_utf16().count() > MAX_GROUP_NAME_LENGTH {
        return Err(ApiError::InvalidResponse);
    }
    Ok(())
}

/// A SyncPlay playback command. This type only describes a request: it names
/// the route and builds the JSON body, and it never sends anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackCommand {
    /// `POST /SyncPlay/Pause`, no body.
    Pause,
    /// `POST /SyncPlay/Unpause`, no body.
    Unpause,
    /// `POST /SyncPlay/Stop`, no body.
    Stop,
    /// `POST /SyncPlay/Seek` with `{"PositionTicks": ...}`. The server reads
    /// this field as a 64-bit signed integer, so [`PlaybackCommand::body`]
    /// refuses values above `i64::MAX`.
    Seek { position_ticks: u64 },
    /// `POST /SyncPlay/Ping` with `{"Ping": ...}`.
    Ping { ping: i64 },
}

impl PlaybackCommand {
    /// Path segments of the route, relative to the server base URL.
    pub fn route(&self) -> &'static [&'static str] {
        match self {
            Self::Pause => &["SyncPlay", "Pause"],
            Self::Unpause => &["SyncPlay", "Unpause"],
            Self::Stop => &["SyncPlay", "Stop"],
            Self::Seek { .. } => &["SyncPlay", "Seek"],
            Self::Ping { .. } => &["SyncPlay", "Ping"],
        }
    }

    /// The JSON body for the route, or `None` when the route takes no body.
    /// Seek positions above `i64::MAX` are refused with
    /// [`ApiError::InvalidResponse`] before any request could be built.
    pub fn body(&self) -> Result<Option<Value>, ApiError> {
        match self {
            Self::Pause | Self::Unpause | Self::Stop => Ok(None),
            Self::Seek { position_ticks } => {
                let ticks =
                    i64::try_from(*position_ticks).map_err(|_| ApiError::InvalidResponse)?;
                Ok(Some(json!({ "PositionTicks": ticks })))
            }
            Self::Ping { ping } => Ok(Some(json!({ "Ping": ping }))),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::{
        GroupState, MAX_GROUP_NAME_LENGTH, PlayRequestDto, PlaybackCommand, SyncPlayGroup,
        SyncPlayPlaybackStatus, join_group_body, new_group_body, validate_group_id,
        validate_group_name,
    };
    use crate::{ApiError, ClientConfig, JellyfinClient, Session, UserDto};

    const GROUP: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";
    const ITEM_A: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3302";
    const ITEM_B: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3303";
    const PLAYLIST_ENTRY_ID: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3304";

    fn session() -> Session {
        Session {
            access_token: "fixture-token".into(),
            user: UserDto {
                policy: None,
                id: "user-id".into(),
                name: "fixture".into(),
                server_id: None,
            },
            server_id: None,
            session_info: None,
        }
    }

    async fn receive_request(listener: TcpListener, response_status: &str) -> String {
        let (mut socket, _) = listener.accept().await.expect("request connection");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            let count = socket.read(&mut chunk).await.expect("request bytes");
            assert_ne!(count, 0, "request ended before headers");
            request.extend_from_slice(&chunk[..count]);
            if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
            assert!(request.len() <= 16 * 1024, "request headers too large");
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .expect("content length");
        while request.len() - header_end < content_length {
            let count = socket.read(&mut chunk).await.expect("request body");
            assert_ne!(count, 0, "request ended before body");
            request.extend_from_slice(&chunk[..count]);
        }
        let text = String::from_utf8_lossy(&request).into_owned();
        let response =
            format!("HTTP/1.1 {response_status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write fixture response");
        text
    }

    #[test]
    fn accepts_hyphenated_and_simple_guids() {
        assert!(validate_group_id(GROUP).is_ok());
        assert!(validate_group_id("3F2504E04F8911D39A0C0305E82C3301").is_ok());
    }

    #[test]
    fn rejects_values_that_are_not_guids() {
        for value in [
            "",
            "not-a-guid",
            "3f2504e0-4f89-11d3-9a0c-0305e82c330",
            "3f2504e0_4f89-11d3-9a0c-0305e82c3301",
            "3f2504e0-4f89-11d3-9a0c-0305e82c33zz",
            " 3f2504e0-4f89-11d3-9a0c-0305e82c3301",
            "3f2504e0-4f89-11d3-9a0c-0305e82c3301,other",
        ] {
            assert!(
                matches!(validate_group_id(value), Err(ApiError::InvalidResponse)),
                "{value:?} must be refused"
            );
        }
    }

    #[test]
    fn group_name_length_is_capped() {
        assert!(validate_group_name("").is_ok());
        assert!(validate_group_name(&"a".repeat(MAX_GROUP_NAME_LENGTH)).is_ok());
        assert!(matches!(
            validate_group_name(&"a".repeat(MAX_GROUP_NAME_LENGTH + 1)),
            Err(ApiError::InvalidResponse)
        ));
    }

    #[test]
    fn request_bodies_use_pascal_case_property_names() {
        assert_eq!(
            serde_json::to_string(&new_group_body("Movie night")).expect("serialize"),
            r#"{"GroupName":"Movie night"}"#
        );
        assert_eq!(
            serde_json::to_string(&join_group_body(GROUP)).expect("serialize"),
            format!(r#"{{"GroupId":"{GROUP}"}}"#)
        );
    }

    #[test]
    fn play_request_body_matches_server_dto_fields() {
        let request = PlayRequestDto {
            playing_queue: vec![ITEM_A.into(), ITEM_B.into()],
            playing_item_position: 1,
            start_position_ticks: 9_876_543,
        };
        request.validate().expect("valid request");
        assert_eq!(
            request.body(),
            json!({
                "PlayingQueue": [ITEM_A, ITEM_B],
                "PlayingItemPosition": 1,
                "StartPositionTicks": 9876543,
            })
        );
    }

    #[test]
    fn play_request_rejects_empty_queue_bad_index_and_negative_ticks() {
        for request in [
            PlayRequestDto {
                playing_queue: vec![],
                playing_item_position: 0,
                start_position_ticks: 0,
            },
            PlayRequestDto {
                playing_queue: vec!["item".into()],
                playing_item_position: -1,
                start_position_ticks: 0,
            },
            PlayRequestDto {
                playing_queue: vec!["item".into()],
                playing_item_position: 1,
                start_position_ticks: 0,
            },
            PlayRequestDto {
                playing_queue: vec!["item".into()],
                playing_item_position: 0,
                start_position_ticks: -1,
            },
            PlayRequestDto {
                playing_queue: vec!["not-a-guid".into()],
                playing_item_position: 0,
                start_position_ticks: 0,
            },
        ] {
            assert!(matches!(request.validate(), Err(ApiError::InvalidResponse)));
        }
    }

    #[tokio::test]
    async fn set_new_queue_posts_typed_body_and_accepts_no_content() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(receive_request(listener, "204 No Content"));
        let client = JellyfinClient::new(
            ClientConfig::new(&format!("http://{address}"), "test", "test", "test", "test")
                .expect("config"),
        )
        .expect("client");
        client
            .set_sync_play_new_queue(&session(), &[ITEM_A.into(), ITEM_B.into()], 1, 9876543)
            .await
            .expect("204 is successful");
        let request = server.await.expect("fixture task");
        let (headers, body) = request.split_once("\r\n\r\n").expect("request format");
        assert!(headers.starts_with("POST /SyncPlay/SetNewQueue HTTP/1.1\r\n"));
        assert!(headers.to_ascii_lowercase().contains("authorization:"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).expect("JSON body"),
            json!({
                "PlayingQueue": [ITEM_A, ITEM_B],
                "PlayingItemPosition": 1,
                "StartPositionTicks": 9876543,
            })
        );
    }

    #[tokio::test]
    async fn set_new_queue_maps_http_errors_like_other_no_content_calls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(receive_request(listener, "401 Unauthorized"));
        let client = JellyfinClient::new(
            ClientConfig::new(&format!("http://{address}"), "test", "test", "test", "test")
                .expect("config"),
        )
        .expect("client");
        let result = client
            .set_sync_play_new_queue(&session(), &[ITEM_A.into()], 0, 0)
            .await;
        assert!(matches!(result, Err(ApiError::ReauthenticationRequired)));
        let _ = server.await.expect("fixture task");
    }

    #[tokio::test]
    async fn invalid_set_new_queue_is_rejected_before_http_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let client = JellyfinClient::new(
            ClientConfig::new(&format!("http://{address}"), "test", "test", "test", "test")
                .expect("config"),
        )
        .expect("client");
        let result = client.set_sync_play_new_queue(&session(), &[], 0, 0).await;
        assert!(matches!(result, Err(ApiError::InvalidResponse)));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ready_and_buffering_send_the_server_clock_and_playlist_entry_id() {
        for (route, is_ready, is_playing) in [("Ready", true, true), ("Buffering", false, true)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(receive_request(listener, "204 No Content"));
            let client = JellyfinClient::new(
                ClientConfig::new(&format!("http://{address}"), "test", "test", "test", "test")
                    .expect("config"),
            )
            .expect("client");
            let status = SyncPlayPlaybackStatus {
                when: "2026-10-24T20:00:00.1250000Z".into(),
                position_ticks: 9_876_543,
                is_playing,
                playlist_item_id: PLAYLIST_ENTRY_ID.into(),
            };
            let result = if is_ready {
                client.sync_play_ready(&session(), &status).await
            } else {
                client.sync_play_buffering(&session(), &status).await
            };
            result.expect("204 is successful");

            let request = server.await.expect("fixture task");
            let (headers, body) = request.split_once("\r\n\r\n").expect("request format");
            assert!(headers.starts_with(&format!("POST /SyncPlay/{route} HTTP/1.1\r\n")));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(body).expect("JSON body"),
                json!({
                    "When": "2026-10-24T20:00:00.1250000Z",
                    "PositionTicks": 9876543,
                    "IsPlaying": is_playing,
                    "PlaylistItemId": PLAYLIST_ENTRY_ID,
                })
            );
        }
    }

    #[test]
    fn playback_status_rejects_unbounded_or_malformed_local_values() {
        for status in [
            SyncPlayPlaybackStatus {
                when: String::new(),
                position_ticks: 0,
                is_playing: false,
                playlist_item_id: PLAYLIST_ENTRY_ID.into(),
            },
            SyncPlayPlaybackStatus {
                when: "2026-10-24T20:00:00Z".into(),
                position_ticks: u64::MAX,
                is_playing: false,
                playlist_item_id: PLAYLIST_ENTRY_ID.into(),
            },
            SyncPlayPlaybackStatus {
                when: "2026-10-24T20:00:00Z".into(),
                position_ticks: 0,
                is_playing: false,
                playlist_item_id: "not-a-guid".into(),
            },
        ] {
            assert!(matches!(status.body(), Err(ApiError::InvalidResponse)));
        }
    }

    #[test]
    fn parses_group_list_in_pascal_case() {
        let body = r#"[{"GroupId":"3f2504e0-4f89-11d3-9a0c-0305e82c3301","GroupName":"Movie night","State":"Paused","Participants":["alice","bob"],"LastUpdatedAt":"2026-10-15T20:00:00.0000000Z"}]"#;
        let groups: Vec<SyncPlayGroup> = serde_json::from_str(body).expect("valid fixture");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_id, GROUP);
        assert_eq!(groups[0].group_name, "Movie night");
        assert_eq!(groups[0].state, GroupState::Paused);
        assert_eq!(groups[0].participants, vec!["alice", "bob"]);
        assert_eq!(groups[0].last_updated_at, "2026-10-15T20:00:00.0000000Z");
    }

    #[test]
    fn missing_and_unknown_fields_take_defaults() {
        let group: SyncPlayGroup =
            serde_json::from_str(&json!({"GroupId": GROUP, "State": "Buffering"}).to_string())
                .expect("valid fixture");
        assert_eq!(group.group_id, GROUP);
        assert_eq!(group.state, GroupState::Unknown);
        assert_eq!(group.group_name, "");
        assert!(group.participants.is_empty());
        assert_eq!(group.last_updated_at, "");
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "PascalCase", deny_unknown_fields)]
    struct SeekBody {
        position_ticks: u64,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "PascalCase", deny_unknown_fields)]
    struct PingBody {
        ping: i64,
    }

    #[test]
    fn pause_and_unpause_routes_send_no_body() {
        assert_eq!(PlaybackCommand::Pause.route(), &["SyncPlay", "Pause"]);
        assert_eq!(PlaybackCommand::Unpause.route(), &["SyncPlay", "Unpause"]);
        assert_eq!(PlaybackCommand::Stop.route(), &["SyncPlay", "Stop"]);
        assert_eq!(PlaybackCommand::Pause.body().expect("no body"), None);
        assert_eq!(PlaybackCommand::Unpause.body().expect("no body"), None);
        assert_eq!(PlaybackCommand::Stop.body().expect("no body"), None);
    }

    #[test]
    fn seek_body_serializes_position_ticks_property() {
        let command = PlaybackCommand::Seek {
            position_ticks: 1_234_500_000,
        };
        assert_eq!(command.route(), &["SyncPlay", "Seek"]);
        let body = command
            .body()
            .expect("valid seek")
            .expect("seek has a body");
        assert_eq!(
            serde_json::to_string(&body).expect("serialize"),
            r#"{"PositionTicks":1234500000}"#
        );
    }

    #[test]
    fn seek_round_trips_zero_and_int64_maximum_ticks() {
        for ticks in [0_u64, i64::MAX as u64] {
            let command = PlaybackCommand::Seek {
                position_ticks: ticks,
            };
            let text = serde_json::to_string(
                &command
                    .body()
                    .expect("valid seek")
                    .expect("seek has a body"),
            )
            .expect("serialize");
            let parsed: SeekBody = serde_json::from_str(&text).expect("deserialize");
            assert_eq!(
                parsed,
                SeekBody {
                    position_ticks: ticks
                }
            );
        }
        let zero = PlaybackCommand::Seek { position_ticks: 0 }
            .body()
            .expect("valid seek")
            .expect("seek has a body");
        assert_eq!(
            serde_json::to_string(&zero).expect("serialize"),
            r#"{"PositionTicks":0}"#
        );
    }

    #[test]
    fn seek_refuses_ticks_above_int64_maximum() {
        let command = PlaybackCommand::Seek {
            position_ticks: i64::MAX as u64 + 1,
        };
        assert!(matches!(command.body(), Err(ApiError::InvalidResponse)));
        let command = PlaybackCommand::Seek {
            position_ticks: u64::MAX,
        };
        assert!(matches!(command.body(), Err(ApiError::InvalidResponse)));
    }

    #[test]
    fn ping_body_round_trips_at_int64_limit() {
        let command = PlaybackCommand::Ping { ping: i64::MAX };
        assert_eq!(command.route(), &["SyncPlay", "Ping"]);
        let body = command
            .body()
            .expect("valid ping")
            .expect("ping has a body");
        let text = serde_json::to_string(&body).expect("serialize");
        assert_eq!(text, format!(r#"{{"Ping":{}}}"#, i64::MAX));
        let parsed: PingBody = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(parsed, PingBody { ping: i64::MAX });
    }
}
