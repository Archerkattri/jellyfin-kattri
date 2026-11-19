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
#[derive(Clone, Debug, Default, PartialEq)]
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
    /// The item detail panel, owned here so the browse view has one state to read.
    pub details: DetailsState,
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
            details: DetailsState::new(),
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

/// The item detail panel's lifecycle.
///
/// Selecting an item starts a record request; the panel shows a spinner until the record arrives.
/// The request id orders two selections made in quick succession, so an older record cannot replace
/// the newer selection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DetailsState {
    /// The selected item, or `None` when the panel is closed.
    pub selected_item_id: Option<String>,
    /// The selected item's record, once it arrived.
    pub selected_details: Option<jellyfin_api::BaseItemDto>,
    /// Whether the record request is in flight.
    pub details_loading: bool,
    /// The last record failure, shown by the panel.
    pub details_error: Option<String>,
    /// The request id of the record request in flight (or the last one issued).
    pub details_request_id: u64,
}

impl DetailsState {
    pub const fn new() -> Self {
        Self {
            selected_item_id: None,
            selected_details: None,
            details_loading: false,
            details_error: None,
            details_request_id: 0,
        }
    }

    /// Select `item_id` and start a record request under `request_id`. Any previous record and
    /// error are cleared, so the panel never shows the old item's data under the new title.
    pub fn select(&mut self, item_id: String, request_id: u64) {
        self.selected_item_id = Some(item_id);
        self.selected_details = None;
        self.details_error = None;
        self.details_loading = true;
        self.details_request_id = request_id;
    }

    /// The record arrived. Only the request in flight is accepted.
    pub fn loaded(&mut self, request_id: u64, details: jellyfin_api::BaseItemDto) -> bool {
        if request_id != self.details_request_id {
            return false;
        }
        self.selected_details = Some(details);
        self.details_loading = false;
        self.details_error = None;
        true
    }

    /// The record request failed. Only the request in flight is accepted.
    pub fn failed(&mut self, request_id: u64, message: String) -> bool {
        if request_id != self.details_request_id {
            return false;
        }
        self.details_loading = false;
        self.details_error = Some(message);
        true
    }

    /// Close the panel and forget the record. The request id advances so a late record is refused.
    pub fn clear(&mut self, next_request_id: u64) {
        self.selected_item_id = None;
        self.selected_details = None;
        self.details_loading = false;
        self.details_error = None;
        self.details_request_id = next_request_id;
    }

    /// Whether the panel is open.
    pub fn is_open(&self) -> bool {
        self.selected_item_id.is_some()
    }
}
#[cfg(test)]
mod details_tests {
    use super::DetailsState;

    fn item(id: &str) -> jellyfin_api::BaseItemDto {
        serde_json::from_value(serde_json::json!({ "Id": id, "Name": "Fixture" }))
            .expect("item fixture")
    }

    #[test]
    fn a_fresh_panel_is_closed() {
        let state = DetailsState::new();
        assert!(!state.is_open());
        assert!(!state.details_loading);
        assert_eq!(state.selected_details, None);
    }

    #[test]
    fn selecting_starts_a_request_and_clears_the_previous_record() {
        let mut state = DetailsState::new();
        state.select("first".into(), 1);
        assert!(state.is_open());
        assert!(state.details_loading);
        assert_eq!(state.selected_item_id.as_deref(), Some("first"));
        assert_eq!(state.selected_details, None);

        state.select("second".into(), 2);
        assert_eq!(state.selected_item_id.as_deref(), Some("second"));
        assert_eq!(state.details_request_id, 2);
    }

    #[test]
    fn only_the_request_in_flight_can_fill_the_panel() {
        let mut state = DetailsState::new();
        state.select("movie".into(), 2);
        assert!(!state.loaded(1, item("movie")));
        assert!(state.details_loading);
        assert!(state.loaded(2, item("movie")));
        assert!(!state.details_loading);
        assert!(state.selected_details.is_some());
    }

    #[test]
    fn a_stale_failure_is_refused_and_a_current_one_reports() {
        let mut state = DetailsState::new();
        state.select("movie".into(), 2);
        assert!(!state.failed(1, "stale".into()));
        assert!(state.details_loading);
        assert!(state.failed(2, "offline".into()));
        assert_eq!(state.details_error.as_deref(), Some("offline"));
        assert!(!state.details_loading);
    }

    #[test]
    fn clearing_closes_the_panel_and_refuses_a_late_record() {
        let mut state = DetailsState::new();
        state.select("movie".into(), 2);
        state.clear(3);
        assert!(!state.is_open());
        assert!(!state.loaded(2, item("movie")));
        assert_eq!(state.selected_details, None);
    }
}
