//! Pure subtitle selection rules: the auto-selected re-select workaround (M2-21) and the
//! secondary subtitle track rules (M2-09). No mpv I/O happens here; the mpv boundary applies
//! the decisions. Track ids are mpv track ids as the caller reads them from the track list.
//!
//! The mpv writes for these decisions are built here by [`reselect_sid_writes`] and
//! [`secondary_sid_write`], and each value passes the `mpv_options` allowlist.

use crate::mpv_options::{
    MpvOptionError, OptionValue, ValidatedOption, validate, validate_one_shot,
};

/// An mpv subtitle track id.
///
/// [UNCONFIRMED: mpv `sid` / `secondary-sid` take integer track ids; M1-12 owns the inventory type
/// and this id must match it when the two are wired together.]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SubtitleTrackId(i64);

impl SubtitleTrackId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Who chose the currently selected subtitle track.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SelectionOrigin {
    /// mpv picked the track (the `default` or `forced` flag decides the rest).
    Automatic,
    /// The viewer picked the track, or the track was picked on the viewer's behalf.
    UserChoice,
}

/// The subtitle state observed at one mpv position event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReselectObservation {
    /// The value of `subtitles.autoReselect`.
    pub enabled: bool,
    /// The selected subtitle track, or `None` when subtitles are off.
    pub selected: Option<SubtitleTrackId>,
    /// The selected track carries mpv's `default` or `forced` flag.
    pub auto_flagged: bool,
    pub origin: SelectionOrigin,
}

/// What the caller must do after a position event.
///
/// On `ReselectOnce`, the caller sets `sid` to `no`, then back to `track`, using
/// [`reselect_sid_writes`]. This is the only place the workaround touches mpv.
/// [UNCONFIRMED: the reference libmpv may no longer show the defect; the manual comparison in
/// M2-21 decides that.]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReselectDecision {
    NoAction,
    ReselectOnce { track: SubtitleTrackId },
}

/// Per-item gate for the M2-21 workaround. Create a new gate for each item.
///
/// The first position event that sees a selected subtitle track settles the gate. After that the
/// gate never acts again for the item, so seeks do not repeat the re-select. Disabled
/// observations and observations with no selected track do not settle it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SubtitleReselectGate {
    settled: bool,
}

impl SubtitleReselectGate {
    pub const fn new() -> Self {
        Self { settled: false }
    }

    /// True once the gate has made its one decision for this item.
    pub const fn is_settled(self) -> bool {
        self.settled
    }

    pub fn observe_position(&mut self, observation: ReselectObservation) -> ReselectDecision {
        if self.settled || !observation.enabled {
            return ReselectDecision::NoAction;
        }
        let Some(track) = observation.selected else {
            return ReselectDecision::NoAction;
        };

        self.settled = true;
        match (observation.origin, observation.auto_flagged) {
            (SelectionOrigin::Automatic, true) => ReselectDecision::ReselectOnce { track },
            _ => ReselectDecision::NoAction,
        }
    }
}

#[cfg(test)]
mod reselect_tests {
    use super::*;

    fn track(id: i64) -> SubtitleTrackId {
        SubtitleTrackId::new(id)
    }

    fn auto_default(selected: Option<SubtitleTrackId>) -> ReselectObservation {
        ReselectObservation {
            enabled: true,
            selected,
            auto_flagged: true,
            origin: SelectionOrigin::Automatic,
        }
    }

    #[test]
    fn first_position_event_reselects_an_auto_flagged_track() {
        let mut gate = SubtitleReselectGate::new();
        let decision = gate.observe_position(auto_default(Some(track(3))));
        assert_eq!(decision, ReselectDecision::ReselectOnce { track: track(3) });
        assert!(gate.is_settled());
    }

    #[test]
    fn later_position_events_and_seeks_do_not_repeat_the_reselect() {
        let mut gate = SubtitleReselectGate::new();
        gate.observe_position(auto_default(Some(track(3))));
        for _ in 0..100 {
            assert_eq!(
                gate.observe_position(auto_default(Some(track(3)))),
                ReselectDecision::NoAction
            );
        }
    }

    #[test]
    fn forced_flag_counts_as_automatic_selection() {
        let mut gate = SubtitleReselectGate::new();
        let forced = ReselectObservation {
            enabled: true,
            selected: Some(track(7)),
            auto_flagged: true,
            origin: SelectionOrigin::Automatic,
        };
        assert_eq!(
            gate.observe_position(forced),
            ReselectDecision::ReselectOnce { track: track(7) }
        );
    }

    #[test]
    fn user_choice_is_never_reselected() {
        let mut gate = SubtitleReselectGate::new();
        let user = ReselectObservation {
            enabled: true,
            selected: Some(track(2)),
            auto_flagged: true,
            origin: SelectionOrigin::UserChoice,
        };
        assert_eq!(gate.observe_position(user), ReselectDecision::NoAction);
        // The gate is settled by the user's choice, so a later automatic observation is ignored.
        assert_eq!(
            gate.observe_position(auto_default(Some(track(2)))),
            ReselectDecision::NoAction
        );
    }

    #[test]
    fn track_without_default_or_forced_flag_is_not_reselected() {
        let mut gate = SubtitleReselectGate::new();
        let plain = ReselectObservation {
            enabled: true,
            selected: Some(track(4)),
            auto_flagged: false,
            origin: SelectionOrigin::Automatic,
        };
        assert_eq!(gate.observe_position(plain), ReselectDecision::NoAction);
        assert!(gate.is_settled());
    }

    #[test]
    fn setting_off_means_no_action_at_any_position() {
        let mut gate = SubtitleReselectGate::new();
        let off = ReselectObservation {
            enabled: false,
            ..auto_default(Some(track(3)))
        };
        for _ in 0..10 {
            assert_eq!(gate.observe_position(off), ReselectDecision::NoAction);
        }
        assert!(!gate.is_settled());
    }

    #[test]
    fn setting_turned_on_later_still_acts_once_for_the_item() {
        let mut gate = SubtitleReselectGate::new();
        let off = ReselectObservation {
            enabled: false,
            ..auto_default(Some(track(3)))
        };
        gate.observe_position(off);
        assert_eq!(
            gate.observe_position(auto_default(Some(track(3)))),
            ReselectDecision::ReselectOnce { track: track(3) }
        );
        assert_eq!(
            gate.observe_position(auto_default(Some(track(3)))),
            ReselectDecision::NoAction
        );
    }

    #[test]
    fn no_selected_track_is_a_no_op_and_waits_for_a_track() {
        let mut gate = SubtitleReselectGate::new();
        let none = ReselectObservation {
            enabled: true,
            selected: None,
            auto_flagged: false,
            origin: SelectionOrigin::Automatic,
        };
        assert_eq!(gate.observe_position(none), ReselectDecision::NoAction);
        assert!(!gate.is_settled());
        assert_eq!(
            gate.observe_position(auto_default(Some(track(5)))),
            ReselectDecision::ReselectOnce { track: track(5) }
        );
    }

    #[test]
    fn a_new_item_gets_a_fresh_gate() {
        let mut first_item = SubtitleReselectGate::new();
        first_item.observe_position(auto_default(Some(track(3))));
        let mut next_item = SubtitleReselectGate::new();
        assert_eq!(
            next_item.observe_position(auto_default(Some(track(3)))),
            ReselectDecision::ReselectOnce { track: track(3) }
        );
    }
}

// ---------------------------------------------------------------------------
// M2-09 secondary subtitle track rules.
// ---------------------------------------------------------------------------

/// Largest value of `subtitles.secondaryPos` (percent of screen height).
pub const SECONDARY_POS_MAX: u8 = 100;

/// Outcome of a request to set the secondary subtitle track.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecondarySelection {
    /// Secondary is off: the caller writes `secondary-sid` = `no` (see [`secondary_sid_write`]).
    Off,
    /// Secondary is set to this track.
    Track(SubtitleTrackId),
    /// The request named the primary track. The caller shows a message and keeps the current
    /// secondary unchanged (M2-09 requirement 5).
    RejectedSameAsPrimary,
}

/// Decide the secondary selection for one request. `requested` is `None` to turn it off.
pub fn choose_secondary(
    primary: Option<SubtitleTrackId>,
    requested: Option<SubtitleTrackId>,
) -> SecondarySelection {
    match requested {
        None => SecondarySelection::Off,
        Some(track) if Some(track) == primary => SecondarySelection::RejectedSameAsPrimary,
        Some(track) => SecondarySelection::Track(track),
    }
}

/// The mpv write for a secondary selection, checked against the `secondary-sid` allowlist entry.
///
/// `Off` writes `no` and `Track` writes the track id. `RejectedSameAsPrimary` writes nothing and
/// returns `None`, so the current secondary stays in place. A track id that the allowlist refuses
/// (0 or negative) returns an error and writes nothing.
pub fn secondary_sid_write(
    selection: SecondarySelection,
) -> Result<Option<ValidatedOption>, MpvOptionError> {
    match selection {
        SecondarySelection::Off => validate("secondary-sid", OptionValue::Text("no")).map(Some),
        SecondarySelection::Track(track) => {
            validate("secondary-sid", OptionValue::Int(track.get())).map(Some)
        }
        SecondarySelection::RejectedSameAsPrimary => Ok(None),
    }
}

/// The one-shot `sid` writes for a re-select decision, in the order the caller must send them:
/// `no` first, then the track id. `NoAction` writes nothing. Each value passes
/// `validate_one_shot`, so the settings table cannot reach `sid`.
pub fn reselect_sid_writes(
    decision: ReselectDecision,
) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    match decision {
        ReselectDecision::NoAction => Ok(Vec::new()),
        ReselectDecision::ReselectOnce { track } => Ok(vec![
            validate_one_shot("sid", OptionValue::Text("no"))?,
            validate_one_shot("sid", OptionValue::Int(track.get()))?,
        ]),
    }
}

/// The secondary track that stays valid after the primary, the track inventory, or the item
/// changed.
///
/// Returns `None` when the secondary must be cleared (the caller then writes
/// `secondary-sid` = `no`). The secondary is cleared when:
/// - it equals the primary (the viewer changed the primary onto it);
/// - it is no longer in the inventory;
/// - the file has fewer than two subtitle tracks, so the selector is hidden.
///
/// On an item change the caller passes `None` for `secondary`: the secondary never carries over,
/// because M2-10 remembers only audio and subtitle languages.
pub fn reconcile_secondary(
    primary: Option<SubtitleTrackId>,
    secondary: Option<SubtitleTrackId>,
    inventory: &[SubtitleTrackId],
) -> Option<SubtitleTrackId> {
    let track = secondary?;
    if inventory.len() < 2 || !inventory.contains(&track) || Some(track) == primary {
        return None;
    }
    Some(track)
}

/// The secondary selector is shown only when the file has at least two subtitle tracks.
pub const fn secondary_selector_visible(subtitle_track_count: usize) -> bool {
    subtitle_track_count >= 2
}

/// Clamp a `subtitles.secondaryPos` value to `0..=SECONDARY_POS_MAX`.
pub fn clamp_secondary_pos(value: i64) -> u8 {
    u8::try_from(value.clamp(0, i64::from(SECONDARY_POS_MAX))).unwrap_or(SECONDARY_POS_MAX)
}

/// Value to write to `secondary-sub-pos`, or `None` when the setting is unset.
///
/// Legacy wrote the property only when `secondary_pos` was set (PlayerComponent.cpp:1496-1499),
/// so an unset value leaves mpv's own default in place.
pub fn secondary_pos_to_write(setting: Option<i64>) -> Option<u8> {
    setting.map(clamp_secondary_pos)
}

/// Build the allowlisted secondary subtitle position write, if a value is set.
pub fn secondary_position_option(
    setting: Option<i64>,
) -> Result<Option<ValidatedOption>, MpvOptionError> {
    let Some(position) = secondary_pos_to_write(setting) else {
        return Ok(None);
    };
    validate("secondary-sub-pos", OptionValue::Int(i64::from(position))).map(Some)
}

/// Why the secondary selector refused a request. The caller keeps the current secondary track.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecondaryRefusal {
    /// The file has fewer than two subtitle tracks, so the selector is hidden.
    SelectorHidden,
    /// The request named the primary track (M2-09 requirement 5).
    SameAsPrimary,
    /// The request named a track that the file's inventory does not list.
    NotInInventory,
}

/// Tracks the secondary selector offers, in inventory order, without the primary track.
/// Empty when the selector is hidden.
pub fn secondary_candidates(
    primary: Option<SubtitleTrackId>,
    inventory: &[SubtitleTrackId],
) -> Vec<SubtitleTrackId> {
    if !secondary_selector_visible(inventory.len()) {
        return Vec::new();
    }
    inventory
        .iter()
        .copied()
        .filter(|&track| Some(track) != primary)
        .collect()
}

/// Check a secondary request against the file's inventory. `requested` is `None` to turn the
/// secondary track off. Turning it off is always allowed, even when the selector is hidden.
pub fn choose_secondary_in_inventory(
    primary: Option<SubtitleTrackId>,
    inventory: &[SubtitleTrackId],
    requested: Option<SubtitleTrackId>,
) -> Result<SecondarySelection, SecondaryRefusal> {
    let Some(track) = requested else {
        return Ok(SecondarySelection::Off);
    };
    if !secondary_selector_visible(inventory.len()) {
        return Err(SecondaryRefusal::SelectorHidden);
    }
    if Some(track) == primary {
        return Err(SecondaryRefusal::SameAsPrimary);
    }
    if !inventory.contains(&track) {
        return Err(SecondaryRefusal::NotInInventory);
    }
    Ok(SecondarySelection::Track(track))
}

#[cfg(test)]
mod secondary_tests {
    use super::*;

    fn track(id: i64) -> SubtitleTrackId {
        SubtitleTrackId::new(id)
    }

    #[test]
    fn choosing_the_primary_track_as_secondary_is_rejected() {
        assert_eq!(
            choose_secondary(Some(track(2)), Some(track(2))),
            SecondarySelection::RejectedSameAsPrimary
        );
    }

    #[test]
    fn a_different_track_is_accepted_as_secondary() {
        assert_eq!(
            choose_secondary(Some(track(2)), Some(track(3))),
            SecondarySelection::Track(track(3))
        );
        assert_eq!(
            choose_secondary(None, Some(track(3))),
            SecondarySelection::Track(track(3))
        );
    }

    #[test]
    fn disable_value_turns_the_secondary_off() {
        assert_eq!(
            choose_secondary(Some(track(2)), None),
            SecondarySelection::Off
        );
    }

    #[test]
    fn primary_change_onto_the_secondary_clears_the_secondary() {
        let inventory = [track(1), track(2), track(3)];
        assert_eq!(
            reconcile_secondary(Some(track(2)), Some(track(2)), &inventory),
            None
        );
        assert_eq!(
            reconcile_secondary(Some(track(1)), Some(track(2)), &inventory),
            Some(track(2))
        );
    }

    #[test]
    fn secondary_missing_from_inventory_is_cleared() {
        let inventory = [track(1), track(2)];
        assert_eq!(
            reconcile_secondary(Some(track(1)), Some(track(9)), &inventory),
            None
        );
    }

    #[test]
    fn one_subtitle_track_hides_the_selector_and_clears_the_secondary() {
        assert!(!secondary_selector_visible(0));
        assert!(!secondary_selector_visible(1));
        assert!(secondary_selector_visible(2));
        assert_eq!(
            reconcile_secondary(Some(track(1)), Some(track(2)), &[track(1)]),
            None
        );
    }

    #[test]
    fn item_change_passes_none_and_nothing_carries_over() {
        let inventory = [track(1), track(2)];
        assert_eq!(reconcile_secondary(Some(track(1)), None, &inventory), None);
    }

    #[test]
    fn position_is_clamped_to_zero_through_one_hundred() {
        assert_eq!(clamp_secondary_pos(-5), 0);
        assert_eq!(clamp_secondary_pos(0), 0);
        assert_eq!(clamp_secondary_pos(42), 42);
        assert_eq!(clamp_secondary_pos(100), 100);
        assert_eq!(clamp_secondary_pos(101), 100);
        assert_eq!(clamp_secondary_pos(i64::MAX), 100);
        assert_eq!(clamp_secondary_pos(i64::MIN), 0);
    }

    #[test]
    fn unset_position_writes_nothing() {
        assert_eq!(secondary_pos_to_write(None), None);
        assert_eq!(secondary_pos_to_write(Some(250)), Some(100));
        assert_eq!(secondary_pos_to_write(Some(-1)), Some(0));
        assert_eq!(
            secondary_position_option(Some(42)).expect("position is allowlisted"),
            Some(ValidatedOption {
                name: "secondary-sub-pos",
                value: "42".to_owned(),
            })
        );
    }
}

#[cfg(test)]
mod sid_write_tests {
    use super::*;

    fn track(id: i64) -> SubtitleTrackId {
        SubtitleTrackId::new(id)
    }

    fn write(name: &'static str, value: &str) -> ValidatedOption {
        ValidatedOption {
            name,
            value: value.to_owned(),
        }
    }

    #[test]
    fn secondary_off_writes_no_through_the_secondary_sid_entry() {
        assert_eq!(
            secondary_sid_write(SecondarySelection::Off),
            Ok(Some(write("secondary-sid", "no")))
        );
    }

    #[test]
    fn secondary_track_writes_its_id_and_a_rejected_primary_writes_nothing() {
        assert_eq!(
            secondary_sid_write(SecondarySelection::Track(track(3))),
            Ok(Some(write("secondary-sid", "3")))
        );
        assert_eq!(
            secondary_sid_write(SecondarySelection::RejectedSameAsPrimary),
            Ok(None)
        );
        assert_eq!(
            secondary_sid_write(SecondarySelection::Track(track(0))),
            Err(MpvOptionError::OutOfRange {
                name: "secondary-sid"
            })
        );
    }

    #[test]
    fn reselect_writes_no_then_the_track_through_the_one_shot_sid_entry() {
        let writes = reselect_sid_writes(ReselectDecision::ReselectOnce { track: track(3) });
        assert_eq!(writes, Ok(vec![write("sid", "no"), write("sid", "3")]));
        assert_eq!(
            reselect_sid_writes(ReselectDecision::NoAction),
            Ok(Vec::new())
        );
    }

    #[test]
    fn reselect_with_a_non_positive_track_returns_an_error_and_no_writes() {
        let result = reselect_sid_writes(ReselectDecision::ReselectOnce { track: track(-2) });
        assert_eq!(result, Err(MpvOptionError::OutOfRange { name: "sid" }));
    }
}
