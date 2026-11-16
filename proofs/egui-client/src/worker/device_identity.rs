//! Debug-only per-process device identity override for isolated acceptance runs.

use std::io;

#[cfg(debug_assertions)]
use std::ffi::OsString;

#[cfg(debug_assertions)]
use uuid::Uuid;

#[cfg(debug_assertions)]
const INVALID_OVERRIDE: &str = "debug device identity must be a valid UUID";

/// Parse the explicit acceptance override without reading or writing the installation ID file.
#[cfg(debug_assertions)]
pub(super) fn parse_debug_override(value: Option<OsString>) -> io::Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, INVALID_OVERRIDE))?;
    let parsed = Uuid::parse_str(&value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, INVALID_OVERRIDE))?;
    Ok(Some(parsed.hyphenated().to_string()))
}

/// Read the debug override only in debug builds. Release clients keep the stable installation ID.
pub(crate) fn from_environment() -> io::Result<Option<String>> {
    #[cfg(debug_assertions)]
    {
        parse_debug_override(std::env::var_os("KATTRI_DEBUG_DEVICE_ID"))
    }
    #[cfg(not(debug_assertions))]
    {
        Ok(None)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use std::{ffi::OsString, io};

    use super::parse_debug_override;

    fn override_of(value: &str) -> io::Result<Option<String>> {
        parse_debug_override(Some(OsString::from(value)))
    }

    #[test]
    fn an_absent_override_keeps_the_installation_identity() {
        assert_eq!(parse_debug_override(None).expect("absent override"), None);
    }

    #[test]
    fn a_valid_uuid_override_is_normalized() {
        assert_eq!(
            override_of("6F9619FF-8B86-D011-B42D-00C04FC964FF").expect("valid UUID"),
            Some("6f9619ff-8b86-d011-b42d-00c04fc964ff".to_owned())
        );
    }

    #[test]
    fn an_invalid_override_is_refused_rather_than_ignored() {
        assert!(override_of("not-a-uuid").is_err());
        assert!(override_of("").is_err());
        assert!(override_of("6f9619ff-8b86-d011-b42d-00c04fc964ff-extra").is_err());
    }
}
