//! In-memory test doubles for every platform trait.
//!
//! These are real implementations, not stubs: they store state and record calls, so
//! app logic can be tested on any OS. They are also usable as a headless fallback.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crate::{
    AppDirs, CredentialKey, CredentialStore, MediaKey, MediaKeys, Notification, Notifier,
    NowPlaying, PlatformError, PowerGuard, PowerInhibitor, Secret, WindowController, WindowState,
};

/// Credential store held in a `HashMap`. Contents are lost when dropped.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    items: HashMap<CredentialKey, Secret>,
}

impl MemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn get(&self, key: &CredentialKey) -> Result<Option<Secret>, PlatformError> {
        Ok(self.items.get(key).cloned())
    }

    fn set(&mut self, key: &CredentialKey, secret: Secret) -> Result<(), PlatformError> {
        self.items.insert(key.clone(), secret);
        Ok(())
    }

    fn delete(&mut self, key: &CredentialKey) -> Result<bool, PlatformError> {
        Ok(self.items.remove(key).is_some())
    }
}

/// Directories rooted under one base path, so tests never touch the real profile.
#[derive(Debug, Clone)]
pub struct MemoryAppDirs {
    root: PathBuf,
}

impl MemoryAppDirs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl AppDirs for MemoryAppDirs {
    fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    fn pictures_dir(&self) -> Option<PathBuf> {
        Some(self.root.join("pictures"))
    }
}

/// Media-key queue that tests fill with `push_key`. Records the last published now-playing.
#[derive(Debug, Default)]
pub struct MemoryMediaKeys {
    pending: VecDeque<MediaKey>,
    now_playing: Option<NowPlaying>,
}

impl MemoryMediaKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate a key press.
    pub fn push_key(&mut self, key: MediaKey) {
        self.pending.push_back(key);
    }

    pub fn now_playing(&self) -> Option<&NowPlaying> {
        self.now_playing.as_ref()
    }
}

impl MediaKeys for MemoryMediaKeys {
    fn poll(&mut self) -> Result<Vec<MediaKey>, PlatformError> {
        Ok(self.pending.drain(..).collect())
    }

    fn publish_now_playing(&mut self, info: Option<NowPlaying>) -> Result<(), PlatformError> {
        self.now_playing = info;
        Ok(())
    }
}

/// Sleep inhibitor that counts active guards.
#[derive(Debug, Default, Clone)]
pub struct MemoryPowerInhibitor {
    active: Arc<AtomicUsize>,
    reasons: Vec<String>,
}

impl MemoryPowerInhibitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of guards that have not been dropped yet.
    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// Reasons passed to every `inhibit_sleep` call, in order.
    pub fn reasons(&self) -> &[String] {
        &self.reasons
    }
}

impl PowerInhibitor for MemoryPowerInhibitor {
    fn inhibit_sleep(&mut self, reason: &str) -> Result<PowerGuard, PlatformError> {
        self.reasons.push(reason.to_owned());
        self.active.fetch_add(1, Ordering::SeqCst);
        let active = Arc::clone(&self.active);
        Ok(PowerGuard::new(move || {
            active.fetch_sub(1, Ordering::SeqCst);
        }))
    }
}

/// Window controller that stores the state it was last given.
#[derive(Debug, Clone, Default)]
pub struct MemoryWindow {
    state: WindowState,
}

impl MemoryWindow {
    pub fn new(initial: WindowState) -> Self {
        Self { state: initial }
    }
}

impl WindowController for MemoryWindow {
    fn state(&self) -> Result<WindowState, PlatformError> {
        Ok(self.state.clone())
    }

    fn apply(&mut self, state: &WindowState) -> Result<(), PlatformError> {
        self.state = state.clone();
        Ok(())
    }

    fn set_always_on_top(&mut self, on: bool) -> Result<(), PlatformError> {
        self.state.always_on_top = on;
        Ok(())
    }

    fn set_picture_in_picture(&mut self, on: bool) -> Result<(), PlatformError> {
        self.state.picture_in_picture = on;
        if on {
            self.state.always_on_top = true;
        }
        Ok(())
    }
}

/// Notifier that records every notification it was asked to show.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    sent: Vec<Notification>,
}

impl RecordingNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sent(&self) -> &[Notification] {
        &self.sent
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&mut self, notification: Notification) -> Result<(), PlatformError> {
        self.sent.push(notification);
        Ok(())
    }
}

/// Disk space double that reports a fixed amount of free space.
#[derive(Debug, Clone)]
pub struct MemoryDiskSpace {
    available: u64,
}

impl MemoryDiskSpace {
    pub fn new(available: u64) -> Self {
        Self { available }
    }
}

impl crate::DiskSpace for MemoryDiskSpace {
    fn available_bytes(&self, _path: &Path) -> Result<u64, PlatformError> {
        Ok(self.available)
    }
}
