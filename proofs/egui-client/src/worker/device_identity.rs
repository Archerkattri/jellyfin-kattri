//! Debug-only per-process device identity override for isolated acceptance runs.

use std::{ffi::OsString, io};

use uuid::Uuid;

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

/// In release builds the acceptance override is ignored without reading the environment.
#[cfg(not(debug_assertions))]
pub(super) fn parse_debug_override(_value: Option<OsString>) -> io::Result<Option<String>> {
    Ok(None)
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

    #[test]
    fn absent_override_requests_the_persistent_installation_identity() {
        assert_eq!(parse_debug_override(None).expect("absent override"), None);
    }

    #[test]
    fn valid_uuid_is_normalized_to_lowercase_hyphenated_form() {
        let result =
            parse_debug_override(Some(OsString::from("3F2504E0-4F89-11D3-9A0C-0305E82C3301")))
                .expect("valid UUID");
        assert_eq!(
            result.as_deref(),
            Some("3f2504e0-4f89-11d3-9a0c-0305e82c3301")
        );
    }

    #[test]
    fn invalid_uuid_returns_generic_error_without_echoing_input() {
        let error = parse_debug_override(Some(OsString::from("not-a-device-id")))
            .expect_err("invalid UUID is refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "debug device identity must be a valid UUID"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_override_is_refused_without_echoing_input() {
        use std::os::unix::ffi::OsStringExt;

        let error = parse_debug_override(Some(OsString::from_vec(vec![0xff])))
            .expect_err("non-Unicode value is refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "debug device identity must be a valid UUID"
        );
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_override_is_refused_without_echoing_input() {
        use std::os::windows::ffi::OsStringExt;

        let error = parse_debug_override(Some(OsString::from_wide(&[0xd800])))
            .expect_err("non-Unicode value is refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "debug device identity must be a valid UUID"
        );
    }
}
