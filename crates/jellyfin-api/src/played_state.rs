//! Watched-state (played) toggle for one item in the authenticated user's library.
//!
//! Jellyfin's OpenAPI (`UserPlayedItems`, operations `MarkPlayedItem` and
//! `MarkUnplayedItem`) defines `POST` and `DELETE` on
//! `/UserPlayedItems/{itemId}`, with the user passed as the `userId` query
//! parameter. The older `/Users/{userId}/PlayedItems/{itemId}` path named in
//! the M1 spec is not in the stable OpenAPI and is not used.

use reqwest::Method;

use crate::{ApiError, JellyfinClient, Session, UserItemDataDto, home};

/// The HTTP method that changes the played state of an item.
fn played_method(played: bool) -> Method {
    if played { Method::POST } else { Method::DELETE }
}

impl JellyfinClient {
    /// Mark an item played (`played == true`) or unplayed (`played == false`)
    /// for the session user and return the item's updated per-user data.
    ///
    /// The server marks an item played when playback stops at the end; callers
    /// must not send a second mark after playback reaches the end. A 404 means
    /// the item is no longer available ([`ApiError::HttpStatus`] with
    /// `StatusCode::NOT_FOUND`).
    pub async fn set_played(
        &self,
        session: &Session,
        item_id: &str,
        played: bool,
    ) -> Result<UserItemDataDto, ApiError> {
        home::validate_id(item_id)?;
        let url = self.endpoint(&["UserPlayedItems", item_id]);
        let request = self
            .http
            .request(played_method(played), url)
            .query(&[("userId", session.user.id.as_str())]);
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        super::decode_response(response).await
    }
}

#[cfg(test)]
mod tests {
    use reqwest::Method;

    use super::played_method;

    #[test]
    fn played_maps_to_post_and_unplayed_maps_to_delete() {
        assert_eq!(played_method(true), Method::POST);
        assert_eq!(played_method(false), Method::DELETE);
    }

    #[test]
    fn parses_played_user_item_data_from_openapi_shape() {
        let body = r#"{
            "PlaybackPositionTicks": 0,
            "PlayCount": 1,
            "IsFavorite": false,
            "Played": true,
            "Key": "abc",
            "LastPlayedDate": "2026-01-02T03:04:05Z"
        }"#;
        let data: crate::UserItemDataDto = serde_json::from_str(body).expect("valid fixture");
        assert_eq!(data.played, Some(true));
        assert_eq!(data.play_count, Some(1));
        assert_eq!(
            data.last_played_date.as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
    }

    #[test]
    fn missing_played_flag_is_none_not_false() {
        let data: crate::UserItemDataDto =
            serde_json::from_str(r#"{"Key": "abc"}"#).expect("valid fixture");
        assert_eq!(data.played, None);
    }
}
