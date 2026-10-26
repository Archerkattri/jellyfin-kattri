//! A bounded libmpv FFI and OpenGL render-lifecycle proof for an egui_glow host.
//!
//! The crate dynamically loads libmpv, so compilation does not require mpv
//! headers, pkg-config, or an installed libmpv. `GlContextProvider` is the
//! explicit host boundary: the host must supply a current context, preserve
//! egui's GL state, and keep any native display handle alive through renderer
//! shutdown.

mod capabilities;
mod contract;
mod error;
mod ffi;
mod library;
pub mod playback_policy;
mod render;
mod screenshot_capture;
mod session;
mod stream_info;
mod subtitle_sync;
mod track;

pub use capabilities::{CapabilitySnapshot, CapabilityUnavailable, CapabilityValue};
pub use contract::{
    BackendSelection, DecodeInteropEvidence, DecodeInteropResult, DecodeSurfacePath,
    DisplayOutputStatus, GlImplementation, HostPlatform, LinuxGpuVendor, NativeDisplay,
    NativeDisplayKind, RenderContract, SurfacePresentation,
};
pub use error::MpvError;
pub use kattri_client_core::subtitle_sync::CalibrationMarker;
pub use library::MpvLibrary;
pub use playback_policy::ChapterStep;
pub use render::{
    CurrentGlContext, FboTarget, FrameDecision, GlContextProvider, GlStateRestore, MpvRenderer,
    RenderFrameInfo, RepaintHandle, RepaintNotifier,
};
pub use screenshot_capture::{PROVISIONAL_SCREENSHOT_MODE, ScreenshotError};
pub use session::{
    Chapter, ChapterListError, CommandId, EndFileReason, EventKind, MpvEvent, MpvSession,
    ObservationId, PlaybackControlError, PlaybackProperty, PlaybackPropertyChange,
    PlaybackPropertyValue, PlaybackSpeedStep, SubtitleDelayError, SubtitleSelectionError,
};
pub use stream_info::{StreamSnapshot, available_text};
pub use subtitle_sync::{SubtitleSyncController, SubtitleSyncFailure, SubtitleSyncUpdate};
pub use track::{
    MpvTrack, MpvTrackId, TrackInventory, TrackInventoryError, TrackKind, TrackSelection,
    TrackSelectionError, TrackSelectionState,
};
