//! Media info view model (M6-14). Pure: built from plain stream fields the owner has already read.
//! No network, no player calls, no clock.
//!
//! Redaction: the model has no field for a stream URL, token, or server origin. A text value that
//! looks like a URL or a credential is shown as [`REDACTED`], never as-is.
//!
//! Display order is fixed. Rows come in this order: container, video (codec, resolution, frame
//! rate, bit depth, dynamic range, primaries), hardware decode, one row per audio track, the audio
//! output, one row per subtitle track, then the active subtitle. Tracks are sorted by stream
//! index, so the order does not depend on input order.
//!
//! [`crate::media_info_mpv`] builds the input from the strings libmpv reports.

use kattri_guard::{has_scheme_separator, is_secret_like_value};

/// Shown for a scalar that is not known (for example, while the item is still loading).
pub const UNAVAILABLE: &str = "Unavailable";
/// Shown for a track list that is empty.
pub const NONE: &str = "None";
/// Shown in place of a text value that looks like a URL or credential.
pub const REDACTED: &str = "(hidden)";

/// Text longer than this many characters is cut and marked with an ellipsis.
pub const MAX_TEXT_CHARS: usize = 80;
/// Tracks shown per kind. Extra tracks are dropped after sorting.
pub const MAX_TRACKS: usize = 64;

const MAX_DIMENSION: u32 = 16_384;
const MAX_FRAME_RATE: f64 = 1_000.0;
pub(crate) const MIN_BIT_DEPTH: u32 = 8;
pub(crate) const MAX_BIT_DEPTH: u32 = 16;
const MAX_CHANNELS: u32 = 32;

/// Raw stream data as the owner read it. Every field may be missing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaInput {
    pub container: Option<String>,
    pub video: Option<VideoInput>,
    pub audio: Vec<AudioInput>,
    pub subtitles: Vec<SubtitleInput>,
    pub playback: PlaybackInput,
}

/// What the player reports about decoding and output while the stream plays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaybackInput {
    /// The hardware decoder in use. mpv reports `no` for software decoding.
    pub hardware_decoder: Option<String>,
    /// The audio output driver in use, for example `wasapi`.
    pub audio_output: Option<String>,
    /// The audio device setting. `auto` means the system default.
    pub audio_device: Option<String>,
    /// Format of the selected subtitle track. `None` when no subtitle is selected.
    pub subtitle_format: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoInput {
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    pub bit_depth: Option<u32>,
    /// Transfer characteristic name, for example `smpte2084`, `arib-std-b67`, or `bt709` (ffmpeg
    /// names), or `pq`, `hlg`, or `bt.1886` (mpv names).
    pub transfer: Option<String>,
    /// Color primaries name, for example `bt2020` or `bt.2020`.
    pub primaries: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioInput {
    pub index: u32,
    pub codec: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubtitleInput {
    pub index: u32,
    pub format: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
}

/// Dynamic range of the video, from its transfer characteristic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynamicRange {
    Unknown,
    Sdr,
    /// HDR with the PQ transfer (`smpte2084`).
    HdrPq,
    /// HDR with the HLG transfer (`arib-std-b67`).
    HdrHlg,
}

impl DynamicRange {
    pub fn is_hdr(self) -> bool {
        matches!(self, Self::HdrPq | Self::HdrHlg)
    }

    fn from_transfer(transfer: &str) -> Self {
        match transfer.to_ascii_lowercase().as_str() {
            "smpte2084" | "pq" => Self::HdrPq,
            "arib-std-b67" | "hlg" => Self::HdrHlg,
            "bt709" | "bt1886" | "bt.1886" | "srgb" | "iec61966-2-1" | "bt601" | "gamma1.8"
            | "gamma2.0" | "gamma2.2" | "gamma2.4" | "gamma2.6" | "gamma2.8" => Self::Sdr,
            _ => Self::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Unknown => UNAVAILABLE,
            Self::Sdr => "SDR",
            Self::HdrPq => "HDR (PQ)",
            Self::HdrHlg => "HDR (HLG)",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoInfo {
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    pub bit_depth: Option<u32>,
    pub dynamic_range: DynamicRange,
    pub primaries: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTrack {
    pub index: u32,
    pub codec: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtitleTrack {
    pub index: u32,
    pub format: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
}

/// Validated view model. Every text field is already cleaned, truncated, and redacted.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaInfo {
    pub container: Option<String>,
    pub video: Option<VideoInfo>,
    pub audio: Vec<AudioTrack>,
    pub subtitles: Vec<SubtitleTrack>,
    pub playback: PlaybackInfo,
}

/// How the video is decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decoding {
    /// The player did not say, for example while the file is still loading.
    Unknown,
    Software,
    /// Decoded on the GPU by the named API, for example `d3d11va`.
    Hardware(String),
}

impl Decoding {
    fn from_reported(name: Option<&str>) -> Self {
        match name.and_then(clean_text) {
            None => Self::Unknown,
            Some(name) if name.eq_ignore_ascii_case("no") => Self::Software,
            Some(name) => Self::Hardware(name),
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Unknown => UNAVAILABLE.to_owned(),
            Self::Software => "Software".to_owned(),
            Self::Hardware(name) => format!("Hardware ({name})"),
        }
    }
}

/// Validated decoding and output facts. Every text field is already cleaned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackInfo {
    pub decoding: Decoding,
    pub audio_output: Option<String>,
    pub audio_device: Option<String>,
    pub subtitle_format: Option<String>,
}

/// One line of the panel: a label and its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoRow {
    pub label: String,
    pub value: String,
}

impl MediaInfo {
    /// Build the view model. Invalid values become unknown (`None`), so the panel always renders.
    pub fn from_input(input: &MediaInput) -> Self {
        let mut audio: Vec<AudioTrack> = input.audio.iter().map(audio_track).collect();
        audio.sort_by_key(|track| track.index);
        audio.truncate(MAX_TRACKS);

        let mut subtitles: Vec<SubtitleTrack> =
            input.subtitles.iter().map(subtitle_track).collect();
        subtitles.sort_by_key(|track| track.index);
        subtitles.truncate(MAX_TRACKS);

        Self {
            container: input.container.as_deref().and_then(clean_text),
            video: input.video.as_ref().map(video_info),
            audio,
            subtitles,
            playback: PlaybackInfo {
                decoding: Decoding::from_reported(input.playback.hardware_decoder.as_deref()),
                audio_output: input.playback.audio_output.as_deref().and_then(clean_text),
                audio_device: input.playback.audio_device.as_deref().and_then(clean_text),
                subtitle_format: input
                    .playback
                    .subtitle_format
                    .as_deref()
                    .and_then(clean_text),
            },
        }
    }

    /// Rows in the fixed display order.
    pub fn rows(&self) -> Vec<InfoRow> {
        let mut rows = Vec::new();
        push(&mut rows, "Container", self.container.clone());

        let video = self.video.as_ref();
        push(
            &mut rows,
            "Video codec",
            video.and_then(|v| v.codec.clone()),
        );
        push(&mut rows, "Resolution", video.and_then(resolution_text));
        push(
            &mut rows,
            "Frame rate",
            video.and_then(|v| v.frame_rate).map(frame_rate_text),
        );
        push(
            &mut rows,
            "Bit depth",
            video
                .and_then(|v| v.bit_depth)
                .map(|depth| format!("{depth}-bit")),
        );
        let range = video.map_or(DynamicRange::Unknown, |v| v.dynamic_range);
        rows.push(InfoRow {
            label: "Dynamic range".to_owned(),
            value: range.label().to_owned(),
        });
        push(
            &mut rows,
            "Color primaries",
            video.and_then(|v| v.primaries.clone()),
        );
        rows.push(row("Hardware decode", &self.playback.decoding.text()));

        if self.audio.is_empty() {
            rows.push(row("Audio", NONE));
        }
        for (position, track) in self.audio.iter().enumerate() {
            rows.push(InfoRow {
                label: format!("Audio {}", position + 1),
                value: audio_text(track),
            });
        }
        rows.push(row("Audio output", &audio_output_text(&self.playback)));

        if self.subtitles.is_empty() {
            rows.push(row("Subtitles", NONE));
        }
        for (position, track) in self.subtitles.iter().enumerate() {
            rows.push(InfoRow {
                label: format!("Subtitle {}", position + 1),
                value: subtitle_text(track),
            });
        }
        rows.push(row(
            "Active subtitle",
            self.playback.subtitle_format.as_deref().unwrap_or(NONE),
        ));
        rows
    }

    /// Plain text for "copy diagnostics": one `label: value` line per row. Contains only the
    /// cleaned values, so it carries no URL, token, or origin.
    pub fn diagnostics_text(&self) -> String {
        let mut out = String::new();
        for row in self.rows() {
            out.push_str(&row.label);
            out.push_str(": ");
            out.push_str(&row.value);
            out.push('\n');
        }
        out
    }
}

fn video_info(input: &VideoInput) -> VideoInfo {
    VideoInfo {
        codec: input.codec.as_deref().and_then(clean_text),
        width: input.width.filter(|&w| (1..=MAX_DIMENSION).contains(&w)),
        height: input.height.filter(|&h| (1..=MAX_DIMENSION).contains(&h)),
        frame_rate: input
            .frame_rate
            .filter(|&fps| fps.is_finite() && fps > 0.0 && fps <= MAX_FRAME_RATE),
        bit_depth: input
            .bit_depth
            .filter(|&depth| (MIN_BIT_DEPTH..=MAX_BIT_DEPTH).contains(&depth)),
        dynamic_range: input
            .transfer
            .as_deref()
            .and_then(clean_text)
            .map_or(DynamicRange::Unknown, |transfer| {
                DynamicRange::from_transfer(&transfer)
            }),
        primaries: input.primaries.as_deref().and_then(clean_text),
    }
}

fn audio_track(input: &AudioInput) -> AudioTrack {
    AudioTrack {
        index: input.index,
        codec: input.codec.as_deref().and_then(clean_text),
        channels: input.channels.filter(|&c| (1..=MAX_CHANNELS).contains(&c)),
        channel_layout: input.channel_layout.as_deref().and_then(clean_text),
        language: input.language.as_deref().and_then(clean_text),
        title: input.title.as_deref().and_then(clean_text),
    }
}

fn subtitle_track(input: &SubtitleInput) -> SubtitleTrack {
    SubtitleTrack {
        index: input.index,
        format: input.format.as_deref().and_then(clean_text),
        language: input.language.as_deref().and_then(clean_text),
        title: input.title.as_deref().and_then(clean_text),
        forced: input.forced,
    }
}

/// Clean one text value. Control characters are removed and the result is trimmed. An empty
/// result is `None`. A value that looks like a URL or credential becomes [`REDACTED`]. Longer
/// text is cut to [`MAX_TEXT_CHARS`] with an ellipsis.
fn clean_text(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|ch| !ch.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if has_scheme_separator(trimmed) || is_secret_like_value(trimmed) {
        return Some(REDACTED.to_owned());
    }
    if trimmed.chars().count() > MAX_TEXT_CHARS {
        let kept: String = trimmed.chars().take(MAX_TEXT_CHARS - 1).collect();
        return Some(format!("{kept}…"));
    }
    Some(trimmed.to_owned())
}

fn push(rows: &mut Vec<InfoRow>, label: &str, value: Option<String>) {
    rows.push(InfoRow {
        label: label.to_owned(),
        value: value.unwrap_or_else(|| UNAVAILABLE.to_owned()),
    });
}

fn row(label: &str, value: &str) -> InfoRow {
    InfoRow {
        label: label.to_owned(),
        value: value.to_owned(),
    }
}

fn resolution_text(video: &VideoInfo) -> Option<String> {
    match (video.width, video.height) {
        (Some(width), Some(height)) => Some(format!("{width} x {height}")),
        _ => None,
    }
}

fn frame_rate_text(fps: f64) -> String {
    format!("{fps:.3} fps")
}

fn audio_text(track: &AudioTrack) -> String {
    let layout = match (track.channel_layout.as_deref(), track.channels) {
        (Some(layout), Some(count)) => format!("{layout} ({count} ch)"),
        (Some(layout), None) => layout.to_owned(),
        (None, Some(count)) => format!("{count} ch"),
        (None, None) => UNAVAILABLE.to_owned(),
    };
    join_parts(&[
        track.codec.clone(),
        Some(layout),
        track.language.clone(),
        track.title.clone(),
    ])
}

/// The device setting `auto` is the system default, so it reads as that.
fn audio_output_text(playback: &PlaybackInfo) -> String {
    let device = playback.audio_device.as_deref().map(|device| {
        if device == "auto" {
            "default device".to_owned()
        } else {
            device.to_owned()
        }
    });
    join_parts(&[playback.audio_output.clone(), device])
}

fn subtitle_text(track: &SubtitleTrack) -> String {
    let forced = track.forced.then(|| "forced".to_owned());
    join_parts(&[
        track.format.clone(),
        track.language.clone(),
        track.title.clone(),
        forced,
    ])
}

/// Join the present parts with ", ". When every part is missing, the value is [`UNAVAILABLE`].
fn join_parts(parts: &[Option<String>]) -> String {
    let present: Vec<&str> = parts.iter().flatten().map(String::as_str).collect();
    if present.is_empty() {
        UNAVAILABLE.to_owned()
    } else {
        present.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioInput, Decoding, DynamicRange, MAX_TEXT_CHARS, MediaInfo, MediaInput, NONE,
        PlaybackInput, REDACTED, SubtitleInput, UNAVAILABLE, VideoInput,
    };

    fn fixture() -> MediaInput {
        MediaInput {
            container: Some("mkv".to_owned()),
            video: Some(VideoInput {
                codec: Some("hevc".to_owned()),
                width: Some(3840),
                height: Some(2160),
                frame_rate: Some(23.976),
                bit_depth: Some(10),
                transfer: Some("smpte2084".to_owned()),
                primaries: Some("bt2020".to_owned()),
            }),
            audio: vec![
                AudioInput {
                    index: 2,
                    codec: Some("eac3".to_owned()),
                    channels: Some(6),
                    channel_layout: Some("5.1".to_owned()),
                    language: Some("eng".to_owned()),
                    title: None,
                },
                AudioInput {
                    index: 1,
                    codec: Some("aac".to_owned()),
                    channels: Some(2),
                    channel_layout: Some("stereo".to_owned()),
                    language: Some("jpn".to_owned()),
                    title: Some("Commentary".to_owned()),
                },
            ],
            subtitles: vec![
                SubtitleInput {
                    index: 4,
                    format: Some("subrip".to_owned()),
                    language: Some("eng".to_owned()),
                    title: None,
                    forced: true,
                },
                SubtitleInput {
                    index: 3,
                    format: Some("ass".to_owned()),
                    language: Some("eng".to_owned()),
                    title: Some("Full".to_owned()),
                    forced: false,
                },
            ],
            playback: PlaybackInput {
                hardware_decoder: Some("d3d11va".to_owned()),
                audio_output: Some("wasapi".to_owned()),
                audio_device: Some("auto".to_owned()),
                subtitle_format: Some("ass".to_owned()),
            },
        }
    }

    fn value_of(info: &MediaInfo, label: &str) -> String {
        info.rows()
            .into_iter()
            .find(|row| row.label == label)
            .map(|row| row.value)
            .unwrap_or_else(|| panic!("missing row {label}"))
    }

    #[test]
    fn fixture_renders_its_known_properties() {
        let info = MediaInfo::from_input(&fixture());
        assert_eq!(value_of(&info, "Container"), "mkv");
        assert_eq!(value_of(&info, "Video codec"), "hevc");
        assert_eq!(value_of(&info, "Resolution"), "3840 x 2160");
        assert_eq!(value_of(&info, "Frame rate"), "23.976 fps");
        assert_eq!(value_of(&info, "Bit depth"), "10-bit");
        assert_eq!(value_of(&info, "Dynamic range"), "HDR (PQ)");
        assert_eq!(value_of(&info, "Color primaries"), "bt2020");
    }

    #[test]
    fn rows_follow_a_stable_order_regardless_of_input_order() {
        let info = MediaInfo::from_input(&fixture());
        let labels: Vec<String> = info.rows().into_iter().map(|row| row.label).collect();
        assert_eq!(
            labels,
            [
                "Container",
                "Video codec",
                "Resolution",
                "Frame rate",
                "Bit depth",
                "Dynamic range",
                "Color primaries",
                "Hardware decode",
                "Audio 1",
                "Audio 2",
                "Audio output",
                "Subtitle 1",
                "Subtitle 2",
                "Active subtitle",
            ]
        );
        assert_eq!(
            value_of(&info, "Audio 1"),
            "aac, stereo (2 ch), jpn, Commentary"
        );
        assert_eq!(value_of(&info, "Audio 2"), "eac3, 5.1 (6 ch), eng");
        assert_eq!(value_of(&info, "Subtitle 1"), "ass, eng, Full");
        assert_eq!(value_of(&info, "Subtitle 2"), "subrip, eng, forced");
        assert_eq!(value_of(&info, "Hardware decode"), "Hardware (d3d11va)");
        assert_eq!(value_of(&info, "Audio output"), "wasapi, default device");
        assert_eq!(value_of(&info, "Active subtitle"), "ass");

        let mut reversed = fixture();
        reversed.audio.reverse();
        reversed.subtitles.reverse();
        assert_eq!(MediaInfo::from_input(&reversed).rows(), info.rows());
    }

    #[test]
    fn empty_input_renders_placeholders_without_panicking() {
        let info = MediaInfo::from_input(&MediaInput::default());
        assert_eq!(value_of(&info, "Container"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Video codec"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Resolution"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Dynamic range"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Hardware decode"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Audio"), NONE);
        assert_eq!(value_of(&info, "Audio output"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Subtitles"), NONE);
        assert_eq!(value_of(&info, "Active subtitle"), NONE);
    }

    #[test]
    fn invalid_numbers_become_unavailable() {
        let input = MediaInput {
            video: Some(VideoInput {
                width: Some(0),
                height: Some(100_000),
                frame_rate: Some(f64::NAN),
                bit_depth: Some(3),
                ..VideoInput::default()
            }),
            ..MediaInput::default()
        };
        let info = MediaInfo::from_input(&input);
        assert_eq!(value_of(&info, "Resolution"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Frame rate"), UNAVAILABLE);
        assert_eq!(value_of(&info, "Bit depth"), UNAVAILABLE);
    }

    #[test]
    fn hdr_flag_comes_from_the_transfer() {
        let mut input = fixture();
        input.video.as_mut().expect("video").transfer = Some("ARIB-STD-B67".to_owned());
        let hlg = MediaInfo::from_input(&input);
        assert_eq!(
            hlg.video.as_ref().map(|v| v.dynamic_range),
            Some(DynamicRange::HdrHlg)
        );
        assert!(DynamicRange::HdrHlg.is_hdr());

        input.video.as_mut().expect("video").transfer = Some("bt709".to_owned());
        assert_eq!(
            value_of(&MediaInfo::from_input(&input), "Dynamic range"),
            "SDR"
        );

        input.video.as_mut().expect("video").transfer = Some("mystery".to_owned());
        assert_eq!(
            value_of(&MediaInfo::from_input(&input), "Dynamic range"),
            UNAVAILABLE
        );
        assert!(!DynamicRange::Sdr.is_hdr());
    }

    #[test]
    fn long_titles_are_truncated() {
        let mut input = fixture();
        // Input index 4 sorts to "Subtitle 2".
        input.subtitles[0].title = Some("x".repeat(500));
        let value = value_of(&MediaInfo::from_input(&input), "Subtitle 2");
        assert!(value.contains('…'));
        assert!(value.chars().count() <= MAX_TEXT_CHARS + "subrip, eng, , forced".len());
    }

    #[test]
    fn urls_and_credentials_are_never_shown() {
        let mut input = fixture();
        input.container = Some("https://media.example:8920/Videos/1?api_key=secret".to_owned());
        // Sorted by index: audio[1] is "Audio 1" and subtitles[1] is "Subtitle 1".
        input.audio[1].title = Some("token=abc123".to_owned());
        input.subtitles[1].title = Some("http://10.0.0.5:8096/Subtitles".to_owned());
        let info = MediaInfo::from_input(&input);
        assert_eq!(value_of(&info, "Container"), REDACTED);
        assert!(value_of(&info, "Audio 1").contains(REDACTED));
        assert!(value_of(&info, "Subtitle 1").contains(REDACTED));
    }

    #[test]
    fn diagnostics_text_contains_no_token_key_or_origin() {
        let mut input = fixture();
        input.video.as_mut().expect("video").codec =
            Some("https://media.example/stream?api_key=hidden".to_owned());
        input.subtitles[0].language = Some("token=xyz".to_owned());
        let text = MediaInfo::from_input(&input).diagnostics_text();
        for forbidden in [
            "api_key",
            "token",
            "media.example",
            "http",
            "://",
            "hidden=",
            "xyz",
        ] {
            assert!(
                !text.contains(forbidden),
                "diagnostics leaked {forbidden}: {text}"
            );
        }
        assert!(text.contains("Video codec: (hidden)"));
        assert_eq!(text.lines().count(), 14);
    }

    #[test]
    fn mpv_transfer_names_map_to_a_dynamic_range() {
        for (name, range) in [
            ("pq", DynamicRange::HdrPq),
            ("hlg", DynamicRange::HdrHlg),
            ("bt.1886", DynamicRange::Sdr),
            ("srgb", DynamicRange::Sdr),
            ("gamma2.2", DynamicRange::Sdr),
            ("linear", DynamicRange::Unknown),
            ("v-log", DynamicRange::Unknown),
        ] {
            let mut input = fixture();
            input.video.as_mut().expect("video").transfer = Some(name.to_owned());
            let info = MediaInfo::from_input(&input);
            assert_eq!(
                info.video.as_ref().map(|v| v.dynamic_range),
                Some(range),
                "{name}"
            );
        }
    }

    #[test]
    fn the_hardware_decoder_name_is_what_the_player_reported() {
        for (reported, expected) in [
            (None, Decoding::Unknown),
            (Some("no"), Decoding::Software),
            (Some("NO"), Decoding::Software),
            (Some("nvdec"), Decoding::Hardware("nvdec".to_owned())),
            (
                Some("d3d11va-copy"),
                Decoding::Hardware("d3d11va-copy".to_owned()),
            ),
            (Some("  "), Decoding::Unknown),
        ] {
            let mut input = fixture();
            input.playback.hardware_decoder = reported.map(str::to_owned);
            assert_eq!(
                MediaInfo::from_input(&input).playback.decoding,
                expected,
                "{reported:?}"
            );
        }
    }

    #[test]
    fn the_audio_output_row_names_the_driver_and_a_non_default_device() {
        let mut input = fixture();
        input.playback.audio_device = Some("wasapi/speakers".to_owned());
        assert_eq!(
            value_of(&MediaInfo::from_input(&input), "Audio output"),
            "wasapi, wasapi/speakers"
        );
        input.playback.audio_output = None;
        assert_eq!(
            value_of(&MediaInfo::from_input(&input), "Audio output"),
            "wasapi/speakers"
        );
    }

    #[test]
    fn player_text_that_looks_like_a_url_or_credential_is_hidden() {
        let mut input = fixture();
        input.playback.audio_device = Some("https://media.example/audio".to_owned());
        input.playback.subtitle_format = Some("token=abc123".to_owned());
        let info = MediaInfo::from_input(&input);
        assert!(value_of(&info, "Audio output").contains(REDACTED));
        assert_eq!(value_of(&info, "Active subtitle"), REDACTED);
        assert!(!info.diagnostics_text().contains("media.example"));
    }
}
