//! Deterministic playback-method selection from Jellyfin's negotiated sources.
//!
//! The server's media-source order is preserved (it carries version/source
//! preference). Within a source, prefer no server-side media work, then stream
//! copy, then transcoding. The caller still owns URL construction, stream
//! authentication, and open-failure fallback.

use jellyfin_api::{DeviceProfile, MediaSourceInfo, PlaybackInfoResponse};

pub use crate::profile_match::{direct_play_match, direct_stream_profile_match};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackMethod {
    DirectPlay,
    DirectStream,
    Transcode,
}

/// Result of matching a media source's exact container/codec tuple against a
/// validated MPV profile. Unknown, failed, or untested tuples are never
/// eligible for direct play.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DirectPlayProfileMatch {
    ValidatedPass,
    ValidatedFail,
    Untested,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaybackSourceCapabilities {
    pub supports_direct_play: bool,
    pub supports_direct_stream: bool,
    /// True only when codecs are profile-supported but this container needs remuxing.
    pub direct_stream_profile_match: bool,
    pub supports_transcoding: bool,
    /// Must be `ValidatedPass` for this source's exact tuple before direct
    /// play can be selected. The default is deliberately conservative.
    pub direct_play_profile_match: DirectPlayProfileMatch,
    /// A transcode is selectable only when the server supplied its negotiated
    /// transcoding URL. Direct play/stream URLs are built separately.
    pub has_transcoding_url: bool,
}

impl PlaybackSourceCapabilities {
    pub const fn preferred_method(self) -> Option<PlaybackMethod> {
        if self.supports_direct_play
            && matches!(
                self.direct_play_profile_match,
                DirectPlayProfileMatch::ValidatedPass
            )
        {
            Some(PlaybackMethod::DirectPlay)
        } else if self.supports_direct_stream && self.direct_stream_profile_match {
            Some(PlaybackMethod::DirectStream)
        } else if self.supports_transcoding && self.has_transcoding_url {
            Some(PlaybackMethod::Transcode)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackSelection {
    /// Index into the original ordered `MediaSources` response.
    pub source_index: usize,
    pub method: PlaybackMethod,
}

/// A selection from Jellyfin's playback-info response that borrows the chosen
/// source metadata without copying or exposing its potentially sensitive URLs.
#[derive(Clone, Copy)]
pub struct PlaybackInfoSelection<'a> {
    pub source_index: usize,
    pub method: PlaybackMethod,
    pub media_source: &'a MediaSourceInfo,
}

/// Choose the first server-ordered source the client can play. The selected
/// method is the least server-intensive negotiated mode supported by that
/// source. Does not inspect or expose potentially sensitive source URLs.
pub fn select_playback_source(sources: &[PlaybackSourceCapabilities]) -> Option<PlaybackSelection> {
    sources
        .iter()
        .copied()
        .enumerate()
        .find_map(|(source_index, capabilities)| {
            capabilities
                .preferred_method()
                .map(|method| PlaybackSelection {
                    source_index,
                    method,
                })
        })
}

/// Select the first playable source in Jellyfin's response order. Direct
/// methods are eligible only when their container passes the same safety
/// constraint used by `jellyfin-api`'s playback-source builder. Transcoding
/// requires both server support and a nonblank negotiated URL.
pub fn select_playback_info_source(
    response: &PlaybackInfoResponse,
) -> Option<PlaybackInfoSelection<'_>> {
    select_playback_info_source_with_profile(response, DirectPlayProfileMatch::Unknown)
}

/// Select a negotiated source using the caller's match result for the exact
/// media-source tuple. Callers must provide `ValidatedPass` only when the
/// container, video codec, and audio codec match a validated/pass profile
/// entry; Jellyfin's `SupportsDirectPlay` flag alone is not that evidence.
pub fn select_playback_info_source_with_profile(
    response: &PlaybackInfoResponse,
    direct_play_profile_match: DirectPlayProfileMatch,
) -> Option<PlaybackInfoSelection<'_>> {
    response
        .media_sources
        .iter()
        .enumerate()
        .find_map(|(source_index, media_source)| {
            let has_safe_container = media_source
                .container
                .as_deref()
                .is_some_and(is_safe_container);
            let method = PlaybackSourceCapabilities {
                supports_direct_play: media_source.supports_direct_play && has_safe_container,
                supports_direct_stream: media_source.supports_direct_stream && has_safe_container,
                // The caller must explicitly match the source against its profile.
                direct_stream_profile_match: false,
                supports_transcoding: media_source.supports_transcoding,
                direct_play_profile_match,
                has_transcoding_url: media_source
                    .transcoding_url
                    .as_deref()
                    .is_some_and(|url| !url.trim().is_empty()),
            }
            .preferred_method()?;
            Some(PlaybackInfoSelection {
                source_index,
                method,
                media_source,
            })
        })
}

/// Select a negotiated source using exact per-source profile validation.
/// Direct methods require a safe container; stream copy additionally requires
/// supported codecs with a profile container mismatch. Transcoding requires a URL.
pub fn select_playback_info_source_for_profile<'a>(
    response: &'a PlaybackInfoResponse,
    profile: &DeviceProfile,
) -> Option<PlaybackInfoSelection<'a>> {
    response
        .media_sources
        .iter()
        .enumerate()
        .find_map(|(source_index, media_source)| {
            let direct_play_match = direct_play_match(profile, media_source);
            let direct_stream_match = direct_stream_profile_match(profile, media_source);
            let has_safe_container = media_source
                .container
                .as_deref()
                .is_some_and(is_safe_container);
            let method = PlaybackSourceCapabilities {
                supports_direct_play: media_source.supports_direct_play && has_safe_container,
                supports_direct_stream: media_source.supports_direct_stream && has_safe_container,
                direct_stream_profile_match: direct_stream_match,
                supports_transcoding: media_source.supports_transcoding,
                direct_play_profile_match: direct_play_match,
                has_transcoding_url: media_source
                    .transcoding_url
                    .as_deref()
                    .is_some_and(|url| !url.trim().is_empty()),
            }
            .preferred_method()?;
            Some(PlaybackInfoSelection {
                source_index,
                method,
                media_source,
            })
        })
}

// Keep in sync with jellyfin-api's private `is_safe_container` validation.
fn is_safe_container(container: &str) -> bool {
    !container.is_empty()
        && container
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jellyfin_api::baseline_device_profile;

    #[derive(Clone, Copy)]
    struct Case {
        name: &'static str,
        server_direct_play: bool,
        server_direct_stream: bool,
        server_transcode: bool,
        transcode_url: bool,
        profile_match: DirectPlayProfileMatch,
        direct_stream_profile_match: bool,
        expected: Option<PlaybackMethod>,
    }

    #[test]
    fn selects_only_profile_validated_direct_play_and_falls_back_safely() {
        let cases = [
            Case {
                name: "validated pass permits direct play",
                server_direct_play: true,
                server_direct_stream: true,
                server_transcode: true,
                transcode_url: true,
                profile_match: DirectPlayProfileMatch::ValidatedPass,
                direct_stream_profile_match: false,
                expected: Some(PlaybackMethod::DirectPlay),
            },
            Case {
                name: "validated fail falls back to direct stream",
                server_direct_play: true,
                server_direct_stream: true,
                server_transcode: true,
                transcode_url: true,
                profile_match: DirectPlayProfileMatch::ValidatedFail,
                direct_stream_profile_match: true,
                expected: Some(PlaybackMethod::DirectStream),
            },
            Case {
                name: "untested does not permit direct play",
                server_direct_play: true,
                server_direct_stream: false,
                server_transcode: true,
                transcode_url: true,
                profile_match: DirectPlayProfileMatch::Untested,
                direct_stream_profile_match: false,
                expected: Some(PlaybackMethod::Transcode),
            },
            Case {
                name: "unknown does not permit direct play",
                server_direct_play: true,
                server_direct_stream: false,
                server_transcode: true,
                transcode_url: true,
                profile_match: DirectPlayProfileMatch::Unknown,
                direct_stream_profile_match: false,
                expected: Some(PlaybackMethod::Transcode),
            },
            Case {
                name: "server direct stream flag alone is not sufficient",
                server_direct_play: false,
                server_direct_stream: true,
                server_transcode: true,
                transcode_url: true,
                profile_match: DirectPlayProfileMatch::ValidatedPass,
                direct_stream_profile_match: false,
                expected: Some(PlaybackMethod::Transcode),
            },
            Case {
                name: "transcode requires negotiated URL",
                server_direct_play: false,
                server_direct_stream: false,
                server_transcode: true,
                transcode_url: true,
                profile_match: DirectPlayProfileMatch::Unknown,
                direct_stream_profile_match: false,
                expected: Some(PlaybackMethod::Transcode),
            },
            Case {
                name: "transcode without URL is unsupported",
                server_direct_play: false,
                server_direct_stream: false,
                server_transcode: true,
                transcode_url: false,
                profile_match: DirectPlayProfileMatch::ValidatedPass,
                direct_stream_profile_match: false,
                expected: None,
            },
            Case {
                name: "server with no playable mode is unsupported",
                server_direct_play: false,
                server_direct_stream: false,
                server_transcode: false,
                transcode_url: false,
                profile_match: DirectPlayProfileMatch::ValidatedPass,
                direct_stream_profile_match: false,
                expected: None,
            },
            Case {
                name: "direct play alone is rejected for unknown tuple",
                server_direct_play: true,
                server_direct_stream: false,
                server_transcode: false,
                transcode_url: false,
                profile_match: DirectPlayProfileMatch::Unknown,
                direct_stream_profile_match: false,
                expected: None,
            },
            Case {
                name: "direct play alone is rejected for failed tuple",
                server_direct_play: true,
                server_direct_stream: false,
                server_transcode: false,
                transcode_url: false,
                profile_match: DirectPlayProfileMatch::ValidatedFail,
                direct_stream_profile_match: false,
                expected: None,
            },
        ];

        for case in cases {
            let actual = PlaybackSourceCapabilities {
                supports_direct_play: case.server_direct_play,
                supports_direct_stream: case.server_direct_stream,
                direct_stream_profile_match: case.direct_stream_profile_match,
                supports_transcoding: case.server_transcode,
                has_transcoding_url: case.transcode_url,
                direct_play_profile_match: case.profile_match,
            }
            .preferred_method();
            assert_eq!(actual, case.expected, "{}", case.name);
        }
    }

    #[test]
    fn profile_selector_blocks_codec_mismatch_and_allows_supported_remux() {
        let profile = baseline_device_profile();
        let incompatible: PlaybackInfoResponse = serde_json::from_value(serde_json::json!({
            "MediaSources": [{
                "Container": "mkv",
                "SupportsDirectPlay": true,
                "SupportsDirectStream": true,
                "SupportsTranscoding": true,
                "TranscodingUrl": "http://localhost/encoded",
                "MediaStreams": [
                    { "Type": "Video", "Codec": "unsupported", "BitDepth": 8, "VideoRangeType": "SDR" },
                    { "Type": "Audio", "Codec": "aac", "IsDefault": true, "Index": 1 }
                ]
            }]
        }))
        .expect("incompatible source fixture");
        let selected = select_playback_info_source_for_profile(&incompatible, &profile)
            .expect("transcode fallback");
        assert_eq!(selected.method, PlaybackMethod::Transcode);

        let remux: PlaybackInfoResponse = serde_json::from_value(serde_json::json!({
            "MediaSources": [{
                "Container": "custom",
                "SupportsDirectPlay": true,
                "SupportsDirectStream": true,
                "SupportsTranscoding": true,
                "TranscodingUrl": "http://localhost/encoded",
                "MediaStreams": [
                    { "Type": "Video", "Codec": "h264", "BitDepth": 8, "VideoRangeType": "SDR" },
                    { "Type": "Audio", "Codec": "aac", "IsDefault": true, "Index": 1 }
                ]
            }]
        }))
        .expect("remux source fixture");
        let selected = select_playback_info_source_for_profile(&remux, &profile)
            .expect("direct-stream source");
        assert_eq!(selected.method, PlaybackMethod::DirectStream);
    }
}
