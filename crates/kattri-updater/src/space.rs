//! Disk-space gate for the staging write (M4-16). Pure: the caller supplies the free space.
//!
//! The check does not query the OS. The app reads free space for the store's volume and passes
//! it in, so the rule is testable on any platform. A refusal happens before any write.

use crate::apply::{ApplyError, InstallStore, apply_verified};
use crate::verify::VerifiedManifest;

/// Refuse the staging write unless `free_bytes` covers `staged_bytes` plus `margin_bytes`.
///
/// Equal is enough: a write that leaves exactly `margin_bytes` free is allowed.
pub fn check_staging_space(
    free_bytes: u64,
    staged_bytes: u64,
    margin_bytes: u64,
) -> Result<(), ApplyError> {
    // The sum does not fit in u64, so no real volume can hold the write.
    let Some(required) = staged_bytes.checked_add(margin_bytes) else {
        return Err(ApplyError::InsufficientSpace {
            required: u64::MAX,
            free: free_bytes,
        });
    };
    if free_bytes < required {
        return Err(ApplyError::InsufficientSpace {
            required,
            free: free_bytes,
        });
    }
    Ok(())
}

/// [`apply_verified`] with the staging write gated on free space.
///
/// The space check runs before the store is touched, so a refusal leaves the installed build,
/// the journal and the staging folder unchanged.
pub fn apply_verified_with_space<S: InstallStore>(
    store: &mut S,
    verified: &VerifiedManifest,
    platform: &str,
    arch: &str,
    bytes: &[u8],
    free_bytes: u64,
    margin_bytes: u64,
) -> Result<(), ApplyError> {
    let staged = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    check_staging_space(free_bytes, staged, margin_bytes)?;
    apply_verified(store, verified, platform, arch, bytes)
}

#[cfg(test)]
mod tests {
    use super::check_staging_space;
    use crate::apply::ApplyError;

    #[test]
    fn exact_fit_is_allowed() {
        assert!(check_staging_space(1_100, 1_000, 100).is_ok());
    }

    #[test]
    fn one_byte_short_is_refused_with_both_sizes() {
        assert_eq!(
            check_staging_space(1_099, 1_000, 100),
            Err(ApplyError::InsufficientSpace {
                required: 1_100,
                free: 1_099
            })
        );
    }

    #[test]
    fn zero_margin_needs_only_the_staged_size() {
        assert!(check_staging_space(500, 500, 0).is_ok());
        assert!(check_staging_space(499, 500, 0).is_err());
    }

    #[test]
    fn empty_build_with_no_margin_fits_on_a_full_disk() {
        assert!(check_staging_space(0, 0, 0).is_ok());
    }

    #[test]
    fn margin_is_required_even_when_the_build_is_empty() {
        assert!(check_staging_space(9, 0, 10).is_err());
    }

    #[test]
    fn overflowing_requirement_is_refused_even_with_maximum_free_space() {
        assert_eq!(
            check_staging_space(u64::MAX, u64::MAX, 1),
            Err(ApplyError::InsufficientSpace {
                required: u64::MAX,
                free: u64::MAX
            })
        );
    }
}
