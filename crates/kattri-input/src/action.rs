//! The set of things an input can ask the app to do.
//!
//! Player and navigation actions use the same names as the legacy Neo keymaps so
//! that legacy JSON files parse without edits. Host actions (`host:*` in legacy
//! files) are commands for the application shell rather than the player.

use std::fmt;

/// An abstract user intent produced by a key, button or remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // Navigation
    Up,
    Down,
    Left,
    Right,
    Enter,
    Back,
    Exit,
    Home,
    Search,
    /// Open the Settings page. The legacy keymap has no close equivalent, so the name is new.
    Settings,

    // Playback
    PlayPause,
    Pause,
    Stop,
    SeekBackward,
    SeekForward,
    StepBackward,
    StepForward,

    // Tracks and delays
    CycleAudio,
    CycleAudioBack,
    CycleSubtitles,
    CycleSubtitlesBack,
    ToggleSubtitles,
    IncreaseAudioDelay,
    DecreaseAudioDelay,
    IncreaseSubtitleDelay,
    DecreaseSubtitleDelay,
    /// Set the subtitle delay back to zero (M2-17; no legacy name).
    ResetSubtitleDelay,

    // Volume, speed and chapters. `IncreaseVolume` and `DecreaseVolume` keep the legacy
    // names; the rest are spec-only (M2-17) and have no legacy spelling.
    IncreaseVolume,
    DecreaseVolume,
    Mute,
    SpeedUp,
    SpeedDown,
    ChapterNext,
    ChapterPrevious,

    /// Commands for the application shell (window, screenshot, web mode, ...).
    Host(HostAction),
}

/// Application-shell commands. Legacy spelling is `host:<name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostAction {
    Screenshot,
    Minimize,
    Fullscreen,
    ToggleWebMode,
    Quit,
    Close,
    ToggleDebug,
    /// Picture-in-picture on or off (M2-17 action; no legacy spelling).
    TogglePip,
}

/// Every action in the vocabulary, in declaration order. Coverage checks (defaults,
/// keyboard reachability) iterate this list. The exhaustive match in
/// `input_map::legacy_name` stops a new variant from compiling until it has a name.
pub const ALL_ACTIONS: &[Action] = &[
    Action::Up,
    Action::Down,
    Action::Left,
    Action::Right,
    Action::Enter,
    Action::Back,
    Action::Exit,
    Action::Home,
    Action::Search,
    Action::Settings,
    Action::PlayPause,
    Action::Pause,
    Action::Stop,
    Action::SeekBackward,
    Action::SeekForward,
    Action::StepBackward,
    Action::StepForward,
    Action::CycleAudio,
    Action::CycleAudioBack,
    Action::CycleSubtitles,
    Action::CycleSubtitlesBack,
    Action::ToggleSubtitles,
    Action::IncreaseAudioDelay,
    Action::DecreaseAudioDelay,
    Action::IncreaseSubtitleDelay,
    Action::DecreaseSubtitleDelay,
    Action::ResetSubtitleDelay,
    Action::IncreaseVolume,
    Action::DecreaseVolume,
    Action::Mute,
    Action::SpeedUp,
    Action::SpeedDown,
    Action::ChapterNext,
    Action::ChapterPrevious,
    Action::Host(HostAction::Screenshot),
    Action::Host(HostAction::Minimize),
    Action::Host(HostAction::Fullscreen),
    Action::Host(HostAction::ToggleWebMode),
    Action::Host(HostAction::Quit),
    Action::Host(HostAction::Close),
    Action::Host(HostAction::ToggleDebug),
    Action::Host(HostAction::TogglePip),
];

/// Why a legacy action name could not be turned into an [`Action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionParseError {
    /// The name is not one this build knows. Legacy-only names such as `space` land here.
    Unknown(String),
    /// The name uses a feature this build does not implement yet (for example `%1` text input).
    Unsupported(String),
}

impl Action {
    /// Parse a legacy keymap action name such as `play_pause` or `host:screenshot`.
    pub fn from_legacy(name: &str) -> Result<Self, ActionParseError> {
        let action = match name {
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "enter" => Self::Enter,
            "back" => Self::Back,
            "exit" => Self::Exit,
            "home" => Self::Home,
            "search" => Self::Search,
            "settings" => Self::Settings,
            "play_pause" => Self::PlayPause,
            "pause" => Self::Pause,
            "stop" => Self::Stop,
            "seek_backward" => Self::SeekBackward,
            "seek_forward" => Self::SeekForward,
            "step_backward" => Self::StepBackward,
            "step_forward" => Self::StepForward,
            "cycle_audio" => Self::CycleAudio,
            "cycle_audio_back" => Self::CycleAudioBack,
            "cycle_subtitles" => Self::CycleSubtitles,
            "cycle_subtitles_back" => Self::CycleSubtitlesBack,
            "toggle_subtitles" => Self::ToggleSubtitles,
            "increase_audio_delay" => Self::IncreaseAudioDelay,
            "decrease_audio_delay" => Self::DecreaseAudioDelay,
            "increase_subtitles_delay" => Self::IncreaseSubtitleDelay,
            "decrease_subtitles_delay" => Self::DecreaseSubtitleDelay,
            "reset_subtitles_delay" => Self::ResetSubtitleDelay,
            "increase_volume" => Self::IncreaseVolume,
            "decrease_volume" => Self::DecreaseVolume,
            "mute" => Self::Mute,
            "speed_up" => Self::SpeedUp,
            "speed_down" => Self::SpeedDown,
            "chapter_next" => Self::ChapterNext,
            "chapter_previous" => Self::ChapterPrevious,
            "host:screenshot" => Self::Host(HostAction::Screenshot),
            "host:minimize" => Self::Host(HostAction::Minimize),
            "host:fullscreen" => Self::Host(HostAction::Fullscreen),
            "host:toggleWebMode" => Self::Host(HostAction::ToggleWebMode),
            "host:quit" => Self::Host(HostAction::Quit),
            "host:close" => Self::Host(HostAction::Close),
            "host:toggleDebug" => Self::Host(HostAction::ToggleDebug),
            "host:togglePip" => Self::Host(HostAction::TogglePip),
            _ if name.contains("%1") => {
                return Err(ActionParseError::Unsupported(name.to_owned()));
            }
            _ => return Err(ActionParseError::Unknown(name.to_owned())),
        };
        Ok(action)
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_navigation_and_playback_names() {
        assert_eq!(Action::from_legacy("left"), Ok(Action::Left));
        assert_eq!(Action::from_legacy("settings"), Ok(Action::Settings));
        assert_eq!(Action::from_legacy("play_pause"), Ok(Action::PlayPause));
        assert_eq!(
            Action::from_legacy("cycle_subtitles_back"),
            Ok(Action::CycleSubtitlesBack)
        );
    }

    #[test]
    fn parses_host_actions_with_legacy_spelling() {
        assert_eq!(
            Action::from_legacy("host:toggleWebMode"),
            Ok(Action::Host(HostAction::ToggleWebMode))
        );
        assert_eq!(
            Action::from_legacy("host:screenshot"),
            Ok(Action::Host(HostAction::Screenshot))
        );
    }

    #[test]
    fn text_passthrough_is_unsupported_not_unknown() {
        assert_eq!(
            Action::from_legacy("%1"),
            Err(ActionParseError::Unsupported("%1".to_owned()))
        );
        assert_eq!(
            Action::from_legacy("jump+%1"),
            Err(ActionParseError::Unsupported("jump+%1".to_owned()))
        );
    }

    #[test]
    fn unknown_names_are_reported_verbatim() {
        assert_eq!(
            Action::from_legacy("space"),
            Err(ActionParseError::Unknown("space".to_owned()))
        );
    }

    #[test]
    fn parses_each_m2_17_action_name() {
        let cases = [
            ("reset_subtitles_delay", Action::ResetSubtitleDelay),
            ("increase_volume", Action::IncreaseVolume),
            ("decrease_volume", Action::DecreaseVolume),
            ("mute", Action::Mute),
            ("speed_up", Action::SpeedUp),
            ("speed_down", Action::SpeedDown),
            ("chapter_next", Action::ChapterNext),
            ("chapter_previous", Action::ChapterPrevious),
            ("host:togglePip", Action::Host(HostAction::TogglePip)),
        ];
        for (name, action) in cases {
            assert_eq!(Action::from_legacy(name), Ok(action), "{name}");
        }
    }

    #[test]
    fn all_actions_lists_each_action_once() {
        use std::collections::HashSet;
        let unique: HashSet<Action> = ALL_ACTIONS.iter().copied().collect();
        assert_eq!(unique.len(), ALL_ACTIONS.len());
    }
}
