//! Presence text for one playback state.
//!
//! The title is shown only when the user turns [`PresenceOptions::show_title`] on. With it
//! off, the details line is the generic [`GENERIC_DETAILS`] and the state line carries only
//! the play state and the position, so no item identity leaves the machine.

use crate::discord::{MAX_PRESENCE_CHARS, PresenceActivity, check_presence_text};

/// Details line used when the title is hidden or refused by the presence text rules.
pub const GENERIC_DETAILS: &str = "Watching";

/// What the plugin knows about the current item and position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackState {
    pub title: String,
    pub paused: bool,
    pub position_ms: u64,
}

/// User choices that change what the presence shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PresenceOptions {
    /// Off by default. The title is shared only after the user turns it on.
    pub show_title: bool,
}

/// The activity shown for one playback state.
pub fn activity_for(state: &PlaybackState, options: PresenceOptions) -> PresenceActivity {
    let verb = if state.paused { "Paused" } else { "Playing" };
    PresenceActivity {
        state: Some(format!("{verb} {}", format_position(state.position_ms))),
        details: Some(details_for(&state.title, options)),
        large_image_key: None,
    }
}

fn details_for(title: &str, options: PresenceOptions) -> String {
    if !options.show_title {
        return GENERIC_DETAILS.to_owned();
    }
    // A refused title (URL, token, control characters) falls back to the generic line.
    // Refusing it must not stop the update, or playback reporting would stall on one
    // odd item name.
    let clipped: String = title.chars().take(MAX_PRESENCE_CHARS).collect();
    match check_presence_text("discord.details", &clipped) {
        Ok(()) => clipped,
        Err(_) => GENERIC_DETAILS.to_owned(),
    }
}

/// Clock text for a position: `m:ss`, or `h:mm:ss` from one hour on.
pub fn format_position(position_ms: u64) -> String {
    let total = position_ms / 1000;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
