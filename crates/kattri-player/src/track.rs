//! Track model and the audio/subtitle cycling rule behind the `cycle_audio` and
//! `cycle_subtitles` actions.

/// Engine-assigned track id. Stable only within one loaded item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// One track in the loaded item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: TrackId,
    pub kind: TrackKind,
    /// BCP 47 or ISO 639 code as reported by the container, if present.
    pub language: Option<String>,
    pub title: Option<String>,
    pub codec: String,
    pub is_default: bool,
    pub is_forced: bool,
    /// True for sidecar subtitle files delivered by the server.
    pub is_external: bool,
}

/// What a subtitle or audio slot is set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Track(TrackId),
    /// Subtitles off. Audio has no off state.
    Off,
}

/// Choose the next (`step = 1`) or previous (`step = -1`) track of `kind`.
///
/// Returns `None` if the item has no tracks of that kind. For subtitles, "off" is an
/// extra stop after the last track, so cycling forward wraps through it. Audio never
/// selects "off". If `current` is not among the candidates, forward starts at the
/// first stop and backward at the last. Any `step` other than 1 or -1 is treated as its sign.
pub fn next_selection(
    tracks: &[Track],
    kind: TrackKind,
    current: Selection,
    step: i32,
) -> Option<Selection> {
    let mut stops: Vec<Selection> = tracks
        .iter()
        .filter(|t| t.kind == kind)
        .map(|t| Selection::Track(t.id))
        .collect();
    if stops.is_empty() {
        return None;
    }
    if kind == TrackKind::Subtitle {
        stops.push(Selection::Off);
    }

    let len = stops.len();
    let index = match stops.iter().position(|s| *s == current) {
        Some(i) => {
            if step >= 0 {
                (i + 1) % len
            } else {
                (i + len - 1) % len
            }
        }
        None => {
            if step >= 0 {
                0
            } else {
                len - 1
            }
        }
    };
    Some(stops[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: u32, kind: TrackKind) -> Track {
        Track {
            id: TrackId(id),
            kind,
            language: None,
            title: None,
            codec: "x".to_owned(),
            is_default: false,
            is_forced: false,
            is_external: false,
        }
    }

    fn sample() -> Vec<Track> {
        vec![
            track(1, TrackKind::Video),
            track(2, TrackKind::Audio),
            track(3, TrackKind::Audio),
            track(4, TrackKind::Subtitle),
            track(5, TrackKind::Subtitle),
        ]
    }

    #[test]
    fn audio_cycles_forward_and_wraps_without_off() {
        let t = sample();
        let a = |id| Selection::Track(TrackId(id));
        assert_eq!(next_selection(&t, TrackKind::Audio, a(2), 1), Some(a(3)));
        assert_eq!(next_selection(&t, TrackKind::Audio, a(3), 1), Some(a(2)));
    }

    #[test]
    fn audio_cycles_backward() {
        let t = sample();
        let a = |id| Selection::Track(TrackId(id));
        assert_eq!(next_selection(&t, TrackKind::Audio, a(2), -1), Some(a(3)));
    }

    #[test]
    fn subtitles_include_off_as_a_stop_after_the_last_track() {
        let t = sample();
        let s = |id| Selection::Track(TrackId(id));
        assert_eq!(
            next_selection(&t, TrackKind::Subtitle, s(5), 1),
            Some(Selection::Off)
        );
        assert_eq!(
            next_selection(&t, TrackKind::Subtitle, Selection::Off, 1),
            Some(s(4))
        );
        assert_eq!(
            next_selection(&t, TrackKind::Subtitle, s(4), -1),
            Some(Selection::Off)
        );
    }

    #[test]
    fn unknown_current_starts_at_the_appropriate_end() {
        let t = sample();
        let a = |id| Selection::Track(TrackId(id));
        assert_eq!(
            next_selection(&t, TrackKind::Audio, Selection::Off, 1),
            Some(a(2))
        );
        assert_eq!(
            next_selection(&t, TrackKind::Audio, Selection::Off, -1),
            Some(a(3))
        );
    }

    #[test]
    fn no_tracks_of_kind_returns_none() {
        let only_video = vec![track(1, TrackKind::Video)];
        assert_eq!(
            next_selection(&only_video, TrackKind::Audio, Selection::Off, 1),
            None
        );
        assert_eq!(
            next_selection(&[], TrackKind::Subtitle, Selection::Off, 1),
            None
        );
    }

    #[test]
    fn single_subtitle_toggles_between_track_and_off() {
        let t = vec![track(9, TrackKind::Subtitle)];
        assert_eq!(
            next_selection(&t, TrackKind::Subtitle, Selection::Track(TrackId(9)), 1),
            Some(Selection::Off)
        );
        assert_eq!(
            next_selection(&t, TrackKind::Subtitle, Selection::Off, 1),
            Some(Selection::Track(TrackId(9)))
        );
    }
}
