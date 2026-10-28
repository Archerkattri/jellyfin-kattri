mod app;
mod catalog;
#[cfg(debug_assertions)]
pub mod debug_log;
mod device_id;
mod input;
mod parental;
mod saved_servers;
mod settings_state;
mod sign_in;
mod theme;
mod trickplay;
mod ui;
mod worker;

pub use app::{KattriApp, Page, PlaybackReportSubmitError};
pub use kattri_client_core::playback_reporting::PlaybackReportAction;
pub use trickplay::SeekThumbnail;
pub use ui::{live_tv, music};
pub use worker::{DownloadedSubtitle, PreparedPlayback};

/// Label of the player's Search subtitles entry, from the UI catalog.
pub fn search_subtitles_label() -> &'static str {
    ui::ui_text(kattri_i18n::ids::ITEM_MENU_SEARCH_SUBTITLES)
}
