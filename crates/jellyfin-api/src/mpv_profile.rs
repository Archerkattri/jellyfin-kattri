//! Validated, measured libmpv capabilities and their Jellyfin profile projection.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    io::Read,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::device_profile::ContainerProfile;
use crate::{
    CodecProfile, DeviceProfile, DirectPlayProfile, ProfileCondition, SubtitleProfile,
    TranscodingProfile,
};

const MAX_PROFILE_BYTES: usize = 1024 * 1024;

const EMBEDDED_BASELINE: &str = include_str!("../profiles/mpv-baseline.json");

/// Versioned, build-independent software-decoding capability assumptions.
/// Runtime inventory can only narrow these lists; hardware/output claims remain separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MpvSoftwareBaseline {
    pub schema_version: u32,
    pub software_baseline: SoftwareCapabilities,
    pub provenance: Vec<ProfileProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SoftwareCapabilities {
    pub video_containers: Vec<String>,
    pub audio_containers: Vec<String>,
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    pub subtitle_profiles: Vec<BaselineSubtitleProfile>,
    pub max_video_bit_depth: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BaselineSubtitleProfile {
    pub format: String,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileProvenance {
    pub source: String,
    pub revision: String,
    pub url: String,
    pub informs: Vec<String>,
}

/// Normalized runtime inventory. `None` means unavailable, not unsupported.
/// These are software inventory facts; hardware and output are opt-in evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    pub video_containers: Option<BTreeSet<String>>,
    pub audio_containers: Option<BTreeSet<String>>,
    pub video_codecs: Option<BTreeSet<String>>,
    pub audio_codecs: Option<BTreeSet<String>>,
    pub max_video_bit_depth: Option<u8>,
    pub hdr_output: Option<bool>,
    pub audio_passthrough: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveMpvProfile {
    pub video_containers: BTreeSet<String>,
    pub audio_containers: BTreeSet<String>,
    pub video_codecs: BTreeSet<String>,
    pub audio_codecs: BTreeSet<String>,
    pub max_video_bit_depth: u8,
    pub hdr_output: bool,
    pub audio_passthrough: bool,
}

/// Errors loading the committed baseline. Callers should use `conservative()` as fallback.
#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    #[error("unsupported baseline schema version {0}")]
    SchemaVersion(u32),
    #[error("baseline is malformed: {0}")]
    Parse(String),
    #[error("baseline contains an empty capability value")]
    EmptyCapability,
}

impl MpvSoftwareBaseline {
    /// Parse and validate the committed baseline embedded in the binary.
    pub fn embedded() -> Result<Self, BaselineError> {
        let baseline: Self = serde_json::from_str(EMBEDDED_BASELINE)
            .map_err(|error| BaselineError::Parse(error.to_string()))?;
        baseline.validate()?;
        Ok(baseline)
    }

    /// Minimal safe fallback used if the committed baseline cannot be parsed.
    pub fn conservative() -> Self {
        Self {
            schema_version: 1,
            software_baseline: SoftwareCapabilities {
                video_containers: vec!["mp4".into()],
                audio_containers: vec!["mp4".into()],
                video_codecs: vec!["h264".into()],
                audio_codecs: vec!["aac".into()],
                subtitle_profiles: vec![BaselineSubtitleProfile {
                    format: "srt".into(),
                    method: "External".into(),
                }],
                max_video_bit_depth: 8,
            },
            provenance: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), BaselineError> {
        if self.schema_version != 1 {
            return Err(BaselineError::SchemaVersion(self.schema_version));
        }
        let caps = &self.software_baseline;
        if caps.max_video_bit_depth == 0
            || caps
                .video_containers
                .iter()
                .chain(caps.audio_containers.iter())
                .any(|value| value.trim().is_empty())
            || caps
                .video_codecs
                .iter()
                .any(|value| value.trim().is_empty())
            || caps
                .audio_codecs
                .iter()
                .any(|value| value.trim().is_empty())
            || caps
                .subtitle_profiles
                .iter()
                .any(|profile| profile.format.trim().is_empty() || profile.method.trim().is_empty())
        {
            return Err(BaselineError::EmptyCapability);
        }
        Ok(())
    }

    /// Refine the software baseline. Missing inventory preserves baseline; a present
    /// inventory intersects it. Unknown hardware/HDR/passthrough never becomes true.
    pub fn merge(&self, runtime: &RuntimeCapabilities) -> EffectiveMpvProfile {
        let base = &self.software_baseline;
        EffectiveMpvProfile {
            video_containers: intersect(&base.video_containers, runtime.video_containers.as_ref()),
            audio_containers: intersect(&base.audio_containers, runtime.audio_containers.as_ref()),
            video_codecs: intersect(&base.video_codecs, runtime.video_codecs.as_ref()),
            audio_codecs: intersect(&base.audio_codecs, runtime.audio_codecs.as_ref()),
            max_video_bit_depth: runtime
                .max_video_bit_depth
                .filter(|depth| *depth > 0)
                .map_or(base.max_video_bit_depth, |depth| {
                    depth.min(base.max_video_bit_depth)
                }),
            hdr_output: runtime.hdr_output == Some(true),
            audio_passthrough: runtime.audio_passthrough == Some(true),
        }
    }

    /// Convert libmpv's build inventory names into the Jellyfin naming used by
    /// this baseline. An unavailable property (`None`) remains unknown and does
    /// not narrow the baseline; an available empty list disables that category.
    pub fn runtime_capabilities_from_inventory(
        &self,
        decoder_names: Option<&[String]>,
        demuxer_names: Option<&[String]>,
    ) -> RuntimeCapabilities {
        let decoder_names = decoder_names.map(normalized_names);
        let demuxer_names = demuxer_names.map(normalized_names);
        let baseline = &self.software_baseline;
        RuntimeCapabilities {
            video_containers: demuxer_names.as_ref().map(|names| {
                baseline
                    .video_containers
                    .iter()
                    .filter(|container| container_demuxer_supported(container, names))
                    .cloned()
                    .collect()
            }),
            audio_containers: demuxer_names.as_ref().map(|names| {
                baseline
                    .audio_containers
                    .iter()
                    .filter(|container| container_demuxer_supported(container, names))
                    .cloned()
                    .collect()
            }),
            video_codecs: decoder_names.as_ref().map(|names| {
                baseline
                    .video_codecs
                    .iter()
                    .filter(|codec| decoder_supported(codec, names))
                    .cloned()
                    .collect()
            }),
            audio_codecs: decoder_names.as_ref().map(|names| {
                baseline
                    .audio_codecs
                    .iter()
                    .filter(|codec| decoder_supported(codec, names))
                    .cloned()
                    .collect()
            }),
            ..RuntimeCapabilities::default()
        }
    }
}

/// Lower-cased, trimmed names. FFmpeg groups several names for one demuxer in a single
/// comma-separated entry (`matroska,webm`, `mov,mp4,m4a,3gp,3g2,mj2`), so each entry is split.
fn normalized_names(values: &[String]) -> BTreeSet<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect()
}

fn decoder_supported(codec: &str, decoders: &BTreeSet<String>) -> bool {
    if decoders.contains(codec) {
        return true;
    }
    match codec {
        // FFmpeg's DTS decoder handles the DTS-HD/DTS-HD MA bitstreams; keep
        // this scoped to decode support, not HDMI bitstream passthrough.
        "dts-hd" => decoders.contains("dts"),
        _ => false,
    }
}

fn container_demuxer_supported(container: &str, demuxers: &BTreeSet<String>) -> bool {
    let aliases: &[&str] = match container {
        "mkv" => &["matroska"],
        "m2ts" | "mpegts" | "ts" => &["mpegts", "mpegtsraw"],
        "m4v" | "mp4" | "mov" => &["mov"],
        "webm" => &["webm", "matroska"],
        "ogg" => &["ogg"],
        "avi" => &["avi"],
        "flac" => &["flac"],
        "mp3" => &["mp3"],
        "wav" => &["wav"],
        _ => &[""],
    };
    aliases.iter().any(|alias| demuxers.contains(*alias))
}

impl EffectiveMpvProfile {
    /// Project normalized capabilities to Jellyfin's current DeviceProfile schema.
    pub fn to_device_profile(&self, baseline: &MpvSoftwareBaseline) -> DeviceProfile {
        let mut direct_play_profiles = Vec::new();
        if !self.video_containers.is_empty()
            && !self.video_codecs.is_empty()
            && !self.audio_codecs.is_empty()
        {
            direct_play_profiles.push(DirectPlayProfile {
                container: Some(join(&self.video_containers)),
                audio_codec: nonempty_join(&self.audio_codecs),
                video_codec: nonempty_join(&self.video_codecs),
                profile_type: Some("Video".into()),
                ..DirectPlayProfile::default()
            });
        }
        if !self.audio_containers.is_empty() && !self.audio_codecs.is_empty() {
            direct_play_profiles.push(DirectPlayProfile {
                container: Some(join(&self.audio_containers)),
                audio_codec: nonempty_join(&self.audio_codecs),
                profile_type: Some("Audio".into()),
                ..DirectPlayProfile::default()
            });
        }

        let mut codec_profiles = Vec::new();
        if !self.video_codecs.is_empty() {
            let mut conditions = vec![ProfileCondition {
                condition: Some("LessThanEqual".into()),
                property: Some("VideoBitDepth".into()),
                value: Some(self.max_video_bit_depth.to_string()),
                is_required: Some(true),
            }];
            if !self.hdr_output {
                conditions.push(ProfileCondition {
                    condition: Some("Equals".into()),
                    property: Some("VideoRangeType".into()),
                    value: Some("SDR".into()),
                    is_required: Some(true),
                });
            }
            codec_profiles.push(CodecProfile {
                profile_type: Some("Video".into()),
                conditions,
                codec: nonempty_join(&self.video_codecs),
                ..CodecProfile::default()
            });
        }

        let subtitle_profiles = baseline
            .software_baseline
            .subtitle_profiles
            .iter()
            .map(|profile| SubtitleProfile {
                format: Some(profile.format.clone()),
                method: Some(profile.method.clone()),
                ..SubtitleProfile::default()
            })
            .collect();
        let transcoding_audio_codecs: Vec<&str> = ["aac", "ac3", "eac3"]
            .into_iter()
            .filter(|codec| self.audio_codecs.contains(*codec))
            .collect();
        let transcoding_profiles = if self.video_containers.contains("ts")
            && self.video_codecs.contains("h264")
            && !transcoding_audio_codecs.is_empty()
        {
            vec![TranscodingProfile {
                container: Some("ts".into()),
                profile_type: Some("Video".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some(transcoding_audio_codecs.join(",")),
                protocol: Some("hls".into()),
                context: Some("Streaming".into()),
                ..TranscodingProfile::default()
            }]
        } else {
            Vec::new()
        };

        DeviceProfile {
            name: Some("Jellyfin Kattri (libmpv)".into()),
            direct_play_profiles: Some(direct_play_profiles),
            container_profiles: Some(
                self.video_containers
                    .iter()
                    .map(|container| ContainerProfile {
                        profile_type: Some("Video".into()),
                        container: Some(container.clone()),
                        ..ContainerProfile::default()
                    })
                    .chain(
                        self.audio_containers
                            .iter()
                            .map(|container| ContainerProfile {
                                profile_type: Some("Audio".into()),
                                container: Some(container.clone()),
                                ..ContainerProfile::default()
                            }),
                    )
                    .collect(),
            ),
            codec_profiles: Some(codec_profiles),
            transcoding_profiles: Some(transcoding_profiles),
            subtitle_profiles: Some(subtitle_profiles),
            ..DeviceProfile::default()
        }
    }
}

fn intersect(base: &[String], observed: Option<&BTreeSet<String>>) -> BTreeSet<String> {
    match observed {
        Some(observed) => base
            .iter()
            .filter(|value| observed.contains(&value.to_ascii_lowercase()))
            .cloned()
            .collect(),
        None => base.iter().cloned().collect(),
    }
}

fn join(values: &BTreeSet<String>) -> String {
    values.iter().cloned().collect::<Vec<_>>().join(",")
}

fn nonempty_join(values: &BTreeSet<String>) -> Option<String> {
    (!values.is_empty()).then(|| join(values))
}

/// A platform-specific libmpv capability capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MpvProfile {
    schema_version: u32,
    platform: String,
    libmpv_version: String,
    gpu: String,
    driver: String,
    render_path: RenderPath,
    measured_at: String,
    entries: Vec<MpvProfileEntry>,
}

/// One tested container/video/audio tuple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MpvProfileEntry {
    pub container: String,
    pub video_codec: String,
    pub audio_codec: String,
    pub max_bit_depth: u8,
    pub hdr: Vec<String>,
    pub result: MpvEntryResult,
    pub evidence: String,
}

/// Outcome of a real-render-path test. Only `Pass` is eligible for direct play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MpvEntryResult {
    Pass,
    Fail,
    Untested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RenderPath {
    GlFbo,
}

/// Why a measured profile could not be loaded or trusted.
#[derive(Debug, thiserror::Error)]
pub enum MpvProfileError {
    #[error("MPV profile is malformed or invalid: {0}")]
    Parse(String),
    #[error("MPV profile exceeds the 1 MiB size limit")]
    TooLarge,
    #[error("MPV profile could not be read")]
    Io,
    #[error("unsupported MPV profile schema version {0}")]
    SchemaVersion(u32),
    #[error("MPV profile field `{0}` must not be empty")]
    EmptyField(&'static str),
    #[error("MPV profile entry {index} has an invalid `{field}` value")]
    InvalidEntry { index: usize, field: &'static str },
    #[error("MPV profile entry {index} duplicates an earlier container/codec tuple")]
    DuplicateEntry { index: usize },
}

impl MpvProfile {
    /// An empty, safe profile used when the capability file is unavailable.
    pub fn transcode_only() -> Self {
        Self {
            schema_version: 1,
            platform: String::new(),
            libmpv_version: String::new(),
            gpu: String::new(),
            driver: String::new(),
            render_path: RenderPath::GlFbo,
            measured_at: String::new(),
            entries: Vec::new(),
        }
    }

    /// Parse and validate profile JSON bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MpvProfileError> {
        if bytes.len() > MAX_PROFILE_BYTES {
            return Err(MpvProfileError::TooLarge);
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|error| MpvProfileError::Parse(error.to_string()))?;
        let profile: Self = serde_json::from_str(text)
            .map_err(|error| MpvProfileError::Parse(error.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> Result<(), MpvProfileError> {
        if self.schema_version != 1 {
            return Err(MpvProfileError::SchemaVersion(self.schema_version));
        }
        for (name, value) in [
            ("platform", self.platform.as_str()),
            ("libmpvVersion", self.libmpv_version.as_str()),
            ("gpu", self.gpu.as_str()),
            ("driver", self.driver.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(MpvProfileError::EmptyField(name));
            }
        }
        if !is_rfc3339(&self.measured_at) {
            return Err(MpvProfileError::Parse(
                "measuredAt must be an RFC 3339 timestamp".into(),
            ));
        }

        let mut seen = HashSet::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            for (field, value) in [
                ("container", entry.container.as_str()),
                ("videoCodec", entry.video_codec.as_str()),
                ("audioCodec", entry.audio_codec.as_str()),
                ("evidence", entry.evidence.as_str()),
            ] {
                if value.trim().is_empty() {
                    return Err(MpvProfileError::InvalidEntry { index, field });
                }
            }
            if entry.max_bit_depth == 0 {
                return Err(MpvProfileError::InvalidEntry {
                    index,
                    field: "maxBitDepth",
                });
            }
            if entry.evidence.len() > 240
                || entry.evidence.contains(['/', '\\', '\n', '\r'])
                || entry.evidence.contains("://")
            {
                return Err(MpvProfileError::InvalidEntry {
                    index,
                    field: "evidence",
                });
            }
            for (field, value) in [
                ("container", entry.container.as_str()),
                ("videoCodec", entry.video_codec.as_str()),
                ("audioCodec", entry.audio_codec.as_str()),
            ] {
                if value.contains([',', '|']) || value.chars().any(char::is_control) {
                    return Err(MpvProfileError::InvalidEntry { index, field });
                }
            }
            if entry.hdr.iter().any(|format| {
                format.trim().is_empty()
                    || format.contains(['|', ','])
                    || format.chars().any(char::is_control)
            }) {
                return Err(MpvProfileError::InvalidEntry {
                    index,
                    field: "hdr",
                });
            }
            if !seen.insert((
                entry.container.as_str(),
                entry.video_codec.as_str(),
                entry.audio_codec.as_str(),
            )) {
                return Err(MpvProfileError::DuplicateEntry { index });
            }
        }
        Ok(())
    }

    /// Build a Jellyfin profile using measured `pass` entries only.
    ///
    /// Codec conditions use the most conservative bit-depth and HDR intersection
    /// for each video codec, so a more capable tuple cannot widen a less capable
    /// tuple's direct-play claim.
    pub fn to_device_profile(&self) -> DeviceProfile {
        let mut direct_play_profiles = Vec::new();
        let mut codec_limits: BTreeMap<&str, (u8, BTreeSet<&str>)> = BTreeMap::new();

        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.result == MpvEntryResult::Pass)
        {
            direct_play_profiles.push(DirectPlayProfile {
                container: Some(entry.container.clone()),
                audio_codec: Some(entry.audio_codec.clone()),
                video_codec: Some(entry.video_codec.clone()),
                profile_type: Some("Video".into()),
                ..DirectPlayProfile::default()
            });

            let mut ranges: BTreeSet<&str> = entry.hdr.iter().map(String::as_str).collect();
            ranges.insert("SDR");
            codec_limits
                .entry(&entry.video_codec)
                .and_modify(|(max_depth, supported_ranges)| {
                    *max_depth = (*max_depth).min(entry.max_bit_depth);
                    *supported_ranges = supported_ranges.intersection(&ranges).copied().collect();
                })
                .or_insert((entry.max_bit_depth, ranges));
        }

        let codec_profiles = codec_limits
            .into_iter()
            .map(|(codec, (max_bit_depth, supported_ranges))| CodecProfile {
                profile_type: Some("Video".into()),
                conditions: vec![
                    ProfileCondition {
                        condition: Some("LessThanEqual".into()),
                        property: Some("VideoBitDepth".into()),
                        value: Some(max_bit_depth.to_string()),
                        is_required: Some(true),
                    },
                    ProfileCondition {
                        condition: Some("EqualsAny".into()),
                        property: Some("VideoRangeType".into()),
                        value: Some(supported_ranges.into_iter().collect::<Vec<_>>().join("|")),
                        is_required: Some(true),
                    },
                ],
                codec: Some(codec.to_owned()),
                ..CodecProfile::default()
            })
            .collect();

        DeviceProfile {
            direct_play_profiles: Some(direct_play_profiles),
            codec_profiles: Some(codec_profiles),
            transcoding_profiles: Some(vec![TranscodingProfile {
                container: Some("ts".into()),
                profile_type: Some("Video".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                protocol: Some("hls".into()),
                context: Some("Streaming".into()),
                ..TranscodingProfile::default()
            }]),
            ..DeviceProfile::default()
        }
    }
}

/// Load a profile from disk. A missing file is a warned, transcode-only result.
pub fn load_mpv_profile(path: impl AsRef<Path>) -> Result<MpvProfile, MpvProfileError> {
    let path = path.as_ref();
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(profile_path = %path.display(), "MPV capability profile is missing; using transcode-only playback");
            return Ok(MpvProfile::transcode_only());
        }
        Err(_) => return Err(MpvProfileError::Io),
    };

    let mut bytes = Vec::new();
    file.take((MAX_PROFILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| MpvProfileError::Io)?;
    MpvProfile::from_bytes(&bytes)
}

/// Load the generated device profile, falling back safely on any invalid file.
pub fn load_device_profile(path: impl AsRef<Path>) -> DeviceProfile {
    let path = path.as_ref();
    match load_mpv_profile(path) {
        Ok(profile) if !profile.entries.is_empty() => profile.to_device_profile(),
        Ok(_) => baseline_device_profile(),
        Err(error) => {
            tracing::warn!(profile_path = %path.display(), error = %error, "MPV capability profile is invalid; using the embedded software baseline");
            baseline_device_profile()
        }
    }
}

/// Build a profile from the committed baseline when runtime probing is unavailable.
pub fn baseline_device_profile() -> DeviceProfile {
    let baseline = match MpvSoftwareBaseline::embedded() {
        Ok(baseline) => baseline,
        Err(error) => {
            tracing::warn!(error = %error, "embedded MPV baseline is invalid; using conservative fallback");
            MpvSoftwareBaseline::conservative()
        }
    };
    baseline
        .merge(&RuntimeCapabilities::default())
        .to_device_profile(&baseline)
}

/// Build the effective Jellyfin profile from the embedded software baseline and
/// the decoder/demuxer identifiers returned by the active libmpv build.
/// Unavailable properties preserve the baseline and remain a caller-visible warning.
pub fn device_profile_from_runtime_inventory(
    decoder_names: Option<&[String]>,
    demuxer_names: Option<&[String]>,
) -> DeviceProfile {
    let baseline = MpvSoftwareBaseline::embedded().unwrap_or_else(|error| {
        tracing::warn!(error = %error, "embedded MPV baseline is invalid; using conservative fallback");
        MpvSoftwareBaseline::conservative()
    });
    let runtime = baseline.runtime_capabilities_from_inventory(decoder_names, demuxer_names);
    baseline.merge(&runtime).to_device_profile(&baseline)
}

fn is_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return false;
    }
    let Some(year) = decimal(&bytes[0..4]) else {
        return false;
    };
    let Some(month) = decimal(&bytes[5..7]) else {
        return false;
    };
    let Some(day) = decimal(&bytes[8..10]) else {
        return false;
    };
    let Some(hour) = decimal(&bytes[11..13]) else {
        return false;
    };
    let Some(minute) = decimal(&bytes[14..16]) else {
        return false;
    };
    let Some(second) = decimal(&bytes[17..19]) else {
        return false;
    };
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return false,
    };
    if day == 0 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        return false;
    }

    let mut zone_start = 19;
    if bytes.get(zone_start) == Some(&b'.') {
        zone_start += 1;
        let fraction_start = zone_start;
        while bytes.get(zone_start).is_some_and(u8::is_ascii_digit) {
            zone_start += 1;
        }
        if zone_start == fraction_start {
            return false;
        }
    }
    match bytes.get(zone_start..) {
        Some([b'Z' | b'z']) => true,
        Some(
            [
                sign @ (b'+' | b'-'),
                hour_tens,
                hour_ones,
                b':',
                minute_tens,
                minute_ones,
            ],
        ) => {
            let offset_hour = decimal(&[*hour_tens, *hour_ones]);
            let offset_minute = decimal(&[*minute_tens, *minute_ones]);
            let _ = sign;
            offset_hour.is_some_and(|value| value <= 23)
                && offset_minute.is_some_and(|value| value <= 59)
        }
        _ => false,
    }
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, digit| {
        digit
            .is_ascii_digit()
            .then(|| value * 10 + u32::from(digit - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{
        "schemaVersion": 1,
        "platform": "windows-x64",
        "libmpvVersion": "mpv 0.40.0",
        "gpu": "test-gpu",
        "driver": "test-driver",
        "renderPath": "gl-fbo",
        "measuredAt": "2026-10-14T12:30:00Z",
        "entries": [
            {"container":"mkv","videoCodec":"h264","audioCodec":"aac","maxBitDepth":10,"hdr":["HDR10"],"result":"pass","evidence":"h264-aac.mkv: rendered 30s; hwdec=d3d11va"},
            {"container":"mp4","videoCodec":"hevc","audioCodec":"aac","maxBitDepth":10,"hdr":[],"result":"fail","evidence":"hevc-aac.mp4: decoder error"},
            {"container":"webm","videoCodec":"vp9","audioCodec":"opus","maxBitDepth":8,"hdr":[],"result":"untested","evidence":"vp9-opus.webm: not run"}
        ]
    }"#;

    #[test]
    fn valid_profile_round_trips() {
        let profile = MpvProfile::from_bytes(VALID.as_bytes()).expect("valid profile");
        let encoded = serde_json::to_vec(&profile).expect("serialize profile");
        assert_eq!(
            MpvProfile::from_bytes(&encoded).expect("round trip"),
            profile
        );
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let input = VALID.replace("\"schemaVersion\": 1", "\"schemaVersion\": 2");
        assert!(matches!(
            MpvProfile::from_bytes(input.as_bytes()),
            Err(MpvProfileError::SchemaVersion(2))
        ));
    }

    #[test]
    fn rejects_duplicate_container_codec_tuples() {
        let input = VALID.replace(
            "\"entries\": [",
            "\"entries\": [{\"container\":\"mkv\",\"videoCodec\":\"h264\",\"audioCodec\":\"aac\",\"maxBitDepth\":8,\"hdr\":[],\"result\":\"fail\",\"evidence\":\"duplicate.mkv: fail\"},",
        );
        assert!(matches!(
            MpvProfile::from_bytes(input.as_bytes()),
            Err(MpvProfileError::DuplicateEntry { index: 1 })
        ));
    }

    #[test]
    fn rejects_unknown_result_values() {
        let input = VALID.replace("\"result\":\"fail\"", "\"result\":\"maybe\"");
        assert!(matches!(
            MpvProfile::from_bytes(input.as_bytes()),
            Err(MpvProfileError::Parse(_))
        ));
    }

    #[test]
    fn rejects_empty_malformed_non_utf8_and_oversized_inputs() {
        assert!(matches!(
            MpvProfile::from_bytes(b""),
            Err(MpvProfileError::Parse(_))
        ));
        assert!(matches!(
            MpvProfile::from_bytes(b"{not json}"),
            Err(MpvProfileError::Parse(_))
        ));
        assert!(matches!(
            MpvProfile::from_bytes(&[0xff, 0xfe]),
            Err(MpvProfileError::Parse(_))
        ));
        assert!(matches!(
            MpvProfile::from_bytes(&vec![b' '; MAX_PROFILE_BYTES + 1]),
            Err(MpvProfileError::TooLarge)
        ));
    }

    #[test]
    fn invalid_timestamp_and_path_evidence_are_rejected() {
        let invalid_date = VALID.replace("2026-10-14T12:30:00Z", "2026-02-30T12:30:00Z");
        assert!(matches!(
            MpvProfile::from_bytes(invalid_date.as_bytes()),
            Err(MpvProfileError::Parse(_))
        ));
        let path_evidence = VALID.replace(
            "h264-aac.mkv: rendered",
            "C:\\\\media\\\\h264-aac.mkv: rendered",
        );
        assert!(matches!(
            MpvProfile::from_bytes(path_evidence.as_bytes()),
            Err(MpvProfileError::InvalidEntry {
                field: "evidence",
                ..
            })
        ));
    }

    #[test]
    fn missing_file_generates_transcode_only_profile() {
        let missing = std::env::temp_dir().join(format!(
            "kattri-mpv-profile-missing-{}.json",
            std::process::id()
        ));
        let profile = load_mpv_profile(missing).expect("missing is transcode-only");
        let generated = profile.to_device_profile();
        assert!(
            generated
                .direct_play_profiles
                .as_ref()
                .is_some_and(Vec::is_empty)
        );
        assert_eq!(
            generated.transcoding_profiles.as_ref().map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn generated_profile_contains_only_pass_entries_and_bit_depth_constraints() {
        let profile = MpvProfile::from_bytes(VALID.as_bytes()).expect("valid profile");
        let generated = profile.to_device_profile();
        let direct = generated
            .direct_play_profiles
            .as_ref()
            .expect("direct profiles");
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].container.as_deref(), Some("mkv"));
        let codec = &generated.codec_profiles.as_ref().expect("codec profiles")[0];
        assert_eq!(codec.codec.as_deref(), Some("h264"));
        assert!(codec.conditions.iter().any(|condition| {
            condition.property.as_deref() == Some("VideoBitDepth")
                && condition.condition.as_deref() == Some("LessThanEqual")
                && condition.value.as_deref() == Some("10")
                && condition.is_required == Some(true)
        }));
        let generated_json = serde_json::to_string(&generated).expect("serialize device profile");
        for private_field in ["libmpvVersion", "test-gpu", "test-driver", "evidence"] {
            assert!(!generated_json.contains(private_field));
        }
    }

    #[test]
    fn transcode_only_profile_has_no_direct_play_claims() {
        let generated = MpvProfile::transcode_only().to_device_profile();
        assert!(
            generated
                .direct_play_profiles
                .as_ref()
                .is_some_and(Vec::is_empty)
        );
        let fallback = &generated.transcoding_profiles.as_ref().expect("fallback")[0];
        assert_eq!(fallback.container.as_deref(), Some("ts"));
        assert_eq!(fallback.protocol.as_deref(), Some("hls"));
        assert_eq!(fallback.video_codec.as_deref(), Some("h264"));
        assert_eq!(fallback.audio_codec.as_deref(), Some("aac"));
        assert_eq!(fallback.context.as_deref(), Some("Streaming"));
    }

    #[test]
    fn committed_software_baseline_is_deterministic() {
        let first = MpvSoftwareBaseline::embedded().expect("baseline parses");
        let second = MpvSoftwareBaseline::embedded().expect("baseline parses");
        assert_eq!(first, second);
        let serialized_first = serde_json::to_vec(&first).expect("serialize baseline");
        let serialized_second = serde_json::to_vec(&second).expect("serialize baseline");
        assert_eq!(serialized_first, serialized_second);
        assert!(
            first
                .provenance
                .iter()
                .any(|source| source.source == "FFmpeg decoder documentation")
        );
    }

    #[test]
    fn runtime_inventory_only_narrows_software_claims_and_unknown_outputs_stay_off() {
        let baseline = MpvSoftwareBaseline::embedded().expect("baseline parses");
        let runtime = RuntimeCapabilities {
            video_containers: Some(["mp4".to_owned(), "ts".to_owned()].into_iter().collect()),
            audio_containers: Some(["flac".to_owned()].into_iter().collect()),
            video_codecs: Some(
                ["h264".to_owned(), "unknown-codec".to_owned()]
                    .into_iter()
                    .collect(),
            ),
            audio_codecs: Some(["aac".to_owned()].into_iter().collect()),
            max_video_bit_depth: Some(8),
            hdr_output: None,
            audio_passthrough: None,
        };
        let effective = baseline.merge(&runtime);
        assert_eq!(
            effective.video_containers,
            ["mp4".to_owned(), "ts".to_owned()].into_iter().collect()
        );
        assert_eq!(
            effective.audio_containers,
            ["flac".to_owned()].into_iter().collect()
        );
        assert_eq!(
            effective.video_codecs,
            ["h264".to_owned()].into_iter().collect()
        );
        assert_eq!(
            effective.audio_codecs,
            ["aac".to_owned()].into_iter().collect()
        );
        assert_eq!(effective.max_video_bit_depth, 8);
        assert!(!effective.hdr_output);
        assert!(!effective.audio_passthrough);

        let narrowed = effective.to_device_profile(&baseline);
        let json = serde_json::to_value(narrowed).expect("serialize profile");
        assert_eq!(json["DirectPlayProfiles"][0]["VideoCodec"], "h264");
        assert_eq!(json["DirectPlayProfiles"][0]["AudioCodec"], "aac");
        assert_eq!(json["DirectPlayProfiles"][1]["Type"], "Audio");
        assert_eq!(json["DirectPlayProfiles"][1]["Container"], "flac");
        assert_eq!(json["TranscodingProfiles"][0]["AudioCodec"], "aac");
        assert_eq!(json["CodecProfiles"][0]["Conditions"][1]["Value"], "SDR");
    }

    #[test]
    fn missing_runtime_audio_decoders_do_not_create_unrestricted_video_profile() {
        let baseline = MpvSoftwareBaseline::embedded().expect("baseline parses");
        let decoders = vec!["h264".to_owned()];
        let demuxers = vec!["matroska".to_owned()];
        let runtime =
            baseline.runtime_capabilities_from_inventory(Some(&decoders), Some(&demuxers));
        let effective = baseline.merge(&runtime);
        assert!(effective.video_codecs.contains("h264"));
        assert!(effective.audio_codecs.is_empty());

        let profile = effective.to_device_profile(&baseline);
        let direct = profile
            .direct_play_profiles
            .as_ref()
            .expect("direct profile list");
        assert!(
            direct.is_empty(),
            "unknown audio support must not be a wildcard"
        );
        assert!(
            profile
                .transcoding_profiles
                .as_ref()
                .expect("transcoding profile list")
                .is_empty()
        );
    }

    #[test]
    fn runtime_transcode_requires_a_supported_container_video_and_audio_codec() {
        for (decoders, demuxers) in [
            (["hevc", "aac"], ["mpegts"]),
            (["h264", "aac"], ["matroska"]),
            (["h264", "flac"], ["mpegts"]),
        ] {
            let decoders = decoders.map(str::to_owned);
            let demuxers = demuxers.map(str::to_owned);
            let profile = device_profile_from_runtime_inventory(Some(&decoders), Some(&demuxers));
            assert!(
                profile
                    .transcoding_profiles
                    .as_ref()
                    .is_some_and(Vec::is_empty),
                "known unsupported fallback must not be advertised"
            );
        }
    }

    #[test]
    fn comma_grouped_ffmpeg_demuxer_names_keep_their_containers() {
        let baseline = MpvSoftwareBaseline::embedded().expect("baseline parses");
        let demuxers: Vec<String> = [
            "matroska,webm",
            "mov,mp4,m4a,3gp,3g2,mj2",
            "mpegts",
            "avi",
            "flac",
            "mp3",
            "ogg",
            "wav",
        ]
        .map(String::from)
        .to_vec();
        let decoders: Vec<String> = ["h264", "hevc", "aac"].map(String::from).to_vec();
        let runtime =
            baseline.runtime_capabilities_from_inventory(Some(&decoders), Some(&demuxers));
        let effective = baseline.merge(&runtime);

        for container in ["mkv", "webm", "mp4", "mov", "m4v", "ts", "avi"] {
            assert!(
                effective.video_containers.contains(container),
                "{container} must stay a direct-play container"
            );
        }
    }

    #[test]
    fn recorded_libmpv_inventory_fixture_refines_jellyfin_aliases() {
        use serde::Deserialize;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Fixture {
            decoder_identifiers: Vec<String>,
            demuxer_identifiers: Vec<String>,
        }

        let fixture: Fixture = serde_json::from_str(include_str!(
            "../profiles/fixtures/libmpv-shinchiro-20261014.json"
        ))
        .expect("recorded runtime property fixture parses");
        let baseline = MpvSoftwareBaseline::embedded().expect("baseline parses");
        let runtime = baseline.runtime_capabilities_from_inventory(
            Some(&fixture.decoder_identifiers),
            Some(&fixture.demuxer_identifiers),
        );
        let effective = baseline.merge(&runtime);

        assert!(effective.video_codecs.contains("h264"));
        assert!(effective.video_codecs.contains("hevc"));
        assert!(effective.audio_codecs.contains("truehd"));
        assert!(effective.audio_codecs.contains("pcm_s16le"));
        assert!(effective.audio_codecs.contains("dts-hd"));
        assert!(effective.video_containers.contains("mkv"));
        assert!(effective.video_containers.contains("m2ts"));
        assert!(effective.video_containers.contains("ts"));
        assert!(effective.audio_containers.contains("flac"));
        assert!(!effective.hdr_output);
        assert!(!effective.audio_passthrough);
    }

    #[test]
    fn every_baseline_container_and_codec_family_survives_the_recorded_inventory() {
        use serde::Deserialize;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Fixture {
            decoder_identifiers: Vec<String>,
            demuxer_identifiers: Vec<String>,
        }

        let fixture: Fixture = serde_json::from_str(include_str!(
            "../profiles/fixtures/libmpv-shinchiro-20261014.json"
        ))
        .expect("recorded runtime property fixture parses");
        let baseline = MpvSoftwareBaseline::embedded().expect("baseline parses");
        let runtime = baseline.runtime_capabilities_from_inventory(
            Some(&fixture.decoder_identifiers),
            Some(&fixture.demuxer_identifiers),
        );
        let effective = baseline.merge(&runtime);

        // A real libmpv build supports everything the baseline claims, so intersecting with
        // its inventory must not drop any container or codec.
        let missing = |kind: &str, wanted: &[String], got: &BTreeSet<String>| {
            let lost: Vec<&String> = wanted.iter().filter(|name| !got.contains(*name)).collect();
            assert!(lost.is_empty(), "{kind} lost from the baseline: {lost:?}");
        };
        let base = &baseline.software_baseline;
        missing(
            "video containers",
            &base.video_containers,
            &effective.video_containers,
        );
        missing(
            "audio containers",
            &base.audio_containers,
            &effective.audio_containers,
        );
        missing("video codecs", &base.video_codecs, &effective.video_codecs);
        missing("audio codecs", &base.audio_codecs, &effective.audio_codecs);
    }

    #[test]
    fn explicit_evidence_can_enable_output_flags_but_not_expand_decoder_inventory() {
        let baseline = MpvSoftwareBaseline::embedded().expect("baseline parses");
        let runtime = RuntimeCapabilities {
            video_codecs: Some(["h264".to_owned()].into_iter().collect()),
            hdr_output: Some(true),
            audio_passthrough: Some(true),
            ..RuntimeCapabilities::default()
        };
        let effective = baseline.merge(&runtime);
        assert!(effective.hdr_output);
        assert!(effective.audio_passthrough);
        assert_eq!(
            effective.video_codecs,
            ["h264".to_owned()].into_iter().collect()
        );
    }

    #[test]
    fn jellyfin_projection_uses_current_field_names() {
        let profile = baseline_device_profile();
        let json = serde_json::to_value(profile).expect("serialize device profile");
        assert!(json.get("Name").is_some());
        assert!(json.get("DirectPlayProfiles").is_some());
        assert!(json.get("ContainerProfiles").is_some());
        assert!(json.get("CodecProfiles").is_some());
        assert!(json.get("TranscodingProfiles").is_some());
        assert!(json.get("SubtitleProfiles").is_some());
        assert!(json["DirectPlayProfiles"][0].get("Type").is_some());
        assert!(json["ContainerProfiles"][0].get("Type").is_some());
        assert!(
            json["TranscodingProfiles"][0]
                .get("EnableMpegtsM2TsMode")
                .is_none()
        );
        assert!(json.get("ResponseProfiles").is_none());
        assert!(json["SubtitleProfiles"][0].get("Method").is_some());
    }

    #[test]
    fn invalid_measured_profile_falls_back_to_embedded_baseline() {
        let path = std::env::temp_dir().join(format!(
            "kattri-mpv-profile-invalid-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, b"not json").expect("write invalid test profile");
        let profile = load_device_profile(&path);
        let _ = std::fs::remove_file(path);
        assert!(
            profile
                .direct_play_profiles
                .as_ref()
                .is_some_and(|profiles| !profiles.is_empty())
        );
    }

    #[test]
    fn missing_measured_profile_falls_back_to_embedded_baseline() {
        let missing = std::env::temp_dir().join(format!(
            "kattri-mpv-profile-absent-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&missing);
        let profile = load_device_profile(missing);
        assert!(
            profile
                .direct_play_profiles
                .as_ref()
                .is_some_and(|profiles| !profiles.is_empty())
        );
    }
}
