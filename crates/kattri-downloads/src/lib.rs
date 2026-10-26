//! Offline download core: filesystem lifecycle, durable writes, the HTTP driver
//! that drives `kattri_client_core::downloads::DownloadJob`, checksum verification,
//! manifests and subtitle child jobs.
//!
//! Modules:
//! - [`fs`]: temp file lifecycle and durable chunk writes.
//! - [`driver`]: HTTP driver; every state change goes through `DownloadJob`.
//! - [`messages`]: user text for transitions, failures and notices.

pub mod driver;
pub mod fs;
pub mod manager;
pub mod manifest;
pub mod messages;
pub mod persist;
pub mod subtitles;
pub mod verify;
