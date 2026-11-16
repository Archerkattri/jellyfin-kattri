//! Pure policy for explicitly isolated debug playback validation.

/// Whether the stream-only run explicitly opted in to SyncPlay group status messages.
pub(crate) fn stream_only_syncplay_opt_in(stream_only: bool, status_opt_in: bool) -> bool {
    stream_only && status_opt_in
}

/// SyncPlay status messages remain available during normal playback or with the narrow
/// stream-only acceptance opt-in.
pub(crate) fn should_poll_syncplay_status(
    reporting_enabled: bool,
    stream_only_status_opt_in: bool,
) -> bool {
    reporting_enabled || stream_only_status_opt_in
}

#[cfg(test)]
mod tests {
    use super::{should_poll_syncplay_status, stream_only_syncplay_opt_in};

    #[test]
    fn stream_only_status_opt_in_requires_both_flags() {
        assert!(!stream_only_syncplay_opt_in(false, false));
        assert!(!stream_only_syncplay_opt_in(false, true));
        assert!(!stream_only_syncplay_opt_in(true, false));
        assert!(stream_only_syncplay_opt_in(true, true));
    }

    #[test]
    fn normal_reporting_or_explicit_stream_only_opt_in_keeps_syncplay_status_enabled() {
        assert!(!should_poll_syncplay_status(false, false));
        assert!(should_poll_syncplay_status(false, true));
        assert!(should_poll_syncplay_status(true, false));
        assert!(should_poll_syncplay_status(true, true));
    }
}
