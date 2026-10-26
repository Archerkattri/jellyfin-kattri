use std::{os::raw::c_ulong, path::Path, sync::Arc};

use crate::{MpvError, ffi::Api};

/// A dynamically loaded libmpv runtime. No mpv SDK or headers are needed to
/// compile this crate; loading happens only when this value is constructed.
#[derive(Clone)]
pub struct MpvLibrary {
    pub(crate) api: Arc<Api>,
    path: std::path::PathBuf,
}

impl MpvLibrary {
    /// Load the first standard platform library name that is available.
    pub fn load() -> Result<Self, MpvError> {
        let candidates = platform_candidates();
        let mut errors = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            match Api::load(Path::new(candidate)) {
                Ok(api) => {
                    return Ok(Self {
                        api: Arc::new(api),
                        path: candidate.into(),
                    });
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        Err(MpvError::LibraryNotFound {
            attempted: candidates
                .into_iter()
                .zip(errors)
                .map(|(name, error)| format!("{name} ({error})"))
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
