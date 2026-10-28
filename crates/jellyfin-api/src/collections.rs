//! Collection (box set) member listing and the pure runtime summary shown on
//! the collection detail page.
//!
//! Members come from `GET /Items?parentId={boxSetId}`. The OpenAPI document
//! (`https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json`, operation
//! `GetItems`) confirms that `parentId` and `recursive` exist as query
//! parameters. That a box set's children are returned by this query is
//! [UNCONFIRMED] against a live server, and the mock does not model it.
//!
//! Box sets are read-only here. Creating, adding members, and removing members
//! are out of scope for M3-16.

use crate::home::{MAX_HOME_LIMIT, validate_id};
use crate::{ApiError, BaseItemDto, ItemQuery, ItemsResponse, JellyfinClient, Session};

/// Ticks per second for Jellyfin `RunTimeTicks` values (100 ns units).
const TICKS_PER_SECOND: i64 = 10_000_000;

impl JellyfinClient {
    /// List the direct members of a collection, in the server's collection
    /// order (`GET /Items?parentId={boxSetId}&recursive=false`). `limit` is
    /// clamped to `1..=MAX_HOME_LIMIT`.
    ///
    /// Nested box sets come back as items with type `BoxSet` and should be
    /// shown as collection tiles. Members the user cannot access are not
    /// listed by the server, so no per-item error is returned here.
    pub async fn collection_members(
        &self,
        session: &Session,
        box_set_id: &str,
        start_index: u32,
        limit: u32,
    ) -> Result<ItemsResponse, ApiError> {
        validate_id(box_set_id)?;
        self.list_items(
            session,
            &ItemQuery {
                recursive: false,
                start_index,
                limit: limit.clamp(1, MAX_HOME_LIMIT),
                parent_id: Some(box_set_id.to_owned()),
                enable_total_record_count: Some(true),
                ..ItemQuery::default()
            },
        )
        .await
    }
}

/// Runtime summary for a set of collection members.
///
/// `total_ticks` is the sum of known runtimes. Items with no runtime, or a
/// non-positive one, are counted in `items_without_runtime` and are not added
/// to the total, so the UI can show the total with a "some items have no
/// runtime" marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CollectionRuntime {
    pub total_ticks: i64,
    pub items_with_runtime: usize,
    pub items_without_runtime: usize,
}

impl CollectionRuntime {
    /// True when at least one member has no usable runtime, so the total is
    /// a lower bound.
    #[must_use]
    pub const fn has_unknown_runtime(&self) -> bool {
        self.items_without_runtime > 0
    }

    /// Whole seconds in the known-runtime total.
    #[must_use]
    pub const fn total_seconds(&self) -> i64 {
        self.total_ticks / TICKS_PER_SECOND
    }
}

/// Sum the known runtimes of collection members, counting the rest as unknown.
#[must_use]
pub fn collection_runtime(items: &[BaseItemDto]) -> CollectionRuntime {
    items
        .iter()
        .fold(CollectionRuntime::default(), |mut summary, item| {
            match item.run_time_ticks {
                Some(ticks) if ticks > 0 => {
                    summary.total_ticks = summary.total_ticks.saturating_add(ticks);
                    summary.items_with_runtime += 1;
                }
                _ => summary.items_without_runtime += 1,
            }
            summary
        })
}

#[cfg(test)]
mod tests {
    use super::{CollectionRuntime, collection_runtime};
    use crate::BaseItemDto;

    fn item(id: &str, ticks: Option<i64>) -> BaseItemDto {
        BaseItemDto {
            id: id.to_owned(),
            name: id.to_owned(),
            item_type: Some("Movie".to_owned()),
            overview: None,
            production_year: None,
            official_rating: None,
            run_time_ticks: ticks,
            premiere_date: None,
            series_name: None,
            parent_index_number: None,
            index_number: None,
            series_id: None,
            series_primary_image_tag: None,
            genres: None,
            people: None,
            primary_image_aspect_ratio: None,
            image_tags: None,
            backdrop_image_tags: None,
            user_data: None,
        }
    }

    #[test]
    fn runtime_sums_known_values_and_marks_missing_ones() {
        let items = [
            item("a", Some(7_200 * 10_000_000)),
            item("b", None),
            item("c", Some(3_600 * 10_000_000)),
            item("d", Some(0)),
        ];
        let summary = collection_runtime(&items);
        assert_eq!(
            summary,
            CollectionRuntime {
                total_ticks: 10_800 * 10_000_000,
                items_with_runtime: 2,
                items_without_runtime: 2,
            }
        );
        assert!(summary.has_unknown_runtime());
        assert_eq!(summary.total_seconds(), 10_800);
    }

    #[test]
    fn runtime_of_empty_collection_is_zero_with_no_unknown_marker() {
        let summary = collection_runtime(&[]);
        assert_eq!(summary, CollectionRuntime::default());
        assert!(!summary.has_unknown_runtime());
    }

    #[test]
    fn negative_runtime_is_treated_as_unknown() {
        let summary = collection_runtime(&[item("x", Some(-5))]);
        assert_eq!(summary.total_ticks, 0);
        assert_eq!(summary.items_without_runtime, 1);
    }

    #[test]
    fn runtime_total_saturates_instead_of_overflowing() {
        let items = [item("a", Some(i64::MAX)), item("b", Some(i64::MAX))];
        assert_eq!(collection_runtime(&items).total_ticks, i64::MAX);
    }
}
