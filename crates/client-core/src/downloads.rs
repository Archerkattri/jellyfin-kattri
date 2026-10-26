//! Pure download-job state and HTTP range-resume contract.
//!
//! This module does not perform HTTP or filesystem operations. In particular,
//! callers must honor `RestartFromZero` by creating a fresh temporary file and
//! must only advance the durable offset after bytes have actually been flushed.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadState {
    Queued,
    Negotiating,
    Downloading,
    Verifying,
    Committed,
    Paused,
    Cancelled,
    Failed,
    NeedsAuth,
}

impl DownloadState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Cancelled | Self::Failed)
    }
}

/// Representation validators associated with the bytes in the current temp.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Validators {
    /// A strong ETag is required to trust local bytes as the complete response
    /// to a range request. Weak ETags and dates alone are not strong identity.
    pub fn has_strong_identity(&self) -> bool {
        self.etag.as_deref().is_some_and(|etag| {
            let etag = etag.trim();
            let bytes = etag.as_bytes();
            bytes.len() >= 2
                && bytes.first() == Some(&b'"')
                && bytes.last() == Some(&b'"')
                && !etag.starts_with("W/")
                && !bytes[1..bytes.len() - 1]
                    .iter()
                    .any(|byte| *byte == b'"' || *byte < 0x21 || *byte == 0x7f)
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadSnapshot {
    pub state: DownloadState,
    pub validators: Validators,
    pub expected_total_length: Option<u64>,
    /// Bytes known durable in the current temporary file.
    pub durable_offset: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadTransitionError {
    InvalidTransition {
        from: DownloadState,
        to: DownloadState,
    },
    InvalidContentRange,
    RangeOffsetMismatch {
        expected: u64,
        actual: u64,
    },
    ValidatorMismatch,
    MissingTrustedValidator,
    TotalLengthMismatch {
        expected: u64,
        actual: u64,
    },
    ArithmeticOverflow,
    ExceedsExpectedLength {
        expected: u64,
        actual: u64,
    },
    ExceedsResponseRange {
        end_exclusive: u64,
        actual: u64,
    },
    IncompleteDownload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeDecision {
    /// Append the response body starting at the current durable offset.
    Append { start: u64, end_inclusive: u64 },
    /// Start a full GET and write it to a newly-created temporary file.
    RestartFromZero,
    /// All local bytes are present; content verification is still required.
    VerifyLocalBytes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadJob {
    snapshot: DownloadSnapshot,
    response_end_exclusive: Option<u64>,
}

impl Default for DownloadJob {
    fn default() -> Self {
        Self::new()
    }
}

impl DownloadJob {
    pub fn new() -> Self {
        Self {
            snapshot: DownloadSnapshot {
                state: DownloadState::Queued,
                validators: Validators::default(),
                expected_total_length: None,
                durable_offset: 0,
            },
            response_end_exclusive: None,
        }
    }

    pub fn snapshot(&self) -> &DownloadSnapshot {
        &self.snapshot
    }

    /// Restores a job from a snapshot that was persisted by an earlier process.
    ///
    /// The restored durable offset is kept exactly as recorded. The next range
    /// request must therefore start at that offset (see [`Self::handle_response`]),
    /// and the job keeps no HTTP response bounds, so the caller must re-negotiate.
    ///
    /// In-flight states that depend on a live response (`Negotiating`,
    /// `Downloading`) cannot be restored because the response is gone after a
    /// restart; the caller should persist `Paused` or `Queued` instead. The
    /// following impossible combinations are refused:
    ///
    /// - a durable offset larger than a known total length;
    /// - `Verifying` or `Committed` without a total length equal to the offset;
    /// - `Negotiating` or `Downloading`, which need a live response.
    pub fn from_snapshot(snapshot: DownloadSnapshot) -> Result<Self, DownloadTransitionError> {
        if matches!(
            snapshot.state,
            DownloadState::Negotiating | DownloadState::Downloading
        ) {
            return Err(DownloadTransitionError::InvalidTransition {
                from: snapshot.state,
                to: DownloadState::Queued,
            });
        }
        if let Some(expected) = snapshot.expected_total_length
            && snapshot.durable_offset > expected
        {
            return Err(DownloadTransitionError::ExceedsExpectedLength {
                expected,
                actual: snapshot.durable_offset,
            });
        }
        if matches!(
            snapshot.state,
            DownloadState::Verifying | DownloadState::Committed
        ) && snapshot.expected_total_length != Some(snapshot.durable_offset)
        {
            return Err(DownloadTransitionError::IncompleteDownload);
        }
        Ok(Self {
            snapshot,
            response_end_exclusive: None,
        })
    }

    pub fn begin_negotiation(&mut self) -> Result<(), DownloadTransitionError> {
        let from = self.snapshot.state;
        let valid = from == DownloadState::Queued
            || (from == DownloadState::Downloading
                && self
                    .response_end_exclusive
                    .is_some_and(|end| self.snapshot.durable_offset == end));
        if !valid {
            return Err(DownloadTransitionError::InvalidTransition {
                from,
                to: DownloadState::Negotiating,
            });
        }
        self.snapshot.state = DownloadState::Negotiating;
        Ok(())
    }

    /// Resolve response headers for a range/fresh GET negotiation.
    ///
    /// A 200 always discards the prior offset logically and signals that the
    /// caller must use a fresh temp. A 206 must begin at the exact durable
    /// offset; after any bytes have been saved, it also requires unchanged
    /// validators and a strong ETag identity. A 416 is accepted as complete
    /// only for an exact byte count and unchanged strong ETag; all other 416s
    /// reset the job to queued and request a fresh full download.
    pub fn handle_response(
        &mut self,
        response: RangeResponse,
    ) -> Result<RangeDecision, DownloadTransitionError> {
        self.require_state(DownloadState::Negotiating)?;
        match response {
            RangeResponse::Ok {
                content_length,
                validators,
            } => {
                self.snapshot.durable_offset = 0;
                self.snapshot.expected_total_length = content_length;
                self.snapshot.validators = validators;
                self.response_end_exclusive = content_length;
                self.snapshot.state = DownloadState::Downloading;
                Ok(RangeDecision::RestartFromZero)
            }
            RangeResponse::Partial {
                start,
                end_inclusive,
                total,
                validators,
            } => {
                if total == 0 || start > end_inclusive || end_inclusive >= total {
                    return Err(DownloadTransitionError::InvalidContentRange);
                }
                if start != self.snapshot.durable_offset {
                    return Err(DownloadTransitionError::RangeOffsetMismatch {
                        expected: self.snapshot.durable_offset,
                        actual: start,
                    });
                }
                if start > 0 {
                    if self.snapshot.validators != validators {
                        return Err(DownloadTransitionError::ValidatorMismatch);
                    }
                    if !self.snapshot.validators.has_strong_identity() {
                        return Err(DownloadTransitionError::MissingTrustedValidator);
                    }
                    self.check_total(total)?;
                } else {
                    // There are no prior bytes to protect, so a new resource
                    // identity can be adopted at offset zero.
                    self.snapshot.validators = validators;
                }
                let end_exclusive = end_inclusive
                    .checked_add(1)
                    .ok_or(DownloadTransitionError::ArithmeticOverflow)?;
                self.snapshot.expected_total_length = Some(total);
                self.response_end_exclusive = Some(end_exclusive);
                self.snapshot.state = DownloadState::Downloading;
                Ok(RangeDecision::Append {
                    start,
                    end_inclusive,
                })
            }
            RangeResponse::Unsatisfied { total, validators } => {
                let trusted_completion = total == self.snapshot.durable_offset
                    && self.snapshot.validators == validators
                    && self.snapshot.validators.has_strong_identity()
                    && self
                        .snapshot
                        .expected_total_length
                        .is_none_or(|expected| expected == total);
                if trusted_completion {
                    self.snapshot.expected_total_length = Some(total);
                    self.response_end_exclusive = None;
                    self.snapshot.state = DownloadState::Verifying;
                    Ok(RangeDecision::VerifyLocalBytes)
                } else {
                    self.snapshot.durable_offset = 0;
                    self.snapshot.expected_total_length = Some(total);
                    self.snapshot.validators = validators;
                    self.response_end_exclusive = None;
                    self.snapshot.state = DownloadState::Queued;
                    Ok(RangeDecision::RestartFromZero)
                }
            }
        }
    }

    /// Record bytes only after the caller has made them durable.
    pub fn record_durable_bytes(&mut self, byte_count: u64) -> Result<(), DownloadTransitionError> {
        self.require_state(DownloadState::Downloading)?;
        let new_offset = self
            .snapshot
            .durable_offset
            .checked_add(byte_count)
            .ok_or(DownloadTransitionError::ArithmeticOverflow)?;
        if let Some(expected) = self.snapshot.expected_total_length
            && new_offset > expected
        {
            return Err(DownloadTransitionError::ExceedsExpectedLength {
                expected,
                actual: new_offset,
            });
        }
        if let Some(end_exclusive) = self.response_end_exclusive
            && new_offset > end_exclusive
        {
            return Err(DownloadTransitionError::ExceedsResponseRange {
                end_exclusive,
                actual: new_offset,
            });
        }
        self.snapshot.durable_offset = new_offset;
        Ok(())
    }

    pub fn finish_download(&mut self) -> Result<(), DownloadTransitionError> {
        self.require_state(DownloadState::Downloading)?;
        if let Some(expected) = self.snapshot.expected_total_length {
            if self.snapshot.durable_offset != expected {
                return Err(DownloadTransitionError::IncompleteDownload);
            }
        } else {
            // A clean EOF is sufficient when the server used a streaming body
            // without Content-Length; callers have already advanced only
            // flushed bytes into `durable_offset`.
            self.snapshot.expected_total_length = Some(self.snapshot.durable_offset);
        }
        self.transition(DownloadState::Verifying)
    }

    pub fn commit_verified(&mut self) -> Result<(), DownloadTransitionError> {
        self.transition(DownloadState::Committed)
    }

    pub fn pause(&mut self) -> Result<(), DownloadTransitionError> {
        self.transition(DownloadState::Paused)
    }

    pub fn resume(&mut self) -> Result<(), DownloadTransitionError> {
        self.require_state(DownloadState::Paused)?;
        self.snapshot.state = DownloadState::Queued;
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), DownloadTransitionError> {
        self.transition(DownloadState::Cancelled)
    }

    pub fn fail(&mut self) -> Result<(), DownloadTransitionError> {
        self.transition(DownloadState::Failed)
    }

    pub fn require_auth(&mut self) -> Result<(), DownloadTransitionError> {
        self.transition(DownloadState::NeedsAuth)
    }

    /// Resume a paused-for-auth job after credentials have been renewed.
    /// Existing bytes remain subject to the normal validator checks.
    pub fn credentials_restored(&mut self) -> Result<(), DownloadTransitionError> {
        self.require_state(DownloadState::NeedsAuth)?;
        self.snapshot.state = DownloadState::Queued;
        Ok(())
    }

    fn check_total(&self, actual: u64) -> Result<(), DownloadTransitionError> {
        if let Some(expected) = self.snapshot.expected_total_length
            && expected != actual
        {
            return Err(DownloadTransitionError::TotalLengthMismatch { expected, actual });
        }
        Ok(())
    }

    fn require_state(&self, expected: DownloadState) -> Result<(), DownloadTransitionError> {
        if self.snapshot.state == expected {
            Ok(())
        } else {
            Err(DownloadTransitionError::InvalidTransition {
                from: self.snapshot.state,
                to: expected,
            })
        }
    }

    fn transition(&mut self, to: DownloadState) -> Result<(), DownloadTransitionError> {
        let from = self.snapshot.state;
        let valid = match to {
            DownloadState::Negotiating => from == DownloadState::Queued,
            DownloadState::Verifying => from == DownloadState::Downloading,
            DownloadState::Committed => from == DownloadState::Verifying,
            DownloadState::Paused => matches!(
                from,
                DownloadState::Queued
                    | DownloadState::Negotiating
                    | DownloadState::Downloading
                    | DownloadState::Verifying
            ),
            DownloadState::Cancelled => !from.is_terminal(),
            DownloadState::Failed | DownloadState::NeedsAuth => {
                !from.is_terminal() && from != DownloadState::Paused
            }
            DownloadState::Queued | DownloadState::Downloading => false,
        };
        if !valid {
            return Err(DownloadTransitionError::InvalidTransition { from, to });
        }
        self.snapshot.state = to;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RangeResponse {
    Ok {
        content_length: Option<u64>,
        validators: Validators,
    },
    Partial {
        start: u64,
        end_inclusive: u64,
        total: u64,
        validators: Validators,
    },
    Unsatisfied {
        total: u64,
        validators: Validators,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trusted(etag: &str) -> Validators {
        Validators {
            etag: Some(etag.to_owned()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
        }
    }

    #[test]
    fn only_well_formed_strong_etags_count_as_identity_validators() {
        assert!(trusted("\"version-1\"").has_strong_identity());
        assert!(!trusted("W/\"version-1\"").has_strong_identity());
        assert!(!trusted("version-1").has_strong_identity());
        assert!(
            !Validators {
                etag: None,
                last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
            }
            .has_strong_identity()
        );
    }

    fn partial(
        start: u64,
        end_inclusive: u64,
        total: u64,
        validators: Validators,
    ) -> RangeResponse {
        RangeResponse::Partial {
            start,
            end_inclusive,
            total,
            validators,
        }
    }

    fn seeded_download(offset: u64, total: u64, validators: Validators) -> DownloadJob {
        let mut job = DownloadJob::new();
        job.snapshot.state = DownloadState::Downloading;
        job.snapshot.durable_offset = offset;
        job.snapshot.expected_total_length = Some(total);
        job.snapshot.validators = validators;
        job.response_end_exclusive = Some(total);
        job
    }

    #[test]
    fn happy_path_resumes_verifies_and_commits() {
        let mut job = seeded_download(4, 10, trusted("\"v1\""));
        job.pause().unwrap();
        job.resume().unwrap();
        job.begin_negotiation().unwrap();
        assert_eq!(
            job.handle_response(partial(4, 9, 10, trusted("\"v1\"")))
                .unwrap(),
            RangeDecision::Append {
                start: 4,
                end_inclusive: 9
            }
        );
        job.record_durable_bytes(6).unwrap();
        job.finish_download().unwrap();
        job.commit_verified().unwrap();
        assert_eq!(job.snapshot().state, DownloadState::Committed);
        assert_eq!(job.snapshot().durable_offset, 10);
    }

    #[test]
    fn first_partial_response_can_establish_the_resource_validator() {
        let validators = trusted("\"first-response\"");
        let mut job = DownloadJob::new();
        job.begin_negotiation().unwrap();
        assert_eq!(
            job.handle_response(partial(0, 3, 8, validators.clone()))
                .unwrap(),
            RangeDecision::Append {
                start: 0,
                end_inclusive: 3
            }
        );
        assert_eq!(job.snapshot().validators, validators);
        job.record_durable_bytes(4).unwrap();
        job.begin_negotiation().unwrap();
        assert_eq!(
            job.handle_response(partial(4, 7, 8, validators)).unwrap(),
            RangeDecision::Append {
                start: 4,
                end_inclusive: 7
            }
        );
    }

    #[test]
    fn rejects_wrong_range_offset_and_validator() {
        let mut offset = seeded_download(4, 10, trusted("\"v1\""));
        offset.snapshot.state = DownloadState::Negotiating;
        assert_eq!(
            offset.handle_response(partial(3, 9, 10, trusted("\"v1\""))),
            Err(DownloadTransitionError::RangeOffsetMismatch {
                expected: 4,
                actual: 3
            })
        );

        let mut validator = seeded_download(4, 10, trusted("\"v1\""));
        validator.snapshot.state = DownloadState::Negotiating;
        assert_eq!(
            validator.handle_response(partial(4, 9, 10, trusted("\"v2\""))),
            Err(DownloadTransitionError::ValidatorMismatch)
        );
    }

    #[test]
    fn full_response_restarts_from_zero_with_new_temp_contract() {
        let mut job = seeded_download(8, 10, trusted("\"old\""));
        job.snapshot.state = DownloadState::Negotiating;
        assert_eq!(
            job.handle_response(RangeResponse::Ok {
                content_length: Some(12),
                validators: trusted("\"new\""),
            })
            .unwrap(),
            RangeDecision::RestartFromZero
        );
        assert_eq!(job.snapshot().state, DownloadState::Downloading);
        assert_eq!(job.snapshot().durable_offset, 0);
        assert_eq!(job.snapshot().expected_total_length, Some(12));
    }

    #[test]
    fn clean_eof_allows_verifying_a_stream_without_content_length() {
        let mut job = DownloadJob::new();
        job.begin_negotiation().unwrap();
        assert_eq!(
            job.handle_response(RangeResponse::Ok {
                content_length: None,
                validators: Validators::default(),
            })
            .unwrap(),
            RangeDecision::RestartFromZero
        );
        job.record_durable_bytes(37).unwrap();
        job.finish_download().unwrap();
        assert_eq!(job.snapshot().state, DownloadState::Verifying);
        assert_eq!(job.snapshot().expected_total_length, Some(37));
    }

    #[test]
    fn exact_trusted_416_enters_verification() {
        let mut job = seeded_download(10, 10, trusted("\"v1\""));
        job.snapshot.state = DownloadState::Negotiating;
        assert_eq!(
            job.handle_response(RangeResponse::Unsatisfied {
                total: 10,
                validators: trusted("\"v1\""),
            })
            .unwrap(),
            RangeDecision::VerifyLocalBytes
        );
        assert_eq!(job.snapshot().state, DownloadState::Verifying);
    }

    #[test]
    fn range_segments_bound_durable_bytes_before_next_negotiation() {
        let mut job = DownloadJob::new();
        job.begin_negotiation().unwrap();
        job.handle_response(partial(0, 3, 8, Validators::default()))
            .unwrap();
        assert_eq!(
            job.record_durable_bytes(5),
            Err(DownloadTransitionError::ExceedsResponseRange {
                end_exclusive: 4,
                actual: 5
            })
        );
        job.record_durable_bytes(4).unwrap();
        job.begin_negotiation().unwrap();
        assert_eq!(job.snapshot().state, DownloadState::Negotiating);
    }

    #[test]
    fn conflicting_416_restarts_and_untrusted_exact_416_does_not_complete() {
        let mut conflicting = seeded_download(8, 10, trusted("\"v1\""));
        conflicting.snapshot.state = DownloadState::Negotiating;
        assert_eq!(
            conflicting.handle_response(RangeResponse::Unsatisfied {
                total: 10,
                validators: trusted("\"v2\""),
            }),
            Ok(RangeDecision::RestartFromZero)
        );
        assert_eq!(conflicting.snapshot().state, DownloadState::Queued);
        assert_eq!(conflicting.snapshot().durable_offset, 0);

        let mut untrusted = seeded_download(10, 10, Validators::default());
        untrusted.snapshot.state = DownloadState::Negotiating;
        assert_eq!(
            untrusted.handle_response(RangeResponse::Unsatisfied {
                total: 10,
                validators: Validators::default(),
            }),
            Ok(RangeDecision::RestartFromZero)
        );
        assert_eq!(untrusted.snapshot().state, DownloadState::Queued);
    }

    #[test]
    fn rejects_illegal_transitions_and_terminal_reopening() {
        let mut job = DownloadJob::new();
        assert!(matches!(
            job.commit_verified(),
            Err(DownloadTransitionError::InvalidTransition { .. })
        ));
        job.cancel().unwrap();
        assert!(job.snapshot().state.is_terminal());
        assert!(matches!(
            job.begin_negotiation(),
            Err(DownloadTransitionError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn checked_offset_arithmetic_and_total_bounds() {
        let mut overflow = seeded_download(u64::MAX, u64::MAX, trusted("\"v1\""));
        assert_eq!(
            overflow.record_durable_bytes(1),
            Err(DownloadTransitionError::ArithmeticOverflow)
        );

        let mut overrun = seeded_download(9, 10, trusted("\"v1\""));
        assert_eq!(
            overrun.record_durable_bytes(2),
            Err(DownloadTransitionError::ExceedsExpectedLength {
                expected: 10,
                actual: 11
            })
        );
    }

    fn persisted(state: DownloadState, offset: u64, total: Option<u64>) -> DownloadSnapshot {
        DownloadSnapshot {
            state,
            validators: trusted("\"v1\""),
            expected_total_length: total,
            durable_offset: offset,
        }
    }

    #[test]
    fn restore_keeps_durable_offset_and_state() {
        let job = DownloadJob::from_snapshot(persisted(DownloadState::Paused, 4, Some(10)))
            .expect("restore");
        assert_eq!(job.snapshot().state, DownloadState::Paused);
        assert_eq!(job.snapshot().durable_offset, 4);
        assert_eq!(job.snapshot().expected_total_length, Some(10));
        assert_eq!(job.snapshot().validators, trusted("\"v1\""));
    }

    #[test]
    fn restore_refuses_offset_beyond_total_length() {
        assert_eq!(
            DownloadJob::from_snapshot(persisted(DownloadState::Paused, 11, Some(10))),
            Err(DownloadTransitionError::ExceedsExpectedLength {
                expected: 10,
                actual: 11
            })
        );
    }

    #[test]
    fn restore_refuses_completed_states_with_bytes_mismatch() {
        for state in [DownloadState::Verifying, DownloadState::Committed] {
            assert_eq!(
                DownloadJob::from_snapshot(persisted(state, 9, Some(10))),
                Err(DownloadTransitionError::IncompleteDownload)
            );
            assert_eq!(
                DownloadJob::from_snapshot(persisted(state, 10, None)),
                Err(DownloadTransitionError::IncompleteDownload)
            );
        }
        assert!(
            DownloadJob::from_snapshot(persisted(DownloadState::Committed, 10, Some(10))).is_ok()
        );
    }

    #[test]
    fn restore_refuses_in_flight_states_that_need_a_live_response() {
        for state in [DownloadState::Negotiating, DownloadState::Downloading] {
            assert_eq!(
                DownloadJob::from_snapshot(persisted(state, 4, Some(10))),
                Err(DownloadTransitionError::InvalidTransition {
                    from: state,
                    to: DownloadState::Queued
                })
            );
        }
    }

    #[test]
    fn restored_job_resumes_range_from_exactly_the_durable_offset() {
        let mut job = DownloadJob::from_snapshot(persisted(DownloadState::Paused, 4, Some(10)))
            .expect("restore");
        job.resume().expect("resume");
        job.begin_negotiation().expect("negotiate");
        assert_eq!(
            job.handle_response(partial(3, 9, 10, trusted("\"v1\""))),
            Err(DownloadTransitionError::RangeOffsetMismatch {
                expected: 4,
                actual: 3
            })
        );

        let mut job = DownloadJob::from_snapshot(persisted(DownloadState::Paused, 4, Some(10)))
            .expect("restore");
        job.resume().expect("resume");
        job.begin_negotiation().expect("negotiate");
        assert_eq!(
            job.handle_response(partial(4, 9, 10, trusted("\"v1\""))),
            Ok(RangeDecision::Append {
                start: 4,
                end_inclusive: 9
            })
        );
        job.record_durable_bytes(6).expect("bytes");
        job.finish_download().expect("finish");
        job.commit_verified().expect("commit");
        assert_eq!(job.snapshot().durable_offset, 10);
    }

    #[test]
    fn auth_can_resume_after_credentials_and_cancel_is_terminal() {
        let mut auth = DownloadJob::new();
        auth.require_auth().unwrap();
        assert!(!auth.snapshot().state.is_terminal());
        auth.credentials_restored().unwrap();
        assert_eq!(auth.snapshot().state, DownloadState::Queued);
        auth.begin_negotiation().unwrap();

        let mut cancelled = DownloadJob::new();
        cancelled.cancel().unwrap();
        assert!(cancelled.snapshot().state.is_terminal());
        assert_eq!(
            cancelled.begin_negotiation(),
            Err(DownloadTransitionError::InvalidTransition {
                from: DownloadState::Cancelled,
                to: DownloadState::Negotiating
            })
        );
        assert_eq!(
            cancelled.pause(),
            Err(DownloadTransitionError::InvalidTransition {
                from: DownloadState::Cancelled,
                to: DownloadState::Paused
            })
        );

        let mut paused = DownloadJob::new();
        paused.pause().unwrap();
        paused.cancel().unwrap();
        assert_eq!(paused.snapshot().state, DownloadState::Cancelled);
    }
}
