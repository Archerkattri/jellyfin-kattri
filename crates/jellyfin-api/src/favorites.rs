//! Favorite toggle for one item in the authenticated user's library, plus a
//! pure optimistic-toggle model shared with the watched toggle.
//!
//! Jellyfin's official OpenAPI (`MarkFavoriteItem` and `UnmarkFavoriteItem`,
//! checked 2026-10-15) defines `POST` and `DELETE` on
//! `/UserFavoriteItems/{itemId}` with the user passed as the `userId` query
//! parameter, and both return `UserItemDataDto`. The `/Users/{userId}/FavoriteItems/{itemId}`
//! path named in the M3 spec is not in the stable OpenAPI and is not used.
//! Watched state is not duplicated here: use `JellyfinClient::set_played`.

use reqwest::Method;

use crate::{ApiError, JellyfinClient, Session, UserItemDataDto, home};

/// The HTTP method that sets the favorite state of an item.
fn favorite_method(favorite: bool) -> Method {
    if favorite {
        Method::POST
    } else {
        Method::DELETE
    }
}

impl JellyfinClient {
    /// Mark an item favorite (`favorite == true`) or remove the favorite
    /// (`favorite == false`) for the session user, and return the item's
    /// updated per-user data.
    ///
    /// A 404 means the item is no longer available ([`ApiError::HttpStatus`]
    /// with `StatusCode::NOT_FOUND`). Callers should remove the item from
    /// their local lists in that case.
    pub async fn set_favorite(
        &self,
        session: &Session,
        item_id: &str,
        favorite: bool,
    ) -> Result<UserItemDataDto, ApiError> {
        home::validate_id(item_id)?;
        let url = self.endpoint(&["UserFavoriteItems", item_id]);
        let request = self
            .http
            .request(favorite_method(favorite), url)
            .query(&[("userId", session.user.id.as_str())]);
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        crate::decode_response(response).await
    }
}

/// Optimistic toggle state for one favorite or watched flag on one item.
///
/// The model is pure: it never sends anything. The caller sends the request
/// that [`toggle`](Self::toggle) or [`complete_success`](Self::complete_success)
/// asks for, then reports the outcome. At most one request is in flight. A
/// toggle made while a request is in flight changes the displayed value at
/// once, but it waits for the response before any new request is sent, so
/// requests never reorder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FavoriteToggle {
    confirmed: bool,
    displayed: bool,
    in_flight: bool,
    queued: Option<bool>,
}

impl FavoriteToggle {
    /// Start from the last state the server reported.
    pub fn new(confirmed: bool) -> Self {
        Self {
            confirmed,
            displayed: confirmed,
            in_flight: false,
            queued: None,
        }
    }

    /// The value the UI shows: the optimistic value while a request is pending.
    pub fn displayed(&self) -> bool {
        self.displayed
    }

    /// The last value the server confirmed.
    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// Whether a request is waiting for its response.
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Flip the displayed value. Returns `Some(target)` when the caller must
    /// send a request for `target` now. Returns `None` when a request is
    /// already in flight; the target is sent after that response arrives.
    pub fn toggle(&mut self) -> Option<bool> {
        self.displayed = !self.displayed;
        if self.in_flight {
            self.queued = Some(self.displayed);
            None
        } else {
            self.in_flight = true;
            Some(self.displayed)
        }
    }

    /// Record a successful response that reports `server_state`. Returns
    /// `Some(target)` when a toggle was queued behind the request and differs
    /// from the server state; the caller must send it next. Otherwise `None`.
    pub fn complete_success(&mut self, server_state: bool) -> Option<bool> {
        self.in_flight = false;
        self.confirmed = server_state;
        match self.queued.take() {
            Some(next) if next != server_state => {
                self.in_flight = true;
                Some(next)
            }
            _ => {
                self.displayed = server_state;
                None
            }
        }
    }

    /// Record a failed request. The displayed value reverts to the last
    /// confirmed state, and queued toggles are dropped.
    pub fn complete_failure(&mut self) {
        self.in_flight = false;
        self.queued = None;
        self.displayed = self.confirmed;
    }
}

#[cfg(test)]
mod tests {
    use reqwest::Method;

    use super::{FavoriteToggle, favorite_method};
    use crate::UserItemDataDto;

    #[test]
    fn favorite_maps_to_post_and_unfavorite_maps_to_delete() {
        assert_eq!(favorite_method(true), Method::POST);
        assert_eq!(favorite_method(false), Method::DELETE);
    }

    #[test]
    fn parses_favorite_user_item_data_from_openapi_shape() {
        let body = r#"{"IsFavorite": true, "Played": false, "PlayCount": 0, "Key": "k"}"#;
        let data: UserItemDataDto = serde_json::from_str(body).expect("valid fixture");
        assert_eq!(data.is_favorite, Some(true));
        assert_eq!(data.played, Some(false));
    }

    #[test]
    fn toggle_sends_immediately_and_flips_display() {
        let mut toggle = FavoriteToggle::new(false);
        assert_eq!(toggle.toggle(), Some(true));
        assert!(toggle.displayed());
        assert!(toggle.in_flight());
        assert!(!toggle.confirmed());
    }

    #[test]
    fn toggle_while_in_flight_waits_for_response() {
        let mut toggle = FavoriteToggle::new(false);
        assert_eq!(toggle.toggle(), Some(true));
        assert_eq!(toggle.toggle(), None, "second toggle must not send");
        assert!(!toggle.displayed());
        assert!(toggle.in_flight());
    }

    #[test]
    fn queued_toggle_is_sent_after_response_when_it_differs() {
        let mut toggle = FavoriteToggle::new(false);
        assert_eq!(toggle.toggle(), Some(true));
        assert_eq!(toggle.toggle(), None);
        // The queued target (false) differs from the server state (true), so
        // it is sent only now, after the first response arrived.
        assert_eq!(toggle.complete_success(true), Some(false));
        assert!(toggle.in_flight());
        assert_eq!(toggle.complete_success(false), None);
        assert!(!toggle.in_flight());
        assert!(!toggle.displayed());
    }

    #[test]
    fn queued_toggle_matching_server_result_is_not_resent() {
        let mut toggle = FavoriteToggle::new(false);
        assert_eq!(toggle.toggle(), Some(true));
        assert_eq!(toggle.toggle(), None);
        assert_eq!(toggle.toggle(), None);
        // Queue ends at true, which equals the server result: no second send.
        assert_eq!(toggle.complete_success(true), None);
        assert!(toggle.displayed());
        assert!(!toggle.in_flight());
    }

    #[test]
    fn success_takes_server_state_when_nothing_is_queued() {
        let mut toggle = FavoriteToggle::new(false);
        assert_eq!(toggle.toggle(), Some(true));
        assert_eq!(toggle.complete_success(false), None);
        assert!(!toggle.displayed());
        assert!(!toggle.confirmed());
    }

    #[test]
    fn failure_reverts_to_confirmed_and_drops_queue() {
        let mut toggle = FavoriteToggle::new(true);
        assert_eq!(toggle.toggle(), Some(false));
        assert_eq!(toggle.toggle(), None);
        toggle.complete_failure();
        assert!(toggle.displayed());
        assert!(toggle.confirmed());
        assert!(!toggle.in_flight());
        // A later toggle sends again without any leftover queued state.
        assert_eq!(toggle.toggle(), Some(false));
    }

    #[test]
    fn at_most_one_request_is_in_flight_at_a_time() {
        let mut toggle = FavoriteToggle::new(false);
        let mut sends = 0;
        if toggle.toggle().is_some() {
            sends += 1;
        }
        for _ in 0..5 {
            if toggle.toggle().is_some() {
                sends += 1;
            }
        }
        assert_eq!(sends, 1);
    }
}
