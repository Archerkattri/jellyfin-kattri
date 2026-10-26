use jellyfin_api::{DeviceProfile, PlaybackInfoResponse, baseline_device_profile};
use kattri_client_core::playback_plan::{PlaybackMethod, select_playback_info_source_for_profile};
use serde_json::{Value, json};

fn profile() -> DeviceProfile {
    baseline_device_profile()
}

fn response(source: Value) -> PlaybackInfoResponse {
    serde_json::from_value(json!({ "MediaSources": [source] }))
        .expect("valid playback-info fixture")
}

fn av_source(container: &str, flags: Value) -> Value {
    let mut source = json!({
        "Container": container,
        "MediaStreams": [
            { "Type": "Video", "Codec": "h264", "BitDepth": 8, "VideoRangeType": "SDR" },
            { "Type": "Audio", "Codec": "aac", "IsDefault": true, "Index": 1 }
        ]
    });
    let source_object = source.as_object_mut().expect("source object");
    source_object.extend(flags.as_object().expect("flag object").clone());
    source
}

#[test]
fn valid_profile_tuple_prefers_direct_play_when_server_sets_both_direct_flags() {
    let response = response(av_source(
        "mkv",
        json!({
            "SupportsDirectPlay": true,
            "SupportsDirectStream": true,
            "SupportsTranscoding": true,
            "TranscodingUrl": "Videos/item/master.m3u8"
        }),
    ));

    let selected =
        select_playback_info_source_for_profile(&response, &profile()).expect("negotiated source");
    assert_eq!(selected.method, PlaybackMethod::DirectPlay);
}

#[test]
fn supported_codecs_with_only_a_container_mismatch_select_direct_stream() {
    let response = response(av_source(
        "custom",
        json!({
            "SupportsDirectPlay": false,
            "SupportsDirectStream": true,
            "SupportsTranscoding": true,
            "TranscodingUrl": "Videos/item/master.m3u8"
        }),
    ));

    let selected =
        select_playback_info_source_for_profile(&response, &profile()).expect("negotiated source");
    assert_eq!(selected.method, PlaybackMethod::DirectStream);
}

#[test]
fn invalid_codec_tuple_cannot_use_misleading_direct_flags() {
    let mut source = av_source(
        "mkv",
        json!({
            "SupportsDirectPlay": true,
            "SupportsDirectStream": true,
            "SupportsTranscoding": true,
            "TranscodingUrl": "Videos/item/master.m3u8"
        }),
    );
    source["MediaStreams"][0]["Codec"] = json!("unsupported");
    let response = response(source);

    let selected =
        select_playback_info_source_for_profile(&response, &profile()).expect("transcode fallback");
    assert_eq!(selected.method, PlaybackMethod::Transcode);
}

#[test]
fn failed_video_constraint_falls_back_to_transcode() {
    let mut profile = profile();
    let mut codec_profiles = profile.codec_profiles.take().unwrap_or_default();
    codec_profiles.push(
        serde_json::from_value(json!({
            "Type": "Video",
            "Codec": "h264",
            "Conditions": [{
                "Condition": "LessThanEqual",
                "Property": "VideoBitDepth",
                "Value": "8",
                "IsRequired": true
            }]
        }))
        .expect("codec constraint fixture"),
    );
    profile.codec_profiles = Some(codec_profiles);

    let mut source = av_source(
        "mkv",
        json!({
            "SupportsDirectPlay": true,
            "SupportsDirectStream": true,
            "SupportsTranscoding": true,
            "TranscodingUrl": "Videos/item/master.m3u8"
        }),
    );
    source["MediaStreams"][0]["BitDepth"] = json!(10);
    let response = response(source);

    let selected =
        select_playback_info_source_for_profile(&response, &profile).expect("transcode fallback");
    assert_eq!(selected.method, PlaybackMethod::Transcode);
}

#[test]
// The selected subtitle is already included in the server's negotiated source flags.
fn unsupported_selected_subtitle_uses_server_negotiated_transcode_flags() {
    let mut profile = profile();
    profile.subtitle_profiles = Some(
        serde_json::from_value(json!([
            { "Format": "srt", "Method": "External" }
        ]))
        .expect("subtitle profile fixture"),
    );
    let source = json!({
        "Container": "mkv",
        "SupportsDirectPlay": false,
        "SupportsDirectStream": false,
        "SupportsTranscoding": true,
        "TranscodingUrl": "Videos/item/master.m3u8?SubtitleStreamIndex=7",
        "DefaultSubtitleStreamIndex": 7,
        "MediaStreams": [
            { "Type": "Video", "Codec": "h264", "BitDepth": 8, "VideoRangeType": "SDR" },
            { "Type": "Audio", "Codec": "aac", "IsDefault": true, "Index": 1 },
            { "Type": "Subtitle", "Codec": "dvbsub", "Index": 7, "IsDefault": true }
        ]
    });
    let response = response(source);

    let selected = select_playback_info_source_for_profile(&response, &profile)
        .expect("server-negotiated transcode source");
    assert_eq!(selected.method, PlaybackMethod::Transcode);
}

#[test]
fn transcode_requires_a_nonblank_negotiated_url() {
    let response = response(av_source(
        "mkv",
        json!({
            "SupportsDirectPlay": false,
            "SupportsDirectStream": false,
            "SupportsTranscoding": true,
            "TranscodingUrl": "  "
        }),
    ));

    assert!(select_playback_info_source_for_profile(&response, &profile()).is_none());
}
