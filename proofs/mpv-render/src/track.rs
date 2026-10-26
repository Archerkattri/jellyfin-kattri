//! Owned track metadata and pure validation for libmpv track selection.

use std::collections::HashSet;

pub(crate) const MAX_TRACK_COUNT: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MpvTrackId(i64);

impl MpvTrackId {
    pub const fn get(self) -> i64 {
        self.0
    }

    fn from_raw(value: i64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TrackKind {
    Audio,
    Video,
    Subtitle,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MpvTrack {
    pub id: MpvTrackId,
    pub kind: TrackKind,
    pub title: Option<String>,
    pub language: Option<String>,
    pub codec: Option<String>,
    /// Decoder selected for this track, when libmpv exposes it.
    pub decoder: Option<String>,
    pub selected: Option<bool>,
    pub external: Option<bool>,
    pub default: Option<bool>,
    pub forced: Option<bool>,
    pub visual_impaired: Option<bool>,
    pub hearing_impaired: Option<bool>,
    pub channels: Option<String>,
    /// Demuxer metadata; it may differ from the decoder's eventual output rate.
    pub sample_rate_hz: Option<u64>,
}

impl MpvTrack {
    /// The fields the media info panel shows, as plain values.
    pub fn media_values(&self) -> kattri_client_core::media_info_mpv::MpvTrackValues {
        kattri_client_core::media_info_mpv::MpvTrackValues {
            id: self.id.get(),
            codec: self.codec.clone(),
            language: self.language.clone(),
            title: self.title.clone(),
            channels: self.channels.clone(),
            forced: self.forced.unwrap_or(false),
            selected: self.selected.unwrap_or(false),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackSelection {
    Automatic,
    Disabled,
    Track(MpvTrackId),
}

impl TrackSelection {
    pub(crate) fn command_value(self) -> String {
        match self {
            Self::Automatic => "auto".to_owned(),
            Self::Disabled => "no".to_owned(),
            Self::Track(id) => id.get().to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackSelectionState {
    Automatic,
    Disabled,
    Track(MpvTrackId),
    Unknown,
}

impl TrackSelectionState {
    pub(crate) fn parse(value: Option<&str>) -> Self {
        match value {
            Some("auto") => Self::Automatic,
            Some("no") => Self::Disabled,
            Some(value) => value
                .parse::<i64>()
                .ok()
                .filter(|id| *id >= 0)
                .map(MpvTrackId::from_raw)
                .map_or(Self::Unknown, Self::Track),
            None => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackInventory {
    tracks: Vec<MpvTrack>,
    audio_selection: TrackSelectionState,
    subtitle_selection: TrackSelectionState,
    pub(crate) session_id: u64,
    pub(crate) generation: u64,
}

impl TrackInventory {
    pub fn tracks(&self) -> &[MpvTrack] {
        &self.tracks
    }

    pub const fn audio_selection(&self) -> TrackSelectionState {
        self.audio_selection
    }

    pub const fn subtitle_selection(&self) -> TrackSelectionState {
        self.subtitle_selection
    }

    pub fn audio_tracks(&self) -> impl Iterator<Item = &MpvTrack> {
        self.tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
    }

    pub fn subtitle_tracks(&self) -> impl Iterator<Item = &MpvTrack> {
        self.tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Subtitle)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackInventoryError {
    InvalidTrackCount,
    TooManyTracks { count: usize, limit: usize },
    TooManyPendingInventories { limit: usize },
    MissingTrackId { index: usize },
    InvalidTrackId { index: usize, id: i64 },
    MissingTrackType { index: usize },
    DuplicateTrackId { kind: TrackKind, id: MpvTrackId },
    UnexpectedPropertyFormat { property: String, format: i32 },
    InvalidPropertyValue { property: String },
    PropertyReadFailed { property: String, code: i32 },
    StaleSnapshot,
}

impl TrackInventoryError {
    pub(crate) fn mpv_error(&self) -> i32 {
        match self {
            Self::PropertyReadFailed { code, .. } => *code,
            _ => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackSelectionError {
    InventoryFromAnotherSession,
    StaleInventory,
    TrackNotInInventory,
    WrongTrackKind,
    TrackListObservationUnavailable,
    QueueFull,
    WorkerUnavailable,
    SubmissionFailed(i32),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RawTrack {
    pub id: Option<i64>,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub language: Option<String>,
    pub codec: Option<String>,
    pub decoder: Option<String>,
    pub selected: Option<bool>,
    pub external: Option<bool>,
    pub default: Option<bool>,
    pub forced: Option<bool>,
    pub visual_impaired: Option<bool>,
    pub hearing_impaired: Option<bool>,
    pub channels: Option<String>,
    pub sample_rate_hz: Option<i64>,
}

pub(crate) fn parse_track_inventory(
    raw_tracks: Vec<RawTrack>,
    audio_selection: Option<&str>,
    subtitle_selection: Option<&str>,
    session_id: u64,
    generation: u64,
) -> Result<TrackInventory, TrackInventoryError> {
    if raw_tracks.len() > MAX_TRACK_COUNT {
        return Err(TrackInventoryError::TooManyTracks {
            count: raw_tracks.len(),
            limit: MAX_TRACK_COUNT,
        });
    }

    let mut seen_ids = HashSet::with_capacity(raw_tracks.len());
    let mut tracks = Vec::with_capacity(raw_tracks.len());
    for (index, raw) in raw_tracks.into_iter().enumerate() {
        let id = raw
            .id
            .ok_or(TrackInventoryError::MissingTrackId { index })?;
        if id < 0 {
            return Err(TrackInventoryError::InvalidTrackId { index, id });
        }
        let track_id = MpvTrackId::from_raw(id);
        let raw_kind = raw
            .kind
            .ok_or(TrackInventoryError::MissingTrackType { index })?;
        let kind = match raw_kind.as_str() {
            "audio" => TrackKind::Audio,
            "video" => TrackKind::Video,
            "sub" => TrackKind::Subtitle,
            _ => TrackKind::Other(raw_kind),
        };
        if !seen_ids.insert((kind.clone(), track_id)) {
            return Err(TrackInventoryError::DuplicateTrackId { kind, id: track_id });
        }

        tracks.push(MpvTrack {
            id: track_id,
            kind,
            title: clean_optional_text(raw.title),
            language: clean_optional_text(raw.language),
            codec: clean_optional_text(raw.codec),
            decoder: clean_optional_text(raw.decoder),
            selected: raw.selected,
            external: raw.external,
            default: raw.default,
            forced: raw.forced,
            visual_impaired: raw.visual_impaired,
            hearing_impaired: raw.hearing_impaired,
            channels: clean_optional_text(raw.channels),
            sample_rate_hz: raw
                .sample_rate_hz
                .and_then(|value| u64::try_from(value).ok()),
        });
    }

    Ok(TrackInventory {
        tracks,
        audio_selection: TrackSelectionState::parse(audio_selection),
        subtitle_selection: TrackSelectionState::parse(subtitle_selection),
        session_id,
        generation,
    })
}

pub(crate) fn validate_selection(
    inventory: &TrackInventory,
    kind: TrackKind,
    selection: TrackSelection,
) -> Result<String, TrackSelectionError> {
    if let TrackSelection::Track(id) = selection {
        let Some(track) = inventory.tracks.iter().find(|track| track.id == id) else {
            return Err(TrackSelectionError::TrackNotInInventory);
        };
        if track.kind != kind {
            return Err(TrackSelectionError::WrongTrackKind);
        }
    }
    Ok(selection.command_value())
}

pub(crate) fn validate_inventory_identity(
    inventory: &TrackInventory,
    session_id: u64,
    generation: u64,
) -> Result<(), TrackSelectionError> {
    if inventory.session_id != session_id {
        return Err(TrackSelectionError::InventoryFromAnotherSession);
    }
    if inventory.generation != generation {
        return Err(TrackSelectionError::StaleInventory);
    }
    Ok(())
}

fn clean_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|value| (!value.trim().is_empty()).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(id: i64, kind: &str) -> RawTrack {
        RawTrack {
            id: Some(id),
            kind: Some(kind.to_owned()),
            ..RawTrack::default()
        }
    }

    fn inventory() -> TrackInventory {
        let mut external_subtitle = raw(2, "sub");
        external_subtitle.external = Some(true);
        parse_track_inventory(
            vec![raw(0, "audio"), external_subtitle],
            Some("0"),
            Some("no"),
            7,
            11,
        )
        .unwrap()
    }

    #[test]
    fn parses_owned_track_metadata_and_external_subtitles() {
        let mut audio = raw(0, "audio");
        audio.title = Some("  Director mix  ".to_owned());
        audio.language = Some("eng".to_owned());
        audio.codec = Some("flac".to_owned());
        audio.decoder = Some("flac".to_owned());
        audio.selected = Some(true);
        audio.channels = Some("5.1".to_owned());
        audio.sample_rate_hz = Some(48_000);

        let mut subtitle = raw(2, "sub");
        subtitle.title = Some("English SDH".to_owned());
        subtitle.external = Some(true);
        subtitle.hearing_impaired = Some(true);

        let parsed =
            parse_track_inventory(vec![audio, subtitle], Some("0"), Some("2"), 4, 9).unwrap();

        assert_eq!(
            parsed.audio_selection,
            TrackSelectionState::Track(MpvTrackId(0))
        );
        assert_eq!(
            parsed.subtitle_selection,
            TrackSelectionState::Track(MpvTrackId(2))
        );
        assert_eq!(parsed.audio_tracks().count(), 1);
        assert_eq!(parsed.subtitle_tracks().count(), 1);
        assert_eq!(parsed.tracks[0].title.as_deref(), Some("  Director mix  "));
        assert_eq!(parsed.tracks[0].decoder.as_deref(), Some("flac"));
        assert_eq!(parsed.tracks[0].sample_rate_hz, Some(48_000));
        assert_eq!(parsed.tracks[1].external, Some(true));
        assert_eq!(parsed.tracks[1].hearing_impaired, Some(true));
    }

    #[test]
    fn media_values_carry_the_panel_fields_and_default_missing_flags_to_false() {
        let mut audio = raw(3, "audio");
        audio.codec = Some("ac3".to_owned());
        audio.language = Some("eng".to_owned());
        audio.title = Some("Main".to_owned());
        audio.channels = Some("unknown6".to_owned());
        audio.selected = Some(true);
        let subtitle = raw(4, "sub");
        let parsed = parse_track_inventory(vec![audio, subtitle], None, None, 1, 1).unwrap();

        let audio = parsed.tracks[0].media_values();
        assert_eq!(audio.id, 3);
        assert_eq!(audio.codec.as_deref(), Some("ac3"));
        assert_eq!(audio.language.as_deref(), Some("eng"));
        assert_eq!(audio.title.as_deref(), Some("Main"));
        assert_eq!(audio.channels.as_deref(), Some("unknown6"));
        assert!(audio.selected);
        assert!(!audio.forced);

        let subtitle = parsed.tracks[1].media_values();
        assert_eq!(subtitle.id, 4);
        assert!(!subtitle.selected);
        assert!(!subtitle.forced);
    }

    #[test]
    fn accepts_same_id_for_different_track_types_but_rejects_same_type_duplicates() {
        assert!(
            parse_track_inventory(vec![raw(1, "audio"), raw(1, "sub")], None, None, 1, 1,).is_ok()
        );
        assert_eq!(
            parse_track_inventory(vec![raw(1, "audio"), raw(1, "audio")], None, None, 1, 1,),
            Err(TrackInventoryError::DuplicateTrackId {
                kind: TrackKind::Audio,
                id: MpvTrackId(1),
            })
        );
    }

    #[test]
    fn parser_requires_an_id_and_rejects_negative_track_ids() {
        let mut missing = raw(0, "audio");
        missing.id = None;
        assert_eq!(
            parse_track_inventory(vec![missing], None, None, 1, 1),
            Err(TrackInventoryError::MissingTrackId { index: 0 })
        );
        assert_eq!(
            parse_track_inventory(vec![raw(-1, "audio")], None, None, 1, 1),
            Err(TrackInventoryError::InvalidTrackId { index: 0, id: -1 })
        );
    }

    #[test]
    fn selection_state_preserves_auto_disabled_and_explicit_ids() {
        assert_eq!(
            TrackSelectionState::parse(Some("auto")),
            TrackSelectionState::Automatic
        );
        assert_eq!(
            TrackSelectionState::parse(Some("no")),
            TrackSelectionState::Disabled
        );
        assert_eq!(
            TrackSelectionState::parse(Some("0")),
            TrackSelectionState::Track(MpvTrackId(0))
        );
        assert_eq!(
            TrackSelectionState::parse(Some("-1")),
            TrackSelectionState::Unknown
        );
        assert_eq!(
            TrackSelectionState::parse(Some("not-a-track-id")),
            TrackSelectionState::Unknown
        );
    }

    #[test]
    fn selection_values_keep_mpv_auto_no_and_external_track_ids() {
        let inventory = inventory();
        assert_eq!(
            inventory.subtitle_tracks().next().unwrap().external,
            Some(true)
        );
        assert_eq!(
            validate_selection(&inventory, TrackKind::Audio, TrackSelection::Automatic),
            Ok("auto".to_owned())
        );
        assert_eq!(
            validate_selection(&inventory, TrackKind::Subtitle, TrackSelection::Disabled),
            Ok("no".to_owned())
        );
        assert_eq!(
            validate_selection(
                &inventory,
                TrackKind::Subtitle,
                TrackSelection::Track(MpvTrackId(2))
            ),
            Ok("2".to_owned())
        );
    }

    #[test]
    fn selection_rejects_absent_and_wrong_kind_track_ids() {
        let inventory = inventory();
        assert_eq!(
            validate_selection(
                &inventory,
                TrackKind::Subtitle,
                TrackSelection::Track(MpvTrackId(0))
            ),
            Err(TrackSelectionError::WrongTrackKind)
        );
        assert_eq!(
            validate_selection(
                &inventory,
                TrackKind::Audio,
                TrackSelection::Track(MpvTrackId(19))
            ),
            Err(TrackSelectionError::TrackNotInInventory)
        );
    }

    #[test]
    fn inventory_identity_rejects_other_sessions_and_stale_generations() {
        let inventory = inventory();
        assert_eq!(validate_inventory_identity(&inventory, 7, 11), Ok(()));
        assert_eq!(
            validate_inventory_identity(&inventory, 8, 11),
            Err(TrackSelectionError::InventoryFromAnotherSession)
        );
        assert_eq!(
            validate_inventory_identity(&inventory, 7, 12),
            Err(TrackSelectionError::StaleInventory)
        );
    }

    #[test]
    fn unknown_track_types_are_preserved_but_not_selectable_as_audio_or_subtitles() {
        let inventory =
            parse_track_inventory(vec![raw(1, "future-kind")], None, None, 1, 1).unwrap();
        assert_eq!(
            inventory.tracks[0].kind,
            TrackKind::Other("future-kind".to_owned())
        );
        assert_eq!(
            validate_selection(
                &inventory,
                TrackKind::Audio,
                TrackSelection::Track(MpvTrackId(1))
            ),
            Err(TrackSelectionError::WrongTrackKind)
        );
    }

    #[test]
    fn rejects_inventory_larger_than_the_bounded_limit() {
        let raw_tracks = (0..=MAX_TRACK_COUNT)
            .map(|id| raw(id as i64, "audio"))
            .collect();
        assert_eq!(
            parse_track_inventory(raw_tracks, None, None, 1, 1),
            Err(TrackInventoryError::TooManyTracks {
                count: MAX_TRACK_COUNT + 1,
                limit: MAX_TRACK_COUNT,
            })
        );
    }
}
