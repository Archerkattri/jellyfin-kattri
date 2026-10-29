//! Typed playlist calls: list, read entries, create, add, remove, move, rename,
//! and delete.
//!
//! Endpoints, confirmed against the official OpenAPI document at
//! `https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json`:
//!
//! - `GET /Items` with `includeItemTypes=Playlist` (operation `GetItems`)
//! - `GET /Playlists/{playlistId}` (`GetPlaylist`, schema `PlaylistDto`)
//! - `GET /Playlists/{playlistId}/Items` (`GetPlaylistItems`)
//! - `POST /Playlists` with a `CreatePlaylistDto` body (`CreatePlaylist`)
//! - `POST /Playlists/{playlistId}` with an `UpdatePlaylistDto` body (`UpdatePlaylist`)
//! - `POST /Playlists/{playlistId}/Items?ids=` (`AddItemToPlaylist`)
//! - `DELETE /Playlists/{playlistId}/Items?entryIds=` (`RemoveItemFromPlaylist`)
//! - `POST /Playlists/{playlistId}/Items/{itemId}/Move/{newIndex}` (`MoveItem`)
//! - `DELETE /Items/{itemId}` (`DeleteItem`)
//!
//! The OpenAPI describes the Move path parameter only as "The item id.". This
//! client passes the playlist entry ID (`PlaylistItemId`) there, because removal
//! also works on entry IDs. Confirmed from Jellyfin server source 2026-10-19.
//!
//! Playlist requests always send the session token. Identifiers are checked
//! before they are put into a URL or query string, and large ID lists are split
//! into bounded requests.

use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};

use crate::{
    ApiError, BaseItemDto, ItemQuery, ItemsResponse, JellyfinClient, Session, decode_response,
    home::{MAX_HOME_LIMIT, validate_id},
};

/// Most item or entry IDs sent in one `ids` or `entryIds` query. Longer lists are
/// split into several requests so the URL stays a bounded length.
pub const MAX_PLAYLIST_IDS_PER_REQUEST: usize = 100;

/// Playlist metadata from `GET /Playlists/{playlistId}`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlaylistDto {
    pub item_ids: Vec<String>,
    pub open_access: bool,
    pub shares: Vec<PlaylistUserPermissions>,
}

/// A user's permissions on a playlist.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlaylistUserPermissions {
    pub user_id: String,
    pub can_edit: bool,
}

/// One entry of a playlist. `item` is the library item; `playlist_item_id` is
/// the entry ID used for removal and reordering. The same item can appear
/// several times with different entry IDs.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct PlaylistEntry {
    #[serde(flatten)]
    pub item: BaseItemDto,
    #[serde(default)]
    pub playlist_item_id: Option<String>,
}

/// One page of a playlist's entries, in playlist order.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlaylistItemsResponse {
    pub items: Vec<PlaylistEntry>,
    pub total_record_count: Option<u32>,
    pub start_index: u32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PlaylistCreationResult {
    id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct CreatePlaylistRequest<'a> {
    name: &'a str,
    ids: &'a [String],
    user_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct UpdatePlaylistRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ids: Option<&'a [String]>,
}

impl JellyfinClient {
    /// List the user's playlists (`GET /Items?includeItemTypes=Playlist&recursive=true`).
    ///
    /// `limit` is clamped to `1..=MAX_HOME_LIMIT`.
    pub async fn list_playlists(
        &self,
        session: &Session,
        limit: u32,
    ) -> Result<ItemsResponse, ApiError> {
        self.list_items(
            session,
            &ItemQuery {
                recursive: true,
                limit: limit.clamp(1, MAX_HOME_LIMIT),
                include_item_types: Some(vec!["Playlist".to_owned()]),
                ..ItemQuery::default()
            },
        )
        .await
    }

    /// Fetch a playlist's metadata and item IDs (`GET /Playlists/{playlistId}`).
    /// A 404 means the playlist no longer exists.
    pub async fn get_playlist(
        &self,
        session: &Session,
        playlist_id: &str,
    ) -> Result<PlaylistDto, ApiError> {
        validate_playlist_id(playlist_id)?;
        self.playlist_request_json(session, Method::GET, &["Playlists", playlist_id], &[])
            .await
    }

    /// Fetch one page of a playlist's entries in playlist order
    /// (`GET /Playlists/{playlistId}/Items`). `limit` is clamped to
    /// `1..=MAX_HOME_LIMIT`.
    pub async fn list_playlist_items(
        &self,
        session: &Session,
        playlist_id: &str,
        start_index: u32,
        limit: u32,
    ) -> Result<PlaylistItemsResponse, ApiError> {
        validate_playlist_id(playlist_id)?;
        let query = [
            ("userId", session.user.id.clone()),
            ("startIndex", start_index.to_string()),
            ("limit", limit.clamp(1, MAX_HOME_LIMIT).to_string()),
        ];
        self.playlist_request_json(
            session,
            Method::GET,
            &["Playlists", playlist_id, "Items"],
            &query,
        )
        .await
    }

    /// Create a playlist owned by the session user (`POST /Playlists`) and
    /// return its new ID. `item_ids` seeds the playlist in order and may be
    /// empty.
    pub async fn create_playlist(
        &self,
        session: &Session,
        name: &str,
        item_ids: &[String],
    ) -> Result<String, ApiError> {
        validate_ids(item_ids)?;
        let body = CreatePlaylistRequest {
            name,
            ids: item_ids,
            user_id: session.user.id.as_str(),
        };
        let result: PlaylistCreationResult = self
            .playlist_send_json(session, Method::POST, &["Playlists"], &[], &body)
            .await?;
        validate_id(&result.id)?;
        Ok(result.id)
    }

    /// Rename a playlist (`POST /Playlists/{playlistId}` with only `Name` set;
    /// the server keeps fields that are omitted).
    pub async fn rename_playlist(
        &self,
        session: &Session,
        playlist_id: &str,
        name: &str,
    ) -> Result<(), ApiError> {
        validate_playlist_id(playlist_id)?;
        let body = UpdatePlaylistRequest {
            name: Some(name),
            ids: None,
        };
        self.playlist_send_empty(
            session,
            Method::POST,
            &["Playlists", playlist_id],
            &[],
            Some(&body),
        )
        .await
    }

    /// Append items to a playlist (`POST /Playlists/{playlistId}/Items?ids=`).
    /// Duplicates are allowed. Lists longer than
    /// [`MAX_PLAYLIST_IDS_PER_REQUEST`] are sent in several requests, in order.
    pub async fn add_playlist_items(
        &self,
        session: &Session,
        playlist_id: &str,
        item_ids: &[String],
    ) -> Result<(), ApiError> {
        validate_playlist_id(playlist_id)?;
        validate_ids(item_ids)?;
        for chunk in item_ids.chunks(MAX_PLAYLIST_IDS_PER_REQUEST) {
            let query = [
                ("ids", chunk.join(",")),
                ("userId", session.user.id.clone()),
            ];
            self.playlist_send_empty(
                session,
                Method::POST,
                &["Playlists", playlist_id, "Items"],
                &query,
                None::<&()>,
            )
            .await?;
        }
        Ok(())
    }

    /// Remove entries from a playlist by entry ID
    /// (`DELETE /Playlists/{playlistId}/Items?entryIds=`). Entry IDs are used
    /// rather than item IDs so one duplicate can be removed without the others.
    pub async fn remove_playlist_entries(
        &self,
        session: &Session,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), ApiError> {
        validate_playlist_id(playlist_id)?;
        validate_ids(entry_ids)?;
        for chunk in entry_ids.chunks(MAX_PLAYLIST_IDS_PER_REQUEST) {
            let query = [("entryIds", chunk.join(","))];
            self.playlist_send_empty(
                session,
                Method::DELETE,
                &["Playlists", playlist_id, "Items"],
                &query,
                None::<&()>,
            )
            .await?;
        }
        Ok(())
    }

    /// Move one entry to `new_index`, 0-based
    /// (`POST /Playlists/{playlistId}/Items/{entryId}/Move/{newIndex}`).
    /// Server source confirms this path parameter is the linked-child entry ID.
    pub async fn move_playlist_entry(
        &self,
        session: &Session,
        playlist_id: &str,
        entry_id: &str,
        new_index: u32,
    ) -> Result<(), ApiError> {
        validate_playlist_id(playlist_id)?;
        validate_playlist_id(entry_id)?;
        let new_index = new_index.to_string();
        self.playlist_send_empty(
            session,
            Method::POST,
            &[
                "Playlists",
                playlist_id,
                "Items",
                entry_id,
                "Move",
                new_index.as_str(),
            ],
            &[],
            None::<&()>,
        )
        .await
    }

    /// Delete a playlist (`DELETE /Items/{playlistId}`, OpenAPI `DeleteItem`).
    /// A 404 means it is already gone.
    pub async fn delete_playlist(
        &self,
        session: &Session,
        playlist_id: &str,
    ) -> Result<(), ApiError> {
        validate_playlist_id(playlist_id)?;
        self.playlist_send_empty(
            session,
            Method::DELETE,
            &["Items", playlist_id],
            &[],
            None::<&()>,
        )
        .await
    }

    async fn playlist_request_json<T: for<'de> Deserialize<'de>>(
        &self,
        session: &Session,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<T, ApiError> {
        let request = self
            .http
            .request(method, self.endpoint(segments))
            .query(query);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }

    async fn playlist_send_json<T: for<'de> Deserialize<'de>, B: Serialize>(
        &self,
        session: &Session,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: &B,
    ) -> Result<T, ApiError> {
        let request = self
            .http
            .request(method, self.endpoint(segments))
            .query(query)
            .json(body);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }

    async fn playlist_send_empty<B: Serialize>(
        &self,
        session: &Session,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<&B>,
    ) -> Result<(), ApiError> {
        let mut request = self
            .http
            .request(method, self.endpoint(segments))
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        expect_no_content(response.status(), response.headers())
    }
}

/// Map a status from an endpoint with no response body. Success is any 2xx
/// status; 429/503 keep their Retry-After like the shared `decode_response` path.
fn expect_no_content(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Result<(), ApiError> {
    if status == StatusCode::UNAUTHORIZED {
        return Err(ApiError::ReauthenticationRequired);
    }
    if status == StatusCode::FORBIDDEN {
        return Err(ApiError::Forbidden);
    }
    if !status.is_success() {
        return Err(crate::status_error(status, headers));
    }
    Ok(())
}

/// Playlist and entry IDs are Jellyfin GUIDs: 32 hex digits, optionally with
/// dashes. Anything else is rejected before a request is sent, so a value
/// cannot add a separator to the comma-joined `ids` query.
pub(crate) fn validate_playlist_id(value: &str) -> Result<(), ApiError> {
    validate_id(value)?;
    if !value.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(ApiError::InvalidResponse);
    }
    Ok(())
}

fn validate_ids(values: &[String]) -> Result<(), ApiError> {
    values
        .iter()
        .try_for_each(|value| validate_playlist_id(value))
}

#[cfg(test)]
mod tests {
    use super::{
        PlaylistCreationResult, PlaylistItemsResponse, UpdatePlaylistRequest, expect_no_content,
        validate_playlist_id,
    };
    use crate::ApiError;
    use reqwest::StatusCode;

    #[test]
    fn parses_playlist_entries_with_entry_ids_and_duplicates() {
        let body = r#"{
            "Items": [
                {"Id": "a1b2c3d4e5f60718293a4b5c6d7e8f90", "Name": "Pilot", "Type": "Episode", "PlaylistItemId": "0f0e0d0c0b0a09080706050403020100"},
                {"Id": "a1b2c3d4e5f60718293a4b5c6d7e8f90", "Name": "Pilot", "Type": "Episode", "PlaylistItemId": "1f0e0d0c0b0a09080706050403020100"}
            ],
            "TotalRecordCount": 2,
            "StartIndex": 0
        }"#;
        let parsed: PlaylistItemsResponse = serde_json::from_str(body).expect("valid fixture");
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.items[0].item.id, parsed.items[1].item.id);
        assert_eq!(
            parsed.items[0].playlist_item_id.as_deref(),
            Some("0f0e0d0c0b0a09080706050403020100")
        );
        assert_ne!(
            parsed.items[0].playlist_item_id,
            parsed.items[1].playlist_item_id
        );
        assert_eq!(parsed.total_record_count, Some(2));
    }

    #[test]
    fn entry_without_playlist_item_id_is_none_not_error() {
        let parsed: PlaylistItemsResponse =
            serde_json::from_str(r#"{"Items":[{"Id":"abc","Name":"X"}]}"#).expect("fixture");
        assert_eq!(parsed.items[0].playlist_item_id, None);
        assert_eq!(parsed.items[0].item.name, "X");
    }

    #[test]
    fn creation_result_parses_playlist_id() {
        let parsed: PlaylistCreationResult =
            serde_json::from_str(r#"{"Id":"0123456789abcdef0123456789abcdef"}"#).expect("fixture");
        assert_eq!(parsed.id, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn rename_body_sends_only_the_name() {
        let body = UpdatePlaylistRequest {
            name: Some("Weekend"),
            ids: None,
        };
        let json = serde_json::to_value(&body).expect("serialize");
        assert_eq!(json, serde_json::json!({"Name": "Weekend"}));
    }

    #[test]
    fn playlist_id_rejects_separators_and_non_guid_characters() {
        assert!(validate_playlist_id("0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_playlist_id("01234567-89ab-cdef-0123-456789abcdef").is_ok());
        assert!(validate_playlist_id("a,b").is_err());
        assert!(validate_playlist_id("abc def").is_err());
        assert!(validate_playlist_id("").is_err());
    }

    #[test]
    fn empty_success_statuses_are_ok_and_errors_map_like_json_calls() {
        let headers = reqwest::header::HeaderMap::new();
        assert!(expect_no_content(StatusCode::NO_CONTENT, &headers).is_ok());
        assert!(expect_no_content(StatusCode::OK, &headers).is_ok());
        assert!(matches!(
            expect_no_content(StatusCode::UNAUTHORIZED, &headers),
            Err(ApiError::ReauthenticationRequired)
        ));
        assert!(matches!(
            expect_no_content(StatusCode::FORBIDDEN, &headers),
            Err(ApiError::Forbidden)
        ));
        assert!(matches!(
            expect_no_content(StatusCode::NOT_FOUND, &headers),
            Err(ApiError::HttpStatus(StatusCode::NOT_FOUND))
        ));
    }
}
