//! Runtime capability observations and an identity-scoped, bounded cache.
//!
//! This module is independent of Jellyfin and libmpv FFI. The platform/libmpv
//! adapter supplies owned strings, and the API layer decides which observations
//! are safe to project into a Jellyfin `DeviceProfile`.

use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};

const MAX_CACHE_ENTRIES: usize = 8;
static RUNTIME_CAPABILITY_CACHE: OnceLock<Mutex<CapabilityCache>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RuntimeIdentity {
    pub mpv_version: String,
    pub ffmpeg_version: Option<String>,
    pub operating_system: String,
    pub gpu: Option<String>,
    pub driver: Option<String>,
}

impl RuntimeIdentity {
    pub fn current_os() -> String {
        format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn cache_key(&self) -> CapabilityCacheKey {
        CapabilityCacheKey {
            mpv_version: self.mpv_version.clone(),
            ffmpeg_version: self.ffmpeg_version.clone(),
            operating_system: self.operating_system.clone(),
            gpu: self.gpu.clone(),
            driver: self.driver.clone(),
        }
    }

    /// Hardware-specific observations are not cached without both identity fields.
    pub fn has_complete_hardware_identity(&self) -> bool {
        !self.mpv_version.trim().is_empty()
            && !self.operating_system.trim().is_empty()
            && self
                .gpu
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
            && self
                .driver
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CapabilityCacheKey {
    pub mpv_version: String,
    pub ffmpeg_version: Option<String>,
    pub operating_system: String,
    pub gpu: Option<String>,
    pub driver: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeWarning {
    RuntimeUnavailable,
    TimedOut,
    PropertyUnavailable { property: String },
    MalformedProperty { property: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeValue<T> {
    Available(T),
    Unavailable(ProbeWarning),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCapabilitySnapshot {
    pub identity: RuntimeIdentity,
    pub decoder_names: ProbeValue<Vec<String>>,
    pub demuxer_names: ProbeValue<Vec<String>>,
    pub configured_hwdec: ProbeValue<String>,
    /// This is an observation for the active media item only, not a general
    /// hardware capability claim.
    pub active_hwdec: ProbeValue<String>,
    pub hwdec_interop: ProbeValue<String>,
    pub audio_devices: ProbeValue<Vec<String>>,
    pub warning: Option<ProbeWarning>,
}

impl RuntimeCapabilitySnapshot {
    pub fn unavailable(identity: RuntimeIdentity, warning: ProbeWarning) -> Self {
        Self {
            identity,
            decoder_names: ProbeValue::Unavailable(warning.clone()),
            demuxer_names: ProbeValue::Unavailable(warning.clone()),
            configured_hwdec: ProbeValue::Unavailable(warning.clone()),
            active_hwdec: ProbeValue::Unavailable(warning.clone()),
            hwdec_interop: ProbeValue::Unavailable(warning.clone()),
            audio_devices: ProbeValue::Unavailable(warning.clone()),
            warning: Some(warning),
        }
    }
}

/// Small in-process cache; identity includes runtime, OS/architecture, GPU, and driver.
/// Entries without an observed GPU+driver are not cached, preventing hardware-specific
/// observations from being reused across unidentified devices.
#[derive(Debug, Default)]
pub struct CapabilityCache {
    entries: BTreeMap<CapabilityCacheKey, RuntimeCapabilitySnapshot>,
}

impl CapabilityCache {
    pub fn get(&self, identity: &RuntimeIdentity) -> Option<&RuntimeCapabilitySnapshot> {
        if !identity.has_complete_hardware_identity() {
            return None;
        }
        self.entries.get(&identity.cache_key())
    }

    pub fn insert(&mut self, snapshot: RuntimeCapabilitySnapshot) -> bool {
        if !snapshot.identity.has_complete_hardware_identity() {
            return false;
        }
        let key = snapshot.identity.cache_key();
        if !self.entries.contains_key(&key)
            && self.entries.len() == MAX_CACHE_ENTRIES
            && let Some(eviction_key) = self.entries.keys().next().cloned()
        {
            self.entries.remove(&eviction_key);
        }
        self.entries.insert(key, snapshot);
        true
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Read a process-local snapshot only when every identity component, including
/// the GPU and driver, matches the requested runtime.
pub fn cached_runtime_capabilities(
    identity: &RuntimeIdentity,
) -> Option<RuntimeCapabilitySnapshot> {
    if !identity.has_complete_hardware_identity() {
        return None;
    }
    let cache = RUNTIME_CAPABILITY_CACHE.get_or_init(|| Mutex::new(CapabilityCache::default()));
    let guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.get(identity).cloned()
}

/// Keep only a small process-local snapshot cache; unidentified hardware is
/// never cached, and no device or media data is persisted to disk.
pub fn cache_runtime_capabilities(snapshot: RuntimeCapabilitySnapshot) -> bool {
    let cache = RUNTIME_CAPABILITY_CACHE.get_or_init(|| Mutex::new(CapabilityCache::default()));
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.insert(snapshot)
}

#[cfg(test)]
mod tests {
    use super::{
        CapabilityCache, ProbeValue, ProbeWarning, RuntimeCapabilitySnapshot, RuntimeIdentity,
        cache_runtime_capabilities, cached_runtime_capabilities,
    };

    fn identity(gpu: Option<&str>, driver: Option<&str>) -> RuntimeIdentity {
        RuntimeIdentity {
            mpv_version: "mpv-test-build".into(),
            ffmpeg_version: Some("ffmpeg-test-build".into()),
            operating_system: "windows-x86_64".into(),
            gpu: gpu.map(str::to_owned),
            driver: driver.map(str::to_owned),
        }
    }

    fn snapshot(identity: RuntimeIdentity) -> RuntimeCapabilitySnapshot {
        RuntimeCapabilitySnapshot {
            identity,
            decoder_names: ProbeValue::Available(vec!["h264".into()]),
            demuxer_names: ProbeValue::Available(vec!["mov".into()]),
            configured_hwdec: ProbeValue::Available("no".into()),
            active_hwdec: ProbeValue::Unavailable(ProbeWarning::PropertyUnavailable {
                property: "hwdec-current".into(),
            }),
            hwdec_interop: ProbeValue::Unavailable(ProbeWarning::PropertyUnavailable {
                property: "hwdec-interop".into(),
            }),
            audio_devices: ProbeValue::Unavailable(ProbeWarning::PropertyUnavailable {
                property: "audio-device-list".into(),
            }),
            warning: None,
        }
    }

    #[test]
    fn cache_key_scopes_runtime_os_gpu_and_driver() {
        let mut cache = CapabilityCache::default();
        let observed = identity(Some("gpu-a"), Some("driver-a"));
        assert!(cache.insert(snapshot(observed.clone())));
        assert!(cache.get(&observed).is_some());
        assert!(
            cache
                .get(&identity(Some("gpu-b"), Some("driver-a")))
                .is_none()
        );
        assert!(
            cache
                .get(&identity(Some("gpu-a"), Some("driver-b")))
                .is_none()
        );
        assert!(
            cache
                .get(&RuntimeIdentity {
                    operating_system: "linux-x86_64".into(),
                    ..observed.clone()
                })
                .is_none()
        );
        assert!(
            cache
                .get(&RuntimeIdentity {
                    mpv_version: "different-build".into(),
                    ..observed
                })
                .is_none()
        );
    }

    #[test]
    fn incomplete_hardware_identity_is_not_cached() {
        let mut cache = CapabilityCache::default();
        let without_gpu = identity(None, None);
        assert!(!cache.insert(snapshot(without_gpu.clone())));
        assert!(cache.get(&without_gpu).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn process_cache_round_trips_only_a_complete_identity() {
        let complete = identity(
            Some("unique-gpu-cache-test"),
            Some("unique-driver-cache-test"),
        );
        let observed = snapshot(complete.clone());
        assert!(cache_runtime_capabilities(observed.clone()));
        assert_eq!(cached_runtime_capabilities(&complete), Some(observed));

        let incomplete = identity(None, None);
        assert!(cached_runtime_capabilities(&incomplete).is_none());
    }

    #[test]
    fn unavailable_probe_returns_typed_fallback_observations() {
        let fallback = RuntimeCapabilitySnapshot::unavailable(
            identity(None, None),
            ProbeWarning::RuntimeUnavailable,
        );
        assert_eq!(fallback.warning, Some(ProbeWarning::RuntimeUnavailable));
        assert_eq!(
            fallback.decoder_names,
            ProbeValue::Unavailable(ProbeWarning::RuntimeUnavailable)
        );
        assert_eq!(
            fallback.configured_hwdec,
            ProbeValue::Unavailable(ProbeWarning::RuntimeUnavailable)
        );
    }

    #[test]
    fn cache_remains_bounded() {
        let mut cache = CapabilityCache::default();
        for index in 0..16 {
            let gpu = format!("gpu-{index}");
            let driver = format!("driver-{index}");
            assert!(cache.insert(snapshot(identity(Some(&gpu), Some(&driver)))));
        }
        assert_eq!(cache.len(), 8);
    }
}
