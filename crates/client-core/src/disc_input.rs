//! Local disc-image input detection and the gated open policy (M6-16).
//!
//! The module classifies a local path as a Blu-ray folder, a DVD-Video folder, or an ISO image,
//! and turns that into an open plan only when the product gate is open and the user asked for it
//! explicitly. It is pure: the caller supplies what it found on disk through [`DiscProbe`], so
//! nothing here reads the filesystem, opens a disc, or loads libmpv.
//!
//! The gate:
//! - M6-16 does not start until the owner completes a legal and product review and records it in
//!   `docs/disc-playback.md`. That file does not exist, so [`DISC_PLAYBACK_GATE`] is
//!   [`DiscPlaybackGate::Closed`] and every request is refused as gated. The constant is the only
//!   place the built state lives; a future recorded decision flips it together with that record.
//! - Opening must be an explicit user action. [`DiscOpenTrigger::Automatic`] is refused even when
//!   the gate is open, so no automatic open, autoplay, or URI-triggered open can use this policy.
//! - The rights notice is part of every plan, so a future host cannot open a disc without the
//!   text available to show. This module never obtains, stores, or uses decryption keys, and it
//!   has no code that tries to work around copy protection.
//!
//! What is known, and from where:
//! - The captured pinned libmpv runtime inventory
//!   (`crates/client-core/tests/fixtures/mpv-runtime-options.json`, mpv v0.41.0-1286-gfa0534a72)
//!   contains `bluray-device` and `disc-menu`, so the Blu-ray device option exists in this build.
//!   It contains no `dvd-device` option, so DVD folder support in this build is unconfirmed and
//!   [`DiscProtocol::Dvd`] stays model-only.
//! - The mpv manual documents that a Blu-ray or DVD-Video `.iso` passed directly is detected and
//!   opened, and that folder playback needs `bd://`/`dvd://` with the matching device option.
//!
//! What is unknown or unvalidated:
//! - No disc, ISO, or drive was opened. Whether a specific disc library can open a given image,
//!   and whether the pinned build includes DVD support, needs a runtime run that has not happened.
//! - Title selection (main title vs. a picker), menu handling, and unsupported-encryption messages
//!   are not modelled here; the spec assigns them to the host once the gate opens.

use std::fmt;
use std::path::Path;

/// The owner's legal and product gate. `Open` requires the recorded decision in
/// `docs/disc-playback.md`; while that file is absent the built state is `Closed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscPlaybackGate {
    /// The owner recorded the legal and product decision, so playback may be built and offered.
    Open,
    /// No recorded decision: the feature is gated off and every open request is refused.
    Closed,
}

/// The gate as built. The legal record does not exist, so disc playback is gated off.
pub const DISC_PLAYBACK_GATE: DiscPlaybackGate = DiscPlaybackGate::Closed;

/// Who asked to open the disc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscOpenTrigger {
    /// The user chose to open this exact path or image.
    ExplicitUserAction,
    /// Anything automatic: autoplay, a URI intent, or a remembered last item.
    Automatic,
}

/// The rights notice that must be available before a disc is opened. Plain ASCII, no markup.
pub const DISC_RIGHTS_NOTICE: &str = "Only play Blu-ray, DVD, and disc image content you own or are otherwise entitled to play. Kattri does not decrypt copy-protected content and cannot play a disc or image that needs decryption keys.";

/// What the disc path is, once classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscInput {
    /// An `.iso` image. The disc library decides Blu-ray or DVD when it opens the image.
    IsoImage,
    /// A folder whose `BDMV/index.bdmv` is present (Blu-ray structure).
    BdmvFolder,
    /// A folder whose `VIDEO_TS/VIDEO_TS.IFO` is present (DVD-Video structure).
    VideoTsFolder,
}

/// What the caller found on disk. Filling this in is the only I/O the classification needs, so
/// the policy stays testable without fixture media or a drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscProbe {
    /// The path exists as a file, a directory, or does not exist.
    pub kind: DiscPathKind,
    /// `<path>/BDMV/index.bdmv` exists, or `<path>/index.bdmv` when the path itself is `BDMV`.
    pub bdmv_index: bool,
    /// `<path>/VIDEO_TS/VIDEO_TS.IFO` exists, or `<path>/VIDEO_TS.IFO` when the path itself is
    /// `VIDEO_TS`.
    pub video_ts_ifo: bool,
}

impl DiscProbe {
    /// A probe for a path that does not exist.
    pub const MISSING: Self = Self {
        kind: DiscPathKind::Missing,
        bdmv_index: false,
        video_ts_ifo: false,
    };

    /// A probe for an existing directory with no disc structure inside it.
    pub const EMPTY_DIRECTORY: Self = Self {
        kind: DiscPathKind::Directory,
        bdmv_index: false,
        video_ts_ifo: false,
    };

    /// Build a probe from the filesystem. Only existence and file-type checks are made; no file
    /// content is read and no directory is opened beyond the two structure markers.
    pub fn from_path(path: &Path) -> Self {
        let kind = match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => DiscPathKind::Directory,
            Ok(_) => DiscPathKind::File,
            Err(_) => DiscPathKind::Missing,
        };
        let is_named = |name: &str| {
            path.file_name()
                .and_then(|file_name| file_name.to_str())
                .is_some_and(|file_name| file_name.eq_ignore_ascii_case(name))
        };
        let bdmv_index = path.join("BDMV").join("index.bdmv").is_file()
            || (is_named("BDMV") && path.join("index.bdmv").is_file());
        let video_ts_ifo = path.join("VIDEO_TS").join("VIDEO_TS.IFO").is_file()
            || (is_named("VIDEO_TS") && path.join("VIDEO_TS.IFO").is_file());
        Self {
            kind,
            bdmv_index,
            video_ts_ifo,
        }
    }
}

/// The kind of entry a path names on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscPathKind {
    File,
    Directory,
    Missing,
}

/// Why a path is not a usable disc input. No variant carries the path or an image name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscInputError {
    /// The path is empty or has no final component.
    EmptyPath,
    /// The path does not exist.
    NotFound,
    /// A disc folder path names a file.
    NotADiscFolder,
    /// An ISO path names a directory.
    NotADiscFile,
    /// The path has neither an ISO suffix nor a Blu-ray or DVD-Video structure.
    NotDiscStructure,
    /// Both disc structures are present, so the kind is not guessed.
    AmbiguousStructure,
    /// The selected folder has the right name but the structure marker is missing.
    IncompleteStructure,
    /// The path has no UTF-8 file name, so it cannot be checked as a disc path.
    UnsupportedPathName,
}

impl fmt::Display for DiscInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => f.write_str("the disc path is empty"),
            Self::NotFound => f.write_str("the disc path does not exist"),
            Self::NotADiscFolder => f.write_str("a disc folder path names a file"),
            Self::NotADiscFile => f.write_str("an ISO path names a folder"),
            Self::NotDiscStructure => {
                f.write_str("the path is not an ISO, a BDMV folder, or a VIDEO_TS folder")
            }
            Self::AmbiguousStructure => {
                f.write_str("the folder has both BDMV and VIDEO_TS structures")
            }
            Self::IncompleteStructure => {
                f.write_str("the disc folder is missing its structure marker")
            }
            Self::UnsupportedPathName => f.write_str("the disc path has no valid file name"),
        }
    }
}

impl std::error::Error for DiscInputError {}

/// The mpv protocol a folder needs, and the device option that carries its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscProtocol {
    /// `bd://` with the `bluray-device` option (present in the captured pinned runtime).
    BluRay,
    /// `dvd://` with the `dvd-device` option. That option is absent from the captured pinned
    /// runtime, so DVD folder support in this build is unconfirmed.
    Dvd,
}

impl DiscProtocol {
    /// The mpv URL prefix. Names the player's protocol, not a command this client runs.
    pub const fn mpv_prefix(self) -> &'static str {
        match self {
            Self::BluRay => "bd://",
            Self::Dvd => "dvd://",
        }
    }

    /// The mpv option that carries the disc folder path, when the captured runtime has it.
    pub const fn device_option(self) -> Option<&'static str> {
        match self {
            Self::BluRay => Some("bluray-device"),
            Self::Dvd => None,
        }
    }
}

/// The checked plan for one disc open. It carries no path and no secret, so it is safe to log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscOpenPlan {
    /// What the path was classified as.
    pub input: DiscInput,
    /// The protocol for a folder. `None` for an ISO, which mpv detects and opens directly.
    pub protocol: Option<DiscProtocol>,
    /// The rights notice the host must show before opening.
    pub rights_notice: &'static str,
}

impl DiscOpenPlan {
    /// The value a future host passes to `loadfile` for a folder (`bd://`/`dvd://`); an ISO is
    /// passed as its own path instead. This is the player's spelling, not a call that happens here.
    pub const fn load_argument(self) -> Option<&'static str> {
        match self.protocol {
            Some(protocol) => Some(protocol.mpv_prefix()),
            None => None,
        }
    }
}

/// Why a disc open was refused. No variant carries the path or an image name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscOpenRefusal {
    /// The legal and product gate is still closed.
    FeatureGated,
    /// The open was not an explicit user action.
    AutomaticOpenRefused,
    /// The path is not a usable disc input.
    InputRejected(DiscInputError),
}

impl fmt::Display for DiscOpenRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FeatureGated => f.write_str(
                "disc playback is gated off until the legal and product review is recorded",
            ),
            Self::AutomaticOpenRefused => {
                f.write_str("disc playback needs an explicit user action")
            }
            Self::InputRejected(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for DiscOpenRefusal {}

/// Classify a path using the probe the caller built from the filesystem.
///
/// - An `.iso` suffix (case-insensitive) is an ISO image; a directory with that suffix is refused.
/// - A path whose final component is `BDMV` or `VIDEO_TS` must contain that structure's marker.
/// - Any other path is a disc root when its `BDMV/index.bdmv` or `VIDEO_TS/VIDEO_TS.IFO` exists.
///   Both markers present is refused rather than guessed.
pub fn classify(path: &Path, probe: DiscProbe) -> Result<DiscInput, DiscInputError> {
    if path.as_os_str().is_empty() {
        return Err(DiscInputError::EmptyPath);
    }
    if probe.kind == DiscPathKind::Missing {
        return Err(DiscInputError::NotFound);
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(DiscInputError::UnsupportedPathName);
    };
    let extension_is_iso = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("iso"));
    if extension_is_iso {
        return match probe.kind {
            DiscPathKind::File => Ok(DiscInput::IsoImage),
            _ => Err(DiscInputError::NotADiscFile),
        };
    }
    if name.eq_ignore_ascii_case("BDMV") {
        return match probe.kind {
            DiscPathKind::Directory if probe.bdmv_index => Ok(DiscInput::BdmvFolder),
            DiscPathKind::Directory => Err(DiscInputError::IncompleteStructure),
            _ => Err(DiscInputError::NotADiscFolder),
        };
    }
    if name.eq_ignore_ascii_case("VIDEO_TS") {
        return match probe.kind {
            DiscPathKind::Directory if probe.video_ts_ifo => Ok(DiscInput::VideoTsFolder),
            DiscPathKind::Directory => Err(DiscInputError::IncompleteStructure),
            _ => Err(DiscInputError::NotADiscFolder),
        };
    }
    match probe.kind {
        DiscPathKind::Directory => match (probe.bdmv_index, probe.video_ts_ifo) {
            (true, true) => Err(DiscInputError::AmbiguousStructure),
            (true, false) => Ok(DiscInput::BdmvFolder),
            (false, true) => Ok(DiscInput::VideoTsFolder),
            (false, false) => Err(DiscInputError::NotDiscStructure),
        },
        DiscPathKind::File => Err(DiscInputError::NotDiscStructure),
        DiscPathKind::Missing => Err(DiscInputError::NotFound),
    }
}

/// The open plan for a classified input, only when the gate is open and the user asked for it.
///
/// The checks run in a fixed order: an automatic request is refused first (so it cannot reveal
/// whether the gate is open), then the closed gate, then the input. A successful plan still
/// carries the rights notice and no key material.
pub fn plan_open(
    input: Result<DiscInput, DiscInputError>,
    trigger: DiscOpenTrigger,
    gate: DiscPlaybackGate,
) -> Result<DiscOpenPlan, DiscOpenRefusal> {
    if trigger == DiscOpenTrigger::Automatic {
        return Err(DiscOpenRefusal::AutomaticOpenRefused);
    }
    if gate == DiscPlaybackGate::Closed {
        return Err(DiscOpenRefusal::FeatureGated);
    }
    let input = input.map_err(DiscOpenRefusal::InputRejected)?;
    let protocol = match input {
        DiscInput::IsoImage => None,
        DiscInput::BdmvFolder => Some(DiscProtocol::BluRay),
        DiscInput::VideoTsFolder => Some(DiscProtocol::Dvd),
    };
    Ok(DiscOpenPlan {
        input,
        protocol,
        rights_notice: DISC_RIGHTS_NOTICE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(kind: DiscPathKind, bdmv_index: bool, video_ts_ifo: bool) -> DiscProbe {
        DiscProbe {
            kind,
            bdmv_index,
            video_ts_ifo,
        }
    }

    fn bluray_root() -> DiscProbe {
        probe(DiscPathKind::Directory, true, false)
    }

    fn dvd_root() -> DiscProbe {
        probe(DiscPathKind::Directory, false, true)
    }

    #[test]
    fn an_iso_image_is_recognised_case_insensitively() {
        for name in ["disc.iso", "DISC.ISO", "my movie.Iso"] {
            assert_eq!(
                classify(Path::new(name), probe(DiscPathKind::File, false, false)),
                Ok(DiscInput::IsoImage),
                "{name}"
            );
        }
    }

    #[test]
    fn an_iso_path_that_names_a_folder_is_refused() {
        assert_eq!(
            classify(
                Path::new("disc.iso"),
                probe(DiscPathKind::Directory, true, true)
            ),
            Err(DiscInputError::NotADiscFile)
        );
    }

    #[test]
    fn a_folder_with_a_bdmv_structure_is_a_blu_ray_folder() {
        assert_eq!(
            classify(Path::new("Some Movie"), bluray_root()),
            Ok(DiscInput::BdmvFolder)
        );
        assert_eq!(
            classify(Path::new("Some Movie"), dvd_root()),
            Ok(DiscInput::VideoTsFolder)
        );
    }

    #[test]
    fn a_selected_bdmv_or_video_ts_directory_is_accepted_with_its_marker() {
        assert_eq!(
            classify(
                Path::new("Some Movie/BDMV"),
                probe(DiscPathKind::Directory, true, false)
            ),
            Ok(DiscInput::BdmvFolder)
        );
        assert_eq!(
            classify(
                Path::new("Some Movie/VIDEO_TS"),
                probe(DiscPathKind::Directory, false, true)
            ),
            Ok(DiscInput::VideoTsFolder)
        );
    }

    #[test]
    fn a_named_disc_directory_without_its_marker_is_incomplete() {
        assert_eq!(
            classify(
                Path::new("BDMV"),
                probe(DiscPathKind::Directory, false, false)
            ),
            Err(DiscInputError::IncompleteStructure)
        );
        assert_eq!(
            classify(
                Path::new("VIDEO_TS"),
                probe(DiscPathKind::Directory, false, false)
            ),
            Err(DiscInputError::IncompleteStructure)
        );
    }

    #[test]
    fn a_folder_with_both_structures_is_refused_rather_than_guessed() {
        assert_eq!(
            classify(
                Path::new("Hybrid Disc"),
                probe(DiscPathKind::Directory, true, true)
            ),
            Err(DiscInputError::AmbiguousStructure)
        );
    }

    #[test]
    fn non_disc_paths_are_refused() {
        for (path, probe) in [
            ("movie.mkv", probe(DiscPathKind::File, false, false)),
            ("folder", DiscProbe::EMPTY_DIRECTORY),
            ("", DiscProbe::MISSING),
            ("missing", DiscProbe::MISSING),
        ] {
            assert!(
                classify(Path::new(path), probe).is_err(),
                "{path:?} was accepted"
            );
        }
        assert_eq!(
            classify(
                Path::new("movie.mkv"),
                probe(DiscPathKind::File, false, false)
            ),
            Err(DiscInputError::NotDiscStructure)
        );
        assert_eq!(
            classify(Path::new("missing.iso"), DiscProbe::MISSING),
            Err(DiscInputError::NotFound)
        );
    }

    #[test]
    fn the_gate_is_closed_as_built_and_refuses_explicit_opens() {
        assert_eq!(DISC_PLAYBACK_GATE, DiscPlaybackGate::Closed);
        assert_eq!(
            plan_open(
                Ok(DiscInput::BdmvFolder),
                DiscOpenTrigger::ExplicitUserAction,
                DISC_PLAYBACK_GATE
            ),
            Err(DiscOpenRefusal::FeatureGated)
        );
    }

    #[test]
    fn an_automatic_open_is_refused_even_with_the_gate_open() {
        for input in [
            DiscInput::IsoImage,
            DiscInput::BdmvFolder,
            DiscInput::VideoTsFolder,
        ] {
            assert_eq!(
                plan_open(
                    Ok(input),
                    DiscOpenTrigger::Automatic,
                    DiscPlaybackGate::Open
                ),
                Err(DiscOpenRefusal::AutomaticOpenRefused),
                "{input:?}"
            );
        }
    }

    #[test]
    fn an_explicit_open_of_a_checked_input_gets_a_plan_and_the_rights_notice() {
        for (input, expected_protocol) in [
            (DiscInput::IsoImage, None),
            (DiscInput::BdmvFolder, Some(DiscProtocol::BluRay)),
            (DiscInput::VideoTsFolder, Some(DiscProtocol::Dvd)),
        ] {
            let plan = plan_open(
                Ok(input),
                DiscOpenTrigger::ExplicitUserAction,
                DiscPlaybackGate::Open,
            )
            .expect("checked input with the gate open");
            assert_eq!(plan.input, input);
            assert_eq!(plan.protocol, expected_protocol);
            assert_eq!(plan.rights_notice, DISC_RIGHTS_NOTICE);
            assert!(!plan.rights_notice.is_empty());
            assert!(
                plan.rights_notice.is_ascii(),
                "the notice must be plain ASCII"
            );
        }
    }

    #[test]
    fn an_iso_is_opened_directly_and_folders_use_the_protocol_prefix() {
        let iso = plan_open(
            Ok(DiscInput::IsoImage),
            DiscOpenTrigger::ExplicitUserAction,
            DiscPlaybackGate::Open,
        )
        .expect("checked input");
        assert_eq!(iso.load_argument(), None);

        let bluray = plan_open(
            Ok(DiscInput::BdmvFolder),
            DiscOpenTrigger::ExplicitUserAction,
            DiscPlaybackGate::Open,
        )
        .expect("checked input");
        assert_eq!(bluray.load_argument(), Some("bd://"));

        let dvd = plan_open(
            Ok(DiscInput::VideoTsFolder),
            DiscOpenTrigger::ExplicitUserAction,
            DiscPlaybackGate::Open,
        )
        .expect("checked input");
        assert_eq!(dvd.load_argument(), Some("dvd://"));
    }

    #[test]
    fn the_rights_notice_states_the_entitlement_and_no_decryption_rule() {
        let notice = DISC_RIGHTS_NOTICE.to_ascii_lowercase();
        assert!(notice.contains("entitled"));
        assert!(notice.contains("does not decrypt"));
        assert!(notice.contains("copy-protected"));
        assert!(!notice.contains("key file"));
    }

    #[test]
    fn the_captured_pinned_runtime_has_the_bluray_device_option_and_no_dvd_one() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mpv-runtime-options.json"))
                .expect("the captured runtime-options fixture is valid JSON");
        let options = fixture
            .get("options")
            .and_then(serde_json::Value::as_array)
            .expect("the fixture has an options array");
        let names: Vec<&str> = options
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(names.contains(&"bluray-device"));
        assert!(!names.contains(&"dvd-device"));
    }

    #[test]
    fn only_the_bluray_device_option_is_named() {
        assert_eq!(DiscProtocol::BluRay.device_option(), Some("bluray-device"));
        assert_eq!(
            DiscProtocol::Dvd.device_option(),
            None,
            "the DVD device option is absent from the captured runtime"
        );
    }

    #[test]
    fn an_invalid_input_is_surfaced_as_an_input_refusal() {
        let refusal = plan_open(
            Err(DiscInputError::NotDiscStructure),
            DiscOpenTrigger::ExplicitUserAction,
            DiscPlaybackGate::Open,
        );
        assert_eq!(
            refusal,
            Err(DiscOpenRefusal::InputRejected(
                DiscInputError::NotDiscStructure
            ))
        );
    }
}
