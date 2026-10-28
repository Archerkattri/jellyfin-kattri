//! Typed contract for Jellyfin's initial playback-info request.

use std::{collections::HashMap, ops::Deref};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::device_profile::DeviceProfile;
use crate::playback_source::is_safe_item_id;
use crate::{ApiError, JellyfinClient, Session, decode_response};

/// A string that may contain credentials or other playback secrets.
///
/// The value is zeroized when dropped, deliberately has no `Debug`
/// implementation, and dereferences to `str` for ordinary read-only use.
#[derive(Clone, PartialEq, Eq)]
pub struct SensitiveString(Zeroizing<String>);

impl<'de> Deserialize<'de> for SensitiveString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(|value| Self(Zeroizing::new(value)))
    }
}

impl Deref for SensitiveString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

/// Body for `POST /Items/{itemId}/PlaybackInfo`.
///
/// Fields mirror the corresponding nullable fields in OpenAPI 12.2
/// `PlaybackInfoDto`. Unset fields are omitted from the JSON body.
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfoRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_streaming_bitrate: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time_ticks: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_audio_channels: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    /// Caller-supplied playback negotiation capabilities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_profile: Option<DeviceProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_direct_play: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_direct_stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_transcoding: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_video_stream_copy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_audio_stream_copy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_open_live_stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub always_burn_in_subtitle_when_transcoding: Option<bool>,
}

/// User-controlled limits that affect Jellyfin's playback negotiation.
/// These map to the existing `kattri-settings` playback bitrate and
/// force-transcode settings; no credentials or media identifiers are carried.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaybackRequestOverrides {
    /// Bits per second; `None` or zero leaves the server's limit unchanged.
    pub max_streaming_bitrate: Option<u64>,
    pub force_transcode: bool,
}

impl PlaybackInfoRequest {
    /// Build the initial request with a generated libmpv device profile and
    /// the corresponding user's playback overrides.
    pub fn for_mpv_profile(
        device_profile: DeviceProfile,
        overrides: PlaybackRequestOverrides,
    ) -> Self {
        let bitrate = overrides
            .max_streaming_bitrate
            .filter(|value| *value > 0)
            .map(|value| i32::try_from(value).unwrap_or(i32::MAX));
        let allow_direct = !overrides.force_transcode;
        Self {
            max_streaming_bitrate: bitrate,
            device_profile: Some(device_profile),
            enable_direct_play: Some(allow_direct),
            enable_direct_stream: Some(allow_direct),
            enable_transcoding: Some(true),
            allow_video_stream_copy: Some(allow_direct),
            allow_audio_stream_copy: Some(allow_direct),
            ..Self::default()
        }
    }
}

/// Response for `POST /Items/{itemId}/PlaybackInfo`.
///
/// This type intentionally does not implement `Debug`: nested source metadata
/// may contain filesystem paths, signed URLs, open tokens, or request headers.
#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfoResponse {
    #[serde(default)]
    pub media_sources: Vec<MediaSourceInfo>,
    #[serde(default)]
    pub play_session_id: Option<String>,
    #[serde(default)]
    pub error_code: Option<PlaybackErrorCode>,
}

/// Playback negotiation result code returned by Jellyfin.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum PlaybackErrorCode {
    NotAllowed,
    NoCompatibleStream,
    RateLimitExceeded,
    /// Preserve forward compatibility with server versions adding new codes.
    #[serde(other)]
    Unknown,
}

/// A negotiated media source and its direct/transcode capabilities.
///
/// Enum-like fields remain strings so values added by newer Jellyfin servers
/// are preserved. No `Debug` implementation is provided because this type can
/// contain filesystem paths, signed URLs, open tokens, and HTTP headers.
#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSourceInfo {
    /// Source transport, for example `File` or `Http`.
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub encoder_path: Option<String>,
    #[serde(default)]
    pub encoder_protocol: Option<String>,
    #[serde(rename = "Type", default)]
    pub source_type: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub is_remote: bool,
    #[serde(rename = "ETag", default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub run_time_ticks: Option<i64>,
    #[serde(default)]
    pub bitrate: Option<i32>,
    #[serde(default)]
    pub fallback_max_streaming_bitrate: Option<i32>,
    #[serde(default)]
    pub supports_transcoding: bool,
    #[serde(default)]
    pub supports_direct_stream: bool,
    #[serde(default)]
    pub supports_direct_play: bool,
    #[serde(default)]
    pub is_infinite_stream: bool,
    #[serde(default)]
    pub requires_opening: bool,
    /// Opaque server-provided value; do not log or format this type.
    #[serde(default)]
    pub open_token: Option<SensitiveString>,
    #[serde(default)]
    pub requires_closing: bool,
    #[serde(default)]
    pub live_stream_id: Option<String>,
    #[serde(default)]
    pub buffer_ms: Option<i32>,
    #[serde(default)]
    pub requires_looping: bool,
    #[serde(default)]
    pub supports_probing: bool,
    /// Server-extensible enum values are kept as strings.
    #[serde(default)]
    pub video_type: Option<String>,
    /// Server-extensible enum values are kept as strings.
    #[serde(default)]
    pub iso_type: Option<String>,
    /// Server-extensible enum values are kept as strings.
    #[serde(default)]
    pub video_3d_format: Option<String>,
    #[serde(default)]
    pub media_streams: Option<Vec<MediaStreamInfo>>,
    #[serde(default)]
    pub formats: Option<Vec<String>>,
    /// May contain credentials; this enclosing type intentionally has no `Debug`.
    #[serde(default)]
    pub required_http_headers: Option<HashMap<String, Option<SensitiveString>>>,
    #[serde(default)]
    pub transcoding_url: Option<SensitiveString>,
    /// Server-extensible enum value, such as `http` or `hls`.
    #[serde(default)]
    pub transcoding_sub_protocol: Option<String>,
    #[serde(default)]
    pub transcoding_container: Option<String>,
    #[serde(default)]
    pub analyze_duration_ms: Option<i32>,
    #[serde(default)]
    pub default_audio_stream_index: Option<i32>,
    #[serde(default)]
    pub default_subtitle_stream_index: Option<i32>,
    #[serde(default)]
    pub has_segments: bool,
}

/// Stream metadata and selected audio/subtitle indices for a media source.
/// Enum-like values remain strings for compatibility with newer server values.
/// This type intentionally does not implement `Debug`, as `delivery_url` and
/// `path` can be sensitive.
#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct MediaStreamInfo {
    #[serde(rename = "Type", default)]
    pub stream_type: Option<String>,
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub codec_tag: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub display_title: Option<String>,
    #[serde(default)]
    pub localized_language: Option<String>,
    #[serde(default)]
    pub localized_default: Option<String>,
    #[serde(default)]
    pub localized_forced: Option<String>,
    #[serde(default)]
    pub localized_external: Option<String>,
    #[serde(default)]
    pub localized_hearing_impaired: Option<String>,
    #[serde(default)]
    pub localized_original: Option<String>,
    #[serde(default)]
    pub localized_undefined: Option<String>,
    #[serde(default)]
    pub channel_layout: Option<String>,
    #[serde(default)]
    pub bitrate: Option<i32>,
    #[serde(default)]
    pub bit_depth: Option<i32>,
    #[serde(default)]
    pub channels: Option<i32>,
    #[serde(default)]
    pub sample_rate: Option<i32>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub is_forced: bool,
    #[serde(default)]
    pub is_hearing_impaired: bool,
    #[serde(default)]
    pub is_original: bool,
    #[serde(default)]
    pub height: Option<i32>,
    #[serde(default)]
    pub width: Option<i32>,
    #[serde(default)]
    pub average_frame_rate: Option<f64>,
    #[serde(default)]
    pub real_frame_rate: Option<f64>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    #[serde(default)]
    pub index: Option<i32>,
    #[serde(default)]
    pub score: Option<i32>,
    #[serde(default)]
    pub is_external: bool,
    /// Server-extensible enum value such as `Embed` or `External`.
    #[serde(default)]
    pub delivery_method: Option<String>,
    /// May contain a signed or otherwise credential-bearing subtitle URL.
    #[serde(default)]
    pub delivery_url: Option<SensitiveString>,
    #[serde(default)]
    pub is_external_url: Option<bool>,
    #[serde(default)]
    pub is_text_subtitle_stream: bool,
    #[serde(default)]
    pub supports_external_stream: bool,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub pixel_format: Option<String>,
    #[serde(default)]
    pub level: Option<i32>,
    /// Server-extensible video-range values remain strings.
    #[serde(default)]
    pub video_range: Option<String>,
    /// Server-extensible video-range values remain strings.
    #[serde(default)]
    pub video_range_type: Option<String>,
    #[serde(default)]
    pub audio_spatial_format: Option<String>,
}

impl JellyfinClient {
    /// Request playback negotiation for one item using the authenticated session.
    ///
    /// The authenticated user ID is used when `request.user_id` is not supplied.
    /// Item IDs are appended as an encoded URL path segment, and the request
    /// uses the client's existing MediaBrowser auth, redirect, and timeout policy.
    /// An item ID that is not a single safe path segment is refused before any
    /// request is sent.
    pub async fn playback_info(
        &self,
        session: &Session,
        item_id: &str,
        request: &PlaybackInfoRequest,
    ) -> Result<PlaybackInfoResponse, ApiError> {
        if !is_safe_item_id(item_id) {
            return Err(ApiError::InvalidConfiguration);
        }
        let url = self.endpoint(&["Items", item_id, "PlaybackInfo"]);
        let mut body = request.clone();
        if body.user_id.is_none() {
            body.user_id = Some(session.user.id.clone());
        }
        let request = self.http.post(url).json(&body);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }
}

#[cfg(test)]
mod error_code_tests {
    use super::{PlaybackErrorCode, PlaybackInfoResponse};

    #[test]
    fn known_playback_error_codes_are_typed() {
        for (wire, expected) in [
            ("NotAllowed", PlaybackErrorCode::NotAllowed),
            ("NoCompatibleStream", PlaybackErrorCode::NoCompatibleStream),
            ("RateLimitExceeded", PlaybackErrorCode::RateLimitExceeded),
        ] {
            let response: PlaybackInfoResponse =
                serde_json::from_value(serde_json::json!({ "ErrorCode": wire }))
                    .expect("playback error fixture");
            assert_eq!(response.error_code, Some(expected));
            assert!(response.media_sources.is_empty());
        }
    }

    #[test]
    fn unknown_playback_error_is_generic_and_debug_safe() {
        for wire in [
            "FutureServerCode",
            "https://private.invalid/?token=fixture-secret",
        ] {
            let response: PlaybackInfoResponse =
                serde_json::from_value(serde_json::json!({ "ErrorCode": wire }))
                    .expect("unknown error fixture");
            assert_eq!(response.error_code, Some(PlaybackErrorCode::Unknown));
            assert_eq!(format!("{:?}", response.error_code), "Some(Unknown)");
        }
    }

    #[test]
    fn missing_or_null_playback_error_code_is_not_a_failure() {
        for fixture in [
            serde_json::json!({}),
            serde_json::json!({ "ErrorCode": null }),
        ] {
            let response: PlaybackInfoResponse =
                serde_json::from_value(fixture).expect("no-error fixture");
            assert_eq!(response.error_code, None);
        }
    }
}

#[cfg(test)]
mod item_id_tests {
    use crate::playback_source::is_safe_item_id;
    use crate::{ApiError, ClientConfig, JellyfinClient, PlaybackInfoRequest, Session, UserDto};

    #[tokio::test]
    async fn playback_info_refuses_unsafe_item_ids_before_sending() {
        let client = JellyfinClient::new(
            ClientConfig::new(
                "https://jellyfin.example.test/jellyfin/",
                "Kattri test",
                "Test device",
                "device-id",
                "1.0-test",
            )
            .expect("valid client config"),
        )
        .expect("valid client");
        let session = Session {
            access_token: "session-token-secret".to_owned(),
            user: UserDto {
                policy: None,
                id: "user-id".to_owned(),
                name: "test-user".to_owned(),
                server_id: None,
            },
            server_id: None,
            session_info: None,
        };

        // A request that reached the network would fail with a transport error, so
        // only the configuration error shows the refusal happened before sending.
        for item_id in [".", "..", "", "a/b", "a?b", "a#b"] {
            assert!(!is_safe_item_id(item_id));
            let result = client
                .playback_info(&session, item_id, &PlaybackInfoRequest::default())
                .await;
            assert!(
                matches!(result, Err(ApiError::InvalidConfiguration)),
                "{item_id:?} must be refused before a request is sent"
            );
        }
    }
}

#[cfg(test)]
mod request_override_tests {
    use super::{PlaybackInfoRequest, PlaybackRequestOverrides};

    #[test]
    fn request_uses_generated_profile_and_user_limits() {
        let request = PlaybackInfoRequest::for_mpv_profile(
            crate::baseline_device_profile(),
            PlaybackRequestOverrides {
                max_streaming_bitrate: Some(20_000_000),
                force_transcode: false,
            },
        );
        let json = serde_json::to_value(request).expect("serialize request");
        assert_eq!(json["MaxStreamingBitrate"], 20_000_000);
        assert!(json.get("DeviceProfile").is_some());
        assert_eq!(json["EnableDirectPlay"], true);
        assert_eq!(json["EnableDirectStream"], true);
        assert_eq!(json["EnableTranscoding"], true);
    }

    #[test]
    fn force_transcode_disables_direct_paths_and_stream_copy() {
        let request = PlaybackInfoRequest::for_mpv_profile(
            crate::baseline_device_profile(),
            PlaybackRequestOverrides {
                max_streaming_bitrate: None,
                force_transcode: true,
            },
        );
        let json = serde_json::to_value(request).expect("serialize request");
        assert_eq!(json["EnableDirectPlay"], false);
        assert_eq!(json["EnableDirectStream"], false);
        assert_eq!(json["EnableTranscoding"], true);
        assert_eq!(json["AllowVideoStreamCopy"], false);
        assert_eq!(json["AllowAudioStreamCopy"], false);
        assert!(json.get("MaxStreamingBitrate").is_none());
    }

    #[test]
    fn bitrate_zero_means_unlimited_and_oversized_values_saturate_to_openapi_int32() {
        let unlimited = PlaybackInfoRequest::for_mpv_profile(
            crate::baseline_device_profile(),
            PlaybackRequestOverrides {
                max_streaming_bitrate: Some(0),
                force_transcode: false,
            },
        );
        let unlimited = serde_json::to_value(unlimited).expect("serialize request");
        assert!(unlimited.get("MaxStreamingBitrate").is_none());

        let bounded = PlaybackInfoRequest::for_mpv_profile(
            crate::baseline_device_profile(),
            PlaybackRequestOverrides {
                max_streaming_bitrate: Some(u64::MAX),
                force_transcode: false,
            },
        );
        let bounded = serde_json::to_value(bounded).expect("serialize request");
        assert_eq!(bounded["MaxStreamingBitrate"], i32::MAX);
    }
}
