use std::{
    os::raw::c_ulong,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{MpvError, ffi::Api};

/// A dynamically loaded libmpv runtime. No mpv SDK or headers are needed to
/// compile this crate; loading happens only when this value is constructed.
#[derive(Clone)]
pub struct MpvLibrary {
    pub(crate) api: Arc<Api>,
    path: std::path::PathBuf,
}

impl MpvLibrary {
    /// Load libmpv: first a copy bundled next to the executable (inside the macOS app bundle's
    /// `Contents/Frameworks`), then the standard platform library names, which the system
    /// loader resolves.
    pub fn load() -> Result<Self, MpvError> {
        let mut candidates: Vec<PathBuf> = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(bundled_candidates))
            .unwrap_or_default()
            .into_iter()
            .filter(|path| path.is_file())
            .collect();
        candidates.extend(platform_candidates().into_iter().map(PathBuf::from));
        let mut errors = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            match Api::load(candidate) {
                Ok(api) => {
                    return Ok(Self {
                        api: Arc::new(api),
                        path: candidate.clone(),
                    });
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        Err(MpvError::LibraryNotFound {
            attempted: candidates
                .into_iter()
                .zip(errors)
                .map(|(name, error)| format!("{} ({error})", name.display()))
                .collect(),
        })
    }

    /// Load a specific runtime path, useful for app bundles and diagnostics.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, MpvError> {
        let path = path.as_ref();
        let api = Api::load(path)?;
        Ok(Self {
            api: Arc::new(api),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn client_api_version(&self) -> c_ulong {
        self.api.client_api_version
    }
}

/// Full paths where a packaged build keeps its own libmpv, relative to the executable's
/// folder. Only paths that exist are tried. On macOS the release bundle copies the library to
/// `Contents/Frameworks` (packaging/macos/build-app.sh), which `dlopen` does not search for a
/// bare name.
fn bundled_candidates(exe_dir: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![exe_dir.join("../Frameworks/libmpv.2.dylib")]
    }
    #[cfg(not(target_os = "macos"))]
    {
        platform_candidates()
            .into_iter()
            .map(|name| exe_dir.join(name))
            .collect()
    }
}

fn platform_candidates() -> Vec<&'static str> {
    #[cfg(target_os = "windows")]
    {
        vec!["mpv-2.dll", "libmpv-2.dll", "mpv.dll"]
    }
    #[cfg(target_os = "macos")]
    {
        vec!["libmpv.2.dylib", "libmpv.dylib"]
    }
    #[cfg(target_os = "linux")]
    {
        vec!["libmpv.so.2", "libmpv.so"]
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_libraries_are_looked_up_beside_the_executable() {
        let exe_dir = Path::new("/opt/kattri/bin");
        let candidates = bundled_candidates(exe_dir);
        assert!(!candidates.is_empty());
        for candidate in &candidates {
            assert!(candidate.starts_with(exe_dir), "{}", candidate.display());
        }
        #[cfg(target_os = "macos")]
        assert_eq!(
            candidates,
            vec![exe_dir.join("../Frameworks/libmpv.2.dylib")]
        );
        #[cfg(target_os = "linux")]
        assert!(candidates.contains(&exe_dir.join("libmpv.so.2")));
        #[cfg(target_os = "windows")]
        assert!(candidates.contains(&exe_dir.join("libmpv-2.dll")));
    }

    #[test]
    fn a_missing_library_reports_every_name_it_tried() {
        // No libmpv beside the test binary, and none of the bare names resolve to a 2.5+
        // library on a machine without mpv; the error lists each attempt.
        if let Err(MpvError::LibraryNotFound { attempted }) = MpvLibrary::load() {
            for name in platform_candidates() {
                assert!(
                    attempted.iter().any(|entry| entry.starts_with(name)),
                    "{name} missing from {attempted:?}"
                );
            }
        }
    }
}
