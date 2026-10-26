//! Authenticated playback lifecycle reports for Jellyfin sessions.
//!
//! Jellyfin's start and progress DTOs share the same fields: on the server,
//! `PlaybackStartInfo` inherits `PlaybackProgressInfo`. Session identity is
//! resolved by Jellyfin from the authenticated request, so these request types
//! intentionally omit `SessionId` and any client-supplied `UserId`.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Serialize;

use crate::{ApiError, JellyfinClient, Session};

/// Number of 100-nanosecond Jellyfin ticks in one second.
pub const TICKS_PER_SECOND: i64 = 10_000_000;

/// A Jellyfin/.NET time tick, where one tick is exactly 100 nanoseconds.
///
/// Serialization is the raw signed 64-bit integer expected by Jellyfin, not a
/// floating-point number of seconds. Use [`JellyfinTicks::from_duration`]
/// when converting a Rust [`Duration`].
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct JellyfinTicks(i64);

impl JellyfinTicks {
    /// Construct from an exact count of 100-nanosecond ticks.
    pub const fn from_ticks(ticks: i64) -> Self {
        Self(ticks)
    }

    /// Return the exact signed tick count serialized to Jellyfin.
    pub const fn as_ticks(self) -> i64 {
        self.0
    }

    /// Convert a duration to ticks, truncating any sub-100-nanosecond remainder.
    /// Returns `None` if the duration is too large for Jellyfin's signed 64-bit
    /// tick field.
    pub fn from_duration(duration: Duration) -> Option<Self> {
        i64::try_from(duration.as_nanos() / 100).ok().map(Self)
    }
}

/// Jellyfin's playback method enum, serialized using its API string values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PlayMethod {
    #[serde(rename = "Transcode")]
    Transcode,
    #[serde(rename = "DirectStream")]
    DirectStream,
    #[serde(rename = "DirectPlay")]
    DirectPlay,
}

/// Jellyfin's playback queue repeat mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum RepeatMode {
    #[serde(rename = "RepeatNone")]
    None,
    #[serde(rename = "RepeatAll")]
    All,
    #[serde(rename = "RepeatOne")]
    One,
}

/// Jellyfin's playback queue ordering mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PlaybackOrder {
    #[serde(rename = "Default")]
    Default,
    #[serde(rename = "Shuffle")]
    Shuffle,
}

/// A queue entry accepted by Jellyfin's playback report DTOs.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct QueueItem {
    /// Jellyfin item identifier (a UUID string in the API contract).
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,
}

impl QueueItem {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            playlist_item_id: None,
        }
    }
}

/// Body fields shared by `POST /Sessions/Playing` and
/// `POST /Sessions/Playing/Progress`.
///
/// `item_id` is required by this client API because Jellyfin's server DTO uses
/// a non-nullable GUID. Optional fields not set by the caller are omitted.
/// `VolumeLevel` is an integer in Jellyfin's server model, and all position
/// fields use [`JellyfinTicks`].
#[derive(Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackProgressInfo {
    pub item_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_seek: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_paused: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_muted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ticks: Option<JellyfinTicks>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_start_time_ticks: Option<JellyfinTicks>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_level: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_method: Option<PlayMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_mode: Option<RepeatMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_order: Option<PlaybackOrder>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_playing_queue: Option<Vec<QueueItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,
}

impl PlaybackProgressInfo {
    pub fn new(item_id: impl Into<String>) -> Self {
        Self {
            item_id: item_id.into(),
            can_seek: None,
            media_source_id: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
            is_paused: None,
            is_muted: None,
            position_ticks: None,
            playback_start_time_ticks: None,
            volume_level: None,
            brightness: None,
            aspect_ratio: None,
            play_method: None,
            live_stream_id: None,
            play_session_id: None,
            repeat_mode: None,
            playback_order: None,
            now_playing_queue: None,
            playlist_item_id: None,
        }
    }
}

/// The start DTO inherits the same request fields as the progress DTO in the
/// official Jellyfin server model.
pub type PlaybackStartInfo = PlaybackProgressInfo;

/// Body for `POST /Sessions/Playing/Stopped`.
#[derive(Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackStopInfo {
    pub item_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ticks: Option<JellyfinTicks>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    pub failed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_playing_queue: Option<Vec<QueueItem>>,
}

impl PlaybackStopInfo {
    pub fn new(item_id: impl Into<String>) -> Self {
        Self {
            item_id: item_id.into(),
            media_source_id: None,
            position_ticks: None,
            live_stream_id: None,
            play_session_id: None,
            failed: false,
            next_media_type: None,
            playlist_item_id: None,
            now_playing_queue: None,
        }
    }
}

impl JellyfinClient {
    /// Report playback start using the authenticated session.
    ///
    /// The session token and the configured client/device metadata are sent via
    /// the existing MediaBrowser `Authorization` header. Jellyfin resolves the
    /// user and current session from that authenticated context; its controller
    /// overwrites `SessionId` from the request context, so neither identifier
    /// is accepted in this body.
    pub async fn report_playback_start(
        &self,
        session: &Session,
        info: &PlaybackStartInfo,
    ) -> Result<(), ApiError> {
        self.post_session_report(session, &["Sessions", "Playing"], info)
            .await
    }

    /// Report current playback progress using the authenticated session.
    pub async fn report_playback_progress(
        &self,
        session: &Session,
        info: &PlaybackProgressInfo,
    ) -> Result<(), ApiError> {
        self.post_session_report(session, &["Sessions", "Playing", "Progress"], info)
            .await
    }

    /// Report playback stopped using the authenticated session.
    pub async fn report_playback_stopped(
        &self,
        session: &Session,
        info: &PlaybackStopInfo,
    ) -> Result<(), ApiError> {
        self.post_session_report(session, &["Sessions", "Playing", "Stopped"], info)
            .await
    }

    async fn post_session_report<T: Serialize + ?Sized>(
        &self,
        session: &Session,
        route: &[&str],
        body: &T,
    ) -> Result<(), ApiError> {
        let url = self.endpoint(route);
        let request = self.http.post(url).json(body);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_empty_response(response.status())
    }
}

fn decode_empty_response(status: StatusCode) -> Result<(), ApiError> {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{
        JellyfinTicks, PlayMethod, PlaybackOrder, PlaybackProgressInfo, PlaybackStartInfo,
        PlaybackStopInfo, QueueItem, RepeatMode, TICKS_PER_SECOND,
    };

    fn assert_json(actual: Value, expected: Value) {
        assert!(
            actual == expected,
            "serialized playback report did not match the expected JSON shape"
        );
    }

    fn assert_duration_ticks(duration: Duration, expected_ticks: Option<i64>) {
        assert!(
            JellyfinTicks::from_duration(duration).map(JellyfinTicks::as_ticks) == expected_ticks,
            "duration converted to an unexpected Jellyfin tick count"
        );
    }

    #[test]
    fn jellyfin_ticks_serialize_as_signed_integer_counts() {
        assert_json(
            serde_json::to_value(JellyfinTicks::from_ticks(-17)).unwrap(),
            json!(-17),
        );
        assert_eq!(TICKS_PER_SECOND, 10_000_000);
    }

    #[test]
    fn duration_conversion_truncates_sub_tick_remainders_and_rejects_overflow() {
        assert_duration_ticks(Duration::ZERO, Some(0));
        assert_duration_ticks(Duration::from_nanos(99), Some(0));
        assert_duration_ticks(Duration::from_nanos(100), Some(1));
        assert_duration_ticks(Duration::from_nanos(199), Some(1));
        assert_duration_ticks(Duration::from_secs(1), Some(TICKS_PER_SECOND));
        assert_duration_ticks(Duration::from_secs(u64::MAX), None);
    }

    #[test]
    fn start_and_progress_serialize_exact_pascal_case_fields() {
        let mut progress = PlaybackProgressInfo::new("item-id");
        progress.can_seek = Some(true);
        progress.media_source_id = Some("source-id".into());
        progress.audio_stream_index = Some(2);
        progress.subtitle_stream_index = Some(3);
        progress.is_paused = Some(false);
        progress.is_muted = Some(true);
        progress.position_ticks = Some(JellyfinTicks::from_ticks(12_345_678));
        progress.playback_start_time_ticks = Some(JellyfinTicks::from_ticks(TICKS_PER_SECOND));
        progress.volume_level = Some(88);
        progress.brightness = Some(71);
        progress.aspect_ratio = Some("16:9".into());
        progress.play_method = Some(PlayMethod::DirectPlay);
        progress.live_stream_id = Some("live-stream-id".into());
        progress.play_session_id = Some("play-session-id".into());
        progress.repeat_mode = Some(RepeatMode::None);
        progress.playback_order = Some(PlaybackOrder::Shuffle);
        progress.now_playing_queue = Some(vec![QueueItem {
            id: "queued-item-id".into(),
            playlist_item_id: Some("queue-entry-id".into()),
        }]);
        progress.playlist_item_id = Some("current-queue-entry-id".into());

        let expected = json!({
            "ItemId": "item-id",
            "CanSeek": true,
            "MediaSourceId": "source-id",
            "AudioStreamIndex": 2,
            "SubtitleStreamIndex": 3,
            "IsPaused": false,
            "IsMuted": true,
            "PositionTicks": 12_345_678,
            "PlaybackStartTimeTicks": TICKS_PER_SECOND,
            "VolumeLevel": 88,
            "Brightness": 71,
            "AspectRatio": "16:9",
            "PlayMethod": "DirectPlay",
            "LiveStreamId": "live-stream-id",
            "PlaySessionId": "play-session-id",
            "RepeatMode": "RepeatNone",
            "PlaybackOrder": "Shuffle",
            "NowPlayingQueue": [{
                "Id": "queued-item-id",
                "PlaylistItemId": "queue-entry-id"
            }],
            "PlaylistItemId": "current-queue-entry-id"
        });

        let start: PlaybackStartInfo = progress.clone();
        assert_json(serde_json::to_value(&start).unwrap(), expected.clone());
        assert_json(serde_json::to_value(&progress).unwrap(), expected);
    }

    #[test]
    fn stop_serializes_exact_fields_and_queue_item_omission() {
        let mut stop = PlaybackStopInfo::new("item-id");
        stop.media_source_id = Some("source-id".into());
        stop.position_ticks = Some(JellyfinTicks::from_ticks(42));
        stop.live_stream_id = Some("live-stream-id".into());
        stop.play_session_id = Some("play-session-id".into());
        stop.failed = true;
        stop.next_media_type = Some("Audio".into());
        stop.playlist_item_id = Some("current-queue-entry-id".into());
        stop.now_playing_queue = Some(vec![QueueItem::new("next-item-id")]);

        assert_json(
            serde_json::to_value(stop).unwrap(),
            json!({
                "ItemId": "item-id",
                "MediaSourceId": "source-id",
                "PositionTicks": 42,
                "LiveStreamId": "live-stream-id",
                "PlaySessionId": "play-session-id",
                "Failed": true,
                "NextMediaType": "Audio",
                "PlaylistItemId": "current-queue-entry-id",
                "NowPlayingQueue": [{ "Id": "next-item-id" }]
            }),
        );
    }

    #[test]
    fn report_constructors_omit_unset_optionals_but_keep_required_fields() {
        assert_json(
            serde_json::to_value(PlaybackStartInfo::new("start-item")).unwrap(),
            json!({ "ItemId": "start-item" }),
        );
        assert_json(
            serde_json::to_value(PlaybackProgressInfo::new("progress-item")).unwrap(),
            json!({ "ItemId": "progress-item" }),
        );
        assert_json(
            serde_json::to_value(PlaybackStopInfo::new("stop-item")).unwrap(),
            json!({ "ItemId": "stop-item", "Failed": false }),
        );
    }
}
