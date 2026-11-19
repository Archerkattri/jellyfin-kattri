//! Owned browse state: the catalog page lifecycle.
//!
//! The app kept the catalog page in seven loose fields spread through `KattriApp`: the request id,
//! the loading/loaded flags, the error, the filter notice, and the paging cursor. They were written
//! from many places and the invariants between them ("loading means a request is in flight",
//! "loaded means at least one page was applied", "next_index is where the next page starts") lived
//! only in the callers' heads.
//!
//! This type owns them together. It is a state container with the transitions that matter, not a
//! reducer for the whole browse feature yet: the page/tab/search sub-areas move here in later
//! slices. Pure: no I/O, no clock.

/// The catalog page lifecycle for one query.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowseState {
    /// The request id of the page request in flight (or the last one issued).
    pub catalog_request_id: u64,
    /// Whether a page request is in flight.
    pub catalog_loading: bool,
    /// Whether at least one page has been applied for the current query.
    pub catalog_loaded: bool,
    /// The last catalog failure, shown by the browse view.
    pub catalog_error: Option<String>,
    /// A filter the server refused, shown as a notice rather than an error.
    pub catalog_filter_notice: Option<String>,
    /// Where the next page starts.
    pub catalog_next_index: u32,
    /// Whether the server reported no more pages.
    pub catalog_end_reached: bool,
}

impl BrowseState {
    pub const fn new() -> Self {
        Self {
            catalog_request_id: 0,
            catalog_loading: false,
            catalog_loaded: false,
            catalog_error: None,
            catalog_filter_notice: None,
            catalog_next_index: 0,
            catalog_end_reached: false,
        }
    }

    /// Start a page request under `request_id`.
    ///
    /// Starting a request does not clear the error or the loaded page: the browse view keeps showing
    /// the previous page while the next one loads, which is what the loose fields did.
    pub fn begin_request(&mut self, request_id: u64) {
        self.catalog_request_id = request_id;
        self.catalog_loading = true;
    }

    /// The request for `request_id` failed before it was submitted (worker stopped or queue full).
    pub fn request_not_submitted(&mut self, request_id: u64, message: String) {
        if self.catalog_request_id == request_id {
            self.catalog_loading = false;
            self.catalog_error = Some(message);
        }
    }

    /// A page was applied.
    pub fn page_applied(&mut self, next_index: u32, end_reached: bool) {
        self.catalog_loading = false;
        self.catalog_loaded = true;
        self.catalog_error = None;
        self.catalog_filter_notice = None;
        self.catalog_next_index = next_index;
        self.catalog_end_reached = end_reached;
    }

    /// A page request failed.
    pub fn page_failed(&mut self, message: String) {
        self.catalog_loading = false;
        self.catalog_error = Some(message);
    }

    /// The query changed (page, tab, sort, filter, search): the next page starts at zero and the
    /// previous page no longer counts as loaded for the new query.
    pub fn restart_query(&mut self) {
        self.catalog_next_index = 0;
        self.catalog_end_reached = false;
        self.catalog_loaded = false;
        self.catalog_error = None;
        self.catalog_filter_notice = None;
    }

    /// The session ended: drop everything.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Whether another page can be requested.
    pub fn can_load_more(&self) -> bool {
        !self.catalog_loading && !self.catalog_end_reached
    }
}

#[cfg(test)]
mod tests {
    use super::BrowseState;

    #[test]
    fn a_fresh_state_has_no_page_and_no_request() {
        let state = BrowseState::new();
        assert_eq!(state.catalog_request_id, 0);
        assert!(!state.catalog_loading);
        assert!(!state.catalog_loaded);
        assert_eq!(state.catalog_error, None);
        assert_eq!(state.catalog_next_index, 0);
        assert!(!state.catalog_end_reached);
        assert!(state.can_load_more());
    }

    #[test]
    fn beginning_a_request_marks_loading_and_keeps_the_previous_page() {
        let mut state = BrowseState::new();
        state.page_applied(50, false);
        state.begin_request(9);
        assert_eq!(state.catalog_request_id, 9);
        assert!(state.catalog_loading);
        assert!(
            state.catalog_loaded,
            "the previous page stays visible while the next loads"
        );
        assert!(!state.can_load_more(), "one request at a time");
    }

    #[test]
    fn applying_a_page_records_the_cursor_and_clears_the_error() {
        let mut state = BrowseState::new();
        state.begin_request(9);
        state.page_failed("offline".into());
        assert_eq!(state.catalog_error.as_deref(), Some("offline"));

        state.begin_request(10);
        state.page_applied(100, true);
        assert!(!state.catalog_loading);
        assert!(state.catalog_loaded);
        assert_eq!(state.catalog_error, None);
        assert_eq!(state.catalog_next_index, 100);
        assert!(state.catalog_end_reached);
        assert!(!state.can_load_more(), "the server said there is no more");
    }

    #[test]
    fn a_failed_page_stops_loading_and_reports() {
        let mut state = BrowseState::new();
        state.begin_request(9);
        state.page_failed("offline".into());
        assert!(!state.catalog_loading);
        assert_eq!(state.catalog_error.as_deref(), Some("offline"));
    }

    #[test]
    fn a_request_that_never_reached_the_worker_reports_against_its_own_id() {
        let mut state = BrowseState::new();
        state.begin_request(9);
        state.request_not_submitted(8, "stale".into());
        assert!(
            state.catalog_loading,
            "another request's failure must not clear this one"
        );
        state.request_not_submitted(9, "stopped".into());
        assert!(!state.catalog_loading);
        assert_eq!(state.catalog_error.as_deref(), Some("stopped"));
    }

    #[test]
    fn restarting_the_query_clears_the_cursor_and_the_loaded_flag() {
        let mut state = BrowseState::new();
        state.page_applied(200, true);
        state.restart_query();
        assert_eq!(state.catalog_next_index, 0);
        assert!(!state.catalog_end_reached);
        assert!(!state.catalog_loaded);
        assert!(state.can_load_more());
    }

    #[test]
    fn reset_drops_everything() {
        let mut state = BrowseState::new();
        state.begin_request(9);
        state.page_applied(10, true);
        state.reset();
        assert_eq!(state, BrowseState::new());
    }
}
