//! Conservative matching of Jellyfin media sources against a client profile.

use jellyfin_api::{
    CodecProfile, DeviceProfile, DirectPlayProfile, MediaSourceInfo, MediaStreamInfo,
};

use crate::playback_plan::DirectPlayProfileMatch;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Check {
    Pass,
    Fail,
    Unknown,
    Untested,
}

#[derive(Clone, Copy)]
struct SourceTuple<'a> {
    container: &'a str,
    video_codec: Option<&'a str>,
    audio_codec: Option<&'a str>,
    video: Option<&'a MediaStreamInfo>,
}

/// Match the source's selected audio/video tuple against the profile.
///
/// Server `SupportsDirectPlay` flags are deliberately not consulted: only a
/// matching profile entry with understood, passing constraints validates it.
pub fn direct_play_match(
    profile: &DeviceProfile,
    source: &MediaSourceInfo,
) -> DirectPlayProfileMatch {
    let Some(tuple) = source_tuple(source) else {
        return DirectPlayProfileMatch::Unknown;
    };
    let Some(profiles) = profile.direct_play_profiles.as_deref() else {
        return DirectPlayProfileMatch::Unknown;
    };

    let mut tuple_unknown = false;
    for entry in profiles {
        match codec_tuple_match(entry, tuple) {
            Check::Pass => match codec_constraints(profile, tuple) {
                Check::Pass => return DirectPlayProfileMatch::ValidatedPass,
                Check::Fail => {}
                Check::Unknown => tuple_unknown = true,
                Check::Untested => return DirectPlayProfileMatch::Untested,
            },
            Check::Unknown => tuple_unknown = true,
            Check::Fail | Check::Untested => {}
        }
    }
    if tuple_unknown {
        DirectPlayProfileMatch::Unknown
    } else {
        DirectPlayProfileMatch::ValidatedFail
    }
}

/// Whether the tuple is profile-supported but needs a container remux.
/// Unknown codecs/metadata and codec mismatches never authorize direct stream.
pub fn direct_stream_profile_match(profile: &DeviceProfile, source: &MediaSourceInfo) -> bool {
    let Some(tuple) = source_tuple(source) else {
        return false;
    };
    let Some(profiles) = profile.direct_play_profiles.as_deref() else {
        return false;
    };

    let mut codec_supported = false;
    let mut container_supported = false;
    for entry in profiles {
        if !matches!(codec_only_match(entry, tuple), Check::Pass) {
            continue;
        }
        codec_supported = true;
        if value_list_contains(entry.container.as_deref(), tuple.container) {
            container_supported = true;
        }
    }
    codec_supported
        && !container_supported
        && matches!(codec_constraints(profile, tuple), Check::Pass)
}

fn source_tuple(source: &MediaSourceInfo) -> Option<SourceTuple<'_>> {
    let container = source.container.as_deref()?.trim();
    if container.is_empty() {
        return None;
    }
    let streams = source.media_streams.as_deref()?;
    let video = streams.iter().find(|stream| stream_kind(stream, "Video"));
    let audios: Vec<_> = streams
        .iter()
        .filter(|stream| stream_kind(stream, "Audio"))
        .collect();

    let selected_audio = source
        .default_audio_stream_index
        .and_then(|index| {
            audios
                .iter()
                .copied()
                .find(|stream| stream.index == Some(index))
        })
        .or_else(|| audios.iter().copied().find(|stream| stream.is_default))
        .or_else(|| audios.first().copied());

    if (video.is_none() && selected_audio.is_none())
        || (video.is_some() && selected_audio.is_none())
    {
        return None;
    }
    let video_codec = match video {
        Some(stream) => Some(stream.codec.as_deref()?.trim()),
        None => None,
    };
    let audio_codec = match selected_audio {
        Some(stream) => Some(stream.codec.as_deref()?.trim()),
        None => None,
    };
    if video_codec.is_some_and(str::is_empty) || audio_codec.is_some_and(str::is_empty) {
        return None;
    }
    Some(SourceTuple {
        container,
        video_codec,
        audio_codec,
        video,
    })
}

fn stream_kind(stream: &MediaStreamInfo, expected: &str) -> bool {
    stream
        .stream_type
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case(expected))
}

fn codec_tuple_match(profile: &DirectPlayProfile, tuple: SourceTuple<'_>) -> Check {
    match list_match(profile.container.as_deref(), tuple.container) {
        Check::Pass => codec_only_match(profile, tuple),
        Check::Fail => Check::Fail,
        Check::Unknown | Check::Untested => Check::Unknown,
    }
}

fn codec_only_match(profile: &DirectPlayProfile, tuple: SourceTuple<'_>) -> Check {
    match optional_list_match(profile.video_codec.as_deref(), tuple.video_codec) {
        Check::Pass => {}
        Check::Fail => return Check::Fail,
        Check::Unknown | Check::Untested => return Check::Unknown,
    }
    match optional_list_match(profile.audio_codec.as_deref(), tuple.audio_codec) {
        Check::Pass => {}
        Check::Fail => return Check::Fail,
        Check::Unknown | Check::Untested => return Check::Unknown,
    }
    Check::Pass
}

fn optional_list_match(profile_values: Option<&str>, source_value: Option<&str>) -> Check {
    match (profile_values, source_value) {
        (None, None) => Check::Pass,
        (Some(values), Some(value)) => list_match(Some(values), value),
        (None, Some(_)) | (Some(_), None) => Check::Fail,
    }
}

fn list_match(values: Option<&str>, value: &str) -> Check {
    match values {
        Some(values) if value_list_contains(Some(values), value) => Check::Pass,
        Some(values) if !values.trim().is_empty() => Check::Fail,
        _ => Check::Unknown,
    }
}

fn value_list_contains(values: Option<&str>, needle: &str) -> bool {
    values.is_some_and(|values| {
        values
            .split(',')
            .map(str::trim)
            .any(|value| !value.is_empty() && value.eq_ignore_ascii_case(needle.trim()))
    })
}

fn codec_constraints(profile: &DeviceProfile, tuple: SourceTuple<'_>) -> Check {
    let Some(codec_profiles) = profile.codec_profiles.as_deref() else {
        return Check::Pass;
    };
    let mut result = Check::Pass;
    for codec_profile in codec_profiles {
        match codec_profile_applies(codec_profile, tuple) {
            Check::Pass => match profile_conditions(codec_profile, tuple) {
                Check::Fail => return Check::Fail,
                Check::Untested => result = Check::Untested,
                Check::Unknown if result == Check::Pass => result = Check::Unknown,
                Check::Pass | Check::Unknown => {}
            },
            Check::Fail => {}
            Check::Unknown if result == Check::Pass => result = Check::Unknown,
            Check::Unknown => {}
            Check::Untested => result = Check::Untested,
        }
    }
    result
}

fn codec_profile_applies(profile: &CodecProfile, tuple: SourceTuple<'_>) -> Check {
    let kind = profile.profile_type.as_deref();
    let stream_codec = match kind {
        Some(kind) if kind.eq_ignore_ascii_case("Video") => tuple.video_codec,
        Some(kind) if kind.eq_ignore_ascii_case("Audio") => tuple.audio_codec,
        None => {
            // A selector-free profile with video predicates can still affect a
            // video tuple; evaluate its conditions conservatively below.
            tuple.video_codec.or(tuple.audio_codec)
        }
        Some(_) => return Check::Untested,
    };
    if stream_codec.is_none() {
        return Check::Fail;
    }
    if let Some(codecs) = profile.codec.as_deref() {
        let Some(stream_codec) = stream_codec else {
            return Check::Unknown;
        };
        if !value_list_contains(Some(codecs), stream_codec) {
            return Check::Fail;
        }
    }
    if let Some(containers) = profile.container.as_deref()
        && !value_list_contains(Some(containers), tuple.container)
    {
        return Check::Fail;
    }
    if profile
        .sub_container
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Check::Untested;
    }
    for condition in &profile.apply_conditions {
        match evaluate_condition(
            condition.property.as_deref(),
            condition.condition.as_deref(),
            condition.value.as_deref(),
            tuple,
        ) {
            Check::Pass => {}
            // ApplyConditions gates whether the restriction applies.
            Check::Fail => return Check::Fail,
            Check::Unknown => return Check::Unknown,
            Check::Untested => return Check::Untested,
        }
    }
    Check::Pass
}

fn profile_conditions(profile: &CodecProfile, tuple: SourceTuple<'_>) -> Check {
    let mut result = Check::Pass;
    for condition in &profile.conditions {
        match evaluate_condition(
            condition.property.as_deref(),
            condition.condition.as_deref(),
            condition.value.as_deref(),
            tuple,
        ) {
            Check::Pass => {}
            Check::Fail if condition.is_required.unwrap_or(true) => return Check::Fail,
            Check::Fail => {}
            Check::Unknown if result == Check::Pass => result = Check::Unknown,
            Check::Unknown => {}
            Check::Untested => return Check::Untested,
        }
    }
    result
}

fn evaluate_condition(
    property: Option<&str>,
    condition: Option<&str>,
    expected: Option<&str>,
    tuple: SourceTuple<'_>,
) -> Check {
    let (Some(property), Some(condition), Some(expected)) = (property, condition, expected) else {
        return Check::Untested;
    };
    if property.eq_ignore_ascii_case("VideoBitDepth")
        && condition.eq_ignore_ascii_case("LessThanEqual")
    {
        let Some(depth) = tuple.video.and_then(|stream| stream.bit_depth) else {
            return Check::Unknown;
        };
        return expected
            .trim()
            .parse::<i32>()
            .map(|limit| {
                if depth <= limit {
                    Check::Pass
                } else {
                    Check::Fail
                }
            })
            .unwrap_or(Check::Untested);
    }
    if property.eq_ignore_ascii_case("VideoRangeType") && condition.eq_ignore_ascii_case("Equals") {
        let Some(range) = tuple
            .video
            .and_then(|stream| stream.video_range_type.as_deref())
        else {
            return Check::Unknown;
        };
        return if range.eq_ignore_ascii_case(expected.trim()) {
            Check::Pass
        } else {
            Check::Fail
        };
    }
    Check::Untested
}

#[cfg(test)]
mod tests {
    use super::*;
    use jellyfin_api::{MpvSoftwareBaseline, baseline_device_profile};

    fn baseline_profile() -> DeviceProfile {
        baseline_device_profile()
    }

    fn source(
        container: Option<&str>,
        video: Option<(&str, i32, &str)>,
        audio: Option<&str>,
    ) -> MediaSourceInfo {
        let mut streams = Vec::new();
        if let Some((codec, depth, range)) = video {
            streams.push(serde_json::json!({
                "Type": "Video", "Codec": codec, "BitDepth": depth, "VideoRangeType": range
            }));
        }
        if let Some(codec) = audio {
            streams.push(serde_json::json!({
                "Type": "Audio", "Codec": codec, "IsDefault": true, "Index": 1
            }));
        }
        serde_json::from_value(serde_json::json!({
            "Container": container,
            "MediaStreams": streams,
            "SupportsDirectPlay": false
        }))
        .expect("source fixture")
    }

    fn add_profile_constraint(
        profile: &mut DeviceProfile,
        property: &str,
        condition: &str,
        value: &str,
    ) {
        let mut codec_profiles = profile.codec_profiles.take().unwrap_or_default();
        codec_profiles.push(CodecProfile {
            profile_type: Some("Video".into()),
            codec: Some("h264, hevc".into()),
            conditions: vec![
                serde_json::from_value(serde_json::json!({
                    "Condition": condition, "Property": property, "Value": value, "IsRequired": true
                }))
                .expect("condition fixture"),
            ],
            ..CodecProfile::default()
        });
        profile.codec_profiles = Some(codec_profiles);
    }

    #[test]
    fn committed_baseline_matches_case_insensitive_profile_values_and_known_constraints() {
        let baseline = MpvSoftwareBaseline::embedded().expect("committed baseline");
        assert_eq!(baseline.schema_version, 1);
        let profile = baseline_device_profile();
        assert_eq!(
            direct_play_match(
                &profile,
                &source(Some("MKV"), Some(("H264", 8, "SDR")), Some("AAC"))
            ),
            DirectPlayProfileMatch::ValidatedPass
        );
    }

    #[test]
    fn rejects_unsupported_codec_and_container() {
        let profile = baseline_profile();
        assert_eq!(
            direct_play_match(
                &profile,
                &source(Some("mkv"), Some(("unknown", 8, "SDR")), Some("aac"))
            ),
            DirectPlayProfileMatch::ValidatedFail
        );
        assert_eq!(
            direct_play_match(
                &profile,
                &source(Some("unknown"), Some(("h264", 8, "SDR")), Some("aac"))
            ),
            DirectPlayProfileMatch::ValidatedFail
        );
    }

    #[test]
    fn rejects_bit_depth_and_hdr_range_mismatches() {
        let mut profile = baseline_profile();
        add_profile_constraint(&mut profile, "VideoBitDepth", "LessThanEqual", "8");
        assert_eq!(
            direct_play_match(
                &profile,
                &source(Some("mkv"), Some(("h264", 10, "SDR")), Some("aac"))
            ),
            DirectPlayProfileMatch::ValidatedFail
        );
        let mut profile = baseline_profile();
        add_profile_constraint(&mut profile, "VideoRangeType", "Equals", "SDR");
        assert_eq!(
            direct_play_match(
                &profile,
                &source(Some("mkv"), Some(("h264", 8, "HDR10")), Some("aac"))
            ),
            DirectPlayProfileMatch::ValidatedFail
        );
    }

    #[test]
    fn missing_tuple_metadata_is_unknown() {
        let profile = baseline_profile();
        assert_eq!(
            direct_play_match(
                &profile,
                &source(Some("mkv"), Some(("h264", 8, "SDR")), None)
            ),
            DirectPlayProfileMatch::Unknown
        );
        let mut unknown = source(Some("mkv"), None, Some("aac"));
        unknown.media_streams = None;
        assert_eq!(
            direct_play_match(&profile, &unknown),
            DirectPlayProfileMatch::Unknown
        );
        let mut missing_bit_depth = source(Some("mkv"), Some(("h264", 8, "SDR")), Some("aac"));
        if let Some(video) = missing_bit_depth
            .media_streams
            .as_mut()
            .and_then(|streams| {
                streams
                    .iter_mut()
                    .find(|stream| stream_kind(stream, "Video"))
            })
        {
            video.bit_depth = None;
        }
        assert_eq!(
            direct_play_match(&profile, &missing_bit_depth),
            DirectPlayProfileMatch::Unknown
        );
    }

    #[test]
    fn unsupported_predicate_is_untested() {
        let mut profile = baseline_profile();
        add_profile_constraint(&mut profile, "VideoBitDepth", "GreaterThan", "8");
        assert_eq!(
            direct_play_match(
                &profile,
                &source(Some("mkv"), Some(("h264", 8, "SDR")), Some("aac"))
            ),
            DirectPlayProfileMatch::Untested
        );
    }

    #[test]
    fn audio_only_uses_audio_profile_and_stream_codec() {
        let profile = baseline_profile();
        assert_eq!(
            direct_play_match(&profile, &source(Some("flac"), None, Some("flac"))),
            DirectPlayProfileMatch::ValidatedPass
        );
        assert!(direct_stream_profile_match(
            &profile,
            &source(Some("unknown"), None, Some("flac"))
        ));
    }

    #[test]
    fn direct_stream_requires_supported_codecs_and_a_missing_container_match() {
        let profile = baseline_profile();
        let remux = source(Some("custom"), Some(("h264", 8, "SDR")), Some("aac"));
        assert!(direct_stream_profile_match(&profile, &remux));
        assert_eq!(
            direct_play_match(&profile, &remux),
            DirectPlayProfileMatch::ValidatedFail
        );
        assert!(!direct_stream_profile_match(
            &profile,
            &source(Some("custom"), Some(("unsupported", 8, "SDR")), Some("aac"))
        ));
        assert!(!direct_stream_profile_match(
            &profile,
            &source(Some("custom"), Some(("h264", 8, "SDR")), None)
        ));
    }
}
