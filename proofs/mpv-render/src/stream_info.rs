//! Owned snapshot of the playing stream's properties, for the media info panel (M6-14).
//!
//! Every field is one libmpv property read as a string on the owner thread, through the same
//! `mpv_get_property_string` path the capability probe uses. No libmpv-owned memory leaves the
//! thread. A property that libmpv reports unavailable stays explicit as
//! [`CapabilityValue::Unavailable`]; it is never turned into an empty string or a zero.
//!
//! The property names were confirmed against mpv 0.37.0 over its JSON IPC on an HEVC/AC-3 file
//! and an H.264/AAC file (see `docs/evidence/media-info-panel.md`). The libmpv client API that
//! this adapter loads (2.5) was not available on the machine that wrote this module, so the
//! FFI read itself is Unvalidated until it runs on Windows.

use std::ffi::CStr;

use kattri_client_core::media_info_mpv::MpvStreamValues;

use crate::{
    capabilities::{CapabilityValue, read_string},
    ffi::{Api, MpvHandle},
};

/// Property names read for the snapshot. Each is an mpv property of the same name.
pub(crate) mod property {
    use std::ffi::CStr;

    pub(crate) const CONTAINER: &CStr = c"file-format";
    pub(crate) const VIDEO_CODEC: &CStr = c"current-tracks/video/codec";
    pub(crate) const WIDTH: &CStr = c"video-params/w";
    pub(crate) const HEIGHT: &CStr = c"video-params/h";
    pub(crate) const FRAME_RATE: &CStr = c"container-fps";
    pub(crate) const PIXEL_FORMAT: &CStr = c"video-params/pixelformat";
    pub(crate) const TRANSFER: &CStr = c"video-params/gamma";
    pub(crate) const PRIMARIES: &CStr = c"video-params/primaries";
    pub(crate) const HARDWARE_DECODER: &CStr = c"hwdec-current";
    pub(crate) const AUDIO_CODEC: &CStr = c"current-tracks/audio/codec";
    pub(crate) const AUDIO_CHANNEL_COUNT: &CStr = c"audio-params/channel-count";
    pub(crate) const AUDIO_CHANNEL_LAYOUT: &CStr = c"audio-params/hr-channels";
    pub(crate) const AUDIO_OUTPUT: &CStr = c"current-ao";
    pub(crate) const AUDIO_DEVICE: &CStr = c"audio-device";
    pub(crate) const SUBTITLE_CODEC: &CStr = c"current-tracks/sub/codec";
}

/// What libmpv reports about the running stream and its outputs at one moment.
///
/// Values are raw mpv strings, for example `pq`, `bt.2020`, or `yuv420p10`. The view model in
/// `kattri-client-core` interprets them. The snapshot carries no URL, path, or token: none of
/// these properties holds one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSnapshot {
    /// Demuxer name, `file-format`.
    pub container: CapabilityValue<String>,
    /// Codec of the selected video track.
    pub video_codec: CapabilityValue<String>,
    /// Width of the decoded frame in pixels, `video-params/w`.
    pub width: CapabilityValue<String>,
    /// Height of the decoded frame in pixels, `video-params/h`.
    pub height: CapabilityValue<String>,
    /// Frame rate the container declares, `container-fps`.
    pub frame_rate: CapabilityValue<String>,
    /// Pixel format after decoding, for example `yuv420p10`.
    pub pixel_format: CapabilityValue<String>,
    /// Transfer characteristic mpv uses, `video-params/gamma`: `pq`, `hlg`, `bt.1886`, ...
    pub transfer: CapabilityValue<String>,
    /// Color primaries mpv uses, `video-params/primaries`: `bt.2020`, `bt.709`, ...
    pub primaries: CapabilityValue<String>,
    /// Hardware decoder in use, `hwdec-current`. mpv reports `no` for software decoding.
    pub hardware_decoder: CapabilityValue<String>,
    /// Codec of the selected audio track.
    pub audio_codec: CapabilityValue<String>,
    /// Channel count of the decoded audio, `audio-params/channel-count`.
    pub audio_channel_count: CapabilityValue<String>,
    /// Human-readable channel layout of the decoded audio, `audio-params/hr-channels`.
    pub audio_channel_layout: CapabilityValue<String>,
    /// Audio output driver in use, `current-ao`.
    pub audio_output: CapabilityValue<String>,
    /// Audio device setting, `audio-device`.
    pub audio_device: CapabilityValue<String>,
    /// Codec of the selected subtitle track. Unavailable when no subtitle is selected.
    pub subtitle_codec: CapabilityValue<String>,
}

impl StreamSnapshot {
    /// Run only on the libmpv owner thread, after the file has loaded.
    pub(crate) fn probe(api: &Api, handle: *mut MpvHandle) -> Self {
        Self::from_reader(|name| {
            // SAFETY: The worker calls this on the owner thread, serialized with every other
            // client API call. `name` is a static NUL-terminated mpv property name; the string
            // libmpv returns is copied and freed inside `read_string`.
            unsafe { read_string(api, handle, name) }
        })
    }

    /// Read every property through `read`. Kept apart from [`Self::probe`] so the property list
    /// is testable without libmpv.
    pub(crate) fn from_reader(mut read: impl FnMut(&CStr) -> CapabilityValue<String>) -> Self {
        Self {
            container: read(property::CONTAINER),
            video_codec: read(property::VIDEO_CODEC),
            width: read(property::WIDTH),
            height: read(property::HEIGHT),
            frame_rate: read(property::FRAME_RATE),
            pixel_format: read(property::PIXEL_FORMAT),
            transfer: read(property::TRANSFER),
            primaries: read(property::PRIMARIES),
            hardware_decoder: read(property::HARDWARE_DECODER),
            audio_codec: read(property::AUDIO_CODEC),
            audio_channel_count: read(property::AUDIO_CHANNEL_COUNT),
            audio_channel_layout: read(property::AUDIO_CHANNEL_LAYOUT),
            audio_output: read(property::AUDIO_OUTPUT),
            audio_device: read(property::AUDIO_DEVICE),
            subtitle_codec: read(property::SUBTITLE_CODEC),
        }
    }

    /// The same values as plain optional strings, for the media info view model in
    /// `kattri-client-core`. Unavailable properties become `None`.
    pub fn stream_values(&self) -> MpvStreamValues {
        let text = |value: &CapabilityValue<String>| available_text(value).map(str::to_owned);
        MpvStreamValues {
            container: text(&self.container),
            video_codec: text(&self.video_codec),
            width: text(&self.width),
            height: text(&self.height),
            frame_rate: text(&self.frame_rate),
            pixel_format: text(&self.pixel_format),
            transfer: text(&self.transfer),
            primaries: text(&self.primaries),
            hardware_decoder: text(&self.hardware_decoder),
            audio_codec: text(&self.audio_codec),
            audio_channel_count: text(&self.audio_channel_count),
            audio_channel_layout: text(&self.audio_channel_layout),
            audio_output: text(&self.audio_output),
            audio_device: text(&self.audio_device),
            subtitle_codec: text(&self.subtitle_codec),
        }
    }
}

/// The value when it is available, or `None` when libmpv reported it unavailable.
pub fn available_text(value: &CapabilityValue<String>) -> Option<&str> {
    match value {
        CapabilityValue::Available(text) => Some(text.as_str()),
        CapabilityValue::Unavailable(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::capabilities::CapabilityUnavailable;

    /// Values copied from mpv 0.37.0 playing an HEVC 10-bit HDR10 file with AC-3 5.1 audio, read
    /// over its JSON IPC with `--vo=null --ao=null`. The sandbox has no GPU or sound card, so
    /// `hwdec-current` is `no` and `current-ao` is `null`.
    fn captured_hdr_values() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("file-format", "mkv"),
            ("current-tracks/video/codec", "hevc"),
            ("video-params/w", "640"),
            ("video-params/h", "360"),
            ("container-fps", "23.976025"),
            ("video-params/pixelformat", "yuv420p10"),
            ("video-params/gamma", "pq"),
            ("video-params/primaries", "bt.2020"),
            ("hwdec-current", "no"),
            ("current-tracks/audio/codec", "ac3"),
            ("audio-params/channel-count", "6"),
            ("audio-params/hr-channels", "5.1(side)"),
            ("current-ao", "null"),
            ("audio-device", "auto"),
        ])
    }

    fn reader(
        values: HashMap<&'static str, &'static str>,
    ) -> impl FnMut(&CStr) -> CapabilityValue<String> {
        move |name| match name.to_str().ok().and_then(|key| values.get(key)) {
            Some(value) => CapabilityValue::Available((*value).to_owned()),
            None => CapabilityValue::Unavailable(CapabilityUnavailable::PropertyNotAvailable),
        }
    }

    #[test]
    fn captured_hdr_file_fills_every_stream_field() {
        let snapshot = StreamSnapshot::from_reader(reader(captured_hdr_values()));
        assert_eq!(available_text(&snapshot.container), Some("mkv"));
        assert_eq!(available_text(&snapshot.video_codec), Some("hevc"));
        assert_eq!(available_text(&snapshot.width), Some("640"));
        assert_eq!(available_text(&snapshot.height), Some("360"));
        assert_eq!(available_text(&snapshot.frame_rate), Some("23.976025"));
        assert_eq!(available_text(&snapshot.pixel_format), Some("yuv420p10"));
        assert_eq!(available_text(&snapshot.transfer), Some("pq"));
        assert_eq!(available_text(&snapshot.primaries), Some("bt.2020"));
        assert_eq!(available_text(&snapshot.hardware_decoder), Some("no"));
        assert_eq!(available_text(&snapshot.audio_codec), Some("ac3"));
        assert_eq!(available_text(&snapshot.audio_channel_count), Some("6"));
        assert_eq!(
            available_text(&snapshot.audio_channel_layout),
            Some("5.1(side)")
        );
        assert_eq!(available_text(&snapshot.audio_output), Some("null"));
        assert_eq!(available_text(&snapshot.audio_device), Some("auto"));
    }

    #[test]
    fn plain_values_keep_what_is_available_and_drop_the_rest() {
        let snapshot = StreamSnapshot::from_reader(reader(captured_hdr_values()));
        let values = snapshot.stream_values();
        assert_eq!(values.container.as_deref(), Some("mkv"));
        assert_eq!(values.transfer.as_deref(), Some("pq"));
        assert_eq!(values.audio_channel_layout.as_deref(), Some("5.1(side)"));
        assert_eq!(values.hardware_decoder.as_deref(), Some("no"));
        assert_eq!(values.subtitle_codec, None);
    }

    #[test]
    fn an_unselected_subtitle_stays_unavailable() {
        // mpv reports `current-tracks/sub/codec` as "property unavailable" while no subtitle
        // track is selected (captured from the same file).
        let snapshot = StreamSnapshot::from_reader(reader(captured_hdr_values()));
        assert_eq!(
            snapshot.subtitle_codec,
            CapabilityValue::Unavailable(CapabilityUnavailable::PropertyNotAvailable)
        );
        assert_eq!(available_text(&snapshot.subtitle_codec), None);
    }

    #[test]
    fn nothing_loaded_leaves_every_field_unavailable() {
        let snapshot = StreamSnapshot::from_reader(reader(HashMap::new()));
        for value in [
            &snapshot.container,
            &snapshot.video_codec,
            &snapshot.width,
            &snapshot.height,
            &snapshot.frame_rate,
            &snapshot.pixel_format,
            &snapshot.transfer,
            &snapshot.primaries,
            &snapshot.hardware_decoder,
            &snapshot.audio_codec,
            &snapshot.audio_channel_count,
            &snapshot.audio_channel_layout,
            &snapshot.audio_output,
            &snapshot.audio_device,
            &snapshot.subtitle_codec,
        ] {
            assert_eq!(available_text(value), None);
        }
    }

    #[test]
    fn the_snapshot_reads_each_property_exactly_once() {
        let mut names = Vec::new();
        StreamSnapshot::from_reader(|name| {
            names.push(name.to_string_lossy().into_owned());
            CapabilityValue::Unavailable(CapabilityUnavailable::PropertyNotAvailable)
        });
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), 15);
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn no_property_can_hold_a_url_or_credential() {
        // The panel must never show a stream URL, token, or server origin. These are the only
        // properties the snapshot reads; none of them is `path`, `stream-open-filename`,
        // `filename`, or `metadata`, which can carry a URL.
        let mut names = Vec::new();
        StreamSnapshot::from_reader(|name| {
            names.push(name.to_string_lossy().into_owned());
            CapabilityValue::Unavailable(CapabilityUnavailable::PropertyNotAvailable)
        });
        for forbidden in ["path", "filename", "stream-open", "metadata", "title"] {
            assert!(
                names.iter().all(|name| !name.contains(forbidden)),
                "snapshot reads {forbidden}"
            );
        }
    }
}
