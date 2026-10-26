//! Maps what libmpv reports about the playing stream to the media info input (M6-14).
//!
//! Pure: the player adapter reads the property strings and hands them over as plain values. The
//! names and shapes below were confirmed against mpv 0.37.0 (`docs/evidence/media-info-panel.md`):
//! the transfer is `pq`, `hlg`, or `bt.1886` rather than ffmpeg's `smpte2084`; the pixel format has
//! no endianness suffix (`yuv420p10`); `hwdec-current` is `no` for software decoding; and a track's
//! `demux-channels` reads `unknown6` when the container does not name the layout.
//!
//! Every text value goes on to [`crate::media_info::MediaInfo::from_input`], which removes control
//! characters, truncates, and hides anything that looks like a URL or credential.

use crate::media_info::{
    AudioInput, MAX_BIT_DEPTH, MIN_BIT_DEPTH, MediaInput, PlaybackInput, SubtitleInput, VideoInput,
};

/// Stream-level properties as libmpv's string getter returned them. `None` means the player
/// reported the property unavailable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MpvStreamValues {
    /// `file-format`, for example `mkv` or `mov,mp4,m4a,3gp,3g2,mj2`.
    pub container: Option<String>,
    /// `current-tracks/video/codec`.
    pub video_codec: Option<String>,
    /// `video-params/w`.
    pub width: Option<String>,
    /// `video-params/h`.
    pub height: Option<String>,
    /// `container-fps`.
    pub frame_rate: Option<String>,
    /// `video-params/pixelformat`.
    pub pixel_format: Option<String>,
    /// `video-params/gamma`.
    pub transfer: Option<String>,
    /// `video-params/primaries`.
    pub primaries: Option<String>,
    /// `hwdec-current`.
    pub hardware_decoder: Option<String>,
    /// `current-tracks/audio/codec`.
    pub audio_codec: Option<String>,
    /// `audio-params/channel-count`.
    pub audio_channel_count: Option<String>,
    /// `audio-params/hr-channels`.
    pub audio_channel_layout: Option<String>,
    /// `current-ao`.
    pub audio_output: Option<String>,
    /// `audio-device`.
    pub audio_device: Option<String>,
    /// `current-tracks/sub/codec`.
    pub subtitle_codec: Option<String>,
}

/// One audio or subtitle entry from mpv's `track-list`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MpvTrackValues {
    pub id: i64,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    /// `demux-channels`: `unknown6`, or a layout name such as `stereo`. Audio tracks only.
    pub channels: Option<String>,
    pub forced: bool,
    /// True for the track mpv currently plays.
    pub selected: bool,
}

/// Build the view model input. The decoded-audio facts (`audio-params`) describe the selected
/// audio track only, so they are attached to that track; other tracks show what the container
/// declares. When the track list has not arrived yet, the selected audio track is built from the
/// stream values alone.
pub fn media_input_from_mpv(
    stream: &MpvStreamValues,
    audio_tracks: &[MpvTrackValues],
    subtitle_tracks: &[MpvTrackValues],
) -> MediaInput {
    let mut audio: Vec<AudioInput> = audio_tracks
        .iter()
        .map(|track| audio_input(stream, track))
        .collect();
    if audio.is_empty() && stream.audio_codec.is_some() {
        audio.push(AudioInput {
            index: 0,
            codec: stream.audio_codec.clone(),
            channels: parse_u32(stream.audio_channel_count.as_deref()),
            channel_layout: stream.audio_channel_layout.clone(),
            language: None,
            title: None,
        });
    }

    MediaInput {
        container: stream.container.clone(),
        video: video_input(stream),
        audio,
        subtitles: subtitle_tracks
            .iter()
            .map(|track| SubtitleInput {
                index: track_index(track.id),
                format: track.codec.clone(),
                language: track.language.clone(),
                title: track.title.clone(),
                forced: track.forced,
            })
            .collect(),
        playback: PlaybackInput {
            hardware_decoder: stream.hardware_decoder.clone(),
            audio_output: stream.audio_output.clone(),
            audio_device: stream.audio_device.clone(),
            subtitle_format: stream.subtitle_codec.clone(),
        },
    }
}

fn video_input(stream: &MpvStreamValues) -> Option<VideoInput> {
    let video = VideoInput {
        codec: stream.video_codec.clone(),
        width: parse_u32(stream.width.as_deref()),
        height: parse_u32(stream.height.as_deref()),
        frame_rate: stream
            .frame_rate
            .as_deref()
            .and_then(|text| text.trim().parse::<f64>().ok()),
        bit_depth: stream
            .pixel_format
            .as_deref()
            .and_then(bit_depth_from_pixel_format),
        transfer: stream.transfer.clone(),
        primaries: stream.primaries.clone(),
    };
    (video != VideoInput::default()).then_some(video)
}

fn audio_input(stream: &MpvStreamValues, track: &MpvTrackValues) -> AudioInput {
    let (channels, channel_layout) = if track.selected {
        // Prefer what the decoder outputs; fall back to the container's claim.
        let (declared_count, declared_layout) = split_declared_channels(track.channels.as_deref());
        (
            parse_u32(stream.audio_channel_count.as_deref()).or(declared_count),
            stream.audio_channel_layout.clone().or(declared_layout),
        )
    } else {
        split_declared_channels(track.channels.as_deref())
    };
    AudioInput {
        index: track_index(track.id),
        codec: track.codec.clone(),
        channels,
        channel_layout,
        language: track.language.clone(),
        title: track.title.clone(),
    }
}

/// Split mpv's `demux-channels`. `unknown6` means six channels with no named layout; any other
/// text is a layout name (`stereo`, `5.1(side)`) whose channel count is not stated.
fn split_declared_channels(declared: Option<&str>) -> (Option<u32>, Option<String>) {
    let Some(text) = declared.map(str::trim).filter(|text| !text.is_empty()) else {
        return (None, None);
    };
    match text.strip_prefix("unknown") {
        Some(count) => match count.parse::<u32>() {
            Ok(count) => (Some(count), None),
            Err(_) => (None, Some(text.to_owned())),
        },
        None => (None, Some(text.to_owned())),
    }
}

fn parse_u32(text: Option<&str>) -> Option<u32> {
    text.and_then(|text| text.trim().parse::<u32>().ok())
}

fn track_index(id: i64) -> u32 {
    u32::try_from(id).unwrap_or(0)
}

/// Bits per component from an ffmpeg/mpv pixel format name such as `yuv420p10`, `p010`, or
/// `nv12`. `None` for a name this table does not know, so the panel shows "Unavailable" rather
/// than a guess.
pub fn bit_depth_from_pixel_format(name: &str) -> Option<u32> {
    let name = name.trim().to_ascii_lowercase();
    let name = name
        .strip_suffix("le")
        .or_else(|| name.strip_suffix("be"))
        .unwrap_or(&name);

    // Names whose trailing digits are not a bit depth (`rgb24`, `nv12`) or that carry no digits.
    match name {
        "nv12" | "nv21" | "nv16" | "nv24" | "nv42" | "gray" | "rgb24" | "bgr24" | "rgba"
        | "bgra" | "argb" | "abgr" | "0rgb" | "rgb0" => return Some(8),
        // Packed 16-bit-per-component RGB formats spell the total bits per pixel, not the
        // per-component depth shown by the media info panel.
        "rgb48" | "bgr48" | "rgba64" | "bgra64" => return Some(16),
        // Semi-planar formats: the digits are the bit depth of `p0xx`, not a chroma layout.
        "p010" | "p210" | "p410" => return Some(10),
        "p012" | "p212" | "p412" => return Some(12),
        "p016" | "p216" | "p416" => return Some(16),
        _ => {}
    }

    let digits_start = name
        .rfind(|ch: char| !ch.is_ascii_digit())
        .map_or(0, |at| at + 1);
    let (stem, digits) = name.split_at(digits_start);
    if !digits.is_empty() {
        // `yuv420p10`, `gbrp12`, `gbrap10`, `gray10`, `yuva444p16`: the run after the pixel
        // format stem is the per-component depth.
        let depth_follows_stem =
            stem.ends_with('p') || stem == "gbrap" || stem == "gray" || stem == "ya";
        return depth_follows_stem
            .then(|| digits.parse::<u32>().ok())
            .flatten()
            .filter(|depth| (MIN_BIT_DEPTH..=MAX_BIT_DEPTH).contains(depth));
    }

    // Planar YUV and GBR without a depth suffix are 8-bit (`yuv420p`, `yuvj420p`, `gbrp`).
    (name.starts_with("yuv") || name.starts_with("gbrp")).then_some(8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_info::{MediaInfo, REDACTED, UNAVAILABLE};

    fn s(text: &str) -> Option<String> {
        Some(text.to_owned())
    }

    /// Values copied from mpv 0.37.0 playing an HEVC 10-bit HDR10 file with AC-3 5.1 audio, read
    /// over its JSON IPC (`--vo=null --ao=null`, so no GPU decoder and the `null` output).
    fn captured_hdr() -> MpvStreamValues {
        MpvStreamValues {
            container: s("mkv"),
            video_codec: s("hevc"),
            width: s("640"),
            height: s("360"),
            frame_rate: s("23.976025"),
            pixel_format: s("yuv420p10"),
            transfer: s("pq"),
            primaries: s("bt.2020"),
            hardware_decoder: s("no"),
            audio_codec: s("ac3"),
            audio_channel_count: s("6"),
            audio_channel_layout: s("5.1(side)"),
            audio_output: s("null"),
            audio_device: s("auto"),
            subtitle_codec: None,
        }
    }

    /// The matching `track-list` entries from the same capture: one audio and one subtitle track.
    fn captured_tracks() -> (Vec<MpvTrackValues>, Vec<MpvTrackValues>) {
        (
            vec![MpvTrackValues {
                id: 1,
                codec: s("ac3"),
                language: s("eng"),
                channels: s("unknown6"),
                selected: true,
                ..MpvTrackValues::default()
            }],
            vec![MpvTrackValues {
                id: 1,
                codec: s("subrip"),
                language: s("eng"),
                ..MpvTrackValues::default()
            }],
        )
    }

    fn value_of(info: &MediaInfo, label: &str) -> String {
        info.rows()
            .into_iter()
            .find(|row| row.label == label)
            .map(|row| row.value)
            .unwrap_or_else(|| panic!("missing row {label}"))
    }

    #[test]
    fn the_captured_hdr_file_renders_its_known_properties() {
        let (audio, subtitles) = captured_tracks();
        let info =
            MediaInfo::from_input(&media_input_from_mpv(&captured_hdr(), &audio, &subtitles));
        assert_eq!(value_of(&info, "Container"), "mkv");
        assert_eq!(value_of(&info, "Video codec"), "hevc");
        assert_eq!(value_of(&info, "Resolution"), "640 x 360");
        assert_eq!(value_of(&info, "Frame rate"), "23.976 fps");
        assert_eq!(value_of(&info, "Bit depth"), "10-bit");
        assert_eq!(value_of(&info, "Dynamic range"), "HDR (PQ)");
        assert_eq!(value_of(&info, "Color primaries"), "bt.2020");
        assert_eq!(value_of(&info, "Hardware decode"), "Software");
        assert_eq!(value_of(&info, "Audio 1"), "ac3, 5.1(side) (6 ch), eng");
        assert_eq!(value_of(&info, "Audio output"), "null, default device");
        assert_eq!(value_of(&info, "Subtitle 1"), "subrip, eng");
        assert_eq!(value_of(&info, "Active subtitle"), "None");
    }

    #[test]
    fn an_sdr_h264_file_reads_as_sdr_with_eight_bit_depth() {
        // Captured from mpv 0.37.0 on an H.264/AAC stereo file.
        let stream = MpvStreamValues {
            container: s("mkv"),
            video_codec: s("h264"),
            width: s("320"),
            height: s("180"),
            frame_rate: s("25.0"),
            pixel_format: s("yuv420p"),
            transfer: s("bt.1886"),
            primaries: s("bt.709"),
            hardware_decoder: s("no"),
            audio_codec: s("aac"),
            audio_channel_count: s("2"),
            audio_channel_layout: s("stereo"),
            ..MpvStreamValues::default()
        };
        let info = MediaInfo::from_input(&media_input_from_mpv(&stream, &[], &[]));
        assert_eq!(value_of(&info, "Dynamic range"), "SDR");
        assert_eq!(value_of(&info, "Bit depth"), "8-bit");
        assert_eq!(value_of(&info, "Frame rate"), "25.000 fps");
        // No track list yet: the selected audio track comes from the stream values.
        assert_eq!(value_of(&info, "Audio 1"), "aac, stereo (2 ch)");
        assert_eq!(value_of(&info, "Subtitles"), "None");
    }

    #[test]
    fn a_hardware_decoder_is_named() {
        let mut stream = captured_hdr();
        stream.hardware_decoder = s("d3d11va");
        let info = MediaInfo::from_input(&media_input_from_mpv(&stream, &[], &[]));
        assert_eq!(value_of(&info, "Hardware decode"), "Hardware (d3d11va)");

        stream.hardware_decoder = None;
        let info = MediaInfo::from_input(&media_input_from_mpv(&stream, &[], &[]));
        assert_eq!(value_of(&info, "Hardware decode"), UNAVAILABLE);
    }

    #[test]
    fn a_selected_subtitle_is_the_active_subtitle() {
        let mut stream = captured_hdr();
        stream.subtitle_codec = s("ass");
        let info = MediaInfo::from_input(&media_input_from_mpv(&stream, &[], &[]));
        assert_eq!(value_of(&info, "Active subtitle"), "ass");
    }

    #[test]
    fn decoded_audio_facts_belong_to_the_selected_track_only() {
        let stream = captured_hdr();
        let audio = vec![
            MpvTrackValues {
                id: 1,
                codec: s("ac3"),
                channels: s("unknown6"),
                selected: true,
                ..MpvTrackValues::default()
            },
            MpvTrackValues {
                id: 2,
                codec: s("aac"),
                channels: s("stereo"),
                language: s("jpn"),
                ..MpvTrackValues::default()
            },
        ];
        let info = MediaInfo::from_input(&media_input_from_mpv(&stream, &audio, &[]));
        assert_eq!(value_of(&info, "Audio 1"), "ac3, 5.1(side) (6 ch)");
        assert_eq!(value_of(&info, "Audio 2"), "aac, stereo, jpn");
    }

    #[test]
    fn an_unselected_track_with_unknown_layout_shows_only_its_count() {
        let audio = vec![MpvTrackValues {
            id: 3,
            codec: s("dts"),
            channels: s("unknown8"),
            ..MpvTrackValues::default()
        }];
        let info = MediaInfo::from_input(&media_input_from_mpv(&captured_hdr(), &audio, &[]));
        assert_eq!(value_of(&info, "Audio 1"), "dts, 8 ch");
    }

    #[test]
    fn an_empty_property_set_renders_placeholders() {
        let info =
            MediaInfo::from_input(&media_input_from_mpv(&MpvStreamValues::default(), &[], &[]));
        assert_eq!(value_of(&info, "Container"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Video codec"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Hardware decode"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Audio output"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Audio"), "None");
        assert_eq!(value_of(&info, "Active subtitle"), "None");
    }

    #[test]
    fn text_that_is_not_a_number_becomes_unavailable() {
        let mut stream = captured_hdr();
        stream.width = s("wide");
        stream.height = s("-1");
        stream.frame_rate = s("fast");
        stream.pixel_format = s("mystery");
        let info = MediaInfo::from_input(&media_input_from_mpv(&stream, &[], &[]));
        assert_eq!(value_of(&info, "Resolution"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Frame rate"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Bit depth"), UNAVAILABLE);
    }

    #[test]
    fn a_negative_track_id_does_not_wrap() {
        let audio = vec![MpvTrackValues {
            id: -4,
            codec: s("aac"),
            ..MpvTrackValues::default()
        }];
        let input = media_input_from_mpv(&captured_hdr(), &audio, &[]);
        assert_eq!(input.audio[0].index, 0);
    }

    #[test]
    fn urls_and_credentials_from_the_player_are_hidden() {
        let mut stream = captured_hdr();
        stream.container = s("https://media.example:8920/Videos/1/stream?api_key=abc");
        stream.audio_device = s("http://10.0.0.5:8096/audio");
        stream.audio_output = s("token=abc123");
        let (mut audio, subtitles) = captured_tracks();
        audio[0].title = s("api_key=hidden");
        let info = MediaInfo::from_input(&media_input_from_mpv(&stream, &audio, &subtitles));
        assert_eq!(value_of(&info, "Container"), REDACTED);
        let text = info.diagnostics_text();
        for forbidden in [
            "api_key",
            "token",
            "media.example",
            "10.0.0.5",
            "://",
            "abc",
        ] {
            assert!(
                !text.contains(forbidden),
                "diagnostics leaked {forbidden}: {text}"
            );
        }
    }

    #[test]
    fn bit_depth_from_the_pixel_formats_mpv_reports() {
        let known = [
            ("yuv420p", 8),
            ("yuvj420p", 8),
            ("yuv444p", 8),
            ("nv12", 8),
            ("rgb24", 8),
            ("bgra", 8),
            ("gray", 8),
            ("gbrp", 8),
            ("yuv420p10", 10),
            ("yuv420p10le", 10),
            ("yuv422p10be", 10),
            ("p010", 10),
            ("p010le", 10),
            ("gbrap10le", 10),
            ("gbrap12be", 12),
            ("yuv444p12", 12),
            ("gbrp12", 12),
            ("gray10", 10),
            ("p016", 16),
            ("gbrap16le", 16),
            ("rgb48le", 16),
            ("bgr48be", 16),
            ("rgba64le", 16),
            ("bgra64be", 16),
            ("yuva444p16", 16),
            ("YUV420P10", 10),
        ];
        for (name, depth) in known {
            assert_eq!(bit_depth_from_pixel_format(name), Some(depth), "{name}");
        }
        for unknown in ["", "mystery", "yuv420p99", "yuv420p4", "vaapi", "d3d11"] {
            assert_eq!(bit_depth_from_pixel_format(unknown), None, "{unknown}");
        }
    }
}
