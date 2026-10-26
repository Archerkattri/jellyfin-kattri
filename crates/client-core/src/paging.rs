//! Pure state for paging a library without retaining stale responses.

use std::collections::HashSet;

/// Jellyfin library pages are requested in fixed batches.
pub const PAGE_SIZE: usize = 50;
/// Request another page when focus enters this many rows from the end.
pub const PREFETCH_ROWS: usize = 2;

/// Identifies the current sort/filter configuration.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PagingGeneration(u64);

impl PagingGeneration {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Unique identity for one request within a paging generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PageRequestId {
    generation: PagingGeneration,
    sequence: u64,
}

impl PageRequestId {
    pub const fn generation(self) -> PagingGeneration {
        self.generation
    }

    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Immutable request description to carry through the async request/response path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageRequest {
    pub id: PageRequestId,
    pub start_index: usize,
    pub limit: usize,
}

/// Summary of applying one page. `Stale` means it was not allowed to mutate state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageApplyOutcome {
    Applied {
        added_count: usize,
        duplicate_count: usize,
        loaded_count: usize,
        total_count: Option<usize>,
        reached_end: bool,
    },
    Stale,
}

/// Failure to advance a generation or assign another request identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagingError {
    GenerationExhausted,
    RequestIdExhausted,
    OffsetExhausted,
}

impl std::fmt::Display for PagingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GenerationExhausted => formatter.write_str("paging generation exhausted"),
            Self::RequestIdExhausted => formatter.write_str("page request ID exhausted"),
            Self::OffsetExhausted => formatter.write_str("page offset exhausted"),
        }
    }
}

impl std::error::Error for PagingError {}

/// Paging state for one library query and one UI row layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PagingState {
    items_per_row: usize,
    generation: PagingGeneration,
    next_sequence: u64,
    next_start_index: usize,
    items: Vec<String>,
    seen_ids: HashSet<String>,
    total_count: Option<usize>,
    in_flight: Option<PageRequest>,
    reached_end: bool,
}

impl PagingState {
    /// Creates state for a layout with the given number of items in each row.
    ///
    /// A zero width is normalized to one so focus distance is always defined.
    pub fn new(items_per_row: usize) -> Self {
        Self {
            items_per_row: items_per_row.max(1),
            generation: PagingGeneration::default(),
            next_sequence: 0,
            next_start_index: 0,
            items: Vec::new(),
            seen_ids: HashSet::new(),
            total_count: None,
            in_flight: None,
            reached_end: false,
        }
    }

    pub const fn generation(&self) -> PagingGeneration {
        self.generation
    }

    pub fn items(&self) -> &[String] {
        &self.items
    }

    pub fn loaded_count(&self) -> usize {
        self.items.len()
    }

    /// The latest total reported by the server, if it supplied one.
    pub const fn total_count(&self) -> Option<usize> {
        self.total_count
    }

    pub const fn reached_end(&self) -> bool {
        self.reached_end
    }

    pub const fn in_flight(&self) -> Option<PageRequest> {
        self.in_flight
    }

    /// Starts the first page, or returns `None` if a request is already active/end is known.
    pub fn request_initial_page(&mut self) -> Result<Option<PageRequest>, PagingError> {
        if self.next_start_index != 0 || !self.items.is_empty() {
            return Ok(None);
        }
        self.begin_request()
    }

    /// Requests a page only when focus is in the final two rows of loaded items.
    pub fn request_for_focus(
        &mut self,
        focused_index: usize,
    ) -> Result<Option<PageRequest>, PagingError> {
        if !self.focus_near_loaded_end(focused_index) {
            return Ok(None);
        }
        self.begin_request()
    }

    /// Whether the focused item is within two rows (inclusive) of the loaded end.
    pub fn focus_near_loaded_end(&self, focused_index: usize) -> bool {
        Self::focus_within_prefetch_range(focused_index, self.items.len(), self.items_per_row)
    }

    /// Evaluates the same two-row trigger when a UI owns the loaded item collection.
    pub fn focus_within_prefetch_range(
        focused_index: usize,
        loaded_count: usize,
        items_per_row: usize,
    ) -> bool {
        if loaded_count == 0 || focused_index >= loaded_count {
            return false;
        }
        let rows = PREFETCH_ROWS.saturating_mul(items_per_row.max(1));
        focused_index >= loaded_count.saturating_sub(rows)
    }

    /// Resets pages when sort or filters change; old request IDs become invalid.
    pub fn reset(&mut self) -> Result<PagingGeneration, PagingError> {
        let next_generation = self
            .generation
            .0
            .checked_add(1)
            .ok_or(PagingError::GenerationExhausted)?;
        self.generation = PagingGeneration(next_generation);
        self.next_sequence = 0;
        self.next_start_index = 0;
        self.items.clear();
        self.seen_ids.clear();
        self.total_count = None;
        self.in_flight = None;
        self.reached_end = false;
        Ok(self.generation)
    }

    /// Applies a response only if it exactly matches the current in-flight request.
    /// Duplicate IDs are omitted while preserving first-seen order.
    pub fn apply_page(
        &mut self,
        request: PageRequest,
        ids: impl IntoIterator<Item = String>,
        total_count: Option<usize>,
    ) -> Result<PageApplyOutcome, PagingError> {
        if self.in_flight != Some(request) || request.id.generation != self.generation {
            return Ok(PageApplyOutcome::Stale);
        }

        let ids: Vec<String> = ids.into_iter().collect();
        let next_start_index = self
            .next_start_index
            .checked_add(ids.len())
            .ok_or(PagingError::OffsetExhausted)?;
        let reached_end = ids.len() < PAGE_SIZE;
        let mut added_count = 0;
        let mut duplicate_count = 0;
        for id in ids {
            if self.seen_ids.insert(id.clone()) {
                self.items.push(id);
                added_count += 1;
            } else {
                duplicate_count += 1;
            }
        }

        self.next_start_index = next_start_index;
        self.total_count = total_count.or(self.total_count);
        self.reached_end = reached_end;
        self.in_flight = None;

        Ok(PageApplyOutcome::Applied {
            added_count,
            duplicate_count,
            loaded_count: self.items.len(),
            total_count: self.total_count,
            reached_end,
        })
    }

    fn begin_request(&mut self) -> Result<Option<PageRequest>, PagingError> {
        if self.in_flight.is_some() || self.reached_end {
            return Ok(None);
        }
        let sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(PagingError::RequestIdExhausted)?;
        let request = PageRequest {
            id: PageRequestId {
                generation: self.generation,
                sequence,
            },
            start_index: self.next_start_index,
            limit: PAGE_SIZE,
        };
        self.next_sequence = sequence;
        self.in_flight = Some(request);
        Ok(Some(request))
    }
}

impl Default for PagingState {
    fn default() -> Self {
        Self::new(5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(start: usize, count: usize) -> Vec<String> {
        (start..start + count).map(|id| id.to_string()).collect()
    }

    #[test]
    fn requests_fixed_pages_and_uses_server_offset() {
        let mut state = PagingState::new(5);
        let first = state.request_initial_page().unwrap().unwrap();
        assert_eq!(first.start_index, 0);
        assert_eq!(first.limit, 50);
        assert!(matches!(
            state.apply_page(first, ids(0, 50), Some(120)).unwrap(),
            PageApplyOutcome::Applied {
                reached_end: false,
                ..
            }
        ));

        let next = state.request_for_focus(40).unwrap().unwrap();
        assert_eq!(next.start_index, 50);
        assert_eq!(next.limit, 50);
    }

    #[test]
    fn focus_prefetch_starts_at_two_row_boundary() {
        let mut state = PagingState::new(5);
        let request = state.request_initial_page().unwrap().unwrap();
        state.apply_page(request, ids(0, 50), None).unwrap();

        assert!(!state.focus_near_loaded_end(39));
        assert!(state.focus_near_loaded_end(40));
        assert!(state.focus_near_loaded_end(49));
        assert!(!state.focus_near_loaded_end(50));
        assert!(state.request_for_focus(39).unwrap().is_none());
        assert!(state.request_for_focus(40).unwrap().is_some());
    }

    #[test]
    fn in_flight_request_suppresses_more_requests() {
        let mut state = PagingState::new(5);
        let first = state.request_initial_page().unwrap().unwrap();
        assert!(state.request_initial_page().unwrap().is_none());
        assert!(state.request_for_focus(0).unwrap().is_none());
        assert_eq!(state.in_flight(), Some(first));
    }

    #[test]
    fn short_page_marks_end_and_exact_page_does_not() {
        let mut state = PagingState::new(5);
        let first = state.request_initial_page().unwrap().unwrap();
        state.apply_page(first, ids(0, 50), Some(51)).unwrap();
        assert!(!state.reached_end());

        let second = state.request_for_focus(49).unwrap().unwrap();
        let result = state.apply_page(second, ids(50, 1), None).unwrap();
        assert!(matches!(
            result,
            PageApplyOutcome::Applied {
                added_count: 1,
                reached_end: true,
                total_count: Some(51),
                ..
            }
        ));
        assert!(state.request_for_focus(50).unwrap().is_none());
    }

    #[test]
    fn reset_invalidates_old_response_and_clears_query_state() {
        let mut state = PagingState::new(5);
        let old = state.request_initial_page().unwrap().unwrap();
        let generation = state.reset().unwrap();
        assert_eq!(generation.get(), old.id.generation().get() + 1);
        let current = state.request_initial_page().unwrap().unwrap();

        assert_eq!(
            state.apply_page(old, ids(0, 50), Some(99)).unwrap(),
            PageApplyOutcome::Stale
        );
        assert_eq!(state.in_flight(), Some(current));
        assert_eq!(state.loaded_count(), 0);
        assert_eq!(state.total_count(), None);
        assert!(!state.reached_end());

        state
            .apply_page(
                current,
                vec!["new-sort-a".into(), "new-sort-b".into()],
                Some(2),
            )
            .unwrap();
        assert_eq!(state.items(), ["new-sort-a", "new-sort-b"]);
    }

    #[test]
    fn deduplicates_preserving_order_and_reports_one_duplicate_count() {
        let mut state = PagingState::new(5);
        let first = state.request_initial_page().unwrap().unwrap();
        let first_ids = (0..50).map(|id| id.to_string()).collect::<Vec<_>>();
        state.apply_page(first, first_ids, Some(60)).unwrap();

        let second = state.request_for_focus(49).unwrap().unwrap();
        let result = state
            .apply_page(
                second,
                vec!["49".to_owned(), "50".to_owned(), "51".to_owned()],
                None,
            )
            .unwrap();
        assert_eq!(
            result,
            PageApplyOutcome::Applied {
                added_count: 2,
                duplicate_count: 1,
                loaded_count: 52,
                total_count: Some(60),
                reached_end: true,
            }
        );
        assert_eq!(state.items()[49..], ["49", "50", "51"]);
        assert_eq!(state.items().iter().collect::<HashSet<_>>().len(), 52);
    }

    #[test]
    fn mismatched_request_is_stale_without_clearing_in_flight() {
        let mut state = PagingState::new(5);
        let request = state.request_initial_page().unwrap().unwrap();
        let forged = PageRequest {
            start_index: 50,
            ..request
        };
        assert_eq!(
            state.apply_page(forged, ids(0, 50), None).unwrap(),
            PageApplyOutcome::Stale
        );
        assert_eq!(state.in_flight(), Some(request));
    }
}
