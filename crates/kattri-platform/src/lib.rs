//! Trait boundary for operating-system services: credential storage, app
//! directories, media keys, sleep inhibition, window control and notifications.
//!
//! Status: the traits, the value types and the in-memory test doubles in [`memory`] are
//! implemented and tested. [`OsCredentialStore`] is the one OS backend so far (Windows
//! Credential Manager, macOS Keychain, Linux Secret Service). Window control, media keys and
//! the other services have no OS backend yet.
//!
//! Layering rules:
//! - UI and app crates depend on these traits only, never on a concrete OS backend.
//! - Each OS backend lives in its own crate (for example `kattri-platform-windows`)
//!   and is selected at startup. Nothing here names an OS.
//! - Secrets are carried in [`Secret`], which redacts `Debug` and zeroes its buffer on drop.
//!   Credentials must never be written to the settings file (`kattri-settings`).

pub mod memory;
mod os_credentials;

pub use os_credentials::OsCredentialStore;

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

/// Failure from a platform service.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    /// This service has no implementation on the current OS yet.
    #[error("{0} is not supported on this platform")]
    Unsupported(&'static str),
    /// The requested item does not exist.
    #[error("requested item was not found")]
    NotFound,
    /// The user or OS refused the request.
    #[error("request was denied: {0}")]
    Denied(String),
    /// The backend failed for another reason. The message must not contain secrets.
    #[error("platform call failed: {0}")]
    Failed(String),
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// Byte buffer for a credential or token. Redacted in `Debug`, zeroed on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Borrow the secret bytes. Keep the borrow short and never log the result.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Identifies one stored credential, for example service `jellyfin-kattri` and
/// account `<server-id>/<user-id>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialKey {
    pub service: String,
    pub account: String,
}

/// Secure storage for access tokens (OS keychain or credential manager in production).
pub trait CredentialStore {
    /// Fetch a stored secret. `Ok(None)` means nothing is stored under this key.
    fn get(&self, key: &CredentialKey) -> Result<Option<Secret>, PlatformError>;
    /// Store or replace a secret.
    fn set(&mut self, key: &CredentialKey, secret: Secret) -> Result<(), PlatformError>;
    /// Remove a secret. Returns whether one existed.
    fn delete(&mut self, key: &CredentialKey) -> Result<bool, PlatformError>;
}

// ---------------------------------------------------------------------------
// Application directories
// ---------------------------------------------------------------------------

/// Locations for config, data, cache, logs and screenshots.
pub trait AppDirs {
    fn config_dir(&self) -> PathBuf;
    fn data_dir(&self) -> PathBuf;
    fn cache_dir(&self) -> PathBuf;
    fn log_dir(&self) -> PathBuf;
    /// Default screenshot folder. `None` if the platform has no pictures folder.
    fn pictures_dir(&self) -> Option<PathBuf>;
}

// ---------------------------------------------------------------------------
// Media keys and now-playing
// ---------------------------------------------------------------------------

/// Hardware or OS media key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKey {
    PlayPause,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    SeekForward,
    SeekBackward,
}

/// Metadata shown by the OS media overlay (Windows SMTC, macOS Now Playing, MPRIS).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NowPlaying {
    pub title: String,
    pub subtitle: Option<String>,
    pub playing: bool,
    /// Position and duration in whole seconds.
    pub position_secs: u64,
    pub duration_secs: u64,
}

/// Media-key delivery and now-playing publication.
pub trait MediaKeys {
    /// Drain key presses since the last call. Must not block.
    fn poll(&mut self) -> Result<Vec<MediaKey>, PlatformError>;
    /// Publish the current item to the OS overlay. `None` clears it.
    fn publish_now_playing(&mut self, info: Option<NowPlaying>) -> Result<(), PlatformError>;
}

// ---------------------------------------------------------------------------
// Power
// ---------------------------------------------------------------------------

/// Keeps the system or display awake while the guard is alive. Dropping the guard releases it.
pub struct PowerGuard {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl PowerGuard {
    /// Build a guard that runs `release` exactly once, on drop.
    pub fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

impl fmt::Debug for PowerGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PowerGuard").finish_non_exhaustive()
    }
}

/// Sleep and display-off inhibition during playback.
pub trait PowerInhibitor {
    /// Ask the OS not to sleep. `reason` is shown in OS diagnostics, for example "Playing video".
    fn inhibit_sleep(&mut self, reason: &str) -> Result<PowerGuard, PlatformError>;
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

/// Window placement and mode. Maps onto `kattri_settings::WindowGeometry` plus mode flags.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub monitor: Option<String>,
    pub fullscreen: bool,
    pub maximized: bool,
    pub always_on_top: bool,
    pub picture_in_picture: bool,
}

/// Window control used for restore-on-launch, fullscreen, always-on-top and PiP.
pub trait WindowController {
    fn state(&self) -> Result<WindowState, PlatformError>;
    /// Move, resize and set mode flags in one call.
    fn apply(&mut self, state: &WindowState) -> Result<(), PlatformError>;
    fn set_always_on_top(&mut self, on: bool) -> Result<(), PlatformError>;
    /// Enter or leave picture-in-picture (small always-on-top window). Backend-defined size.
    fn set_picture_in_picture(&mut self, on: bool) -> Result<(), PlatformError>;
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// A desktop notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

/// Desktop notification delivery.
pub trait Notifier {
    fn notify(&mut self, notification: Notification) -> Result<(), PlatformError>;
}

// ---------------------------------------------------------------------------
// Disk space
// ---------------------------------------------------------------------------

/// Free space on the volume that holds a path, for pre-download checks (M3-09).
pub trait DiskSpace {
    /// Bytes the current user may write on the volume that holds `path`.
    fn available_bytes(&self, path: &Path) -> Result<u64, PlatformError>;
}

/// Windows implementation backed by `GetDiskFreeSpaceExW`.
#[cfg(windows)]
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsDiskSpace;

#[cfg(windows)]
impl DiskSpace for WindowsDiskSpace {
    fn available_bytes(&self, path: &Path) -> Result<u64, PlatformError> {
        use std::os::windows::ffi::OsStrExt;

        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                directory_name: *const u16,
                free_bytes_for_caller: *mut u64,
                total_bytes: *mut u64,
                total_free_bytes: *mut u64,
            ) -> i32;
        }

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free_for_caller: u64 = 0;
        // SAFETY: `wide` is NUL-terminated and lives across the call. The out pointer is
        // a valid `u64`. The two optional out pointers are null, which the API allows.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_for_caller,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(PlatformError::Failed(format!(
                "GetDiskFreeSpaceExW failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(free_for_caller)
    }
}

#[cfg(all(test, windows))]
mod disk_tests {
    use super::*;

    #[test]
    fn windows_reports_free_space_for_a_temp_directory() {
        let dir = std::env::temp_dir();
        let bytes = WindowsDiskSpace.available_bytes(&dir).expect("free space");
        assert!(bytes > 0);
    }
}
