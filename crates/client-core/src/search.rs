//! Pure search debounce, request freshness, and result grouping logic.

use std::time::Duration;

use jellyfin_api::BaseItemDto;

pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
pub const MIN_SEARCH_CHARACTERS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SearchGeneration(u64);

impl SearchGeneration {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SearchRequestId(u64);

impl SearchRequestId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchRequest {
    pub query: String,
    pub generation: SearchGeneration,
    pub request_id: SearchRequestId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchInputAction {
    /// The trimmed query is too short; pending work and prior responses are stale.
    Clear { generation: SearchGeneration },
    /// A valid query is queued and will be emitted after the debounce interval.
    Debouncing { generation: SearchGeneration },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingQuery {
    query: String,
    changed_at: Duration,
    generation: SearchGeneration,
}

/// Fake-clock-friendly debounce state. Call [`Self::input`] with a monotonic
/// elapsed time and poll [`Self::take_ready`] whenever the UI timer fires.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchDebouncer {
    generation: u64,
    next_request_id: u64,
    pending: Option<PendingQuery>,
    active_request: Option<(SearchGeneration, SearchRequestId)>,
}

impl SearchDebouncer {
    /// Record a new input value. Character length is counted in Unicode scalar
    /// values after trimming, rather than bytes or UTF-16 code units.
    pub fn input(&mut self, input: &str, now: Duration) -> SearchInputAction {
        self.generation = self.generation.wrapping_add(1);
        let generation = SearchGeneration(self.generation);
        let query = input.trim();
        self.active_request = None;

        if query.chars().count() < MIN_SEARCH_CHARACTERS {
            self.pending = None;
            return SearchInputAction::Clear { generation };
        }

        self.pending = Some(PendingQuery {
            query: query.to_owned(),
            changed_at: now,
            generation,
        });
        SearchInputAction::Debouncing { generation }
    }

    /// Return the queued request once the full debounce interval has elapsed.
    /// A request is emitted at most once for each input generation.
    pub fn take_ready(&mut self, now: Duration) -> Option<SearchRequest> {
        let pending = self.pending.as_ref()?;
        if now.saturating_sub(pending.changed_at) < SEARCH_DEBOUNCE {
            return None;
        }

        let pending = self.pending.take()?;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let request_id = SearchRequestId(self.next_request_id);
        self.active_request = Some((pending.generation, request_id));

        Some(SearchRequest {
            query: pending.query,
            generation: pending.generation,
            request_id,
        })
    }

    /// Whether a response still belongs to the latest submitted search.
    /// Any subsequent input, including a short query that clears results,
    /// invalidates the request immediately.
    pub fn is_current_response(&self, request: &SearchRequest) -> bool {
        self.active_request == Some((request.generation, request.request_id))
            && request.generation.0 == self.generation
    }

    pub const fn generation(&self) -> SearchGeneration {
        SearchGeneration(self.generation)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SearchResultKind {
    Movies,
    Series,
    Episodes,
    Music,
    Playlists,
    Photos,
    People,
    Channels,
}

impl SearchResultKind {
    const ORDERED: [Self; 8] = [
        Self::Movies,
        Self::Series,
        Self::Episodes,
        Self::Music,
        Self::Playlists,
        Self::Photos,
        Self::People,
        Self::Channels,
    ];

    fn matches(self, item_type: Option<&str>) -> bool {
        match self {
            Self::Movies => item_type == Some("Movie"),
            Self::Series => item_type == Some("Series"),
            Self::Episodes => item_type == Some("Episode"),
            Self::Music => item_type == Some("Audio"),
            Self::Playlists => item_type == Some("Playlist"),
            Self::Photos => item_type == Some("Photo"),
            Self::People => item_type == Some("Person"),
            Self::Channels => matches!(item_type, Some("TvChannel" | "LiveTvChannel")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchResultGroup<'a> {
    pub kind: SearchResultKind,
    pub items: Vec<&'a BaseItemDto>,
}

/// Group supported Jellyfin DTO types in product order, omitting empty groups.
/// Unknown or absent types are ignored rather than shown in the wrong section.
pub fn group_search_results(items: &[BaseItemDto]) -> Vec<SearchResultGroup<'_>> {
    SearchResultKind::ORDERED
        .into_iter()
        .filter_map(|kind| {
            let matching: Vec<_> = items
                .iter()
                .filter(|item| kind.matches(item.item_type.as_deref()))
                .collect();
            (!matching.is_empty()).then_some(SearchResultGroup {
                kind,
                items: matching,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: &str) -> BaseItemDto {
        BaseItemDto {
            id: id.to_owned(),
            name: id.to_owned(),
            item_type: Some(kind.to_owned()),
            overview: None,
            production_year: None,
            official_rating: None,
            run_time_ticks: None,
            premiere_date: None,
            series_name: None,
            parent_index_number: None,
            index_number: None,
            series_id: None,
            genres: None,
            people: None,
            primary_image_aspect_ratio: None,
            image_tags: None,
            backdrop_image_tags: None,
            user_data: None,
        }
    }

    #[test]
    fn debounce_waits_until_exactly_three_hundred_milliseconds() {
        let mut debounce = SearchDebouncer::default();
        let start = Duration::from_secs(2);
        assert!(matches!(
            debounce.input("  dune  ", start),
            SearchInputAction::Debouncing { .. }
        ));
        assert!(
            debounce
                .take_ready(start + Duration::from_millis(299))
                .is_none()
        );
        let request = debounce
            .take_ready(start + SEARCH_DEBOUNCE)
            .expect("request at debounce boundary");
        assert_eq!(request.query, "dune");
        assert!(debounce.take_ready(start + SEARCH_DEBOUNCE).is_none());
    }

    #[test]
    fn short_trimmed_unicode_query_clears_and_cancels_pending_work() {
        let mut debounce = SearchDebouncer::default();
        let start = Duration::ZERO;
        debounce.input("ab", start);
        assert!(matches!(
            debounce.input(" 界 ", start + Duration::from_millis(100)),
            SearchInputAction::Clear { .. }
        ));
        assert!(
            debounce
                .take_ready(start + Duration::from_secs(1))
                .is_none()
        );

        assert!(matches!(
            debounce.input("🙂a", start),
            SearchInputAction::Debouncing { .. }
        ));
    }

    #[test]
    fn new_input_supersedes_pending_and_in_flight_requests() {
        let mut debounce = SearchDebouncer::default();
        debounce.input("old", Duration::ZERO);
        let first_request = debounce
            .take_ready(SEARCH_DEBOUNCE)
            .expect("first query becomes ready");
        assert!(debounce.is_current_response(&first_request));

        let second_start = Duration::from_millis(350);
        debounce.input("new", second_start);
        assert!(!debounce.is_current_response(&first_request));
        assert!(
            debounce
                .take_ready(second_start + Duration::from_millis(299))
                .is_none()
        );
        let second = debounce
            .take_ready(second_start + SEARCH_DEBOUNCE)
            .expect("latest query becomes ready");
        assert_eq!(second.query, "new");
        assert_ne!(first_request.request_id, second.request_id);
        assert!(debounce.is_current_response(&second));

        debounce.input("x", second_start + SEARCH_DEBOUNCE);
        assert!(!debounce.is_current_response(&second));
        assert!(
            debounce
                .take_ready(second_start + Duration::from_secs(2))
                .is_none()
        );
    }

    #[test]
    fn groups_all_supported_types_in_product_order_and_hides_empty_groups() {
        let items = vec![
            item("person", "Person"),
            item("live-channel", "LiveTvChannel"),
            item("photo", "Photo"),
            item("playlist", "Playlist"),
            item("audio", "Audio"),
            item("episode", "Episode"),
            item("movie", "Movie"),
            item("series", "Series"),
            item("tv-channel", "TvChannel"),
            item("ignored", "Book"),
        ];
        let groups = group_search_results(&items);
        assert_eq!(
            groups.iter().map(|group| group.kind).collect::<Vec<_>>(),
            vec![
                SearchResultKind::Movies,
                SearchResultKind::Series,
                SearchResultKind::Episodes,
                SearchResultKind::Music,
                SearchResultKind::Playlists,
                SearchResultKind::Photos,
                SearchResultKind::People,
                SearchResultKind::Channels,
            ]
        );
        assert_eq!(
            groups
                .iter()
                .map(|group| group.items[0].id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "movie",
                "series",
                "episode",
                "audio",
                "playlist",
                "photo",
                "person",
                "live-channel",
            ]
        );
        assert_eq!(
            groups[7]
                .items
                .iter()
                .map(|channel| channel.id.as_str())
                .collect::<Vec<_>>(),
            ["live-channel", "tv-channel"]
        );

        let movie_result = [item("one", "Movie")];
        let only_movies = group_search_results(&movie_result);
        assert_eq!(only_movies.len(), 1);
        assert_eq!(only_movies[0].kind, SearchResultKind::Movies);
    }
}
