//! Plugin runtime for Jellyfin Kattri, built on wasmtime 49.0.3.
//!
//! Guest code runs with fuel metering, a linear memory cap, an epoch deadline for
//! wall-clock time and a host-side output size cap. The limit values come from
//! `kattri_plugins::sandbox`. Each plugin gets its own engine, so an epoch tick fired for
//! one plugin cannot interrupt another.
//!
//! The `kattri-plugin` WIT world is bound in [`bindings`]. Host imports are checked against
//! the plugin manifest grants before any effect runs ([`world`]).

pub mod bindings;
pub mod error;
pub mod sandbox;
pub mod world;
