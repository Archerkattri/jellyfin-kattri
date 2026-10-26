//! Profile picker (M2-13): lists the profiles, switches to one, and creates one.
//!
//! The choice logic is pure and unit-tested here. Switching is restart-based: the new active
//! profile is written to the index first, then the app starts again with `--profile <name>`.
//! The relaunch step is passed in, so a test can check the order without starting a process.
//! The picker is drawn in its own window, so it opens from the Settings page and at launch.
//!
//! A profile with a PIN is not switched to until its PIN is entered here. The PIN check uses the
//! target profile's own record and attempt file, so its wrong-PIN delay is its own.

use super::{ui_text, ui_text_args};
use kattri_i18n::ids;
use std::fmt;
use std::io;
use std::path::PathBuf;

use egui::{Align2, Button, Context, Ui, Vec2, Window};
use kattri_parental::{PinLock, StoredPin, load_attempts, save_attempts};
use kattri_settings::{ProfileError, ProfileStore, validate_profile_name};
use zeroize::Zeroize;

use crate::parental::{GateStep, attempt_file, gate_step, unix_now};

/// One line of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickerRow {
    pub(crate) name: String,
    pub(crate) is_active: bool,
    pub(crate) is_default: bool,
}

/// The rows the picker shows, in the store's creation order, with the active and default marks.
/// `active` is the profile this run uses, which can differ from the index after `--profile`.
pub(crate) fn picker_rows(names: &[String], active: &str, default: &str) -> Vec<PickerRow> {
    names
        .iter()
        .map(|name| PickerRow {
            name: name.clone(),
            is_active: name == active,
            is_default: name == default,
        })
        .collect()
}

/// What "Switch" does for a row. Choosing the active profile changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitchPlan {
    NoChange,
    Restart { profile: String },
}

pub(crate) fn switch_plan(active: &str, selected: &str) -> SwitchPlan {
    if selected == active {
        SwitchPlan::NoChange
    } else {
        SwitchPlan::Restart {
            profile: selected.to_owned(),
        }
    }
}

/// Check a typed name before it is created. Names are compared case-insensitively for
/// duplicates, as the profile store does. The message is shown to the user as it is.
pub(crate) fn check_new_name(names: &[String], candidate: &str) -> Result<(), String> {
    validate_profile_name(candidate).map_err(|err| err.to_string())?;
    if names
        .iter()
        .any(|listed| listed.eq_ignore_ascii_case(candidate))
    {
        return Err(ui_text_args(
            ids::PROFILE_ERROR_DUPLICATE,
            &[("candidate", candidate)],
        ));
    }
    Ok(())
}

/// The profile this run opens, a notice when it is not the one asked for, and whether the
/// launch picker should be offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchPlan {
    pub(crate) profile: String,
    pub(crate) notice: Option<String>,
    pub(crate) show_picker: bool,
}

/// Choose the profile for this launch.
///
/// - No `--profile`: the index's active profile. The picker is offered when several exist.
/// - `--profile <name>` for a listed profile: that profile, with no picker and no index change.
/// - An unknown name falls back to the active profile with a notice, so a typo never blocks
///   startup.
/// - A chosen profile whose folder is missing falls back to `default` with a notice (M2-13).
pub(crate) fn plan_launch(
    names: &[String],
    active: &str,
    default: &str,
    requested: Option<&str>,
    folder_exists: impl Fn(&str) -> bool,
) -> LaunchPlan {
    let show_picker = launch_shows_picker(names.len(), requested);
    let (mut profile, mut notice) = match requested {
        None => (active.to_owned(), None),
        Some(name) if names.iter().any(|listed| listed == name) => (name.to_owned(), None),
        Some(name) => (
            active.to_owned(),
            Some(ui_text_args(
                ids::PROFILE_NOTICE_MISSING,
                &[("name", name), ("active", active)],
            )),
        ),
    };
    if !folder_exists(&profile) {
        notice = Some(ui_text_args(
            ids::PROFILE_NOTICE_NO_FOLDER,
            &[("profile", profile.as_str()), ("default", default)],
        ));
        profile = default.to_owned();
    }
    LaunchPlan {
        profile,
        notice,
        show_picker,
    }
}

/// The picker is shown at launch when more than one profile exists and no `--profile` is given.
pub(crate) fn launch_shows_picker(profile_count: usize, requested: Option<&str>) -> bool {
    profile_count > 1 && requested.is_none()
}

/// Arguments for the restarted process. The profile name is the only value passed on.
pub(crate) fn restart_args(profile: &str) -> Vec<String> {
    vec!["--profile".to_owned(), profile.to_owned()]
}

/// Why a switch did not complete.
#[derive(Debug)]
pub(crate) enum SwitchError {
    /// The index could not be written. Nothing was relaunched and the pointer is unchanged.
    Profile(ProfileError),
    /// The index was written, but the new process could not be started. The next launch
    /// uses the new profile.
    Relaunch(io::Error),
}

impl fmt::Display for SwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Profile(err) => f.write_str(&ui_text_args(
                ids::PROFILE_ERROR_SWITCH,
                &[("err", &err.to_string())],
            )),
            Self::Relaunch(err) => f.write_str(&ui_text_args(
                ids::PROFILE_ERROR_RELAUNCH,
                &[("err", &err.to_string())],
            )),
        }
    }
}

impl std::error::Error for SwitchError {}

/// Persist the switch, then start the app again through `relaunch`. `relaunch` runs only when
/// the index write succeeded, so the new process always finds the new profile active.
pub(crate) fn switch_and_restart<F>(
    store: &mut ProfileStore,
    profile: &str,
    relaunch: F,
) -> Result<(), SwitchError>
where
    F: FnOnce(&[String]) -> io::Result<()>,
{
    store.set_active(profile).map_err(SwitchError::Profile)?;
    relaunch(&restart_args(profile)).map_err(SwitchError::Relaunch)
}

/// Start the current executable with `args`. The caller exits once this returns `Ok`.
pub(crate) fn relaunch_current_exe(args: &[String]) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe).args(args).spawn()?;
    Ok(())
}

/// Text typed into the create field and any message about the last action.
#[derive(Debug, Default)]
pub(crate) struct PickerState {
    pub(crate) new_name: String,
    pub(crate) error: Option<String>,
    /// A switch that waits for the target profile's PIN. `None` when no switch is waiting.
    pub(crate) gate: Option<SwitchGate>,
}

/// A switch to a profile that has a PIN. The switch goes ahead only after the PIN matches.
pub(crate) struct SwitchGate {
    pub(crate) target: String,
    pub(crate) pin_entry: String,
    pub(crate) message: Option<String>,
    lock: PinLock,
    attempts_path: PathBuf,
}

impl SwitchGate {
    /// Opens the gate for `target`. `Ok(None)` means the profile has no PIN, so the switch can
    /// go ahead. An error refuses the switch: a PIN or lock state that cannot be read is not
    /// the same as no PIN.
    pub(crate) fn open(store: &ProfileStore, target: &str) -> Result<Option<Self>, String> {
        let (settings, _) = store.load_settings(target).map_err(|error| {
            ui_text_args(
                ids::PROFILE_ERROR_READ,
                &[("target", target), ("error", &error.to_string())],
            )
        })?;
        let record = settings.parental.pin_record;
        if record.is_empty() {
            return Ok(None);
        }
        let stored = StoredPin::from_record(&record)
            .map_err(|_| ui_text_args(ids::PROFILE_ERROR_PIN_DAMAGED, &[("target", target)]))?;
        let dir = store.profile_dir(target).map_err(|error| {
            ui_text_args(
                ids::PROFILE_ERROR_NO_FOLDER,
                &[("target", target), ("error", &error.to_string())],
            )
        })?;
        let attempts_path = attempt_file(&dir);
        let state = load_attempts(&attempts_path).map_err(|error| {
            ui_text_args(
                ids::PROFILE_ERROR_LOCK_STATE,
                &[("target", target), ("error", &error.to_string())],
            )
        })?;
        Ok(Some(Self {
            target: target.to_owned(),
            pin_entry: String::new(),
            message: None,
            lock: PinLock::restore(Some(stored), state),
            attempts_path,
        }))
    }

    /// Checks the typed PIN at `now`, clears the entry, and saves the attempt state.
    pub(crate) fn submit(&mut self, now: u64) -> GateStep {
        let outcome = self.lock.try_unlock(&self.pin_entry, now);
        self.pin_entry.zeroize();
        let saved = save_attempts(&self.attempts_path, self.lock.attempt_state())
            .map_err(|error| error.to_string());
        gate_step(outcome, saved)
    }
}

impl Drop for SwitchGate {
    fn drop(&mut self) {
        self.pin_entry.zeroize();
    }
}

impl fmt::Debug for SwitchGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The typed PIN is left out on purpose.
        f.debug_struct("SwitchGate")
            .field("target", &self.target)
            .field("lock", &self.lock)
            .finish()
    }
}

/// What the user asked for in one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickerOutcome {
    SwitchTo(String),
    Created(String),
}

/// Draw the picker and return the action the user took this frame, if any. `running` is the
/// profile this run uses.
pub(crate) fn show(
    ui: &mut Ui,
    state: &mut PickerState,
    store: &mut ProfileStore,
    running: &str,
) -> Option<PickerOutcome> {
    let mut outcome = None;
    let rows = picker_rows(store.names(), running, store.default_profile());

    ui.heading(
        egui::RichText::new(ui_text(ids::PROFILE_HEADING_PROFILES))
            .size(28.0)
            .strong(),
    );
    ui.add_space(4.0);
    for row in &rows {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                let mut label = row.name.clone();
                if row.is_active {
                    label.push(' ');
                    label.push_str(ui_text(ids::PROFILE_STATE_ACTIVE));
                }
                if row.is_default {
                    label.push(' ');
                    label.push_str(ui_text(ids::PROFILE_STATE_DEFAULT));
                }
                ui.label(label);
                let can_switch = switch_plan(running, &row.name) != SwitchPlan::NoChange;
                // Every row has a Switch button, so each one names its profile.
                let switch =
                    ui.add_enabled(can_switch, Button::new(ui_text(ids::PROFILE_BUTTON_SWITCH)));
                switch.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::Button,
                        can_switch,
                        ui_text_args(
                            ids::PROFILE_BUTTON_SWITCH_TO,
                            &[("name", row.name.as_str())],
                        ),
                    )
                });
                if switch.clicked() {
                    match SwitchGate::open(store, &row.name) {
                        Ok(None) => outcome = Some(PickerOutcome::SwitchTo(row.name.clone())),
                        Ok(Some(gate)) => {
                            state.gate = Some(gate);
                            state.error = None;
                        }
                        Err(message) => state.error = Some(message),
                    }
                }
            });
        });
    }

    let clicks = state.gate.as_mut().map(|gate| draw_gate(ui, gate));
    match clicks {
        Some((_, true)) => state.gate = None,
        Some((true, false)) => {
            if let Some(gate) = state.gate.as_mut() {
                match gate.submit(unix_now()) {
                    GateStep::Switch => {
                        outcome = Some(PickerOutcome::SwitchTo(gate.target.clone()));
                        state.gate = None;
                    }
                    GateStep::Stay(message) => gate.message = Some(message),
                }
            }
        }
        _ => {}
    }

    ui.separator();
    ui.horizontal_wrapped(|ui| {
        let caption = ui.label(ui_text(ids::PROFILE_LABEL_NEW));
        ui.text_edit_singleline(&mut state.new_name)
            .labelled_by(caption.id);
        if ui.button(ui_text(ids::PROFILE_BUTTON_CREATE)).clicked() {
            let candidate = state.new_name.trim().to_owned();
            let created = check_new_name(store.names(), &candidate)
                .and_then(|()| store.create(&candidate).map_err(|err| err.to_string()));
            match created {
                Ok(()) => {
                    state.new_name.clear();
                    state.error = None;
                    outcome = Some(PickerOutcome::Created(candidate));
                }
                Err(message) => state.error = Some(message),
            }
        }
    });
    if let Some(message) = &state.error {
        ui.label(message.as_str());
    }

    outcome
}

/// Draws the PIN prompt of a waiting switch. Returns whether Unlock and Cancel were clicked.
fn draw_gate(ui: &mut Ui, gate: &mut SwitchGate) -> (bool, bool) {
    let mut submit = false;
    let mut cancel = false;
    ui.separator();
    ui.label(ui_text_args(
        ids::PROFILE_PIN_PROMPT,
        &[("target", gate.target.as_str())],
    ));
    ui.horizontal_wrapped(|ui| {
        let caption = ui.label(ui_text(ids::PROFILE_LABEL_PIN));
        ui.add(
            egui::TextEdit::singleline(&mut gate.pin_entry)
                .password(true)
                .char_limit(8),
        )
        .labelled_by(caption.id);
        let ready = !gate.pin_entry.is_empty();
        submit = ui
            .add_enabled(ready, Button::new(ui_text(ids::PROFILE_BUTTON_UNLOCK)))
            .clicked();
        cancel = ui.button(ui_text(ids::PROFILE_BUTTON_CANCEL)).clicked();
    });
    if let Some(message) = &gate.message {
        ui.label(message.as_str());
    }
    (submit, cancel)
}

/// Draw the picker in a window while `open` holds a state. Closing the window clears `open`
/// and leaves the running profile unchanged.
pub(crate) fn show_window(
    ctx: &Context,
    open: &mut Option<PickerState>,
    store: &mut ProfileStore,
    running: &str,
) -> Option<PickerOutcome> {
    let mut keep_open = open.is_some();
    let mut outcome = None;
    if let Some(state) = open.as_mut() {
        Window::new(ui_text(ids::PROFILE_HEADING_PROFILES))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .collapsible(false)
            .resizable(true)
            .open(&mut keep_open)
            .show(ctx, |ui| {
                outcome = show(ui, state, store, running);
            });
    }
    if !keep_open {
        *open = None;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kattri-profile-picker-{label}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn rows_mark_active_and_default_in_creation_order() {
        let rows = picker_rows(&names(&["default", "kids", "work"]), "kids", "default");
        assert_eq!(
            rows,
            [
                PickerRow {
                    name: "default".to_owned(),
                    is_active: false,
                    is_default: true,
                },
                PickerRow {
                    name: "kids".to_owned(),
                    is_active: true,
                    is_default: false,
                },
                PickerRow {
                    name: "work".to_owned(),
                    is_active: false,
                    is_default: false,
                },
            ]
        );
    }

    #[test]
    fn switching_to_the_active_profile_changes_nothing() {
        assert_eq!(switch_plan("kids", "kids"), SwitchPlan::NoChange);
        assert_eq!(
            switch_plan("kids", "work"),
            SwitchPlan::Restart {
                profile: "work".to_owned()
            }
        );
    }

    #[test]
    fn typed_names_are_checked_before_creation() {
        let listed = names(&["default", "kids"]);
        for bad in ["", "Kids", "my kid", "a/b", "con", "deleted", "kid_1"] {
            assert!(
                check_new_name(&listed, bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(check_new_name(&listed, "kids-2").is_ok());
    }

    #[test]
    fn duplicate_name_is_refused_with_the_existing_name_in_the_message() {
        let listed = names(&["default", "kids"]);
        let message = check_new_name(&listed, "kids").unwrap_err();
        assert_eq!(message, "a profile named `kids` already exists");
    }

    #[test]
    fn launch_without_a_flag_opens_the_active_profile_silently() {
        let plan = plan_launch(&names(&["default"]), "default", "default", None, |_| true);
        assert_eq!(
            plan,
            LaunchPlan {
                profile: "default".to_owned(),
                notice: None,
                show_picker: false,
            }
        );
    }

    #[test]
    fn launch_with_a_known_flag_opens_that_profile_without_a_picker() {
        let plan = plan_launch(
            &names(&["default", "kids"]),
            "default",
            "default",
            Some("kids"),
            |_| true,
        );
        assert_eq!(plan.profile, "kids");
        assert_eq!(plan.notice, None);
        assert!(!plan.show_picker);
    }

    #[test]
    fn launch_with_an_unknown_flag_falls_back_to_the_active_profile() {
        let plan = plan_launch(
            &names(&["default", "kids"]),
            "kids",
            "default",
            Some("gone"),
            |_| true,
        );
        assert_eq!(plan.profile, "kids");
        assert_eq!(
            plan.notice.as_deref(),
            Some("profile `gone` does not exist; using `kids`")
        );
    }

    #[test]
    fn launch_with_a_missing_folder_falls_back_to_default_with_a_notice() {
        let plan = plan_launch(
            &names(&["default", "kids"]),
            "kids",
            "default",
            None,
            |name| name != "kids",
        );
        assert_eq!(plan.profile, "default");
        assert_eq!(
            plan.notice.as_deref(),
            Some("profile `kids` has no folder; using `default`")
        );
    }

    #[test]
    fn launch_with_a_flag_for_a_missing_folder_also_falls_back_to_default() {
        let plan = plan_launch(
            &names(&["default", "kids"]),
            "default",
            "default",
            Some("kids"),
            |name| name != "kids",
        );
        assert_eq!(plan.profile, "default");
        assert!(plan.notice.is_some());
    }

    #[test]
    fn picker_shows_at_launch_only_with_several_profiles_and_no_flag() {
        assert!(!launch_shows_picker(1, None));
        assert!(launch_shows_picker(2, None));
        assert!(!launch_shows_picker(2, Some("kids")));
    }

    #[test]
    fn restart_passes_the_profile_as_a_flag() {
        assert_eq!(restart_args("kids"), ["--profile", "kids"]);
    }

    #[test]
    fn switch_writes_the_index_before_the_relaunch_runs() {
        let root = temp_dir("order");
        let mut store = ProfileStore::open(root.join("profiles")).unwrap();
        store.create("kids").unwrap();

        let mut seen: Option<(Vec<String>, String)> = None;
        let profiles = root.join("profiles");
        let result = switch_and_restart(&mut store, "kids", |args| {
            // The new process would open the store here; the pointer must already be on disk.
            let on_disk = ProfileStore::open(&profiles).unwrap();
            seen = Some((args.to_vec(), on_disk.active_profile().to_owned()));
            Ok(())
        });
        let _ = std::fs::remove_dir_all(&root);

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            seen,
            Some((
                vec!["--profile".to_owned(), "kids".to_owned()],
                "kids".to_owned()
            ))
        );
    }

    #[test]
    fn failed_index_write_does_not_relaunch() {
        let root = temp_dir("no-relaunch");
        let profiles = root.join("profiles");
        let mut store = ProfileStore::open(&profiles).unwrap();
        store.create("kids").unwrap();
        // A directory at the index path makes the write fail.
        let index = profiles.join("index.toml");
        std::fs::remove_file(&index).unwrap();
        std::fs::create_dir(&index).unwrap();

        let mut relaunched = false;
        let result = switch_and_restart(&mut store, "kids", |_| {
            relaunched = true;
            Ok(())
        });
        let _ = std::fs::remove_dir_all(&root);

        assert!(matches!(result, Err(SwitchError::Profile(_))), "{result:?}");
        assert!(!relaunched);
        assert_eq!(store.active_profile(), "default");
    }

    #[test]
    fn relaunch_failure_is_reported_after_the_index_is_written() {
        let root = temp_dir("relaunch-fails");
        let profiles = root.join("profiles");
        let mut store = ProfileStore::open(&profiles).unwrap();
        store.create("kids").unwrap();

        let result = switch_and_restart(&mut store, "kids", |_| {
            Err(io::Error::other("no such executable"))
        });
        let reopened = ProfileStore::open(&profiles).unwrap();
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            matches!(result, Err(SwitchError::Relaunch(_))),
            "{result:?}"
        );
        assert_eq!(reopened.active_profile(), "kids");
    }

    /// A store whose `kids` profile has `pin` set. The record uses cheap test parameters.
    fn store_with_protected_kids(label: &str, pin: &str) -> (PathBuf, ProfileStore) {
        let root = temp_dir(label);
        let mut store = ProfileStore::open(root.join("profiles")).unwrap();
        store.create("kids").unwrap();
        let mut lock = PinLock::new(None);
        let pin = kattri_parental::Pin::parse(pin).unwrap();
        let params = kattri_parental::HashParams::new(8, 1, 1).unwrap();
        lock.set_pin(&pin, params, [7u8; 16]).unwrap();
        let mut settings = kattri_settings::Settings::default();
        settings.parental.pin_record = lock.stored().map(StoredPin::to_record).unwrap();
        store.save_settings("kids", &settings).unwrap();
        (root, store)
    }

    #[test]
    fn a_profile_without_a_pin_switches_without_a_gate() {
        let (root, store) = store_with_protected_kids("no-pin", "4821");
        let open = SwitchGate::open(&store, "default");
        let _ = std::fs::remove_dir_all(&root);
        assert!(matches!(open, Ok(None)), "{open:?}");
    }

    #[test]
    fn a_profile_with_a_pin_switches_only_after_the_pin_matches() {
        let (root, store) = store_with_protected_kids("gate", "4821");
        let kids_dir = store.profile_dir("kids").unwrap();
        let default_dir = store.profile_dir("default").unwrap();

        let mut gate = SwitchGate::open(&store, "kids").unwrap().unwrap();
        assert_eq!(gate.target, "kids");
        gate.pin_entry = "0000".to_owned();
        assert_eq!(
            gate.submit(1_000),
            GateStep::Stay("Wait 1 seconds before the next attempt.".to_owned())
        );
        assert_eq!(
            load_attempts(&attempt_file(&kids_dir)).unwrap().failures,
            1,
            "the wrong PIN is counted in the kids folder"
        );
        assert!(!attempt_file(&default_dir).exists());

        // A fresh gate reads the saved delay, so the right PIN is refused during the wait.
        let mut again = SwitchGate::open(&store, "kids").unwrap().unwrap();
        again.pin_entry = "4821".to_owned();
        assert_eq!(
            again.submit(1_000),
            GateStep::Stay("Wait 1 seconds before the next attempt.".to_owned())
        );
        again.pin_entry = "4821".to_owned();
        assert_eq!(again.submit(1_001), GateStep::Switch);
        assert!(again.pin_entry.is_empty());
        let cleared = load_attempts(&attempt_file(&kids_dir)).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(cleared, kattri_parental::AttemptState::default());
    }
}
