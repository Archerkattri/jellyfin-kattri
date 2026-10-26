//! Soak test for the queue and event bus loops (M5-18, logic level only).
//!
//! This is not the M5-18 app soak. It runs no app, browses nothing, plays nothing, and drives no
//! server. It checks that repeated subscribe, publish, drop, shuffle, and track-change cycles
//! leave the bus and queue in a steady state.
//!
//! Default run (every `cargo test`, including CI): `queue_and_event_bus_bounded_run`, a fixed
//! 200 iterations. It finishes in well under a second.
//!
//! Opt-in run (ignored by default): `queue_and_event_bus_do_not_grow`. Choose the length with:
//!
//! - `SOAK_ITERATIONS=100000`: run that many iterations (default 1,000).
//! - `SOAK_DURATION_SECS=28800`: run for that many wall-clock seconds instead. This takes
//!   precedence over `SOAK_ITERATIONS`.
//!
//! ```text
//! SOAK_ITERATIONS=1000 cargo test -p kattri-bench --test soak -- --ignored --nocapture
//! SOAK_DURATION_SECS=28800 cargo test -p kattri-bench --test soak -- --ignored --nocapture
//! ```
//!
//! The second command is the 8-hour run for this loop only. It has never been run, and no result
//! is recorded in this repository. The 8-hour M5-18 soak (browse, search, play, seek, pause,
//! stop, return home, server restart, network blips, sleep and resume) needs the app running on
//! the reference machine, with crash dumps from M4-21 if enabled. This file does not cover it.
//!
//! Failure conditions: a live subscriber count that changes, a subscription slot that is not
//! released on drop, or a single iteration that takes 10 seconds or longer (M5-18 "zero hangs
//! over 10 seconds").

use std::time::{Duration, Instant};

use jellyfin_api::session_reporting::QueueItem;
use kattri_client_core::events::{BusError, EventBus, PlaybackBusEvent};
use kattri_client_core::queue::Queue;

const BOUNDED_ITERATIONS: usize = 200;
const DEFAULT_ITERATIONS: usize = 1_000;
const QUEUE_ITEMS: usize = 16;
const QUEUE_CAPACITY: usize = 64;
const MAX_SUBSCRIBERS: usize = 8;
const HANG_THRESHOLD: Duration = Duration::from_secs(10);

/// How long the loop runs: a fixed iteration count, or a wall-clock duration.
#[derive(Clone, Copy)]
enum Limit {
    Iterations(usize),
    Duration(Duration),
}

fn configured_limit() -> Limit {
    if let Some(secs) = std::env::var("SOAK_DURATION_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Limit::Duration(Duration::from_secs(secs));
    }
    let iterations = std::env::var("SOAK_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    Limit::Iterations(iterations)
}

fn run_loop(limit: Limit) {
    let started = Instant::now();
    let mut bus = EventBus::new(QUEUE_CAPACITY, MAX_SUBSCRIBERS).expect("bus");
    let mut queue = Queue::with_seed(11);
    for n in 0..QUEUE_ITEMS {
        queue
            .add(QueueItem::new(format!("{n:032x}")))
            .expect("queue accepts soak items");
    }

    let baseline = bus.subscriber_count();
    assert_eq!(baseline, 0, "a new bus has no subscribers");

    let mut iterations = 0usize;
    let mut slowest = Duration::ZERO;
    loop {
        let done = match limit {
            Limit::Iterations(count) => iterations >= count,
            Limit::Duration(length) => started.elapsed() >= length,
        };
        if done {
            break;
        }

        let iteration_start = Instant::now();
        {
            let subscription = bus.subscribe().expect("subscribe within the limit");
            bus.publish(PlaybackBusEvent::Started);
            bus.publish(PlaybackBusEvent::Position {
                position_ms: iterations as u64,
            });
            let _ = subscription.try_recv();
        }
        queue.set_shuffle(iterations.is_multiple_of(2));
        let _ = queue.next_track();
        slowest = slowest.max(iteration_start.elapsed());

        assert_eq!(
            bus.subscriber_count(),
            baseline,
            "live subscriber count changed at iteration {iterations}"
        );
        assert!(
            iteration_start.elapsed() < HANG_THRESHOLD,
            "iteration {iterations} took {:?}, over the 10 s hang threshold",
            iteration_start.elapsed()
        );
        iterations += 1;
    }

    // Every dropped subscription must have released its slot: the full limit is available
    // again, and one more subscriber is refused.
    let mut held = Vec::new();
    for _ in 0..MAX_SUBSCRIBERS {
        held.push(bus.subscribe().expect("slot released after drops"));
    }
    assert!(
        matches!(bus.subscribe(), Err(BusError::SubscriberLimit { .. })),
        "the bus must refuse subscribers beyond its limit"
    );
    drop(held);
    assert_eq!(bus.subscriber_count(), baseline);
    println!(
        "soak: {iterations} iterations in {:?}, slowest iteration {slowest:?}, subscriber count stayed at {baseline}",
        started.elapsed()
    );
}

#[test]
fn queue_and_event_bus_bounded_run() {
    run_loop(Limit::Iterations(BOUNDED_ITERATIONS));
}

#[test]
#[ignore = "soak test; run with --ignored and SOAK_ITERATIONS or SOAK_DURATION_SECS"]
fn queue_and_event_bus_do_not_grow() {
    run_loop(configured_limit());
}
