//! Pure photo grid model: which cells are visible or near-visible for a scrolled
//! grid, the order thumbnail decode requests are issued in, the pending requests
//! to cancel when the window moves, and a bounded LRU of decoded thumbnails.
//! Performs no image decoding and no I/O. The caller reports completions back.
//!
//! Items are laid out row-major in a grid of `columns` cells. Rows are
//! `cell_height` pixels tall. The visible set is the rows the viewport covers.
//! The window (the near-visible set) adds `overscan_screens` viewport heights of
//! rows above and below.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
#[cfg(any(test, feature = "photo-counters"))]
use std::sync::{Arc, Mutex};

/// Screens of rows kept above and below the visible rows. One screen each way
/// is the M3-06 default. Zero requests only the visible set.
pub const DEFAULT_OVERSCAN_SCREENS: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotoGridError {
    /// A viewport or cell dimension is zero.
    ZeroSize,
    /// The decoded cache capacity is zero.
    ZeroCapacity,
}

/// Viewport and cell sizes, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSpec {
    pub viewport_width: usize,
    pub viewport_height: usize,
    pub cell_width: usize,
    pub cell_height: usize,
}

impl GridSpec {
    pub fn new(
        viewport_width: usize,
        viewport_height: usize,
        cell_width: usize,
        cell_height: usize,
    ) -> Result<Self, PhotoGridError> {
        let dimensions = [viewport_width, viewport_height, cell_width, cell_height];
        if dimensions.contains(&0) {
            return Err(PhotoGridError::ZeroSize);
        }
        Ok(Self {
            viewport_width,
            viewport_height,
            cell_width,
            cell_height,
        })
    }

    /// Cells per row. At least one, even when a cell is wider than the viewport.
    pub fn columns(&self) -> usize {
        (self.viewport_width / self.cell_width).max(1)
    }
}

/// Requests to issue and pending requests to cancel after a scroll or a list
/// change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodePlan {
    /// Newly requested items. Visible items come first in index order, then the
    /// rest of the window, nearest first.
    pub requests: Vec<usize>,
    /// Pending items that left the window. Their results must be discarded.
    pub cancelled: Vec<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    /// The decode is cached. `evicted` is the least recently used entry dropped to
    /// stay within capacity.
    Cached { evicted: Option<usize> },
    /// The request was no longer pending (cancelled or never requested). The
    /// result is discarded.
    Discarded,
}

/// Read-only copy of the decode counters at one moment.
#[cfg(any(test, feature = "photo-counters"))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecodeSnapshot {
    /// Decode requests issued.
    pub requested: u64,
    /// Finished decodes that were cached, in total.
    pub decoded_total: u64,
    /// Finished decodes of items in the visible rows.
    pub decoded_visible: u64,
    /// Finished decodes of near-visible items outside the visible rows.
    pub decoded_prefetch: u64,
    /// Pending requests cancelled because they left the window.
    pub cancelled: u64,
    /// Finished decodes whose results were discarded, and failed decodes.
    pub dropped: u64,
    /// Cached items that entered the window, so no decode was needed.
    pub cache_hits: u64,
    /// Item indices in the order their decodes finished.
    pub decoded_indices: Vec<usize>,
}

/// Live decode counters. Each decode path records here. Counters use `u64` and
/// saturate rather than wrap.
#[cfg(any(test, feature = "photo-counters"))]
#[derive(Clone, Debug, Default)]
pub struct DecodeCounters {
    #[cfg(any(test, feature = "photo-counters"))]
    snapshot: Arc<Mutex<DecodeSnapshot>>,
}

#[cfg(any(test, feature = "photo-counters"))]
impl DecodeCounters {
    pub fn record_request(&self) {
        #[cfg(any(test, feature = "photo-counters"))]
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.requested = snapshot.requested.saturating_add(1);
        }
    }

    pub fn record_cancelled(&self, count: usize) {
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        #[cfg(any(test, feature = "photo-counters"))]
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.cancelled = snapshot.cancelled.saturating_add(count);
        }
    }

    pub fn record_cache_hit(&self) {
        #[cfg(any(test, feature = "photo-counters"))]
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.cache_hits = snapshot.cache_hits.saturating_add(1);
        }
    }

    pub fn record_decode(&self, index: usize, visible: bool) {
        #[cfg(any(test, feature = "photo-counters"))]
        let Ok(mut guard) = self.snapshot.lock() else {
            return;
        };
        #[cfg(any(test, feature = "photo-counters"))]
        let snapshot = &mut *guard;
        #[cfg(any(test, feature = "photo-counters"))]
        {
            snapshot.decoded_total = snapshot.decoded_total.saturating_add(1);
            if visible {
                snapshot.decoded_visible = snapshot.decoded_visible.saturating_add(1);
            } else {
                snapshot.decoded_prefetch = snapshot.decoded_prefetch.saturating_add(1);
            }
            snapshot.decoded_indices.push(index);
        }
    }

    pub fn record_dropped(&self) {
        #[cfg(any(test, feature = "photo-counters"))]
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.dropped = snapshot.dropped.saturating_add(1);
        }
    }

    pub fn snapshot(&self) -> DecodeSnapshot {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default()
    }
}

/// Least-recently-used set of decoded item indices, bounded by capacity.
#[derive(Clone, Debug)]
struct DecodedCache {
    capacity: usize,
    tick: u64,
    last_used: BTreeMap<usize, u64>,
}

impl DecodedCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            tick: 0,
            last_used: BTreeMap::new(),
        }
    }

    fn contains(&self, index: usize) -> bool {
        self.last_used.contains_key(&index)
    }

    /// Mark an entry as recently used. Does nothing for an entry not cached.
    fn touch(&mut self, index: usize) {
        self.tick = self.tick.saturating_add(1);
        let tick = self.tick;
        if let Some(last) = self.last_used.get_mut(&index) {
            *last = tick;
        }
    }

    /// Insert an entry and evict the least recently used one if over capacity.
    fn insert(&mut self, index: usize) -> Option<usize> {
        self.tick = self.tick.saturating_add(1);
        self.last_used.insert(index, self.tick);
        if self.last_used.len() <= self.capacity {
            return None;
        }
        let oldest = self
            .last_used
            .iter()
            .min_by_key(|(_, tick)| **tick)
            .map(|(&index, _)| index)?;
        self.last_used.remove(&oldest);
        Some(oldest)
    }

    fn retain_below(&mut self, count: usize) {
        self.last_used.retain(|&index, _| index < count);
    }
}

/// Grid windowing and thumbnail request bookkeeping for one folder listing.
pub struct PhotoGrid {
    item_count: usize,
    spec: GridSpec,
    overscan_screens: usize,
    scroll_y: usize,
    visible: Range<usize>,
    window: Range<usize>,
    pending: BTreeSet<usize>,
    cache: DecodedCache,
    #[cfg(any(test, feature = "photo-counters"))]
    counters: DecodeCounters,
}

impl PhotoGrid {
    /// A grid over `item_count` items. `cache_capacity` bounds decoded thumbnails.
    pub fn new(
        item_count: usize,
        spec: GridSpec,
        overscan_screens: usize,
        cache_capacity: usize,
    ) -> Result<Self, PhotoGridError> {
        if cache_capacity == 0 {
            return Err(PhotoGridError::ZeroCapacity);
        }
        let mut grid = Self {
            item_count,
            spec,
            overscan_screens,
            scroll_y: 0,
            visible: 0..0,
            window: 0..0,
            pending: BTreeSet::new(),
            cache: DecodedCache::new(cache_capacity),
            #[cfg(any(test, feature = "photo-counters"))]
            counters: DecodeCounters::default(),
        };
        let (visible, window) = grid.ranges_at(0);
        grid.visible = visible;
        grid.window = window;
        Ok(grid)
    }

    pub fn item_count(&self) -> usize {
        self.item_count
    }

    /// Scroll offset in pixels, clamped to `max_scroll`.
    pub fn scroll_y(&self) -> usize {
        self.scroll_y
    }

    /// Largest scroll offset that keeps the last row on screen.
    pub fn max_scroll(&self) -> usize {
        let total = self.rows().saturating_mul(self.spec.cell_height);
        total.saturating_sub(self.spec.viewport_height)
    }

    /// Item indices in the visible rows.
    pub fn visible(&self) -> Range<usize> {
        self.visible.clone()
    }

    /// Item indices in the near-visible window: visible rows plus overscan.
    pub fn window(&self) -> Range<usize> {
        self.window.clone()
    }

    /// Indices with a request in flight, in ascending order.
    pub fn pending(&self) -> Vec<usize> {
        self.pending.iter().copied().collect()
    }

    pub fn is_pending(&self, index: usize) -> bool {
        self.pending.contains(&index)
    }

    pub fn is_cached(&self, index: usize) -> bool {
        self.cache.contains(index)
    }

    pub fn cached_len(&self) -> usize {
        self.cache.last_used.len()
    }

    /// Decode counters since construction.
    #[cfg(any(test, feature = "photo-counters"))]
    pub fn decode_counters(&self) -> DecodeSnapshot {
        self.counters.snapshot()
    }

    fn rows(&self) -> usize {
        self.item_count.div_ceil(self.spec.columns())
    }

    /// Visible and window item ranges for a scroll offset. The offset must
    /// already be clamped.
    fn ranges_at(&self, scroll_y: usize) -> (Range<usize>, Range<usize>) {
        let columns = self.spec.columns();
        let rows = self.rows();
        let cell_height = self.spec.cell_height;
        let viewport_height = self.spec.viewport_height;

        let first = (scroll_y / cell_height).min(rows);
        let end = scroll_y
            .saturating_add(viewport_height)
            .div_ceil(cell_height)
            .min(rows);
        let screen_rows = viewport_height.div_ceil(cell_height);
        let margin = self.overscan_screens.saturating_mul(screen_rows);
        let near_first = first.saturating_sub(margin);
        let near_end = end.saturating_add(margin).min(rows);

        let items = |from_row: usize, to_row: usize| {
            let start = from_row.saturating_mul(columns).min(self.item_count);
            let stop = to_row.saturating_mul(columns).min(self.item_count);
            start..stop
        };
        (items(first, end), items(near_first, near_end))
    }

    /// Move the viewport. Pending requests that left the window are cancelled.
    /// New requests go to uncached, not-yet-pending items in the window, visible
    /// first.
    pub fn set_scroll(&mut self, scroll_y: usize) -> DecodePlan {
        self.scroll_y = scroll_y.min(self.max_scroll());
        let (visible, window) = self.ranges_at(self.scroll_y);
        let previous_window = std::mem::replace(&mut self.window, window.clone());
        self.visible = visible.clone();

        let cancelled: Vec<usize> = self
            .pending
            .iter()
            .copied()
            .filter(|index| !window.contains(index))
            .collect();
        for index in &cancelled {
            self.pending.remove(index);
        }
        #[cfg(any(test, feature = "photo-counters"))]
        self.counters.record_cancelled(cancelled.len());

        for index in window.clone() {
            if self.cache.contains(index) {
                self.cache.touch(index);
                if !previous_window.contains(&index) {
                    #[cfg(any(test, feature = "photo-counters"))]
                    self.counters.record_cache_hit();
                }
            }
        }

        let mut requests = Vec::new();
        for index in visible.clone() {
            self.request(index, &mut requests);
        }
        // Near-visible items: alternate one above and one below, nearest first.
        let mut above = visible.start;
        let mut below = visible.end;
        loop {
            let mut progressed = false;
            if above > window.start {
                above -= 1;
                self.request(above, &mut requests);
                progressed = true;
            }
            if below < window.end {
                self.request(below, &mut requests);
                below += 1;
                progressed = true;
            }
            if !progressed {
                break;
            }
        }

        DecodePlan {
            requests,
            cancelled,
        }
    }

    /// Change the number of items, for example after the folder was re-fetched.
    /// Cached entries past the new end are dropped, and the window is recomputed
    /// at the clamped scroll offset.
    pub fn set_item_count(&mut self, item_count: usize) -> DecodePlan {
        self.item_count = item_count;
        self.cache.retain_below(item_count);
        let scroll_y = self.scroll_y;
        self.set_scroll(scroll_y)
    }

    /// Report that a decode of `index` finished.
    pub fn complete(&mut self, index: usize) -> Completion {
        if !self.pending.remove(&index) {
            #[cfg(any(test, feature = "photo-counters"))]
            self.counters.record_dropped();
            return Completion::Discarded;
        }
        #[cfg(any(test, feature = "photo-counters"))]
        self.counters
            .record_decode(index, self.visible.contains(&index));
        Completion::Cached {
            evicted: self.cache.insert(index),
        }
    }

    /// Report that a decode of `index` failed. The item is not cached and may be
    /// requested again when it is in the window.
    pub fn fail(&mut self, index: usize) {
        if self.pending.remove(&index) {
            #[cfg(any(test, feature = "photo-counters"))]
            self.counters.record_dropped();
        }
    }

    fn request(&mut self, index: usize, out: &mut Vec<usize>) {
        if !self.cache.contains(index) && self.pending.insert(index) {
            #[cfg(any(test, feature = "photo-counters"))]
            self.counters.record_request();
            out.push(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Completion, DEFAULT_OVERSCAN_SCREENS, DecodeCounters, GridSpec, PhotoGrid, PhotoGridError,
    };

    const COUNT: usize = 1_000;
    const CELL_H: usize = 150;

    /// 800 by 600 viewport, 200 by 150 cells: 4 columns, 250 rows, 4 rows per screen.
    fn spec() -> GridSpec {
        GridSpec::new(800, 600, 200, CELL_H).expect("spec")
    }

    fn grid(overscan: usize, capacity: usize) -> PhotoGrid {
        PhotoGrid::new(COUNT, spec(), overscan, capacity).expect("grid")
    }

    fn default_grid() -> PhotoGrid {
        grid(DEFAULT_OVERSCAN_SCREENS, 256)
    }

    fn at_row(row: usize) -> usize {
        row * CELL_H
    }

    #[test]
    fn spec_rejects_zero_sizes_and_keeps_one_column() {
        assert_eq!(
            GridSpec::new(0, 600, 200, 150).err(),
            Some(PhotoGridError::ZeroSize)
        );
        assert_eq!(
            GridSpec::new(800, 600, 0, 150).err(),
            Some(PhotoGridError::ZeroSize)
        );
        assert_eq!(
            PhotoGrid::new(COUNT, spec(), 1, 0).err(),
            Some(PhotoGridError::ZeroCapacity)
        );
        let narrow = GridSpec::new(100, 600, 200, 150).expect("spec");
        assert_eq!(narrow.columns(), 1);
    }

    #[test]
    fn visible_and_window_at_the_top() {
        let mut grid = default_grid();
        grid.set_scroll(0);
        assert_eq!(grid.visible(), 0..16);
        assert_eq!(grid.window(), 0..32);
    }

    #[test]
    fn visible_and_window_in_the_middle() {
        let mut grid = default_grid();
        grid.set_scroll(at_row(100));
        assert_eq!(grid.visible(), 400..416);
        assert_eq!(grid.window(), 384..432);
    }

    #[test]
    fn visible_and_window_at_the_bottom() {
        let mut grid = default_grid();
        grid.set_scroll(usize::MAX);
        assert_eq!(grid.max_scroll(), 250 * CELL_H - 600);
        assert_eq!(grid.scroll_y(), grid.max_scroll());
        assert_eq!(grid.visible(), 984..1_000);
        assert_eq!(grid.window(), 968..1_000);
    }

    #[test]
    fn empty_folder_has_no_items_or_requests() {
        let mut grid = PhotoGrid::new(0, spec(), 1, 8).expect("grid");
        let plan = grid.set_scroll(usize::MAX);
        assert_eq!(grid.max_scroll(), 0);
        assert!(grid.visible().is_empty());
        assert!(grid.window().is_empty());
        assert!(plan.requests.is_empty());
        assert!(plan.cancelled.is_empty());
    }

    #[test]
    fn requests_visible_first_then_near_visible_nearest_first() {
        let mut grid = default_grid();
        let plan = grid.set_scroll(0);
        // Top: visible 0..16, then the window below it (16..32) in order.
        assert_eq!(plan.requests, (0..32).collect::<Vec<_>>());

        let mut grid = default_grid();
        let plan = grid.set_scroll(at_row(100));
        assert_eq!(plan.requests[..16], (400..416).collect::<Vec<_>>()[..]);
        assert_eq!(plan.requests.len(), 48);
        // Nearest first: the first near-visible pair is one above and one below.
        assert_eq!(plan.requests[16], 399);
        assert_eq!(plan.requests[17], 416);
        let mut all = plan.requests.clone();
        all.sort_unstable();
        assert_eq!(all, (384..432).collect::<Vec<_>>());
    }

    #[test]
    fn overscan_zero_requests_only_the_visible_set() {
        let mut grid = grid(0, 256);
        let plan = grid.set_scroll(at_row(100));
        assert_eq!(plan.requests, (400..416).collect::<Vec<_>>());
    }

    #[test]
    fn scroll_cancels_pending_requests_that_left_the_window() {
        let mut grid = default_grid();
        grid.set_scroll(0);
        let plan = grid.set_scroll(at_row(100));
        assert_eq!(plan.cancelled, (0..32).collect::<Vec<_>>());
        assert_eq!(plan.requests.len(), 48);
        assert!(plan.requests.iter().all(|index| (384..432).contains(index)));
        assert!(
            grid.pending()
                .iter()
                .all(|index| (384..432).contains(index))
        );
    }

    #[test]
    fn same_position_does_not_request_again() {
        let mut grid = default_grid();
        grid.set_scroll(0);
        let plan = grid.set_scroll(0);
        assert!(plan.requests.is_empty());
        assert!(plan.cancelled.is_empty());
    }

    #[test]
    fn completed_items_are_not_requested_again() {
        let mut grid = default_grid();
        for index in grid.set_scroll(0).requests {
            assert!(matches!(
                grid.complete(index),
                Completion::Cached { evicted: None }
            ));
        }
        assert!(grid.set_scroll(0).requests.is_empty());
        assert_eq!(grid.cached_len(), 32);
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let mut grid = grid(DEFAULT_OVERSCAN_SCREENS, 4);
        let plan = grid.set_scroll(0);
        assert_eq!(plan.requests[..5], [0, 1, 2, 3, 4]);
        for index in 0..4 {
            assert_eq!(grid.complete(index), Completion::Cached { evicted: None });
        }
        assert_eq!(grid.complete(4), Completion::Cached { evicted: Some(0) });
        assert!(!grid.is_cached(0));
        assert!((1..=4).all(|index| grid.is_cached(index)));
        assert_eq!(grid.cached_len(), 4);
    }

    #[test]
    fn completion_after_cancel_is_discarded() {
        let mut grid = default_grid();
        grid.set_scroll(0);
        grid.set_scroll(usize::MAX);
        assert_eq!(grid.complete(5), Completion::Discarded);
        assert!(!grid.is_cached(5));
    }

    #[test]
    fn failed_request_can_be_requested_again() {
        let mut grid = default_grid();
        grid.set_scroll(0);
        grid.fail(3);
        assert!(!grid.is_pending(3));
        let plan = grid.set_scroll(0);
        assert_eq!(plan.requests, vec![3]);
    }

    #[test]
    fn scrolling_through_every_row_keeps_requests_inside_the_window() {
        let mut grid = grid(DEFAULT_OVERSCAN_SCREENS, 64);
        let rows = 250;
        for row in 0..rows {
            let plan = grid.set_scroll(at_row(row));
            let window = grid.window();
            assert!(plan.requests.iter().all(|index| window.contains(index)));
            assert!(grid.pending().iter().all(|index| window.contains(index)));
            assert!(grid.pending().len() <= window.len());
            // Decodes finish instantly in this sweep.
            for index in plan.requests {
                grid.complete(index);
            }
            assert!(grid.pending().is_empty());
            assert!(grid.cached_len() <= 64);
        }
        assert_eq!(grid.cached_len(), 64);
    }

    #[test]
    fn shrinking_the_list_cancels_and_drops_items_past_the_end() {
        let mut grid = default_grid();
        grid.set_scroll(usize::MAX);
        let plan = grid.set_item_count(500);
        assert_eq!(plan.cancelled, (968..1_000).collect::<Vec<_>>());
        assert_eq!(grid.item_count(), 500);
        assert_eq!(grid.scroll_y(), 125 * CELL_H - 600);
        assert_eq!(grid.window(), 468..500);
        assert!(grid.pending().iter().all(|&index| index < 500));
    }

    #[test]
    fn counters_record_each_path_and_snapshot_is_a_copy() {
        let counters = DecodeCounters::default();
        counters.record_request();
        counters.record_request();
        counters.record_decode(7, true);
        counters.record_decode(9, false);
        counters.record_cache_hit();
        counters.record_cancelled(3);
        counters.record_dropped();

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.requested, 2);
        assert_eq!(snapshot.decoded_total, 2);
        assert_eq!(snapshot.decoded_visible, 1);
        assert_eq!(snapshot.decoded_prefetch, 1);
        assert_eq!(snapshot.cache_hits, 1);
        assert_eq!(snapshot.cancelled, 3);
        assert_eq!(snapshot.dropped, 1);
        assert_eq!(snapshot.decoded_indices, vec![7, 9]);

        counters.record_request();
        assert_eq!(snapshot.requested, 2);
        assert_eq!(counters.snapshot().requested, 3);
    }

    #[test]
    fn cancellation_and_discarded_results_are_not_decodes() {
        let mut grid = default_grid();
        grid.set_scroll(0);
        grid.set_scroll(at_row(100));
        let counters = grid.decode_counters();
        assert_eq!(counters.requested, 32 + 48);
        assert_eq!(counters.cancelled, 32);
        assert_eq!(counters.decoded_total, 0);
        assert!(counters.decoded_indices.is_empty());

        // Item 5 was cancelled before its decode finished.
        assert_eq!(grid.complete(5), Completion::Discarded);
        let counters = grid.decode_counters();
        assert_eq!(counters.dropped, 1);
        assert_eq!(counters.decoded_total, 0);
    }

    #[test]
    fn decodes_split_into_visible_and_prefetch() {
        let mut grid = default_grid();
        for index in grid.set_scroll(0).requests {
            grid.complete(index);
        }
        let counters = grid.decode_counters();
        assert_eq!(counters.decoded_total, 32);
        assert_eq!(counters.decoded_visible, 16);
        assert_eq!(counters.decoded_prefetch, 16);
        assert_eq!(counters.decoded_indices, (0..32).collect::<Vec<_>>());
    }

    #[test]
    fn cached_items_entering_the_window_count_as_hits() {
        let mut grid = default_grid();
        for index in grid.set_scroll(0).requests {
            grid.complete(index);
        }
        // Window at row 10 is 24..72. Nothing there is cached except 24..32, which
        // were already in the top window, so no hits.
        grid.set_scroll(at_row(10));
        assert_eq!(grid.decode_counters().cache_hits, 0);
        // Back at the top, cached 0..24 enter the window again.
        grid.set_scroll(0);
        assert_eq!(grid.decode_counters().cache_hits, 24);
    }

    #[test]
    fn jump_to_bottom_decodes_only_the_two_windows() {
        let mut grid = default_grid();
        for index in grid.set_scroll(0).requests {
            grid.complete(index);
        }
        for index in grid.set_scroll(usize::MAX).requests {
            grid.complete(index);
        }
        let counters = grid.decode_counters();
        // Top window (32) plus bottom window (32). Nothing in between.
        assert_eq!(counters.decoded_total, 64);
        assert!(
            counters
                .decoded_indices
                .iter()
                .all(|index| (0..32).contains(index) || (968..1_000).contains(index))
        );
    }

    #[test]
    fn full_sweep_decodes_each_item_once_and_never_outside_the_window() {
        let mut grid = grid(DEFAULT_OVERSCAN_SCREENS, 64);
        let mut peak_step_decodes = 0;
        for row in 0..250 {
            let plan = grid.set_scroll(at_row(row));
            let window = grid.window();
            let before = grid.decode_counters().decoded_total;
            for index in plan.requests {
                assert!(window.contains(&index));
                grid.complete(index);
            }
            let step = grid.decode_counters().decoded_total - before;
            peak_step_decodes = peak_step_decodes.max(step);
            assert!(grid.cached_len() <= 64);
        }
        let counters = grid.decode_counters();
        // Spec M3-07 asks for decoded_total <= visible_max + prefetch_window after
        // a full sweep. A sweep that completes every decode touches every item
        // once, so the cumulative total is 1,000. The bounded quantities are the
        // decodes per step (at most one window) and the resident cache.
        assert_eq!(counters.decoded_total, 1_000);
        assert!(peak_step_decodes <= 48);
        let mut distinct = counters.decoded_indices.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 1_000);
    }
}
