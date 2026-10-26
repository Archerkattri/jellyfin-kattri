//! Typed, forward-compatible models for Jellyfin playback device profiles.
//!
//! These structs describe caller-supplied negotiation data; they intentionally
//! do not supply codec/container lists or default capability claims. Fields
//! outside this minimal request surface are retained in flattened extensions.
//! Enum-like fields are strings so newer Jellyfin values can round-trip.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Playback capabilities and profile rules sent in `PlaybackInfoDto`.
///
/// Every capability is optional. `DeviceProfile::default()` is empty and does
/// not assert support for any container, codec, transport, or subtitle mode.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Jellyfin profile identifier, represented as its UUID string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_streaming_bitrate: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_static_bitrate: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub music_streaming_transcoding_bitrate: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_static_music_bitrate: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_play_profiles: Option<Vec<DirectPlayProfile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_profiles: Option<Vec<ContainerProfile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_profiles: Option<Vec<CodecProfile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcoding_profiles: Option<Vec<TranscodingProfile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle_profiles: Option<Vec<SubtitleProfile>>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Container-level constraints evaluated by Jellyfin's DLNA profile matcher.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerProfile {
    #[serde(rename = "Type", default, skip_serializing_if = "Option::is_none")]
    pub profile_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ProfileCondition>,
}

/// A container and codec combination the client declares it can direct-play.
///
/// Codec and container values are supplied by the caller; this type does not
/// assume any particular player capability.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct DirectPlayProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    /// Server-extensible DLNA profile type.
    #[serde(rename = "Type", default, skip_serializing_if = "Option::is_none")]
    pub profile_type: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Codec constraints Jellyfin evaluates before allowing direct play.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CodecProfile {
    #[serde(rename = "Type", default, skip_serializing_if = "Option::is_none")]
    pub profile_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ProfileCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apply_conditions: Vec<ProfileCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_container: Option<String>,
}

/// One Jellyfin codec-profile predicate. Values stay strings for forward compatibility.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProfileCondition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_required: Option<bool>,
}

/// Transcoding output settings Jellyfin may negotiate for a client.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct TranscodingProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Server-extensible DLNA profile type.
    #[serde(rename = "Type", default, skip_serializing_if = "Option::is_none")]
    pub profile_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    /// Server-extensible media stream protocol, such as `http` or `hls`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_content_length: Option<bool>,
    #[serde(
        rename = "EnableMpegtsM2TsMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub enable_mpegts_m2_ts_mode: Option<bool>,
    /// Server-extensible transcode seek mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode_seek_info: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_timestamps: Option<bool>,
    /// Server-extensible encoding context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_subtitles_in_manifest: Option<bool>,
    /// Jellyfin accepts the maximum channel setting as a string expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_audio_channels: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_segments: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_length: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_on_non_key_frames: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_audio_vbr_encoding: Option<bool>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Subtitle container, language, and delivery negotiation preferences.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SubtitleProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Server-extensible subtitle delivery method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub didl_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}
