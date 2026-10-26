//! User-facing text for download transitions, failures and notices.

use kattri_client_core::downloads::DownloadTransitionError;

use crate::driver::FailureReason;

/// Notice shown in a download item's status. At most one per attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Notice {
    /// The server answered a range request with 200 (range ignored).
    RangesIgnored,
    /// The strong ETag changed between responses; the file was started again.
    ResourceChanged,
    /// The partial file was missing or had the wrong length; started again.
    LocalBytesLost,
    /// The server could not resume (weak ETag or rejected range); started again.
    RangeRejected,
}

impl Notice {
    pub const fn text(self) -> &'static str {
        match self {
            Self::RangesIgnored => {
                "This server does not support resuming downloads. The download started again from the beginning."
            }
            Self::ResourceChanged => {
                "The file changed on the server. The download started again from the beginning."
            }
            Self::LocalBytesLost => {
                "The partly downloaded file was missing or damaged. The download started again from the beginning."
            }
            Self::RangeRejected => {
                "The server could not resume this download. The download started again from the beginning."
            }
        }
    }
}

/// Maps every model error to a user message. The match is exhaustive on purpose:
/// a new `DownloadTransitionError` variant fails to compile until it has a message.
pub const fn transition_message(error: DownloadTransitionError) -> &'static str {
    match error {
        DownloadTransitionError::InvalidTransition { .. } => {
            "The download could not change state. Try again."
        }
        DownloadTransitionError::InvalidContentRange => {
            "The server sent an invalid byte range. The download stopped."
        }
        DownloadTransitionError::RangeOffsetMismatch { .. } => {
            "The server resumed the download at an unexpected position. The download stopped."
        }
        DownloadTransitionError::ValidatorMismatch => "The file changed on the server.",
        DownloadTransitionError::MissingTrustedValidator => {
            "The server did not give a stable file identifier, so this download cannot resume."
        }
        DownloadTransitionError::TotalLengthMismatch { .. } => {
            "The file size on the server changed. The download stopped."
        }
        DownloadTransitionError::ArithmeticOverflow => "The download size is invalid.",
        DownloadTransitionError::ExceedsExpectedLength { .. } => {
            "The server sent more data than expected. The download stopped."
        }
        DownloadTransitionError::ExceedsResponseRange { .. } => {
            "The server sent more data than the requested range. The download stopped."
        }
        DownloadTransitionError::IncompleteDownload => {
            "The download ended before the whole file arrived."
        }
    }
}

/// Message for a failed download.
pub fn failure_message(reason: &FailureReason) -> &'static str {
    match reason {
        FailureReason::Model(error) => transition_message(*error),
        FailureReason::Http(_) => "The server returned an error. The download stopped.",
        FailureReason::DiskFull => "Not enough disk space.",
        FailureReason::Storage => "The partly downloaded file could not be written.",
        FailureReason::NetworkExhausted => {
            "The connection failed repeatedly. The download stopped; try again later."
        }
        FailureReason::InvalidState => "The download could not start from its current state.",
        FailureReason::ChecksumMismatch => {
            "The downloaded file did not match the server's checksum. The download stopped."
        }
        FailureReason::RedirectRefused => {
            "The server redirected the download to another address. The download stopped."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_transition_error_has_a_user_message() {
        let all = [
            DownloadTransitionError::InvalidTransition {
                from: kattri_client_core::downloads::DownloadState::Queued,
                to: kattri_client_core::downloads::DownloadState::Committed,
            },
            DownloadTransitionError::InvalidContentRange,
            DownloadTransitionError::RangeOffsetMismatch {
                expected: 1,
                actual: 2,
            },
            DownloadTransitionError::ValidatorMismatch,
            DownloadTransitionError::MissingTrustedValidator,
            DownloadTransitionError::TotalLengthMismatch {
                expected: 1,
                actual: 2,
            },
            DownloadTransitionError::ArithmeticOverflow,
            DownloadTransitionError::ExceedsExpectedLength {
                expected: 1,
                actual: 2,
            },
            DownloadTransitionError::ExceedsResponseRange {
                end_exclusive: 1,
                actual: 2,
            },
            DownloadTransitionError::IncompleteDownload,
        ];
        for error in all {
            let text = transition_message(error);
            assert!(!text.is_empty(), "{error:?} has no message");
        }
    }

    #[test]
    fn notices_name_the_restart_and_are_distinct() {
        let notices = [
            Notice::RangesIgnored,
            Notice::ResourceChanged,
            Notice::LocalBytesLost,
            Notice::RangeRejected,
        ];
        for notice in notices {
            assert!(notice.text().contains("started again from the beginning"));
        }
        assert_eq!(
            Notice::RangesIgnored.text(),
            "This server does not support resuming downloads. The download started again from the beginning."
        );
    }
}
