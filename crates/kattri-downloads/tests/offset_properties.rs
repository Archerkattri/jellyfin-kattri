//! Property-style tests over random operation sequences (fixed seeds).
//!
//! Invariants checked after every operation:
//! - the durable offset never decreases (the only reset is a 200 restart, not used here);
//! - `Committed` is reached only from `Verifying`;
//! - the offset never exceeds the expected total length.

use kattri_client_core::downloads::{DownloadJob, DownloadState, RangeResponse, Validators};

const TOTAL: u64 = 64;

/// Small deterministic generator so the sequences are reproducible from the seed.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

fn strong() -> Validators {
    Validators {
        etag: Some("\"v1\"".to_owned()),
        last_modified: None,
    }
}

fn run_sequence(seed: u64, steps: usize) {
    let mut rng = Lcg(seed);
    let mut job = DownloadJob::new();
    let mut prev_offset = 0u64;
    let mut prev_state = job.snapshot().state;

    for step in 0..steps {
        let op = rng.next() % 10;
        let chunk = 1 + rng.next() % 8;
        let durable = job.snapshot().durable_offset;
        // Ignoring Err is intentional: illegal transitions must leave the job unchanged.
        match op {
            0 => {
                let _ = job.pause();
            }
            1 => {
                let _ = job.resume();
            }
            2 => {
                let _ = job.begin_negotiation();
            }
            3 => {
                if durable + chunk < TOTAL {
                    let _ = job.handle_response(RangeResponse::Partial {
                        start: durable,
                        end_inclusive: durable + chunk - 1,
                        total: TOTAL,
                        validators: strong(),
                    });
                }
            }
            4 | 5 => {
                let _ = job.record_durable_bytes(chunk);
            }
            6 => {
                let _ = job.finish_download();
            }
            7 => {
                let _ = job.commit_verified();
            }
            8 => {
                let _ = job.require_auth();
                let _ = job.credentials_restored();
            }
            _ => {
                let _ = job.cancel();
            }
        }

        let snapshot = job.snapshot();
        assert!(
            snapshot.durable_offset >= prev_offset,
            "seed {seed} step {step}: offset moved backwards {prev_offset} -> {}",
            snapshot.durable_offset
        );
        if let Some(expected) = snapshot.expected_total_length {
            assert!(
                snapshot.durable_offset <= expected,
                "seed {seed} step {step}: offset {} past expected {expected}",
                snapshot.durable_offset
            );
        }
        if snapshot.state == DownloadState::Committed && prev_state != DownloadState::Committed {
            assert_eq!(
                prev_state,
                DownloadState::Verifying,
                "seed {seed} step {step}: committed without verifying"
            );
        }
        prev_offset = snapshot.durable_offset;
        prev_state = snapshot.state;
    }
}

#[test]
fn offset_never_decreases_and_commit_requires_verifying_fixed_seeds() {
    for seed in [1u64, 2, 3, 42, 7777, 99_991] {
        run_sequence(seed, 400);
    }
}
