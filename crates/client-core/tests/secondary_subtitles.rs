//! M2-09 secondary subtitle selection rules, exercised through the public API.
//!
//! These cover the selector's choice from the file's inventory, the refusal when the secondary
//! would equal the primary, the disable value, and the position clamp. They do not touch mpv.

use kattri_client_core::mpv_options::{MpvOptionError, ValidatedOption};
use kattri_client_core::subtitles::{
    SecondaryRefusal, SecondarySelection, SubtitleTrackId, choose_secondary_in_inventory,
    clamp_secondary_pos, reconcile_secondary, secondary_candidates, secondary_pos_to_write,
    secondary_position_option, secondary_selector_visible, secondary_sid_write,
};

fn track(id: i64) -> SubtitleTrackId {
    SubtitleTrackId::new(id)
}

fn inventory() -> [SubtitleTrackId; 3] {
    [track(1), track(2), track(3)]
}

#[test]
fn candidates_exclude_the_primary_and_keep_inventory_order() {
    assert_eq!(
        secondary_candidates(Some(track(2)), &inventory()),
        vec![track(1), track(3)]
    );
    assert_eq!(
        secondary_candidates(None, &inventory()),
        vec![track(1), track(2), track(3)]
    );
}

#[test]
fn a_single_subtitle_track_offers_no_candidates() {
    assert!(!secondary_selector_visible(1));
    assert!(secondary_candidates(Some(track(1)), &[track(1)]).is_empty());
    assert!(secondary_candidates(None, &[]).is_empty());
}

#[test]
fn choosing_a_different_track_selects_it() {
    assert_eq!(
        choose_secondary_in_inventory(Some(track(1)), &inventory(), Some(track(3))),
        Ok(SecondarySelection::Track(track(3)))
    );
    assert_eq!(
        choose_secondary_in_inventory(None, &inventory(), Some(track(2))),
        Ok(SecondarySelection::Track(track(2)))
    );
}

#[test]
fn choosing_the_primary_track_is_refused() {
    assert_eq!(
        choose_secondary_in_inventory(Some(track(2)), &inventory(), Some(track(2))),
        Err(SecondaryRefusal::SameAsPrimary)
    );
}

#[test]
fn a_track_outside_the_inventory_is_refused() {
    assert_eq!(
        choose_secondary_in_inventory(Some(track(1)), &inventory(), Some(track(9))),
        Err(SecondaryRefusal::NotInInventory)
    );
}

#[test]
fn a_hidden_selector_refuses_a_track_but_still_allows_off() {
    assert_eq!(
        choose_secondary_in_inventory(Some(track(1)), &[track(1)], Some(track(2))),
        Err(SecondaryRefusal::SelectorHidden)
    );
    assert_eq!(
        choose_secondary_in_inventory(Some(track(1)), &[track(1)], None),
        Ok(SecondarySelection::Off)
    );
}

#[test]
fn disable_value_turns_the_secondary_off_and_writes_no() {
    let selection = choose_secondary_in_inventory(Some(track(1)), &inventory(), None);
    assert_eq!(selection, Ok(SecondarySelection::Off));
    assert_eq!(
        secondary_sid_write(SecondarySelection::Off),
        Ok(Some(ValidatedOption {
            name: "secondary-sid",
            value: "no".to_owned(),
        }))
    );
}

#[test]
fn a_refused_primary_write_leaves_the_secondary_unchanged() {
    let selection = choose_secondary_in_inventory(Some(track(2)), &inventory(), Some(track(2)));
    assert!(selection.is_err());
    assert_eq!(
        secondary_sid_write(SecondarySelection::RejectedSameAsPrimary),
        Ok(None)
    );
}

#[test]
fn primary_moved_onto_the_secondary_clears_the_secondary() {
    assert_eq!(
        reconcile_secondary(Some(track(3)), Some(track(3)), &inventory()),
        None
    );
}

#[test]
fn position_clamps_to_zero_through_one_hundred() {
    assert_eq!(clamp_secondary_pos(-1), 0);
    assert_eq!(clamp_secondary_pos(0), 0);
    assert_eq!(clamp_secondary_pos(55), 55);
    assert_eq!(clamp_secondary_pos(100), 100);
    assert_eq!(clamp_secondary_pos(101), 100);
}

#[test]
fn an_unset_position_writes_nothing_and_a_set_one_is_allowlisted() {
    assert_eq!(secondary_pos_to_write(None), None);
    assert_eq!(secondary_pos_to_write(Some(900)), Some(100));
    assert_eq!(
        secondary_position_option(Some(-20)),
        Ok(Some(ValidatedOption {
            name: "secondary-sub-pos",
            value: "0".to_owned(),
        }))
    );
    assert_eq!(secondary_position_option(None), Ok(None));
}

#[test]
fn the_secondary_track_id_must_be_positive() {
    assert_eq!(
        secondary_sid_write(SecondarySelection::Track(track(0))),
        Err(MpvOptionError::OutOfRange {
            name: "secondary-sid"
        })
    );
}
