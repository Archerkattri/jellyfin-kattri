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
mod ui;
mod worker;

pub use app::{KattriApp, Page, PlaybackReportSubmitError};
pub use kattri_client_core::playback_reporting::PlaybackReportAction;
pub use ui::{live_tv, music};
pub use worker::PreparedPlayback;
