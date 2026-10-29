//! Typed calls for the Jellyfin home screen: library views, Continue Watching
//! (resume), Next Up, and latest items.
//!
//! Every call uses the authenticated session's token through the client's
//! shared `MediaBrowser` header, the same redirect and timeout policy as the
//! other endpoints, and the shared [`decode_response`] error mapping. Limits
//! are clamped to `1..=MAX_HOME_LIMIT` so a caller cannot request unbounded
//! pages.

use reqwest::Method;
use serde::Deserialize;
use url::Url;

use crate::{ApiError, BaseItemDto, ItemsResponse, JellyfinClient, Session, decode_response};

/// Upper bound for the `limit` query parameter on home-screen calls.
pub const MAX_HOME_LIMIT: u32 = 100;

/// Largest width accepted by [`JellyfinClient::image_url`].
const MAX_IMAGE_WIDTH: u32 = 4096;

/// A library shown on the home screen, as returned by `GET /UserViews`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct UserView {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Server-extensible collection type such as `movies`, `tvshows`, or `music`.
    #[serde(default)]
    pub collection_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UserViewsResponse {
    #[serde(default)]
    items: Vec<UserView>,
}

impl JellyfinClient {
    /// List the authenticated user's libraries (`GET /UserViews`).
    pub async fn user_views(&self, session: &Session) -> Result<Vec<UserView>, ApiError> {
        let response: UserViewsResponse = self
            .get_home(
                session,
                &["UserViews"],
                &[("userId", session.user.id.clone())],
            )
            .await?;
        Ok(response.items)
    }

    /// Fetch the user's box-set collections (`GET /Items?includeItemTypes=BoxSet`).
    pub async fn box_sets(
        &self,
        session: &Session,
        limit: u32,
    ) -> Result<Vec<BaseItemDto>, ApiError> {
        let response: ItemsResponse = self
            .get_home(
                session,
                &["Items"],
                &[
                    ("userId", session.user.id.clone()),
                    ("recursive", "true".to_owned()),
                    ("includeItemTypes", "BoxSet".to_owned()),
                    ("limit", clamp_limit(limit).to_string()),
                ],
            )
            .await?;
        Ok(response.items)
    }

    /// Fetch Continue Watching items (`GET /UserItems/Resume`), video only.
    ///
    /// Each item's [`BaseItemDto::user_data`] carries the playback position,
    /// and [`BaseItemDto::run_time_ticks`] gives the runtime for a progress bar.
    pub async fn resume_items(
        &self,
        session: &Session,
        limit: u32,
    ) -> Result<ItemsResponse, ApiError> {
        self.get_home(
            session,
            &["UserItems", "Resume"],
            &[
                ("userId", session.user.id.clone()),
                ("limit", clamp_limit(limit).to_string()),
                ("mediaTypes", "Video".to_owned()),
                ("fields", "PrimaryImageAspectRatio,Overview".to_owned()),
                ("enableImageTypes", "Primary,Backdrop,Thumb".to_owned()),
            ],
        )
        .await
    }

    /// Fetch the next unwatched episode per series (`GET /Shows/NextUp`).
    pub async fn next_up(&self, session: &Session, limit: u32) -> Result<ItemsResponse, ApiError> {
        self.get_home(
            session,
            &["Shows", "NextUp"],
            &[
                ("userId", session.user.id.clone()),
                ("limit", clamp_limit(limit).to_string()),
            ],
        )
        .await
    }

    /// Fetch recently added items (`GET /Items/Latest`), optionally scoped to
    /// one library by `parent_id`.
    ///
    /// Jellyfin returns this endpoint as a bare JSON array of items, not a
    /// wrapped `ItemsResponse`.
    pub async fn latest_items(
        &self,
        session: &Session,
        parent_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<BaseItemDto>, ApiError> {
        let mut query = vec![
            ("userId", session.user.id.clone()),
            ("limit", clamp_limit(limit).to_string()),
        ];
        if let Some(parent_id) = parent_id {
            validate_id(parent_id)?;
            query.push(("parentId", parent_id.to_owned()));
        }
        self.get_home(session, &["Items", "Latest"], &query).await
    }

    /// Build an artwork URL for an item without any credentials in it.
    ///
    /// The URL carries no token or API key. Fetching it requires the caller to
    /// use an authenticated request path (see `get_primary_image`) or a server
    /// that serves this image type anonymously. `image_type` is a Jellyfin image
    /// kind such as `Primary`, `Backdrop`, or `Thumb`; `tag` is the cache tag
    /// from the item's `ImageTags` or `BackdropImageTags`.
    pub fn image_url(
        &self,
        item_id: &str,
        image_type: &str,
        max_width: u32,
        tag: &str,
    ) -> Result<Url, ApiError> {
        validate_id(item_id)?;
        validate_id(tag)?;
        if image_type.is_empty()
            || image_type.len() > 32
            || !image_type
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
            || max_width == 0
            || max_width > MAX_IMAGE_WIDTH
        {
            return Err(ApiError::InvalidResponse);
        }

        let mut url = self.endpoint(&["Items", item_id, "Images", image_type]);
        url.query_pairs_mut()
            .append_pair("maxWidth", &max_width.to_string())
            .append_pair("tag", tag);
        Ok(url)
    }

    async fn get_home<T: for<'de> Deserialize<'de>>(
        &self,
        session: &Session,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<T, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(segments))
            .query(query);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }
}

fn clamp_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_HOME_LIMIT)
}

/// Reject empty, oversized, or control-character identifiers and tags, the
/// same bounds used by the primary-artwork path.
pub(crate) fn validate_id(value: &str) -> Result<(), ApiError> {
    if value.is_empty()
        || value.len() > 128
        || value.chars().any(char::is_control)
        // An ID is one URL path segment. `.` and `..` would be dropped by URL joining and
        // silently change the endpoint, so refuse them with the separators.
        || value == "."
        || value == ".."
    {
        return Err(ApiError::InvalidResponse);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{UserView, UserViewsResponse, clamp_limit};
    use crate::{BaseItemDto, ClientConfig, ItemsResponse, JellyfinClient};

    fn client() -> JellyfinClient {
        let config = ClientConfig::new(
            "https://jellyfin.example.test/",
            "Kattri test",
            "test device",
            "device-id",
            "test-version",
        )
        .expect("valid config");
        JellyfinClient::new(config).expect("client")
    }

    #[test]
    fn user_views_parses_libraries_and_missing_collection_type() {
        let json = r#"{
            "Items": [
                {"Name": "Movies", "ServerId": "srv", "Id": "lib-1", "CollectionType": "movies", "Type": "CollectionFolder"},
                {"Name": "Home videos", "Id": "lib-2", "Type": "UserView"}
            ],
            "TotalRecordCount": 2,
            "StartIndex": 0
        }"#;
        let parsed: UserViewsResponse = serde_json::from_str(json).expect("parse views");
        assert_eq!(
            parsed.items,
            vec![
                UserView {
                    id: "lib-1".into(),
                    name: Some("Movies".into()),
                    collection_type: Some("movies".into()),
                },
                UserView {
                    id: "lib-2".into(),
                    name: Some("Home videos".into()),
                    collection_type: None,
                },
            ]
        );
    }

    #[test]
    fn user_view_without_name_is_preserved_for_the_ui_fallback() {
        let views: UserViewsResponse =
            serde_json::from_str(r#"{"Items":[{"Id":"lib-1","CollectionType":"movies"}]}"#)
                .expect("sparse view parses");
        assert_eq!(views.items[0].name, None);
        assert_eq!(views.items[0].id, "lib-1");
    }

    #[test]
    fn user_views_empty_and_missing_items_yield_empty_list() {
        let empty: UserViewsResponse =
            serde_json::from_str(r#"{"Items": [], "TotalRecordCount": 0}"#).expect("empty");
        assert!(empty.items.is_empty());
        let missing: UserViewsResponse = serde_json::from_str("{}").expect("missing items");
        assert!(missing.items.is_empty());
    }

    #[test]
    fn resume_items_parses_progress_fields() {
        let json = r#"{
            "Items": [{
                "Name": "Episode 4",
                "ServerId": "srv",
                "Id": "ep-4",
                "Type": "Episode",
                "SeriesName": "Example Show",
                "SeriesId": "series-1",
                "RunTimeTicks": 27000000000,
                "Overview": "A plot summary.",
                "ImageTags": {"Primary": "tag-a"},
                "BackdropImageTags": ["bd-1"],
                "PrimaryImageAspectRatio": 1.7777,
                "UserData": {
                    "PlaybackPositionTicks": 9000000000,
                    "PlayedPercentage": 33.3,
                    "PlayCount": 0,
                    "IsFavorite": false,
                    "Played": false
                }
            }],
            "TotalRecordCount": 1,
            "StartIndex": 0
        }"#;
        let parsed: ItemsResponse = serde_json::from_str(json).expect("parse resume");
        let item: &BaseItemDto = &parsed.items[0];
        assert_eq!(item.id, "ep-4");
        assert_eq!(item.series_name.as_deref(), Some("Example Show"));
        assert_eq!(item.series_id.as_deref(), Some("series-1"));
        assert_eq!(item.run_time_ticks, Some(27_000_000_000));
        assert_eq!(
            item.backdrop_image_tags.as_deref(),
            Some(&["bd-1".to_owned()][..])
        );
        let user_data = item.user_data.as_ref().expect("user data");
        assert_eq!(user_data.playback_position_ticks, Some(9_000_000_000));
        assert_eq!(user_data.played_percentage, Some(33.3));
    }

    #[test]
    fn resume_items_empty_and_missing_optional_fields_parse() {
        let empty: ItemsResponse =
            serde_json::from_str(r#"{"Items": [], "TotalRecordCount": 0}"#).expect("empty");
        assert!(empty.items.is_empty());

        let sparse: ItemsResponse = serde_json::from_str(
            r#"{"Items": [{"Id": "movie-1", "Name": "Movie", "UserData": {}}]}"#,
        )
        .expect("sparse item");
        let item = &sparse.items[0];
        assert_eq!(item.run_time_ticks, None);
        assert_eq!(item.series_name, None);
        assert_eq!(
            item.user_data
                .as_ref()
                .and_then(|data| data.played_percentage),
            None
        );
        assert_eq!(
            item.user_data
                .as_ref()
                .and_then(|data| data.playback_position_ticks),
            None
        );
    }

    #[test]
    fn next_up_parses_episode_with_series_name() {
        let json = r#"{
            "Items": [{
                "Name": "Pilot",
                "Id": "ep-1",
                "Type": "Episode",
                "SeriesName": "Example Show",
                "SeriesId": "series-1",
                "ParentIndexNumber": 1,
                "IndexNumber": 1
            }],
            "TotalRecordCount": 1
        }"#;
        let parsed: ItemsResponse = serde_json::from_str(json).expect("parse next up");
        assert_eq!(parsed.items[0].series_name.as_deref(), Some("Example Show"));
        assert_eq!(parsed.items[0].user_data, None);
    }

    #[test]
    fn next_up_empty_and_missing_series_fields_parse() {
        let empty: ItemsResponse = serde_json::from_str(r#"{"Items": []}"#).expect("empty");
        assert!(empty.items.is_empty());
        let sparse: ItemsResponse =
            serde_json::from_str(r#"{"Items": [{"Id": "ep-2", "Name": "Untitled"}]}"#)
                .expect("sparse");
        assert_eq!(sparse.items[0].series_name, None);
        assert_eq!(sparse.items[0].series_id, None);
    }

    #[test]
    fn latest_items_parses_bare_array() {
        let json = r#"[
            {"Id": "m-1", "Name": "New Film", "Type": "Movie", "ProductionYear": 2026,
             "ImageTags": {"Primary": "p1"}, "RunTimeTicks": 72000000000},
            {"Id": "e-9", "Name": "Fresh Episode", "Type": "Episode", "SeriesName": "Example Show"}
        ]"#;
        let parsed: Vec<BaseItemDto> = serde_json::from_str(json).expect("parse latest");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].production_year, Some(2026));
        assert_eq!(parsed[1].series_name.as_deref(), Some("Example Show"));
        assert_eq!(parsed[1].run_time_ticks, None);
    }

    #[test]
    fn latest_items_empty_array_parses() {
        let parsed: Vec<BaseItemDto> = serde_json::from_str("[]").expect("empty latest");
        assert!(parsed.is_empty());
    }

    #[test]
    fn limit_is_clamped_to_bounds() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(20), 20);
        assert_eq!(clamp_limit(10_000), super::MAX_HOME_LIMIT);
    }

    #[test]
    fn image_url_has_no_credentials_and_keeps_reverse_proxy_path() {
        let url = client()
            .image_url("ep-4", "Backdrop", 1280, "bd-tag")
            .expect("image url");
        assert_eq!(url.path(), "/Items/ep-4/Images/Backdrop");
        let query = url.query().expect("query");
        assert!(query.contains("maxWidth=1280"));
        assert!(query.contains("tag=bd-tag"));
        assert!(!query.to_ascii_lowercase().contains("token"));
        assert!(!query.to_ascii_lowercase().contains("api_key"));
        assert_eq!(url.username(), "");
        assert!(url.password().is_none());
    }

    #[test]
    fn image_url_rejects_unsafe_inputs() {
        let client = client();
        assert!(client.image_url("", "Primary", 480, "t").is_err());
        assert!(client.image_url("id", "Primary/../x", 480, "t").is_err());
        assert!(client.image_url("id", "Primary", 0, "t").is_err());
        assert!(client.image_url("id", "Primary", 10_000, "t").is_err());
        assert!(client.image_url("id", "Primary", 480, "bad\ntag").is_err());
    }
}
