//! M4-06 latency acceptance: subscribers must not change the playback reporter's per-tick
//! publish cost beyond noise.
//!
//! Playback time is simulated, so the 600-second run never sleeps. Only the `publish_at`
//! loop is timed. Subscriber draining happens after the timer stops, because the question is
//! what the publisher pays, not what a plugin thread pays.

use std::time::{Duration, Instant};

use kattri_client_core::events::{EventBus, PlaybackBusEvent, Subscription};

/// Simulated playback length, at one progress tick per second.
const SIM_SECONDS: u64 = 600;
/// The throttle accepts one progress event per five seconds, so 600 ticks give 120.
const EXPECTED_ACCEPTED: u64 = 120;
const EXPECTED_THROTTLED: u64 = SIM_SECONDS - EXPECTED_ACCEPTED;
const SUBSCRIBERS: usize = 3;
const MAX_SUBSCRIBERS: usize = 8;
/// Zero-subscriber and three-subscriber samples are interleaved. The median of each is compared.
const SAMPLES: usize = 7;
/// Batching runs per sample keeps each sample long enough for the clock to resolve
/// nanosecond-scale publishes.
const RUNS_PER_SAMPLE: usize = 20;
/// Queues hold every accepted event, so the timed run never evicts. The drain afterwards then
/// proves each subscriber received everything.
const QUEUE_CAPACITY: usize = 256;
/// Allowed slowdown over the zero-subscriber median. Three subscribers only do extra work on
/// the 120 accepted ticks, so a healthy bus should land well under this factor.
const MAX_RATIO: f64 = 4.0;
/// Absolute allowance per publish. A tick budget at 1 Hz is one second, so this is far below
/// anything visible. It also absorbs timer and scheduler noise when the baseline is tiny.
const FLOOR_NS: f64 = 1_000.0;

/// Publishes one simulated playback and returns the elapsed time of the tick loop and the
/// number of progress events the bus accepted.
fn timed_playback(bus: &mut EventBus) -> (Duration, u64) {
    bus.publish_at(Duration::ZERO, PlaybackBusEvent::Started);
    let mut accepted = 0;
    let started_at = Instant::now();
    for tick in 0..SIM_SECONDS {
        let report = bus.publish_at(
            Duration::from_secs(tick),
            PlaybackBusEvent::Position {
                position_ms: tick * 1_000,
            },
        );
        if !report.throttled {
            accepted += 1;
        }
    }
    (started_at.elapsed(), accepted)
}

/// Checks the throttle count on the bus and that every subscriber got every accepted event
/// plus the start event, with nothing lost to overflow.
fn check_run(bus: &EventBus, subs: &[Subscription], accepted: u64) {
    assert_eq!(
        accepted, EXPECTED_ACCEPTED,
        "progress events accepted over {SIM_SECONDS} simulated seconds at 1 Hz"
    );
    assert_eq!(bus.throttled_total(), EXPECTED_THROTTLED);
    for sub in subs {
        let mut received = 0u64;
        let mut progress = 0u64;
        while let Some(event) = sub.try_recv() {
            received += 1;
            if matches!(event, PlaybackBusEvent::Position { .. }) {
                progress += 1;
            }
        }
        assert_eq!(progress, EXPECTED_ACCEPTED);
        assert_eq!(
            received,
            EXPECTED_ACCEPTED + 1,
            "accepted progress plus Started"
        );
        assert_eq!(sub.dropped(), 0);
    }
}

/// Mean nanoseconds per publish for one configuration, over `RUNS_PER_SAMPLE` simulated runs.
fn sample_ns_per_publish(subscriber_count: usize) -> f64 {
    let mut total = Duration::ZERO;
    for _ in 0..RUNS_PER_SAMPLE {
        let mut bus = EventBus::new(QUEUE_CAPACITY, MAX_SUBSCRIBERS).expect("valid bus config");
        let subs: Vec<Subscription> = (0..subscriber_count)
            .map(|_| bus.subscribe().expect("subscriber slot"))
            .collect();
        let (elapsed, accepted) = timed_playback(&mut bus);
        total += elapsed;
        check_run(&bus, &subs, accepted);
    }
    // Only the tick loop is timed, so the start event is not in the denominator.
    let publishes = (RUNS_PER_SAMPLE as u64 * SIM_SECONDS) as f64;
    total.as_nanos() as f64 / publishes
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

#[test]
fn three_subscribers_keep_publish_latency_within_bound_over_ten_minutes() {
    // Warm up both paths so the first timed sample does not carry cold-start cost.
    let _ = sample_ns_per_publish(0);
    let _ = sample_ns_per_publish(SUBSCRIBERS);

    let mut baseline = Vec::with_capacity(SAMPLES);
    let mut with_subscribers = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        baseline.push(sample_ns_per_publish(0));
        with_subscribers.push(sample_ns_per_publish(SUBSCRIBERS));
    }

    let baseline_ns = median(baseline);
    let subscribed_ns = median(with_subscribers);
    let bound_ns = MAX_RATIO * baseline_ns + FLOOR_NS;

    println!(
        "M4-06 progress ticks: {SIM_SECONDS} simulated seconds at 1 Hz, accepted {EXPECTED_ACCEPTED}, throttled {EXPECTED_THROTTLED}"
    );
    println!(
        "M4-06 baseline, 0 subscribers: median {baseline_ns:.1} ns per publish ({SAMPLES} samples x {RUNS_PER_SAMPLE} runs)"
    );
    println!(
        "M4-06 {SUBSCRIBERS} subscribers: median {subscribed_ns:.1} ns per publish (ratio {:.2})",
        subscribed_ns / baseline_ns
    );
    println!("M4-06 bound: {bound_ns:.1} ns ({MAX_RATIO}x baseline + {FLOOR_NS:.0} ns floor)");

    assert!(
        subscribed_ns <= bound_ns,
        "median publish with {SUBSCRIBERS} subscribers ({subscribed_ns:.1} ns) exceeds bound ({bound_ns:.1} ns)"
    );
}
