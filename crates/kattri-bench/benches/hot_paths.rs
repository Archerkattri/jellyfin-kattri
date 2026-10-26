//! Criterion benchmarks for three pure hot paths:
//!
//! - queue shuffle (`Queue::set_shuffle`), up to the queue bound `MAX_QUEUE_LEN`;
//! - photo grid scroll window (`PhotoGrid::set_scroll`) on a 100,000 item grid;
//! - keymap lookup (`Keymap::lookup_key`) on the bundled keyboard fixture.
//!
//! Targets, not measurements: none of these numbers has been measured. The only
//! target that applies is the browse frame budget in `docs/performance-budgets.md`
//! (M3, 16.7 ms at 60 fps). Each call here should stay far below that budget. A
//! measured baseline on the reference machine is required before any target is
//! frozen, and no result is recorded in this file.
//!
//! Run with `cargo bench -p kattri-bench`. Compile only with
//! `cargo bench -p kattri-bench --no-run`.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use jellyfin_api::session_reporting::QueueItem;
use kattri_client_core::photo_grid::{DEFAULT_OVERSCAN_SCREENS, GridSpec, PhotoGrid};
use kattri_client_core::queue::{MAX_QUEUE_LEN, Queue};
use kattri_input::keymap::{KeyChord, parse_keymap};

/// The legacy keyboard keymap fixture shipped with `kattri-input`.
const KEYBOARD_KEYMAP: &str = include_str!("../../kattri-input/tests/fixtures/keyboard.json");

fn queue_with(len: usize) -> Queue {
    let mut queue = Queue::with_seed(7);
    for n in 0..len {
        queue
            .add(QueueItem::new(format!("{n:032x}")))
            .expect("queue accepts items up to MAX_QUEUE_LEN");
    }
    queue
}

fn bench_queue_shuffle(c: &mut Criterion) {
    let mut group = c.benchmark_group("queue_shuffle");
    for len in [100usize, 500, MAX_QUEUE_LEN] {
        group.throughput(Throughput::Elements(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, &len| {
            b.iter_batched(
                || queue_with(len),
                |mut queue| {
                    queue.set_shuffle(true);
                    black_box(queue)
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_photo_grid_window(c: &mut Criterion) {
    let spec = GridSpec::new(1920, 1080, 180, 180).expect("valid grid spec");
    let item_count = 100_000;
    let mut grid =
        PhotoGrid::new(item_count, spec, DEFAULT_OVERSCAN_SCREENS, 512).expect("valid photo grid");
    // Step through the whole scroll range so each call computes a different window.
    let max_scroll = grid.max_scroll().max(1);
    c.bench_function("photo_grid_set_scroll_100k", |b| {
        let mut offset = 0usize;
        b.iter(|| {
            offset = (offset + 97 * 180) % max_scroll;
            black_box(grid.set_scroll(black_box(offset)))
        });
    });
}

fn bench_keymap_lookup(c: &mut Criterion) {
    let keymap = parse_keymap(KEYBOARD_KEYMAP).expect("bundled keyboard keymap parses");
    let hit = KeyChord::parse("Left").expect("valid chord");
    let miss = KeyChord::parse("Ctrl+Alt+Shift+Meta+F12").expect("valid chord");
    let mut group = c.benchmark_group("keymap_lookup");
    group.bench_function("hit", |b| {
        b.iter(|| black_box(keymap.lookup_key(black_box(&hit))));
    });
    group.bench_function("miss", |b| {
        b.iter(|| black_box(keymap.lookup_key(black_box(&miss))));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_queue_shuffle,
    bench_photo_grid_window,
    bench_keymap_lookup
);
criterion_main!(benches);
