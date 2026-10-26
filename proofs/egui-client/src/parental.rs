//! Pure parental-control decisions (M4-20): the rating ceiling for browsing, the PIN message
//! after an unlock attempt, the step a profile switch takes after the PIN, and the place a
//! profile keeps its attempt file. No egui or window code is here, so the rules are tested
//! directly.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use egui::Id;
use jellyfin_api::BaseItemDto;
use kattri_parental::{ATTEMPT_FILE_NAME, PinError, RatingDecision, Unlock, check_rating_text};

/// The rating ceiling browsing should apply. The ceiling applies only to a locked profile: one
/// that has a PIN and has not been unlocked in this run.
pub(crate) fn browse_ceiling(
    max_rating: Option<u32>,
    protected: bool,
    unlocked: bool,
) -> Option<u32> {
    if protected && !unlocked {
        max_rating
    } else {
        None
    }
}

/// Whether an item with `official_rating` stays visible under `ceiling`. An item with no rating
/// at all stays visible. Blank or unknown rating text is hidden while a ceiling is set.
pub(crate) fn item_is_shown(ceiling: Option<u32>, official_rating: Option<&str>) -> bool {
    check_rating_text(ceiling, official_rating) != RatingDecision::Block
}

/// The items of one browse page that stay visible under `ceiling`, in their original order.
pub(crate) fn shown_items(items: &[BaseItemDto], ceiling: Option<u32>) -> Vec<BaseItemDto> {
    items
        .iter()
        .filter(|item| item_is_shown(ceiling, item.official_rating.as_deref()))
        .cloned()
        .collect()
}

const RUN_UNLOCK_ID: &str = "kattri_parental_run_unlock";

/// Records that the settings page unlocked `profile` in this run. It lives in egui memory, so
/// a restart locks the profile again.
pub(crate) fn set_run_unlock(ctx: &egui::Context, profile: Option<&str>, unlocked: bool) {
    let id = Id::unique(RUN_UNLOCK_ID);
    ctx.data_mut(|data| {
        if let (true, Some(name)) = (unlocked, profile) {
            data.insert_temp(id, name.to_owned());
        } else {
            data.remove::<String>(id);
        }
    });
}

fn run_unlocked(ctx: &egui::Context, profile: Option<&str>) -> bool {
    let id = Id::unique(RUN_UNLOCK_ID);
    let unlocked_name = ctx.data(|data| data.get_temp::<String>(id));
    match (unlocked_name, profile) {
        (Some(name), Some(current)) => name == current,
        _ => false,
    }
}

/// The ceiling for browsing the running profile. `pin_record` is the stored PIN (empty when
/// none is set). A damaged record still counts as a PIN, so the ceiling stays on.
pub(crate) fn running_profile_ceiling(
    ctx: &egui::Context,
    pin_record: &str,
    max_rating: Option<u32>,
    profile: Option<&str>,
) -> Option<u32> {
    browse_ceiling(
        max_rating,
        !pin_record.is_empty(),
        run_unlocked(ctx, profile),
    )
}

/// What the PIN gate does after an unlock attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateStep {
    /// The PIN matched and the lock state was written. The switch may go ahead.
    Switch,
    /// Stay on the picker and show this message. It never contains the PIN.
    Stay(String),
}

/// Decides the gate from the unlock outcome and the result of writing the attempt file. A switch
/// needs both a matching PIN and a saved lock state. Otherwise a failed write would let guesses
/// go without the delay.
pub(crate) fn gate_step(outcome: Result<Unlock, PinError>, saved: Result<(), String>) -> GateStep {
    match (outcome, saved) {
        (Ok(Unlock::Unlocked), Ok(())) => GateStep::Switch,
        (Ok(Unlock::Unlocked), Err(error)) => GateStep::Stay(format!(
            "The PIN matched, but the lock state was not written: {error}"
        )),
        (outcome, saved) => {
            let mut message = unlock_message(outcome);
            if let Err(error) = saved {
                message.push_str(&format!(" The lock state was not written: {error}"));
            }
            GateStep::Stay(message)
        }
    }
}

/// The message for an unlock outcome. It never includes the PIN.
pub(crate) fn unlock_message(outcome: Result<Unlock, PinError>) -> String {
    match outcome {
        Ok(Unlock::Unlocked) => "Unlocked for this run.".to_owned(),
        Ok(Unlock::NotProtected) => "No PIN is set.".to_owned(),
        Ok(Unlock::WrongPin {
            retry_after_secs: 0,
            ..
        }) => "Wrong PIN.".to_owned(),
        Ok(Unlock::WrongPin {
            retry_after_secs, ..
        })
        | Ok(Unlock::LockedOut { retry_after_secs }) => {
            format!("Wait {retry_after_secs} seconds before the next attempt.")
        }
        Err(_) => "The PIN could not be checked.".to_owned(),
    }
}

/// The attempt file of one profile. It sits in the profile's folder, next to its settings, so
/// each profile counts its own wrong PINs. A missing file means no attempts yet.
pub(crate) fn attempt_file(profile_dir: &Path) -> PathBuf {
    profile_dir.join(ATTEMPT_FILE_NAME)
}

/// Whole seconds since the Unix epoch, the clock the attempt delay is stored against.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kattri_parental::{AttemptState, load_attempts, save_attempts};
    use kattri_settings::ProfileStore;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kattri-parental-client-{label}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn ceiling_applies_only_to_a_locked_profile() {
        assert_eq!(browse_ceiling(Some(7), true, false), Some(7));
        assert_eq!(browse_ceiling(Some(7), true, true), None);
        assert_eq!(browse_ceiling(Some(7), false, false), None);
        assert_eq!(browse_ceiling(None, true, false), None);
    }

    #[test]
    fn item_above_the_ceiling_is_hidden_and_unrated_items_stay() {
        // Ceiling 6 is PG-13 on the ladder (TV-Y is 0).
        assert!(item_is_shown(Some(6), Some("PG-13")));
        assert!(item_is_shown(Some(6), Some("pg")));
        assert!(!item_is_shown(Some(6), Some("R")));
        assert!(item_is_shown(Some(6), None));
        assert!(item_is_shown(None, Some("NC-17")));
    }

    #[test]
    fn unknown_or_blank_rating_text_is_hidden_under_a_ceiling() {
        assert!(!item_is_shown(Some(10), Some("Unrated")));
        assert!(!item_is_shown(Some(10), Some("   ")));
        assert!(item_is_shown(None, Some("Unrated")));
    }

    fn rated(id: &str, rating: Option<&str>) -> BaseItemDto {
        let mut value = serde_json::json!({ "Id": id, "Name": id, "Type": "Movie" });
        if let Some(rating) = rating {
            value["OfficialRating"] = serde_json::Value::from(rating);
        }
        match serde_json::from_value(value) {
            Ok(item) => item,
            Err(error) => panic!("test item should parse: {error}"),
        }
    }

    #[test]
    fn a_filtered_page_hides_items_above_the_ceiling_and_keeps_order() {
        let page = vec![
            rated("kids", Some("G")),
            rated("adult", Some("R")),
            rated("bare", None),
            rated("odd", Some("Unrated")),
            rated("pg13", Some("PG-13")),
        ];
        let shown: Vec<String> = shown_items(&page, Some(6))
            .into_iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(shown, ["kids", "bare", "pg13"]);
        assert_eq!(shown_items(&page, None).len(), page.len());
    }

    #[test]
    fn a_correct_pin_with_a_saved_state_lets_the_switch_go_ahead() {
        assert_eq!(gate_step(Ok(Unlock::Unlocked), Ok(())), GateStep::Switch);
    }

    #[test]
    fn a_failed_write_after_a_correct_pin_still_refuses_the_switch() {
        let step = gate_step(Ok(Unlock::Unlocked), Err("disk full".to_owned()));
        assert_eq!(
            step,
            GateStep::Stay(
                "The PIN matched, but the lock state was not written: disk full".to_owned()
            )
        );
    }

    #[test]
    fn wrong_or_waiting_pins_never_allow_the_switch() {
        let wrong = gate_step(
            Ok(Unlock::WrongPin {
                failures: 1,
                retry_after_secs: 0,
            }),
            Ok(()),
        );
        assert_eq!(wrong, GateStep::Stay("Wrong PIN.".to_owned()));
        let waiting = gate_step(
            Ok(Unlock::LockedOut {
                retry_after_secs: 4,
            }),
            Ok(()),
        );
        assert_eq!(
            waiting,
            GateStep::Stay("Wait 4 seconds before the next attempt.".to_owned())
        );
        assert_eq!(
            gate_step(Ok(Unlock::NotProtected), Ok(())),
            GateStep::Stay("No PIN is set.".to_owned())
        );
    }

    #[test]
    fn unlock_messages_never_contain_the_pin_text() {
        let typed = "4321";
        let messages = [
            unlock_message(Ok(Unlock::Unlocked)),
            unlock_message(Ok(Unlock::WrongPin {
                failures: 1,
                retry_after_secs: 0,
            })),
            unlock_message(Ok(Unlock::WrongPin {
                failures: 2,
                retry_after_secs: 2,
            })),
            unlock_message(Ok(Unlock::LockedOut {
                retry_after_secs: 5,
            })),
            unlock_message(Err(PinError::Hash)),
        ];
        for message in messages {
            assert!(!message.contains(typed), "{message}");
        }
        assert_eq!(
            unlock_message(Ok(Unlock::WrongPin {
                failures: 2,
                retry_after_secs: 2
            })),
            "Wait 2 seconds before the next attempt."
        );
    }

    #[test]
    fn attempt_file_is_inside_the_profile_folder() {
        let dir = Path::new("profiles").join("kids");
        assert_eq!(attempt_file(&dir), dir.join("parental-lock.json"));
    }

    #[test]
    fn each_profile_counts_its_own_attempts_and_a_missing_file_is_zero() {
        let root = temp_root("per-profile");
        let mut store = ProfileStore::open(root.join("profiles")).expect("store opens");
        store.create("kids").expect("profile is created");
        let kids = attempt_file(&store.profile_dir("kids").expect("kids folder"));
        let default = attempt_file(&store.profile_dir("default").expect("default folder"));

        let failed = AttemptState {
            failures: 3,
            locked_until: Some(900),
        };
        save_attempts(&kids, failed).expect("kids state is written");

        let kids_state = load_attempts(&kids);
        let default_state = load_attempts(&default);
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(kids_state, Ok(failed));
        assert_eq!(default_state, Ok(AttemptState::default()));
    }
}
