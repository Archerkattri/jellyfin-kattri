//! Mock favorite routes: `POST` and `DELETE /UserFavoriteItems/{itemId}`, the
//! Jellyfin OpenAPI operations `MarkFavoriteItem` and `UnmarkFavoriteItem`.
//! This is not a real server.
//!
//! Favorite changes are held in the running mock's per-user override map and
//! are visible to item lookup and favorite-filtered item queries. Unknown item
//! IDs get 404.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::json;

use crate::{AppState, is_authorized, json_ok, record_request, seed, unauthorized};

/// Routes for this module. Merge the result into the main mock router.
pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new().route(
        "/UserFavoriteItems/{id}",
        post(mark_favorite).delete(unmark_favorite),
    )
}

fn set_favorite(
    state: &AppState,
    headers: &HeaderMap,
    raw_query: Option<&str>,
    item_id: &str,
    favorite: bool,
) -> Response {
    if !is_authorized(headers) {
        return unauthorized();
    }
    let method = if favorite { "POST" } else { "DELETE" };
    record_request(
        state,
        method,
        &format!("/UserFavoriteItems/{item_id}"),
        raw_query,
    );
    let Some(item) = state.catalog.get(item_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(mut overrides) = state.favorite_overrides.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    overrides.insert(item_id.to_owned(), favorite);
    let mut user_data = seed::user_data_json(item);
    if let Some(object) = user_data.as_object_mut() {
        object.insert("IsFavorite".into(), json!(favorite));
    }
    json_ok(user_data)
}

async fn mark_favorite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    Path(id): Path<String>,
) -> Response {
    set_favorite(&state, &headers, raw_query.as_deref(), &id, true)
}

async fn unmark_favorite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    Path(id): Path<String>,
) -> Response {
    set_favorite(&state, &headers, raw_query.as_deref(), &id, false)
}
