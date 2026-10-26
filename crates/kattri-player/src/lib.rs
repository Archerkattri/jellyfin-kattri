//! Playback engine facade: the contract between the app and a media engine.
//!
//! Status:
//! - Implemented and tested: the track model and track cycling rule
//!   ([`track::next_selection`]) and the command/event vocabulary.
//! - Skeleton: the real engine. It is NOT implemented here yet.
//!
//! Plan: the real libmpv implementation moves from `proofs/mpv-render` into a
//! `kattri-player-mpv` crate that implements [`PlayerBackend`]. That move happens
//! under M1 (direct play) and M6 (HDR and advanced features). The proof code stays
//! in `proofs/` until then, and this crate is the only surface the app talks to.
//!
//! Rules:
//! - [`PlayerCommand::Load`] takes an opaque [`SourceHandle`], never an upstream
//!   Jellyfin URL or token. The playback relay issues handles. The player never sees credentials.
//! - This crate has no UI, OS or Jellyfin-API dependency.

pub mod capabilities;
pub mod command;
pub mod track;

pub use capabilities::{
    CapabilityCache, CapabilityCacheKey, ProbeValue, ProbeWarning, RuntimeCapabilitySnapshot,
    RuntimeIdentity, cache_runtime_capabilities, cached_runtime_capabilities,
};
pub use command::{
    PlayerBackend, PlayerCommand, PlayerError, PlayerEvent, RecordingPlayer, SourceHandle,
};
pub use track::{Selection, Track, TrackId, TrackKind, next_selection};
