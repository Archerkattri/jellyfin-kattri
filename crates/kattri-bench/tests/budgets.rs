//! M5-17 budget checks that run without a display, a running app, or a server.
//!
//! Run with `cargo test -p kattri-bench --test budgets -- --nocapture` to see the numbers.
//! Each timing is the median or p95 of five runs; no run count below five is used.
//!
//! What is asserted here:
//! - Browse scroll p95 (`BROWSE_FRAME_P95`) for the pure photo-grid window computation. This
//!   is a necessary bound only: the frame budget covers the whole frame, and this is a part of it.
//! - The harness self-test from M5-17: a known 50 ms sleep is measured within tolerance.
//!
//! What is measured and printed, with no budget asserted: event bus publish cost, queue save and
//! load, and the list paging walk. The spec gives no number for them, and it says budgets are set
//! from a first measured baseline and frozen after owner review. Asserting a number here would
//! freeze one without that review.
//!
//! What is not measured on this machine: cold start, first frame, idle resident memory, playback
//! first frame, and server-backed search and paging latency. Those tests are `#[ignore]` and panic
//! when run, so they cannot pass by accident. `docs/BLOCKERS.md` has no M5-17 entry yet. The open
//! reference machine and fixture library are in `docs/performance-budgets.md` section 2.

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use jellyfin_api::session_reporting::QueueItem;
use kattri_bench::budgets::BROWSE_FRAME_P95;
use kattri_client_core::events::{EventBus, PlaybackBusEvent};
use kattri_client_core::paging::{PAGE_SIZE, PageApplyOutcome, PagingState};
use kattri_client_core::photo_grid::{DEFAULT_OVERSCAN_SCREENS, GridSpec, PhotoGrid};
use kattri_client_core::queue::Queue;
use kattri_client_core::queue_store::{
    StoredLoadOutcome, load_queue_with_position, save_queue_with_position,
};
use kattri_client_core::search::{SEARCH_DEBOUNCE, SearchDebouncer, SearchInputAction};

const RUNS: usize = 5;

/// Sorted run times with nearest-rank percentiles.
struct Summary {
    sorted: Vec<Duration>,
}

impl Summary {
    fn new(mut samples: Vec<Duration>) -> Self {
        samples.sort();
        Self { sorted: samples }
    }

    fn median(&self) -> Duration {
        self.sorted
            .get(self.sorted.len() / 2)
            .copied()
            .unwrap_or_default()
    }

    fn p95(&self) -> Duration {
        let rank = (self.sorted.len() * 95).div_ceil(100).max(1);
        self.sorted.get(rank - 1).copied().unwrap_or_default()
    }

    fn min(&self) -> Duration {
        self.sorted.first().copied().unwrap_or_default()
    }

    fn max(&self) -> Duration {
        self.sorted.last().copied().unwrap_or_default()
    }

    fn report(&self, label: &str) {
        println!(
            "{label}: median {:?}, p95 {:?}, min {:?}, max {:?}, samples {}",
            self.median(),
            self.p95(),
            self.min(),
            self.max(),
            self.sorted.len()
        );
    }
}

/// Removes its directory when dropped, so a failed assertion still cleans up.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn item_id(n: usize) -> String {
    format!("{n:032x}")
}

#[test]
fn harness_self_test_measures_a_known_sleep() {
    const SLEEP: Duration = Duration::from_millis(50);
    // Generous upper bound: Windows timer resolution can add several milliseconds.
    const SLACK: Duration = Duration::from_millis(250);
    let runs: Vec<Duration> = (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            thread::sleep(SLEEP);
            start.elapsed()
        })
        .collect();
    let summary = Summary::new(runs);
    summary.report("harness self-test, 50 ms sleep");
    assert!(
        summary.min() >= SLEEP,
        "timer ran early: {:?}",
        summary.min()
    );
    assert!(
        summary.max() < SLEEP + SLACK,
        "timer overshot by more than the slack: {:?}",
        summary.max()
    );
}

#[test]
fn browse_scroll_window_p95_fits_the_frame_budget() {
    const SCROLL_CALLS: usize = 1_000;
    let spec = GridSpec::new(1920, 1080, 180, 180).expect("valid grid spec");
    let mut grid =
        PhotoGrid::new(100_000, spec, DEFAULT_OVERSCAN_SCREENS, 512).expect("valid photo grid");
    let max_scroll = grid.max_scroll().max(1);

    let mut calls = Vec::with_capacity(SCROLL_CALLS * RUNS);
    for _ in 0..RUNS {
        let mut offset = 0usize;
        for _ in 0..SCROLL_CALLS {
            offset = (offset + 97 * 180) % max_scroll;
            let start = Instant::now();
            black_box(grid.set_scroll(black_box(offset)));
            calls.push(start.elapsed());
        }
    }
    let summary = Summary::new(calls);
    summary.report("photo grid set_scroll, 100k items, 1920x1080");
    assert!(
        summary.p95() < BROWSE_FRAME_P95,
        "window computation p95 {:?} exceeds the 16.7 ms frame budget on its own",
        summary.p95()
    );
}

#[test]
fn event_bus_publish_cost_is_reported_without_a_budget() {
    const EVENTS: usize = 2_000;
    for subscribers in [0usize, 1] {
        let mut bus = EventBus::new(64, 8).expect("bus");
        let subs: Vec<_> = (0..subscribers)
            .map(|_| bus.subscribe().expect("subscribe"))
            .collect();

        let mut samples = Vec::with_capacity(EVENTS * RUNS);
        for _ in 0..RUNS {
            for n in 0..EVENTS {
                let event = if n.is_multiple_of(2) {
                    PlaybackBusEvent::Paused
                } else {
                    PlaybackBusEvent::Resumed
                };
                let start = Instant::now();
                let report = bus.publish(event);
                samples.push(start.elapsed());
                assert_eq!(
                    report.dropped, 0,
                    "a drained subscriber must not lose events"
                );
                assert_eq!(report.delivered, subscribers);
                // Drain after every publish, outside the timed region, so the subscriber
                // queue never fills and the timing covers only the delivery path.
                for sub in &subs {
                    assert!(sub.try_recv().is_some(), "drained subscriber had no event");
                }
            }
        }
        Summary::new(samples).report(&format!(
            "event bus publish, {subscribers} drained subscriber(s)"
        ));
    }
}

#[test]
fn queue_save_and_load_round_trip_is_reported_without_a_budget() {
    const ITEMS: usize = 500;
    const POSITION_MS: u64 = 12_345;
    let dir =
        TempDir(std::env::temp_dir().join(format!("kattri-bench-budgets-{}", std::process::id())));
    let path = dir.0.join("queue.json");

    let mut queue = Queue::with_seed(3);
    for n in 0..ITEMS {
        queue
            .add(QueueItem::new(item_id(n)))
            .expect("queue accepts items up to its bound");
    }

    let mut saves = Vec::with_capacity(RUNS);
    let mut loads = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let start = Instant::now();
        save_queue_with_position(&path, &queue, POSITION_MS).expect("queue saves");
        saves.push(start.elapsed());

        let start = Instant::now();
        let outcome = load_queue_with_position(&path).expect("queue loads");
        loads.push(start.elapsed());

        let StoredLoadOutcome::Loaded(stored) = outcome else {
            panic!("saved queue did not load back");
        };
        assert_eq!(stored.position_ms, POSITION_MS);
        assert_eq!(stored.queue.len(), ITEMS);
        assert!(
            stored
                .queue
                .items()
                .iter()
                .zip(queue.items())
                .all(|(a, b)| a.id == b.id),
            "loaded queue order differs from the saved order"
        );
    }
    Summary::new(saves).report(&format!("queue save, {ITEMS} items"));
    Summary::new(loads).report(&format!("queue load, {ITEMS} items"));
}

/// Walks a library of `total` items page by page, checking each apply and stale replay.
/// Returns the loaded count and the number of pages requested.
fn walk_library(total: usize) -> (usize, usize) {
    let mut paging = PagingState::new(5);
    let mut request = paging
        .request_initial_page()
        .expect("fresh paging state")
        .expect("initial page is requested");
    let mut pages = 0usize;
    loop {
        pages += 1;
        let start = request.start_index;
        let count = PAGE_SIZE.min(total.saturating_sub(start));
        let ids = (start..start + count).map(item_id);
        let outcome = paging
            .apply_page(request, ids, Some(total))
            .expect("page applies");
        assert!(
            matches!(
                outcome,
                PageApplyOutcome::Applied {
                    duplicate_count: 0,
                    ..
                }
            ),
            "page {pages} was stale or contained duplicates: {outcome:?}"
        );
        let replay = paging
            .apply_page(request, std::iter::empty(), Some(total))
            .expect("replay is accepted as stale");
        assert_eq!(
            replay,
            PageApplyOutcome::Stale,
            "a replayed page must be stale"
        );

        if paging.reached_end() {
            break;
        }
        let last = paging.loaded_count().saturating_sub(1);
        request = paging
            .request_for_focus(last)
            .expect("focus request is accepted")
            .expect("focus on the last loaded row requests the next page");
    }
    (paging.loaded_count(), pages)
}

#[test]
fn list_paging_walk_is_reported_without_a_budget() {
    // 40 full pages and a short last page, so the end-of-list path runs.
    const TOTAL: usize = 2_025;
    let mut per_page = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let start = Instant::now();
        let (loaded, pages) = walk_library(TOTAL);
        let elapsed = start.elapsed();
        assert_eq!(loaded, TOTAL, "every item must be loaded exactly once");
        assert_eq!(pages, 41, "40 full pages and one short page");
        let pages = u32::try_from(pages).unwrap_or(u32::MAX).max(1);
        per_page.push(elapsed / pages);
    }
    Summary::new(per_page).report(&format!("list paging, {TOTAL} items, per page incl. ids"));
}

#[test]
fn search_debounce_holds_then_emits_once() {
    let mut debouncer = SearchDebouncer::default();
    assert!(matches!(
        debouncer.input("ab", Duration::ZERO),
        SearchInputAction::Debouncing { .. }
    ));
    assert!(
        debouncer
            .take_ready(SEARCH_DEBOUNCE - Duration::from_millis(1))
            .is_none(),
        "no request before the debounce interval"
    );
    let request = debouncer
        .take_ready(SEARCH_DEBOUNCE)
        .expect("request after the debounce interval");
    assert_eq!(request.query, "ab");
    assert!(
        debouncer.take_ready(SEARCH_DEBOUNCE * 2).is_none(),
        "one request per input"
    );
    assert!(debouncer.is_current_response(&request));
}

// Not measured on this machine. Each test panics when run so `--ignored` cannot pass by accident.
// The spec-named budgets below need a display, a running app, or a server.

#[test]
#[ignore = "NOT MEASURED: needs a release build and a display"]
fn cold_start_to_first_frame_is_not_measured_here() {
    panic!(
        "NOT MEASURED: cold start budget needs a release build and a display; see docs/performance-budgets.md section 3 M1"
    );
}

#[test]
#[ignore = "NOT MEASURED: needs the running app with one library loaded"]
fn idle_resident_memory_is_not_measured_here() {
    panic!(
        "NOT MEASURED: idle RSS budget needs the running app; see docs/performance-budgets.md section 3 M2"
    );
}

#[test]
#[ignore = "NOT MEASURED: needs a display at 1080p"]
fn browse_scroll_frame_p95_on_a_display_is_not_measured_here() {
    panic!("NOT MEASURED: frame p95 needs a display; see docs/performance-budgets.md section 3 M3");
}

#[test]
#[ignore = "NOT MEASURED: needs a running app and a Jellyfin server"]
fn playback_first_frame_is_not_measured_here() {
    panic!(
        "NOT MEASURED: playback first frame needs libmpv and a server; see docs/performance-budgets.md section 3 M4"
    );
}

#[test]
#[ignore = "NOT MEASURED: needs a Jellyfin server with a fixture library"]
fn server_search_and_paging_latency_is_not_measured_here() {
    panic!(
        "NOT MEASURED: server-backed search and paging latency need a fixture library; no spec budget exists"
    );
}
