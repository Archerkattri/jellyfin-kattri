//! Search across the item types added by M3-17: tracks, music albums,
//! playlists, and photos. Live TV channels use their dedicated endpoint.
//!
//! The call is `GET /Items` with `searchTerm`, `includeItemTypes`,
//! `recursive`, `startIndex`, and `limit` (operation `GetItems`). Checked
//! against the official stable OpenAPI document
//! (`https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json`, 2026-10-15):
//! `includeItemTypes` is `BaseItemKind[]`, and the `BaseItemKind` enum contains
//! `Audio`, `MusicAlbum`, `Playlist`, and `Photo`. The official generated SDK's
//! `GetLiveTvChannels` request has no search-term parameter, so channel names
//! are filtered locally while paging that endpoint.
//!
//! Search terms are trimmed before sending, and an empty term is refused
//! before any request is made. The term is never logged.

use crate::{BaseItemDto, ItemQuery, ItemsResponse, JellyfinClient, Session, home::MAX_HOME_LIMIT};

/// Item types this search requests, in the order sent to the server.
pub const SEARCH_ITEM_TYPES: [&str; 4] = ["Audio", "MusicAlbum", "Playlist", "Photo"];

/// Maximum channel lineup size scanned by one search. This bounds work even
/// when a server reports an unexpectedly large or unstable result set.
pub const MAX_SEARCH_CHANNELS: u32 = 10_000;

/// Result groups in display order (M3-17 requirement 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SearchGroup {
    Movies,
    Series,
    Episodes,
    Music,
    Playlists,
    Photos,
    People,
    Channels,
}

impl SearchGroup {
    /// Every group in display order.
    pub const ALL: [SearchGroup; 8] = [
        SearchGroup::Movies,
        SearchGroup::Series,
        SearchGroup::Episodes,
        SearchGroup::Music,
        SearchGroup::Playlists,
        SearchGroup::Photos,
        SearchGroup::People,
        SearchGroup::Channels,
    ];

    /// The group that an item's `Type` belongs to, or `None` for a type no
    /// group displays.
    pub fn for_item_type(item_type: &str) -> Option<Self> {
        match item_type {
            "Movie" => Some(Self::Movies),
            "Series" => Some(Self::Series),
            "Episode" => Some(Self::Episodes),
            "Audio" | "MusicAlbum" => Some(Self::Music),
            "Playlist" => Some(Self::Playlists),
            "Photo" => Some(Self::Photos),
            "Person" => Some(Self::People),
            "TvChannel" | "LiveTvChannel" => Some(Self::Channels),
            _ => None,
        }
    }
}

/// One non-empty group of results. Items keep the order the server returned.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchResultGroup<'a> {
    pub group: SearchGroup,
    pub items: Vec<&'a BaseItemDto>,
}

/// Group results by display group. Groups appear in [`SearchGroup::ALL`] order,
/// empty groups are omitted (so a missing channel group is hidden, not shown
/// empty), and items whose type no group displays are dropped. Within a group,
/// items keep their input order.
pub fn group_by_type(items: &[BaseItemDto]) -> Vec<SearchResultGroup<'_>> {
    SearchGroup::ALL
        .iter()
        .filter_map(|&group| {
            let members: Vec<&BaseItemDto> = items
                .iter()
                .filter(|item| {
                    item.item_type
                        .as_deref()
                        .and_then(SearchGroup::for_item_type)
                        == Some(group)
                })
                .collect();
            (!members.is_empty()).then_some(SearchResultGroup {
                group,
                items: members,
            })
        })
        .collect()
}

/// Failure modes of [`JellyfinClient::search_items`].
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The term was empty or only whitespace. No request was sent.
    #[error("search term is empty")]
    EmptyQuery,
    #[error(transparent)]
    Api(#[from] crate::ApiError),
}

/// Trim a search term and refuse it when nothing is left.
fn normalize_term(term: &str) -> Result<&str, SearchError> {
    let trimmed = term.trim();
    if trimmed.is_empty() {
        Err(SearchError::EmptyQuery)
    } else {
        Ok(trimmed)
    }
}

impl JellyfinClient {
    /// Search the session user's library for [`SEARCH_ITEM_TYPES`]
    /// (`GET /Items?searchTerm=...&includeItemTypes=...`).
    ///
    /// `start_index` and `limit` page through the results; `limit` is clamped
    /// to `1..=MAX_HOME_LIMIT`. An empty or whitespace-only term returns
    /// [`SearchError::EmptyQuery`] without sending a request.
    pub async fn search_items(
        &self,
        session: &Session,
        term: &str,
        start_index: u32,
        limit: u32,
    ) -> Result<ItemsResponse, SearchError> {
        let term = normalize_term(term)?;
        let query = ItemQuery {
            start_index,
            limit: limit.clamp(1, MAX_HOME_LIMIT),
            include_item_types: Some(
                SEARCH_ITEM_TYPES
                    .iter()
                    .map(|item_type| (*item_type).to_owned())
                    .collect(),
            ),
            search_term: Some(term.to_owned()),
            enable_total_record_count: Some(true),
            ..ItemQuery::default()
        };
        Ok(self.list_items(session, &query).await?)
    }

    /// Search channel names through the paginated Live TV channel endpoint.
    ///
    /// Jellyfin does not define a channel `searchTerm` parameter, so this
    /// scans the bounded channel lineup and filters names case-insensitively
    /// on the client. `start_index` and `limit` paginate the matching results.
    /// A blank term is refused without a request.
    pub async fn search_live_tv_channels(
        &self,
        session: &Session,
        term: &str,
        start_index: u32,
        limit: u32,
    ) -> Result<crate::LiveTvChannelPage, SearchError> {
        let term = normalize_term(term)?.to_lowercase();
        let mut matches = Vec::new();
        let mut next_index = 0_u32;
        loop {
            let page = self
                .list_live_tv_channels(session, next_index, MAX_HOME_LIMIT)
                .await?;
            let count =
                u32::try_from(page.items.len()).map_err(|_| crate::ApiError::InvalidResponse)?;
            if page
                .total_record_count
                .is_some_and(|total| total > MAX_SEARCH_CHANNELS)
            {
                return Err(crate::ApiError::JsonResponseTooLarge.into());
            }
            if next_index.saturating_add(count) > MAX_SEARCH_CHANNELS {
                return Err(crate::ApiError::JsonResponseTooLarge.into());
            }
            let total_record_count = page.total_record_count;
            matches.extend(
                page.items
                    .into_iter()
                    .filter(|channel| channel.item.name.to_lowercase().contains(&term)),
            );
            next_index = next_index
                .checked_add(count)
                .ok_or(crate::ApiError::InvalidResponse)?;
            let reached_total = total_record_count.is_some_and(|total| next_index >= total);
            if count == 0 || reached_total || count < MAX_HOME_LIMIT {
                break;
            }
        }

        let total =
            u32::try_from(matches.len()).map_err(|_| crate::ApiError::JsonResponseTooLarge)?;
        let start = start_index.min(total);
        let end = start
            .saturating_add(limit.clamp(1, MAX_HOME_LIMIT))
            .min(total);
        let start_usize = usize::try_from(start).map_err(|_| crate::ApiError::InvalidResponse)?;
        let end_usize = usize::try_from(end).map_err(|_| crate::ApiError::InvalidResponse)?;
        Ok(crate::LiveTvChannelPage {
            items: matches
                .into_iter()
                .skip(start_usize)
                .take(end_usize.saturating_sub(start_usize))
                .collect(),
            total_record_count: Some(total),
            start_index: start,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{SEARCH_ITEM_TYPES, SearchError, SearchGroup, group_by_type, normalize_term};
    use crate::BaseItemDto;

    fn item(id: &str, item_type: &str) -> BaseItemDto {
        serde_json::from_value(serde_json::json!({
            "Id": id,
            "Name": id,
            "Type": item_type,
        }))
        .expect("test item")
    }

    #[test]
    fn groups_appear_in_display_order_when_all_are_present() {
        let items = [
            item("ch", "LiveTvChannel"),
            item("person", "Person"),
            item("photo", "Photo"),
            item("playlist", "Playlist"),
            item("album", "MusicAlbum"),
            item("episode", "Episode"),
            item("series", "Series"),
            item("movie", "Movie"),
        ];
        let groups = group_by_type(&items);
        let order: Vec<SearchGroup> = groups.iter().map(|group| group.group).collect();
        assert_eq!(order, SearchGroup::ALL.to_vec());
    }

    #[test]
    fn empty_groups_are_omitted_and_order_is_fixed() {
        let items = [
            item("ch", "LiveTvChannel"),
            item("track-1", "Audio"),
            item("photo", "Photo"),
            item("track-2", "Audio"),
        ];
        let groups = group_by_type(&items);
        let order: Vec<SearchGroup> = groups.iter().map(|group| group.group).collect();
        assert_eq!(
            order,
            vec![
                SearchGroup::Music,
                SearchGroup::Photos,
                SearchGroup::Channels
            ]
        );
    }

    #[test]
    fn items_keep_server_order_within_a_group_and_unknown_types_are_dropped() {
        let items = [
            item("track-b", "Audio"),
            item("unknown", "Trailer"),
            item("album", "MusicAlbum"),
            item("track-a", "Audio"),
        ];
        let groups = group_by_type(&items);
        assert_eq!(groups.len(), 1);
        let ids: Vec<&str> = groups[0]
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(ids, vec!["track-b", "album", "track-a"]);
    }

    #[test]
    fn no_results_yield_no_groups() {
        assert!(group_by_type(&[]).is_empty());
    }

    #[test]
    fn requested_types_cover_the_m3_17_set() {
        for required in ["Audio", "MusicAlbum", "Playlist", "Photo"] {
            assert!(SEARCH_ITEM_TYPES.contains(&required), "missing {required}");
        }
        assert!(!SEARCH_ITEM_TYPES.contains(&"LiveTvChannel"));
    }

    #[test]
    fn jellyfin_tv_channel_discriminator_maps_to_channels_group() {
        let channel = [item("channel", "TvChannel")];
        let groups = group_by_type(&channel);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group, SearchGroup::Channels);
    }

    #[test]
    fn blank_terms_are_refused_and_others_are_trimmed() {
        assert!(matches!(normalize_term(""), Err(SearchError::EmptyQuery)));
        assert!(matches!(
            normalize_term("  \t "),
            Err(SearchError::EmptyQuery)
        ));
        assert_eq!(normalize_term("  tide  ").ok(), Some("tide"));
    }
}
