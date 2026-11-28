//! Launch-time profile selection.
//!
//! The desktop app has no local profile switcher: viewers sign in as a Jellyfin user on the
//! server. This module only keeps the settings folder selection (`--profile`) and its notices.

use super::ui_text_args;
use kattri_i18n::ids;

/// The settings profile this run opens, and a notice when it is not the one asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchPlan {
    pub(crate) profile: String,
    pub(crate) notice: Option<String>,
}

/// Choose the profile for this launch.
///
/// - No `--profile`: the index's active profile, opened as-is. No switcher or picker is shown.
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
    LaunchPlan { profile, notice }
}

/// Arguments for the restarted process. The profile name is the only value passed on.
pub(crate) fn restart_args(profile: &str) -> Vec<String> {
    vec!["--profile".to_owned(), profile.to_owned()]
}
