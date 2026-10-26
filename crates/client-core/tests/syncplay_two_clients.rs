//! Two simulated clients on one reference clock, each with its own local clock offset and
//! its own ping-based estimate. Both receive the same server command. This is an in-process
//! model with synthetic delays, not a run against `mock-jellyfin` or a Jellyfin server.

use kattri_client_core::syncplay::{
    ClockEstimate, ClockEstimator, ClockSample, GroupCommand, LocalAction, MAX_ROUND_TRIP_MS,
    map_command,
};

/// Server clock reads `reference + SERVER_BASE_MS`.
const SERVER_BASE_MS: i64 = 1_000_000;
const PING_INTERVAL_MS: i64 = 1_000;
const PING_COUNT: i64 = 60;
/// Reference time at which the server sends the command.
const COMMAND_REF_MS: i64 = 60_000;
/// Ten minutes of evaluation after the command, as in the M4-11 LAN acceptance run.
const RUN_MS: i64 = 600_000;
const TOLERANCE_MS: i64 = 500;
const COMMAND_POSITION_MS: i64 = 60_000;

/// A client whose local clock reads `reference + local_shift_ms`.
struct SimClient {
    local_shift_ms: i64,
    estimator: ClockEstimator,
}

impl SimClient {
    fn new(local_shift_ms: i64) -> Self {
        Self {
            local_shift_ms,
            estimator: ClockEstimator::new(),
        }
    }

    fn local(&self, reference_ms: i64) -> i64 {
        reference_ms + self.local_shift_ms
    }

    /// The true server-minus-local offset for this client.
    fn true_offset(&self) -> i64 {
        SERVER_BASE_MS - self.local_shift_ms
    }

    /// One ping: sent at `send_ref`, with asymmetric legs of `outbound` and `inbound` ms.
    fn ping(&mut self, send_ref: i64, outbound: i64, inbound: i64) {
        let sent_local_ms = self.local(send_ref);
        self.estimator.add_sample(ClockSample {
            sent_local_ms,
            received_local_ms: sent_local_ms + outbound + inbound,
            server_ms: send_ref + outbound + SERVER_BASE_MS,
        });
    }
}

/// Deterministic delays in `lo..=hi` from a fixed-seed generator. Keeps the run reproducible.
fn next_delay(state: &mut u64, lo: i64, hi: i64) -> i64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let span = u64::try_from(hi - lo + 1).expect("positive span");
    lo + i64::try_from((*state >> 33) % span).expect("span fits")
}

#[test]
fn two_clients_reach_the_same_target_for_one_command() {
    let mut seed = 0x5EED_u64;
    let mut clients = [SimClient::new(-5_000), SimClient::new(40_000)];

    for ping in 0..PING_COUNT {
        let send_ref = ping * PING_INTERVAL_MS;
        for (index, client) in clients.iter_mut().enumerate() {
            let outbound = next_delay(&mut seed, 5, 150);
            let inbound = next_delay(&mut seed, 5, 150);
            // Client B loses every seventh reply. Nothing is recorded for a lost reply.
            if index == 1 && ping % 7 == 6 {
                continue;
            }
            client.ping(send_ref, outbound, inbound);
        }
    }

    let estimates: Vec<ClockEstimate> = clients
        .iter()
        .map(|client| client.estimator.estimate().expect("pings were recorded"))
        .collect();
    for (client, estimate) in clients.iter().zip(&estimates) {
        let error = (estimate.offset_ms - client.true_offset()).abs();
        assert!(
            error <= estimate.error_bound_ms,
            "estimate error {error} exceeds its bound {}",
            estimate.error_bound_ms
        );
        assert!(estimate.error_bound_ms <= MAX_ROUND_TRIP_MS / 2 + 1);
    }

    let command_server_ms = COMMAND_REF_MS + SERVER_BASE_MS;
    let command = GroupCommand::Unpause {
        position_ms: COMMAND_POSITION_MS,
        server_time_ms: command_server_ms,
    };

    let mut max_pair_gap = 0_i64;
    let mut max_error_vs_server = [0_i64; 2];
    let mut reference = COMMAND_REF_MS;
    while reference <= COMMAND_REF_MS + RUN_MS {
        // The true target at this reference instant is the command position plus the
        // server time elapsed since the command.
        let true_target = COMMAND_POSITION_MS + (reference + SERVER_BASE_MS - command_server_ms);
        let mut targets = [0_i64; 2];
        for (index, client) in clients.iter().enumerate() {
            let mapped = map_command(command, estimates[index], client.local(reference), true)
                .expect("command is not stamped in the future");
            targets[index] = mapped.position_ms;
            let error = (mapped.position_ms - true_target).abs();
            max_error_vs_server[index] = max_error_vs_server[index].max(error);
        }
        max_pair_gap = max_pair_gap.max((targets[0] - targets[1]).abs());
        reference += 1_000;
    }

    assert!(
        max_pair_gap <= TOLERANCE_MS,
        "clients differ by {max_pair_gap} ms"
    );
    for (index, error) in max_error_vs_server.iter().enumerate() {
        assert!(
            *error <= TOLERANCE_MS,
            "client {index} is {error} ms from server time"
        );
        assert!(
            *error <= estimates[index].error_bound_ms,
            "client {index} error {error} ms exceeds its reported bound"
        );
    }

    // Both clients apply the command at the same server instant, within the tolerance.
    let apply_refs: Vec<i64> = clients
        .iter()
        .zip(&estimates)
        .map(|(client, estimate)| {
            let mapped = map_command(command, *estimate, client.local(COMMAND_REF_MS), true)
                .expect("valid command");
            assert_eq!(mapped.action, LocalAction::Unpause);
            mapped.apply_at_local_ms - client.local_shift_ms
        })
        .collect();
    assert!(
        (apply_refs[0] - apply_refs[1]).abs() <= TOLERANCE_MS,
        "apply instants differ: {apply_refs:?}"
    );
    for apply_ref in apply_refs {
        assert!((apply_ref - COMMAND_REF_MS).abs() <= TOLERANCE_MS);
    }
}
