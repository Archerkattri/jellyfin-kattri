//! Favorite and watched toggles, and error classification, driven through the
//! real `JellyfinClient` against the in-process mock server over loopback HTTP.
//! This validates the client against the mock only, not against a real
//! Jellyfin server. Endpoint paths were checked against the official stable
//! OpenAPI (`MarkFavoriteItem`, `UnmarkFavoriteItem`, `MarkPlayedItem`,
//! `MarkUnplayedItem`) on 2026-10-15.

use jellyfin_api::{
    ApiError, ClientConfig, ErrorClass, FavoriteToggle, ItemFilter, ItemQuery, JellyfinClient,
    Session, connectivity::OfflineTracker,
};
use mock_jellyfin::{MockConfig, RunningServer, spawn};
use reqwest::StatusCode;
use serde_json::Value;

fn client_for(base_url: &str) -> JellyfinClient {
    let config = ClientConfig::new(
        base_url,
        "Kattri test",
        "test device",
        "device-id",
        "test-version",
    )
    .expect("valid config");
    JellyfinClient::new(config).expect("loopback HTTP is allowed")
}

async fn start(config: MockConfig) -> RunningServer {
    spawn(config).await.expect("spawn mock server")
}

async fn demo_session(client: &JellyfinClient) -> Session {
    client
        .authenticate("demo", "demo")
        .await
        .expect("demo sign-in")
}

async fn first_item_id(client: &JellyfinClient, session: &Session, item_type: &str) -> String {
    client
        .list_items(
            session,
            &ItemQuery {
                include_item_types: Some(vec![item_type.to_owned()]),
                limit: 1,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list items")
        .items
        .first()
        .unwrap_or_else(|| panic!("seed has a {item_type}"))
        .id
        .clone()
}

/// Sanitized method/path entries recorded by the mock.
async fn recorded_requests(server: &RunningServer) -> Vec<(String, String)> {
    let value: Value = reqwest::get(format!("{}/__mock/requests", server.base_url()))
        .await
        .expect("request log request completes")
        .json()
        .await
        .expect("request log is JSON");
    value
        .as_array()
        .expect("request log array")
        .iter()
        .map(|entry| {
            (
                entry["Method"].as_str().expect("method").to_owned(),
                entry["Path"]
                    .as_str()
                    .expect("path")
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

fn is_status(error: &ApiError, expected: StatusCode) -> bool {
    matches!(error, ApiError::HttpStatus(status) if *status == expected)
}

#[tokio::test]
async fn favorite_then_unfavorite_movie_hits_expected_paths() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let movie_id = first_item_id(&client, &session, "Movie").await;

    let favorited = client
        .set_favorite(&session, &movie_id, true)
        .await
        .expect("favorite movie");
    assert_eq!(favorited.is_favorite, Some(true));

    let unfavorited = client
        .set_favorite(&session, &movie_id, false)
        .await
        .expect("unfavorite movie");
    assert_eq!(unfavorited.is_favorite, Some(false));

    let requests = recorded_requests(&server).await;
    let path = format!("/UserFavoriteItems/{movie_id}");
    assert!(requests.contains(&("POST".to_owned(), path.clone())));
    assert!(requests.contains(&("DELETE".to_owned(), path)));
    server.shutdown().await;
}

#[tokio::test]
async fn favorite_music_track_hits_expected_path() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let track_id = first_item_id(&client, &session, "Audio").await;

    let data = client
        .set_favorite(&session, &track_id, true)
        .await
        .expect("favorite track");
    assert_eq!(data.is_favorite, Some(true));

    let requests = recorded_requests(&server).await;
    assert!(requests.contains(&("POST".to_owned(), format!("/UserFavoriteItems/{track_id}"))));
    server.shutdown().await;
}

#[tokio::test]
async fn favorite_photo_hits_expected_path() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let photo_id = first_item_id(&client, &session, "Photo").await;

    let data = client
        .set_favorite(&session, &photo_id, true)
        .await
        .expect("favorite photo");
    assert_eq!(data.is_favorite, Some(true));

    let requests = recorded_requests(&server).await;
    assert!(requests.contains(&("POST".to_owned(), format!("/UserFavoriteItems/{photo_id}"))));
    server.shutdown().await;
}

#[tokio::test]
async fn favorite_filter_reflects_favorite_and_unfavorite_mutations() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let movie_id = first_item_id(&client, &session, "Movie").await;

    client
        .set_favorite(&session, &movie_id, true)
        .await
        .expect("favorite movie");
    let favorites = client
        .list_items(
            &session,
            &ItemQuery {
                filters: Some(vec![ItemFilter::IsFavorite]),
                limit: 100,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list favorites");
    assert!(favorites.items.iter().any(|item| item.id == movie_id));
    assert!(
        favorites.items.iter().all(|item| {
            item.user_data.as_ref().and_then(|data| data.is_favorite) == Some(true)
        })
    );

    client
        .set_favorite(&session, &movie_id, false)
        .await
        .expect("unfavorite movie");
    let favorites = client
        .list_items(
            &session,
            &ItemQuery {
                filters: Some(vec![ItemFilter::IsFavorite]),
                limit: 100,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("refresh favorites");
    assert!(!favorites.items.iter().any(|item| item.id == movie_id));
    server.shutdown().await;
}

#[tokio::test]
async fn watched_toggle_works_on_music_track() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let track_id = first_item_id(&client, &session, "Audio").await;

    let played = client
        .set_played(&session, &track_id, true)
        .await
        .expect("mark track played");
    assert_eq!(played.played, Some(true));

    let unplayed = client
        .set_played(&session, &track_id, false)
        .await
        .expect("mark track unplayed");
    assert_eq!(unplayed.played, Some(false));
    server.shutdown().await;
}

#[tokio::test]
async fn favorite_unknown_item_is_not_found() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let missing = "ffffffffffffffffffffffffffffffff";

    let error = client
        .set_favorite(&session, missing, true)
        .await
        .expect_err("unknown item");
    assert!(is_status(&error, StatusCode::NOT_FOUND));
    assert_eq!(error.classify(), ErrorClass::NotFound);
    server.shutdown().await;
}

#[tokio::test]
async fn empty_item_id_is_rejected_before_any_request() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let before = recorded_requests(&server).await.len();

    let error = client
        .set_favorite(&session, "", true)
        .await
        .expect_err("empty id");
    assert!(matches!(error, ApiError::InvalidResponse));
    assert_eq!(recorded_requests(&server).await.len(), before);
    server.shutdown().await;
}

#[tokio::test]
async fn favorite_toggle_model_drives_real_requests() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let movie_id = first_item_id(&client, &session, "Movie").await;

    let mut toggle = FavoriteToggle::new(false);
    let target = toggle.toggle().expect("first toggle sends a request");
    let data = client
        .set_favorite(&session, &movie_id, target)
        .await
        .expect("favorite via model");
    assert_eq!(
        toggle.complete_success(data.is_favorite.unwrap_or(target)),
        None
    );
    assert!(toggle.displayed());
    assert!(toggle.confirmed());

    let target = toggle.toggle().expect("second toggle sends a request");
    assert!(!target);
    let data = client
        .set_favorite(&session, &movie_id, target)
        .await
        .expect("unfavorite via model");
    assert_eq!(
        toggle.complete_success(data.is_favorite.unwrap_or(target)),
        None
    );
    assert!(!toggle.displayed());
    server.shutdown().await;
}

#[tokio::test]
async fn played_toggle_server_errors_classify_for_the_ui() {
    let cases = [
        (500, ErrorClass::ServerUnavailable),
        (503, ErrorClass::ServerUnavailable),
        (401, ErrorClass::AuthExpired),
        (403, ErrorClass::Forbidden),
        (404, ErrorClass::NotFound),
        (400, ErrorClass::Unexpected),
    ];
    for (status, expected) in cases {
        let server = start(MockConfig {
            played_toggle_status: Some(status),
            ..MockConfig::default()
        })
        .await;
        let client = client_for(&server.base_url());
        let session = demo_session(&client).await;
        let movie_id = first_item_id(&client, &session, "Movie").await;

        let error = client
            .set_played(&session, &movie_id, true)
            .await
            .expect_err("forced status must fail");
        assert_eq!(error.classify(), expected, "status {status}");
        server.shutdown().await;
    }
}

#[tokio::test]
async fn refused_connection_classifies_as_offline_and_counts_toward_offline() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let movie_id = first_item_id(&client, &session, "Movie").await;
    // Stop the listener so the next connections are refused.
    server.shutdown().await;

    let mut offline = OfflineTracker::new();
    for attempt in 0..2 {
        let error = client
            .set_favorite(&session, &movie_id, true)
            .await
            .expect_err("no listener");
        assert!(
            matches!(error, ApiError::Unreachable),
            "expected Unreachable, got {error:?}"
        );
        assert_eq!(error.classify(), ErrorClass::Offline);
        assert!(error.classify().counts_toward_offline());
        assert!(error.classify().is_retryable());
        offline.record_error_class(error.classify());
        assert_eq!(offline.is_offline(), attempt == 1);
    }
}

#[tokio::test]
async fn temporary_server_failure_then_success_clears_offline_tracker() {
    let server = start(MockConfig {
        public_info_failures: 1,
        ..MockConfig::default()
    })
    .await;
    let client = client_for(&server.base_url());
    let mut offline = OfflineTracker::new();

    let first = client.public_system_info().await;
    let error = first.as_ref().expect_err("mock fails the first request");
    assert_eq!(error.classify(), ErrorClass::ServerUnavailable);
    offline.record(&first);
    assert!(!offline.is_offline());

    let recovered = client.public_system_info().await;
    assert!(recovered.is_ok());
    offline.record(&recovered);
    assert!(!offline.is_offline());
    assert_eq!(offline.consecutive_failures(), 0);
    server.shutdown().await;
}

#[tokio::test]
async fn refused_plaintext_to_remote_host_is_classified_before_any_request() {
    let config = ClientConfig::new(
        "http://192.0.2.10:8096",
        "Kattri test",
        "test device",
        "device-id",
        "test-version",
    )
    .expect("config parses");
    let error = match JellyfinClient::new(config) {
        Ok(_) => panic!("remote plaintext must be refused without opt-in"),
        Err(error) => error,
    };
    assert_eq!(error.classify(), ErrorClass::RefusedPlaintext);
    assert!(!error.classify().counts_toward_offline());
}
