//! Typed Live TV calls: the channel list and the program guide.
//!
//! Endpoints, confirmed against the official OpenAPI document at
//! `https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json`:
//!
//! - `GET /LiveTv/Channels` (`GetLiveTvChannels`) with `userId`, `startIndex`,
//!   `limit`, and `addCurrentProgram`
//! - `GET /LiveTv/Programs` (`GetLiveTvPrograms`) with `channelIds`, `userId`,
//!   `minStartDate`, `maxStartDate`, `startIndex`, and `limit`
//!
//! [UNCONFIRMED] The OpenAPI gives no `style` or `explode` for the `channelIds`
//! array. The IDs are sent as one comma-joined value, as the playlist ID lists
//! are. The live server's accepted encoding has not been checked.
//!
//! [UNCONFIRMED] The OpenAPI does not say whether `maxStartDate` is inclusive.
//! This client sends the window as given. The mock treats it as exclusive.
//!
//! Channel logo requests use the generic item-image route. `GetLiveTvChannels`
//! returns `BaseItemDto` image tags; Jellyfin's `ImageController.GetItemImage`
//! resolves any library `BaseItem`, including a channel, at
//! `GET /Items/{itemId}/Images/{imageType}`.
//!
//! Guide windows are UTC instants. They are validated when the [`GuideWindow`]
//! is built, so an invalid window cannot reach the network. Channel IDs are
//! checked before they are joined into the query.

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::{
    ApiError, BaseItemDto, DeviceProfile, JellyfinClient, MediaSourceInfo, Session,
    decode_response, home::MAX_HOME_LIMIT, playlists::validate_playlist_id,
};

/// Longest guide window the client requests: the 24 hours in M3-04.
pub const MAX_GUIDE_WINDOW_SECONDS: i64 = 24 * 60 * 60;

/// Most channel IDs sent in one guide request. Callers page the guide rather
/// than list more channels at once.
pub const MAX_GUIDE_CHANNEL_IDS: usize = MAX_HOME_LIMIT as usize;
/// Maximum number of channels accumulated into one UI guide window.
pub const MAX_GUIDE_CHANNELS_PER_WINDOW: usize = 1_000;
/// Bound on the number of programs accumulated for one UI guide window.
pub const MAX_GUIDE_PROGRAMS_PER_WINDOW: usize = 10_000;
/// Maximum JSON body size for a LiveStreams/Open request.
pub const MAX_OPEN_LIVE_STREAM_BODY_BYTES: usize = 256 * 1024;
/// Maximum accepted streaming bitrate for an OpenLiveStream request.
pub const MAX_LIVE_STREAM_BITRATE: i32 = 2_000_000_000;
/// Maximum accepted audio channel count for an OpenLiveStream request.
pub const MAX_LIVE_AUDIO_CHANNELS: i32 = 64;
const MAX_LIVE_STREAM_ID_LENGTH: usize = 256;

const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// JSON body for `POST /LiveStreams/Open`. It deliberately has no `Debug`
/// implementation because the embedded profile may later gain sensitive
/// extension fields.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct OpenLiveStreamBody<'a> {
    user_id: &'a str,
    item_id: &'a str,
    device_profile: &'a DeviceProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_streaming_bitrate: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_time_ticks: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_stream_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle_stream_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_audio_channels: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_direct_play: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_direct_stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    always_burn_in_subtitle_when_transcoding: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_play_protocols: Option<&'a [String]>,
}

/// Response from `POST /LiveStreams/Open`. The source intentionally has no
/// `Debug` implementation: it can contain local paths, opaque tokens, headers,
/// or signed URLs.
#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct LiveStreamResponse {
    pub media_source: MediaSourceInfo,
    #[serde(default)]
    pub play_session_id: Option<String>,
}

/// Open a Live TV source and retain the server's media source for the caller.
/// All request values are checked before any network request is sent.
#[allow(clippy::too_many_arguments)]
async fn open_live_stream(
    client: &JellyfinClient,
    session: &Session,
    item_id: &str,
    device_profile: &DeviceProfile,
    max_streaming_bitrate: Option<i32>,
    start_time_ticks: Option<i64>,
    audio_stream_index: Option<i32>,
    subtitle_stream_index: Option<i32>,
    max_audio_channels: Option<i32>,
    enable_direct_play: Option<bool>,
    enable_direct_stream: Option<bool>,
    always_burn_in_subtitle_when_transcoding: Option<bool>,
    direct_play_protocols: Option<&[String]>,
) -> Result<LiveStreamResponse, ApiError> {
    validate_playlist_id(item_id)?;
    if max_streaming_bitrate.is_some_and(|value| !(1..=MAX_LIVE_STREAM_BITRATE).contains(&value))
        || start_time_ticks.is_some_and(|value| value < 0)
        || audio_stream_index.is_some_and(|value| value < 0)
        || subtitle_stream_index.is_some_and(|value| value < 0)
        || max_audio_channels.is_some_and(|value| !(1..=MAX_LIVE_AUDIO_CHANNELS).contains(&value))
        || direct_play_protocols.is_some_and(|values| {
            values.is_empty()
                || values.len() > 8
                || values.iter().any(|value| {
                    value.is_empty()
                        || value.len() > 32
                        || !value
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || character == '_')
                })
        })
    {
        return Err(ApiError::InvalidConfiguration);
    }

    let body = OpenLiveStreamBody {
        user_id: &session.user.id,
        item_id,
        device_profile,
        max_streaming_bitrate,
        start_time_ticks,
        audio_stream_index,
        subtitle_stream_index,
        max_audio_channels,
        enable_direct_play,
        enable_direct_stream,
        always_burn_in_subtitle_when_transcoding,
        direct_play_protocols,
    };
    let body = serde_json::to_vec(&body).map_err(|_| ApiError::InvalidResponse)?;
    if body.len() > MAX_OPEN_LIVE_STREAM_BODY_BYTES {
        return Err(ApiError::JsonResponseTooLarge);
    }
    let request = client
        .http
        .post(client.endpoint(&["LiveStreams", "Open"]))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    let request = client.add_metadata(request, Some(&session.access_token));
    let response: LiveStreamResponse =
        decode_response(request.send().await.map_err(crate::transport_error)?).await?;
    if response
        .media_source
        .live_stream_id
        .as_deref()
        .is_none_or(|id| {
            id.is_empty()
                || id.len() > MAX_LIVE_STREAM_ID_LENGTH
                || id.chars().any(char::is_control)
        })
    {
        return Err(ApiError::InvalidResponse);
    }
    Ok(response)
}

/// A Live TV channel from `GET /LiveTv/Channels`.
///
/// `channel_number` is the server's `ChannelNumber`, kept as text because
/// channel numbers can contain a dot (for example `5.1`). Sorting by number
/// belongs to the client model, not to this typed call.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct LiveTvChannel {
    #[serde(flatten)]
    pub item: BaseItemDto,
    #[serde(default)]
    pub channel_number: Option<String>,
    /// Channel list endpoint number (`Number`). Programs and other item DTOs
    /// can instead populate `ChannelNumber`; callers should prefer this field.
    #[serde(default)]
    pub number: Option<String>,
    #[serde(default)]
    pub channel_type: Option<String>,
    /// The program airing now. The server fills it when `addCurrentProgram`
    /// is requested.
    #[serde(default)]
    pub current_program: Option<LiveTvProgram>,
}

/// A scheduled program, as the server describes it. Used for a channel's
/// `CurrentProgram`. The times are the server's UTC strings, unparsed.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct LiveTvProgram {
    #[serde(flatten)]
    pub item: BaseItemDto,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
}

/// One page of channels, in server order.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct LiveTvChannelPage {
    pub items: Vec<LiveTvChannel>,
    pub total_record_count: Option<u32>,
    pub start_index: u32,
}

/// One page of programs, in server order.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct LiveTvProgramPage {
    pub items: Vec<LiveTvProgram>,
    pub total_record_count: Option<u32>,
    pub start_index: u32,
}

/// A validated UTC window for the program guide, from `start_unix` (inclusive)
/// to `end_unix` (exclusive), in seconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuideWindow {
    start_unix: i64,
    end_unix: i64,
}

/// Why a guide window was refused. Refused windows never reach the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GuideWindowError {
    #[error("guide window must end after it starts")]
    NotIncreasing,
    #[error("guide window is longer than 24 hours")]
    TooLong,
}

impl GuideWindow {
    /// Validate a window. The end must be after the start, and the window may
    /// be at most [`MAX_GUIDE_WINDOW_SECONDS`] long.
    pub fn new(start_unix: i64, end_unix: i64) -> Result<Self, GuideWindowError> {
        match end_unix.checked_sub(start_unix) {
            Some(length) if length <= 0 => Err(GuideWindowError::NotIncreasing),
            Some(length) if length <= MAX_GUIDE_WINDOW_SECONDS => Ok(Self {
                start_unix,
                end_unix,
            }),
            _ => Err(GuideWindowError::TooLong),
        }
    }

    /// Window start, in seconds since the Unix epoch (UTC).
    pub fn start_unix(&self) -> i64 {
        self.start_unix
    }

    /// Window end, in seconds since the Unix epoch (UTC).
    pub fn end_unix(&self) -> i64 {
        self.end_unix
    }
}

impl JellyfinClient {
    /// Open a Live TV stream (`POST /LiveStreams/Open`). The optional values
    /// mirror Jellyfin's `OpenLiveStreamDto`; invalid or oversized values are
    /// rejected before any request is sent.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_live_stream(
        &self,
        session: &Session,
        item_id: &str,
        device_profile: &DeviceProfile,
        max_streaming_bitrate: Option<i32>,
        start_time_ticks: Option<i64>,
        audio_stream_index: Option<i32>,
        subtitle_stream_index: Option<i32>,
        max_audio_channels: Option<i32>,
        enable_direct_play: Option<bool>,
        enable_direct_stream: Option<bool>,
        always_burn_in_subtitle_when_transcoding: Option<bool>,
        direct_play_protocols: Option<&[String]>,
    ) -> Result<LiveStreamResponse, ApiError> {
        open_live_stream(
            self,
            session,
            item_id,
            device_profile,
            max_streaming_bitrate,
            start_time_ticks,
            audio_stream_index,
            subtitle_stream_index,
            max_audio_channels,
            enable_direct_play,
            enable_direct_stream,
            always_burn_in_subtitle_when_transcoding,
            direct_play_protocols,
        )
        .await
    }

    /// Close a previously opened stream (`POST /LiveStreams/Close`).
    /// The live-stream ID is query-encoded by reqwest and never logged.
    pub async fn close_live_stream(
        &self,
        session: &Session,
        live_stream_id: &str,
    ) -> Result<(), ApiError> {
        if live_stream_id.is_empty()
            || live_stream_id.len() > MAX_LIVE_STREAM_ID_LENGTH
            || live_stream_id.chars().any(char::is_control)
        {
            return Err(ApiError::InvalidConfiguration);
        }
        let request = self
            .http
            .post(self.endpoint(&["LiveStreams", "Close"]))
            .query(&[("liveStreamId", live_stream_id)]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        if !response.status().is_success() {
            return decode_response::<serde_json::Value>(response)
                .await
                .map(|_| ());
        }
        Ok(())
    }

    /// Fetch one page of Live TV channels (`GET /LiveTv/Channels`).
    /// `limit` is clamped to `1..=MAX_HOME_LIMIT`, so the page size is at most 100.
    pub async fn list_live_tv_channels(
        &self,
        session: &Session,
        start_index: u32,
        limit: u32,
    ) -> Result<LiveTvChannelPage, ApiError> {
        let query = [
            ("userId", session.user.id.clone()),
            ("startIndex", start_index.to_string()),
            ("limit", limit.clamp(1, MAX_HOME_LIMIT).to_string()),
            ("addCurrentProgram", "true".to_owned()),
            ("enableImages", "true".to_owned()),
            ("enableUserData", "true".to_owned()),
        ];
        self.live_tv_get_json(session, &["LiveTv", "Channels"], &query)
            .await
    }

    /// Fetch the channel's primary logo as a small JPEG thumbnail.
    ///
    /// Channel DTOs use the same `ImageTags.Primary` and `Items/{id}/Images/Primary`
    /// route as other Jellyfin library items. The server source routes this through
    /// `ImageController.GetItemImage`, which resolves the ID to a `BaseItem`.
    pub async fn get_live_tv_channel_logo(
        &self,
        session: &Session,
        channel_id: &str,
        image_tag: Option<&str>,
    ) -> Result<Vec<u8>, ApiError> {
        validate_playlist_id(channel_id)?;
        self.get_primary_image_at(
            session,
            &["Items", channel_id, "Images", "Primary"],
            image_tag,
            96,
            96,
        )
        .await
    }

    /// Fetch one page of programs that start inside `window`
    /// (`GET /LiveTv/Programs` with `minStartDate` and `maxStartDate`).
    ///
    /// `channel_ids` limits the guide to those channels. An empty list asks the
    /// server for every channel, so the guide view should always pass its
    /// visible channels. More than [`MAX_GUIDE_CHANNEL_IDS`] IDs, or any ID
    /// that is not a GUID, is refused before a request is sent.
    pub async fn list_live_tv_programs(
        &self,
        session: &Session,
        window: GuideWindow,
        channel_ids: &[String],
        start_index: u32,
        limit: u32,
    ) -> Result<LiveTvProgramPage, ApiError> {
        if channel_ids.len() > MAX_GUIDE_CHANNEL_IDS {
            return Err(ApiError::InvalidConfiguration);
        }
        channel_ids
            .iter()
            .try_for_each(|id| validate_playlist_id(id))?;

        let mut query = vec![
            ("minStartDate", format_utc(window.start_unix)),
            ("maxStartDate", format_utc(window.end_unix)),
            ("userId", session.user.id.clone()),
            ("startIndex", start_index.to_string()),
            ("limit", limit.clamp(1, MAX_HOME_LIMIT).to_string()),
        ];
        if !channel_ids.is_empty() {
            query.push(("channelIds", channel_ids.join(",")));
        }
        self.live_tv_get_json(session, &["LiveTv", "Programs"], &query)
            .await
    }

    /// Fetch every program page for the requested channels/window while
    /// keeping the aggregate response bounded.
    pub async fn list_live_tv_programs_for_window(
        &self,
        session: &Session,
        window: GuideWindow,
        channel_ids: &[String],
    ) -> Result<LiveTvProgramPage, ApiError> {
        if channel_ids.len() > MAX_GUIDE_CHANNELS_PER_WINDOW {
            return Err(ApiError::InvalidConfiguration);
        }
        if channel_ids.is_empty() {
            return Ok(LiveTvProgramPage::default());
        }
        let mut items = Vec::new();
        let mut aggregate_total = 0_u32;
        for channel_batch in channel_ids.chunks(MAX_GUIDE_CHANNEL_IDS) {
            let batch_start = items.len();
            let mut start_index = 0_u32;
            let mut batch_total = None;
            loop {
                let page = self
                    .list_live_tv_programs(
                        session,
                        window,
                        channel_batch,
                        start_index,
                        MAX_HOME_LIMIT,
                    )
                    .await?;
                batch_total = page.total_record_count.or(batch_total);
                if let Some(total) = batch_total
                    && usize::try_from(total)
                        .map_or(true, |total| total > MAX_GUIDE_PROGRAMS_PER_WINDOW)
                {
                    return Err(ApiError::JsonResponseTooLarge);
                }
                let count =
                    u32::try_from(page.items.len()).map_err(|_| ApiError::InvalidResponse)?;
                if items.len().saturating_add(page.items.len()) > MAX_GUIDE_PROGRAMS_PER_WINDOW {
                    return Err(ApiError::JsonResponseTooLarge);
                }
                items.extend(page.items);
                let next_index = start_index
                    .checked_add(count)
                    .ok_or(ApiError::InvalidResponse)?;
                if count == 0
                    || batch_total.is_some_and(|total| next_index >= total)
                    || (batch_total.is_none() && count < MAX_HOME_LIMIT)
                {
                    break;
                }
                if next_index <= start_index {
                    return Err(ApiError::InvalidResponse);
                }
                start_index = next_index;
            }
            let batch_count =
                u32::try_from(items.len() - batch_start).map_err(|_| ApiError::InvalidResponse)?;
            aggregate_total = aggregate_total
                .checked_add(batch_total.unwrap_or(batch_count))
                .ok_or(ApiError::JsonResponseTooLarge)?;
            if usize::try_from(aggregate_total)
                .map_or(true, |total| total > MAX_GUIDE_PROGRAMS_PER_WINDOW)
            {
                return Err(ApiError::JsonResponseTooLarge);
            }
        }
        Ok(LiveTvProgramPage {
            items,
            total_record_count: Some(aggregate_total),
            start_index: 0,
        })
    }

    async fn live_tv_get_json<T: for<'de> Deserialize<'de>>(
        &self,
        session: &Session,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<T, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(segments))
            .query(query);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }
}

/// Format a Unix time as `YYYY-MM-DDTHH:MM:SSZ`, the UTC form the guide query
/// uses. Fixed width, so the mock can compare these strings in order.
fn format_utc(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(SECONDS_PER_DAY);
    let second_of_day = unix_seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60,
    )
}

/// Proleptic Gregorian date for a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days` algorithm).
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{
        GuideWindow, GuideWindowError, LiveTvChannelPage, LiveTvProgramPage,
        MAX_GUIDE_WINDOW_SECONDS, format_utc,
    };

    #[test]
    fn parses_channel_page_with_current_program_and_unnumbered_channel() {
        let page: LiveTvChannelPage = serde_json::from_str(
            r#"{
                "Items": [
                    {
                        "Id": "0123456789abcdef0123456789abcdef",
                        "Name": "News One",
                        "Type": "TvChannel",
                        "ChannelNumber": "5.1",
                        "ChannelType": "TV",
                        "CurrentProgram": {
                            "Id": "fedcba9876543210fedcba9876543210",
                            "Name": "Evening News",
                            "ChannelId": "0123456789abcdef0123456789abcdef",
                            "StartDate": "2026-10-15T18:00:00.0000000Z",
                            "EndDate": "2026-10-15T19:00:00.0000000Z"
                        },
                        "FutureField": true
                    },
                    {"Id": "11111111111111111111111111111111", "Name": "Unnumbered"}
                ],
                "TotalRecordCount": 2,
                "StartIndex": 0
            }"#,
        )
        .expect("valid channel page fixture");

        assert_eq!(page.total_record_count, Some(2));
        assert_eq!(page.start_index, 0);
        assert_eq!(page.items.len(), 2);
        let first = &page.items[0];
        assert_eq!(first.item.name, "News One");
        assert_eq!(first.channel_number.as_deref(), Some("5.1"));
        assert_eq!(first.channel_type.as_deref(), Some("TV"));
        let program = first.current_program.as_ref().expect("current program");
        assert_eq!(program.item.name, "Evening News");
        assert_eq!(
            program.start_date.as_deref(),
            Some("2026-10-15T18:00:00.0000000Z")
        );
        assert_eq!(
            program.channel_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        let unnumbered = &page.items[1];
        assert_eq!(unnumbered.item.name, "Unnumbered");
        assert_eq!(unnumbered.channel_number, None);
        assert_eq!(unnumbered.current_program, None);
    }

    #[test]
    fn empty_and_sparse_channel_pages_are_valid() {
        let empty: LiveTvChannelPage =
            serde_json::from_str(r#"{"Items":[],"TotalRecordCount":0,"StartIndex":0}"#)
                .expect("empty page fixture");
        assert!(empty.items.is_empty());
        assert_eq!(empty.total_record_count, Some(0));

        let sparse: LiveTvChannelPage = serde_json::from_str("{}").expect("sparse page fixture");
        assert_eq!(sparse, LiveTvChannelPage::default());
    }

    #[test]
    fn parses_program_page_with_times_and_channel() {
        let page: LiveTvProgramPage = serde_json::from_str(
            r#"{
                "Items": [{
                    "Id": "fedcba9876543210fedcba9876543210",
                    "Name": "Morning Show",
                    "Type": "Program",
                    "ChannelId": "0123456789abcdef0123456789abcdef",
                    "StartDate": "2026-10-15T00:00:00.0000000Z",
                    "EndDate": "2026-10-15T01:00:00.0000000Z"
                }],
                "TotalRecordCount": 1,
                "StartIndex": 0
            }"#,
        )
        .expect("valid program page fixture");

        assert_eq!(page.total_record_count, Some(1));
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].item.name, "Morning Show");
        assert_eq!(
            page.items[0].end_date.as_deref(),
            Some("2026-10-15T01:00:00.0000000Z")
        );
    }

    #[test]
    fn empty_program_page_and_missing_end_are_valid() {
        let empty: LiveTvProgramPage =
            serde_json::from_str(r#"{"Items":[],"TotalRecordCount":0,"StartIndex":0}"#)
                .expect("empty program page fixture");
        assert!(empty.items.is_empty());

        let no_end: LiveTvProgramPage =
            serde_json::from_str(r#"{"Items":[{"Id":"abc","Name":"Open-ended"}]}"#)
                .expect("program without end fixture");
        assert_eq!(no_end.items[0].end_date, None);
    }

    #[test]
    fn window_must_end_after_start() {
        let start = 1_792_022_400;
        assert_eq!(
            GuideWindow::new(start, start),
            Err(GuideWindowError::NotIncreasing)
        );
        assert_eq!(
            GuideWindow::new(start, start - 1),
            Err(GuideWindowError::NotIncreasing)
        );
    }

    #[test]
    fn window_is_bounded_to_24_hours_inclusive_of_the_limit() {
        let start = 1_792_022_400;
        let exact = GuideWindow::new(start, start + MAX_GUIDE_WINDOW_SECONDS)
            .expect("exactly 24 hours is allowed");
        assert_eq!(exact.start_unix(), start);
        assert_eq!(exact.end_unix(), start + MAX_GUIDE_WINDOW_SECONDS);
        assert_eq!(
            GuideWindow::new(start, start + MAX_GUIDE_WINDOW_SECONDS + 1),
            Err(GuideWindowError::TooLong)
        );
        assert_eq!(
            GuideWindow::new(i64::MIN, i64::MAX),
            Err(GuideWindowError::TooLong)
        );
    }

    #[test]
    fn formats_unix_times_as_fixed_width_utc() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_792_022_400), "2026-10-15T00:00:00Z");
        assert_eq!(format_utc(1_792_022_400 + 86_399), "2026-10-15T23:59:59Z");
    }
}
