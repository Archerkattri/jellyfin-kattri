//! Parental controls on the General settings page (M4-20).
//!
//! The PIN is a local convenience lock, not a security boundary. Anyone with access to the
//! settings file or the attempt file can reset it, and the page says so. Only the Argon2id
//! record goes to the settings file. The failure count and the lockout end go to
//! `parental-lock.json` in the running profile's folder, so each profile counts its own
//! attempts. The PIN text is never logged, and the input boxes are cleared after each use.
//! The unlock lasts for this run only.
//!
//! The view state and the messages are pure functions, so they are tested without egui.

use super::{ui_text, ui_text_args};
use kattri_i18n::ids;
use std::fs;
use std::path::{Path, PathBuf};

use egui::{Id, RichText, Ui};
use kattri_i18n::ids::PARENTAL_PIN_LIMITATION;
use kattri_parental::{
    AttemptFileError, AttemptState, HashParams, Pin, PinError, PinLock, RATING_LADDER, StoredPin,
    Unlock, delete_attempts, load_attempts, save_attempts,
};
use zeroize::Zeroize;

use crate::app::KattriApp;
use crate::parental::{attempt_file, set_run_unlock, unix_now, unlock_message};
use crate::theme::Palette;
use crate::ui::settings::muted;

const STATE_ID: &str = "kattri_parental_session";

/// What the parental section shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParentalView {
    /// No PIN is stored. The page offers to set one.
    Unset,
    /// A PIN is stored and this run has not unlocked it. `retry_after_secs` is the wait left
    /// after wrong PINs, or 0 when a PIN can be tried now.
    Locked { retry_after_secs: u64 },
    /// A PIN is stored and this run has unlocked it. The PIN can be changed or removed.
    Unlocked,
}

/// Chooses the view. `protected` is whether a PIN is stored, `unlocked` whether this run has
/// entered it. An unlock hides any wait, since the wait only applies to unlock attempts.
pub(crate) fn view_state(protected: bool, unlocked: bool, retry_after_secs: u64) -> ParentalView {
    match (protected, unlocked) {
        (false, _) => ParentalView::Unset,
        (true, true) => ParentalView::Unlocked,
        (true, false) => ParentalView::Locked { retry_after_secs },
    }
}

/// Seconds left before the next unlock attempt is accepted at time `now` (Unix seconds).
pub(crate) fn retry_after_secs(state: AttemptState, now: u64) -> u64 {
    state
        .locked_until
        .map_or(0, |until| until.saturating_sub(now))
}

/// Why the set-PIN form cannot be used yet, or `None` when it can. No PIN text is returned.
pub(crate) fn set_form_problem(new_pin: &str, confirm_pin: &str) -> Option<&'static str> {
    if new_pin != confirm_pin {
        return Some(ui_text(ids::PARENTAL_ERROR_PIN_MISMATCH));
    }
    if Pin::parse(new_pin).is_err() {
        return Some(ui_text(ids::PARENTAL_ERROR_PIN_LENGTH));
    }
    None
}

/// Per-run state of the section. Kept in egui memory, so it is not written to disk.
#[derive(Clone, Default)]
struct ParentalSession {
    loaded: bool,
    limitation: String,
    attempts: AttemptState,
    /// Set when the attempt file could not be read. Unlock stays refused while it is set.
    attempts_error: Option<String>,
    unlocked: bool,
    pin_entry: String,
    new_pin: String,
    confirm_pin: String,
    message: Option<String>,
}

pub(super) fn show(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let ctx = ui.ctx().clone();
    let id = Id::unique(STATE_ID);
    let mut session = ctx
        .data(|data| data.get_temp::<ParentalSession>(id))
        .unwrap_or_default();
    let path = attempt_path(app);
    if !session.loaded {
        session.loaded = true;
        session.limitation = limitation_text();
        if let Some(path) = path.as_deref() {
            match load_attempts(path) {
                Ok(state) => session.attempts = state,
                Err(error) => session.attempts_error = Some(attempt_error_text(&error)),
            }
        }
    }

    let now = unix_now();
    let record = app.settings.settings().parental.pin_record.clone();
    let stored = if record.is_empty() {
        None
    } else {
        StoredPin::from_record(&record).ok()
    };
    let record_damaged = !record.is_empty() && stored.is_none();
    let mut lock = PinLock::restore(stored, session.attempts);
    let view = view_state(
        lock.is_protected(),
        session.unlocked,
        retry_after_secs(session.attempts, now),
    );

    super::settings::settings_card(ui, |ui| {
        super::settings::subsection_heading(ui, ui_text(ids::PARENTAL_HEADING_TITLE), colors);
        muted(ui, colors, &session.limitation, 12.0);
        ui.add_space(8.0);
        if let Some(error) = &session.attempts_error {
            ui.label(RichText::new(error).size(12.0).color(colors.text));
        }
        if record_damaged {
            ui.label(
                RichText::new(ui_text(ids::PARENTAL_ERROR_PIN_DAMAGED))
                    .size(12.0)
                    .color(colors.text),
            );
        }

        match view {
            ParentalView::Unset => {
                ui.label(
                    RichText::new(ui_text(ids::PARENTAL_STATUS_NO_PIN))
                        .size(14.0)
                        .color(colors.text),
                );
                new_pin_form(
                    ui,
                    &mut session,
                    colors,
                    ui_text(ids::PARENTAL_BUTTON_SET_PIN),
                );
                if ui.button(ui_text(ids::PARENTAL_BUTTON_SAVE_PIN)).clicked() {
                    set_pin(app, &mut session, &mut lock, path.as_deref());
                }
            }
            ParentalView::Locked { retry_after_secs } => {
                ui.label(
                    RichText::new(ui_text(ids::PARENTAL_STATUS_PIN_SET))
                        .size(14.0)
                        .color(colors.text),
                );
                if retry_after_secs > 0 {
                    muted(
                        ui,
                        colors,
                        &ui_text_args(
                            ids::PARENTAL_STATUS_WAIT,
                            &[("seconds", &retry_after_secs.to_string())],
                        ),
                        12.0,
                    );
                }
                ui.horizontal(|ui| {
                    let caption = ui.label(ui_text(ids::PARENTAL_LABEL_PIN));
                    ui.add(
                        egui::TextEdit::singleline(&mut session.pin_entry)
                            .password(true)
                            .char_limit(8),
                    )
                    .labelled_by(caption.id);
                    let can_try = retry_after_secs == 0
                        && session.attempts_error.is_none()
                        && !session.pin_entry.is_empty();
                    if ui
                        .add_enabled(
                            can_try,
                            egui::Button::new(ui_text(ids::PARENTAL_BUTTON_UNLOCK)),
                        )
                        .clicked()
                    {
                        try_unlock(&mut session, &mut lock, path.as_deref(), now);
                    }
                });
                if ui
                    .button(ui_text(ids::PARENTAL_BUTTON_FORGOT_PIN))
                    .clicked()
                {
                    remove_pin(app, &mut session, path.as_deref());
                }
                muted(
                    ui,
                    colors,
                    ui_text(ids::PARENTAL_STATUS_LIMIT_AFTER_PIN),
                    12.0,
                );
            }
            ParentalView::Unlocked => {
                ui.label(
                    RichText::new(ui_text(ids::PARENTAL_STATUS_UNLOCKED))
                        .size(14.0)
                        .color(colors.text),
                );
                new_pin_form(
                    ui,
                    &mut session,
                    colors,
                    ui_text(ids::PARENTAL_BUTTON_CHANGE_PIN),
                );
                ui.horizontal(|ui| {
                    if ui
                        .button(ui_text(ids::PARENTAL_BUTTON_SAVE_NEW_PIN))
                        .clicked()
                    {
                        set_pin(app, &mut session, &mut lock, path.as_deref());
                    }
                    if ui
                        .button(ui_text(ids::PARENTAL_BUTTON_REMOVE_PIN))
                        .clicked()
                    {
                        remove_pin(app, &mut session, path.as_deref());
                    }
                    if ui.button(ui_text(ids::PARENTAL_BUTTON_LOCK_NOW)).clicked() {
                        session.unlocked = false;
                    }
                });
            }
        }

        if !matches!(view, ParentalView::Locked { .. }) {
            rating_limit(ui, app, colors);
        }

        if let Some(message) = &session.message {
            ui.add_space(6.0);
            ui.label(RichText::new(message).size(12.0).color(colors.text));
        }
    });
    // The browse filter reads this flag, so it follows the unlock state of this frame.
    let profile = app.settings.running_name().map(str::to_owned);
    set_run_unlock(&ctx, profile.as_deref(), session.unlocked);
    ctx.data_mut(|data| data.insert_temp(id, session));
}

fn new_pin_form(ui: &mut Ui, session: &mut ParentalSession, colors: Palette, title: &str) {
    ui.label(RichText::new(title).size(14.0).strong().color(colors.text));
    ui.horizontal(|ui| {
        let caption = ui.label(ui_text(ids::PARENTAL_LABEL_NEW_PIN));
        ui.add(
            egui::TextEdit::singleline(&mut session.new_pin)
                .password(true)
                .char_limit(8),
        )
        .labelled_by(caption.id);
    });
    ui.horizontal(|ui| {
        let caption = ui.label(ui_text(ids::PARENTAL_LABEL_REPEAT_PIN));
        ui.add(
            egui::TextEdit::singleline(&mut session.confirm_pin)
                .password(true)
                .char_limit(8),
        )
        .labelled_by(caption.id);
    });
    muted(ui, colors, ui_text(ids::PARENTAL_ERROR_PIN_LENGTH), 12.0);
}

/// Sets a new PIN and resets the failure count. Setting a PIN also unlocks this run.
fn set_pin(
    app: &mut KattriApp,
    session: &mut ParentalSession,
    lock: &mut PinLock,
    path: Option<&Path>,
) {
    let outcome = match set_form_problem(&session.new_pin, &session.confirm_pin) {
        Some(problem) => Err(problem.to_owned()),
        None => set_new_pin(lock, &session.new_pin).map_err(|error| error.to_string()),
    };
    session.new_pin.zeroize();
    session.confirm_pin.zeroize();
    match outcome {
        Ok(record) => {
            app.settings.settings_mut().parental.pin_record = record;
            session.attempts = lock.attempt_state();
            session.attempts_error = None;
            session.unlocked = true;
            session.message = Some(match persist(path, session.attempts) {
                Ok(()) => ui_text(ids::PARENTAL_STATUS_SAVED).to_owned(),
                Err(error) => ui_text_args(
                    ids::PARENTAL_ERROR_SAVED_LOCK_NOT_WRITTEN,
                    &[("error", &error.to_string())],
                ),
            });
        }
        Err(problem) => session.message = Some(problem),
    }
}

fn set_new_pin(lock: &mut PinLock, pin_text: &str) -> Result<String, PinError> {
    let pin = Pin::parse(pin_text)?;
    lock.set_pin_random(&pin, HashParams::OWASP_MINIMUM)?;
    lock.stored()
        .map(StoredPin::to_record)
        .ok_or(PinError::Record)
}

fn try_unlock(session: &mut ParentalSession, lock: &mut PinLock, path: Option<&Path>, now: u64) {
    let outcome = lock.try_unlock(&session.pin_entry, now);
    session.pin_entry.zeroize();
    session.attempts = lock.attempt_state();
    let saved = persist(path, session.attempts);
    if matches!(outcome, Ok(Unlock::Unlocked)) {
        session.unlocked = true;
    }
    let mut message = unlock_message(outcome);
    if let Err(error) = saved {
        message.push(' ');
        message.push_str(&ui_text_args(
            ids::PARENTAL_ERROR_LOCK_NOT_WRITTEN,
            &[("error", &error.to_string())],
        ));
    }
    session.message = Some(message);
}

fn remove_pin(app: &mut KattriApp, session: &mut ParentalSession, path: Option<&Path>) {
    app.settings.settings_mut().parental.pin_record.clear();
    session.attempts = AttemptState::default();
    session.attempts_error = None;
    session.unlocked = false;
    session.pin_entry.zeroize();
    session.message = Some(match remove_attempts(path) {
        Ok(()) => ui_text(ids::PARENTAL_STATUS_REMOVED).to_owned(),
        Err(error) => ui_text_args(
            ids::PARENTAL_ERROR_REMOVED_LOCK_NOT_DELETED,
            &[("error", &error.to_string())],
        ),
    });
}

/// The highest rating shown, picked from the shared ladder. The settings file stores the ladder
/// index, so an unset limit stays `None`.
fn rating_limit(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let current = app.settings.settings().parental.max_rating;
    let mut limited = current.is_some();
    let mut level = current
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
        .min(RATING_LADDER.len() - 1);
    ui.add_space(10.0);
    super::settings::subsection_heading(ui, ui_text(ids::PARENTAL_HEADING_RATING_LIMIT), colors);
    ui.checkbox(&mut limited, ui_text(ids::PARENTAL_RATING_LIMIT));
    ui.horizontal(|ui| {
        ui.label(ui_text(ids::PARENTAL_RATING_HIGHEST));
        ui.add_enabled_ui(limited, |ui| {
            let selected = RATING_LADDER
                .get(level)
                .copied()
                .unwrap_or(RATING_LADDER[0]);
            egui::ComboBox::from_id_salt("kattri_parental_rating_limit")
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for (index, name) in RATING_LADDER.iter().enumerate() {
                        ui.selectable_value(&mut level, index, *name);
                    }
                });
        });
    });
    muted(ui, colors, ui_text(ids::PARENTAL_RATING_HELP), 12.0);
    let next = limited.then_some(u32::try_from(level).unwrap_or(u32::MAX));
    if next != current {
        app.settings.settings_mut().parental.max_rating = next;
    }
}

/// The running profile's attempt file, or `None` when no profile store is open. In that case
/// the attempt count lasts only for this run.
fn attempt_path(app: &mut KattriApp) -> Option<PathBuf> {
    let name = app.settings.running_name()?.to_owned();
    let store = app.settings.profile_store_mut()?;
    store.profile_dir(&name).ok().map(|dir| attempt_file(&dir))
}

fn persist(path: Option<&Path>, state: AttemptState) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| ui_text(ids::PARENTAL_ERROR_FOLDER).to_owned())?;
    }
    save_attempts(path, state).map_err(|error| error.to_string())
}

fn remove_attempts(path: Option<&Path>) -> Result<(), AttemptFileError> {
    match path {
        Some(path) => delete_attempts(path),
        None => Ok(()),
    }
}

fn attempt_error_text(error: &AttemptFileError) -> String {
    ui_text_args(
        ids::PARENTAL_ERROR_LOCK_READ,
        &[("error", &error.to_string())],
    )
}

fn limitation_text() -> String {
    ui_text(PARENTAL_PIN_LIMITATION).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fast cost settings for tests only. The shipped code uses `HashParams::OWASP_MINIMUM`.
    fn test_params() -> HashParams {
        HashParams::new(8, 1, 1).expect("test parameters are valid")
    }

    fn locked_lock(pin: &str) -> PinLock {
        let mut lock = PinLock::new(None);
        let pin = Pin::parse(pin).expect("test PIN is valid");
        lock.set_pin(&pin, test_params(), [7u8; 16])
            .expect("test PIN hashes");
        lock
    }

    #[test]
    fn view_is_unset_without_a_stored_pin_whatever_the_unlock_flag() {
        assert_eq!(view_state(false, false, 0), ParentalView::Unset);
        assert_eq!(view_state(false, true, 9), ParentalView::Unset);
    }

    #[test]
    fn view_is_locked_with_the_wait_when_a_pin_is_stored_and_not_unlocked() {
        assert_eq!(
            view_state(true, false, 0),
            ParentalView::Locked {
                retry_after_secs: 0
            }
        );
        assert_eq!(
            view_state(true, false, 7),
            ParentalView::Locked {
                retry_after_secs: 7
            }
        );
    }

    #[test]
    fn view_is_unlocked_after_the_pin_is_entered_and_hides_any_wait() {
        assert_eq!(view_state(true, true, 0), ParentalView::Unlocked);
        assert_eq!(view_state(true, true, 30), ParentalView::Unlocked);
    }

    #[test]
    fn wait_counts_down_to_zero_and_never_goes_negative() {
        let state = AttemptState {
            failures: 2,
            locked_until: Some(100),
        };
        assert_eq!(retry_after_secs(state, 90), 10);
        assert_eq!(retry_after_secs(state, 100), 0);
        assert_eq!(retry_after_secs(state, 150), 0);
        assert_eq!(retry_after_secs(AttemptState::default(), 5), 0);
    }

    #[test]
    fn set_form_rejects_mismatch_bad_length_and_non_digits() {
        assert_eq!(
            set_form_problem("1234", "4321"),
            Some("The two PIN entries do not match.")
        );
        assert_eq!(
            set_form_problem("123", "123"),
            Some("A PIN is 4 to 8 digits.")
        );
        assert_eq!(
            set_form_problem("12a4", "12a4"),
            Some("A PIN is 4 to 8 digits.")
        );
        assert_eq!(set_form_problem("1234", "1234"), None);
    }

    #[test]
    fn a_wrong_pin_starts_a_wait_and_the_right_pin_then_unlocks() {
        let mut lock = locked_lock("1234");
        let wrong = lock.try_unlock("0000", 1_000).expect("check runs");
        assert_eq!(
            wrong,
            Unlock::WrongPin {
                failures: 1,
                retry_after_secs: 1
            }
        );
        let view = view_state(
            lock.is_protected(),
            false,
            retry_after_secs(lock.attempt_state(), 1_000),
        );
        assert_eq!(
            view,
            ParentalView::Locked {
                retry_after_secs: 1
            }
        );
        assert_eq!(
            lock.try_unlock("1234", 1_000).expect("check runs"),
            Unlock::LockedOut {
                retry_after_secs: 1
            },
            "the right PIN is refused during the wait"
        );
        assert_eq!(
            lock.try_unlock("1234", 1_002).expect("check runs"),
            Unlock::Unlocked
        );
        assert_eq!(lock.failures(), 0);
    }

    #[test]
    fn limitation_text_comes_from_the_en_us_catalog() {
        let text = limitation_text();
        assert_eq!(
            text,
            "Local convenience lock, not a security control. Anyone with access to these files can reset it."
        );
        assert!(!text.contains(PARENTAL_PIN_LIMITATION));
    }
}
