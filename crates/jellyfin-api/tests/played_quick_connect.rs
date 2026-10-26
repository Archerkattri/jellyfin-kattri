//! Drives the real `JellyfinClient` against the in-process mock server over
//! loopback HTTP. This validates the client against the mock only, not
//! against a real Jellyfin server.

use jellyfin_api::{
    ApiError, ClientConfig, ItemQuery, JellyfinClient, QuickConnectStatus, Session,
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

async fn first_movie_id(client: &JellyfinClient, session: &Session) -> String {
    client
        .list_items(
            session,
            &ItemQuery {
                include_item_types: Some(vec!["Movie".to_owned()]),
                limit: 1,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list movies")
        .items
        .first()
        .expect("seed has a movie")
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
                entry["Path"].as_str().expect("path").to_owned(),
            )
        })
        .collect()
}

fn is_status(error: &ApiError, expected: StatusCode) -> bool {
    matches!(error, ApiError::HttpStatus(status) if *status == expected)
}

#[tokio::test]
async fn mark_played_then_unplayed_updates_item_state() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let item_id = first_movie_id(&client, &session).await;

    let played = client
        .set_played(&session, &item_id, true)
        .await
        .expect("mark played");
    assert_eq!(played.played, Some(true));

    let item = client
        .get_item(&session, &item_id)
        .await
        .expect("item after mark played");
    assert_eq!(
        item.user_data.as_ref().and_then(|data| data.played),
        Some(true)
    );

    let unplayed = client
        .set_played(&session, &item_id, false)
        .await
        .expect("mark unplayed");
    assert_eq!(unplayed.played, Some(false));

    let item = client
        .get_item(&session, &item_id)
        .await
        .expect("item after mark unplayed");
    assert_eq!(
        item.user_data.as_ref().and_then(|data| data.played),
        Some(false)
    );

    let requests = recorded_requests(&server).await;
    let played_path = format!("/UserPlayedItems/{item_id}");
    let toggles: Vec<(&str, &str)> = requests
        .iter()
        .filter(|(_, path)| path.starts_with("/UserPlayedItems/"))
        .map(|(method, path)| (method.as_str(), path.split('?').next().unwrap_or_default()))
        .collect();
    assert_eq!(
        toggles,
        [
            ("POST", played_path.as_str()),
            ("DELETE", played_path.as_str())
        ]
    );
    server.shutdown().await;
}

#[tokio::test]
async fn mark_played_unknown_item_is_not_found() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;

    let error = client
        .set_played(&session, "00000000000000000000000000000fff", true)
        .await
        .expect_err("unknown item");
    assert!(is_status(&error, StatusCode::NOT_FOUND), "{error:?}");
    server.shutdown().await;
}

#[tokio::test]
async fn played_toggle_server_error_leaves_item_state_unchanged() {
    let server = start(MockConfig {
        played_toggle_status: Some(500),
        ..MockConfig::default()
    })
    .await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;
    let item_id = first_movie_id(&client, &session).await;
    let before = client
        .get_item(&session, &item_id)
        .await
        .expect("item before toggle")
        .user_data
        .and_then(|data| data.played);

    let error = client
        .set_played(&session, &item_id, true)
        .await
        .expect_err("server error");
    assert!(
        is_status(&error, StatusCode::INTERNAL_SERVER_ERROR),
        "{error:?}"
    );

    let after = client
        .get_item(&session, &item_id)
        .await
        .expect("item after failed toggle")
        .user_data
        .and_then(|data| data.played);
    assert_eq!(before, after);
    server.shutdown().await;
}

#[tokio::test]
async fn empty_item_id_is_rejected_before_any_request() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());
    let session = demo_session(&client).await;

    let error = client
        .set_played(&session, "", true)
        .await
        .expect_err("empty id");
    assert!(matches!(error, ApiError::InvalidResponse), "{error:?}");
    let requests = recorded_requests(&server).await;
    assert!(
        requests
            .iter()
            .all(|(_, path)| !path.starts_with("/UserPlayedItems")),
        "no played request should reach the server"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn quick_connect_enabled_reflects_server_setting() {
    let enabled = start(MockConfig::default()).await;
    let client = client_for(&enabled.base_url());
    assert!(client.quick_connect_enabled().await.expect("enabled check"));
    enabled.shutdown().await;

    let disabled = start(MockConfig {
        quick_connect_disabled: true,
        ..MockConfig::default()
    })
    .await;
    let client = client_for(&disabled.base_url());
    assert!(
        !client
            .quick_connect_enabled()
            .await
            .expect("disabled check")
    );
    disabled.shutdown().await;

    let older_server = start(MockConfig {
        quick_connect_enabled_status: Some(StatusCode::NOT_FOUND.as_u16()),
        ..MockConfig::default()
    })
    .await;
    let client = client_for(&older_server.base_url());
    assert!(
        !client
            .quick_connect_enabled()
            .await
            .expect("404 means Quick Connect is unavailable")
    );
    older_server.shutdown().await;
}

#[tokio::test]
async fn quick_connect_initiate_returns_code_and_hides_secret() {
    let server = start(MockConfig::default()).await;
    let client = client_for(&server.base_url());

    let request = client
        .initiate_quick_connect()
        .await
        .expect("initiate quick connect");
    assert_eq!(request.code().len(), 6);
    assert!(request.code().chars().all(|c| c.is_ascii_digit()));
    let rendered = format!("{request:?}");
    assert!(!rendered.contains("mock-quick-connect-secret"));
    assert!(rendered.contains(request.code()));
    server.shutdown().await;
}

#[tokio::test]
async fn quick_connect_initiate_is_unauthorized_when_inactive() {
    let server = start(MockConfig {
        quick_connect_disabled: true,
        ..MockConfig::default()
    })
    .await;
    let client = client_for(&server.base_url());

    let error = client
        .initiate_quick_connect()
        .await
        .expect_err("quick connect inactive");
    assert!(is_status(&error, StatusCode::UNAUTHORIZED), "{error:?}");
    server.shutdown().await;
}

#[tokio::test]
async fn quick_connect_happy_path_polls_then_authenticates_once() {
    let server = start(MockConfig {
        quick_connect_pending_polls: 2,
        ..MockConfig::default()
    })
    .await;
    let client = client_for(&server.base_url());

    let request = client.initiate_quick_connect().await.expect("initiate");
    let secret = request.secret();

    assert_eq!(
        client.quick_connect_status(secret).await.expect("poll 1"),
        QuickConnectStatus::Pending
    );
    assert_eq!(
        client.quick_connect_status(secret).await.expect("poll 2"),
        QuickConnectStatus::Pending
    );
    assert_eq!(
        client.quick_connect_status(secret).await.expect("poll 3"),
        QuickConnectStatus::Authorized
    );

    let session = client
        .authenticate_with_quick_connect(secret)
        .await
        .expect("quick connect sign-in");
    assert_eq!(session.user.name, "demo");

    // The secret is single use: the server forgets it after authentication.
    let polled_after = client
        .quick_connect_status(secret)
        .await
        .expect_err("secret consumed");
    assert!(
        is_status(&polled_after, StatusCode::NOT_FOUND),
        "{polled_after:?}"
    );
    let reused = client
        .authenticate_with_quick_connect(secret)
        .await
        .expect_err("secret reused");
    assert!(
        matches!(reused, ApiError::ReauthenticationRequired),
        "{reused:?}"
    );

    let requests = recorded_requests(&server).await;
    let paths: Vec<&str> = requests.iter().map(|(_, path)| path.as_str()).collect();
    assert!(paths.contains(&"/QuickConnect/Initiate"));
    assert!(paths.contains(&"/QuickConnect/Connect"));
    assert!(paths.contains(&"/Users/AuthenticateWithQuickConnect"));
    server.shutdown().await;
}

#[tokio::test]
async fn quick_connect_authenticate_before_approval_is_rejected() {
    let server = start(MockConfig {
        quick_connect_pending_polls: 5,
        ..MockConfig::default()
    })
    .await;
    let client = client_for(&server.base_url());
    let request = client.initiate_quick_connect().await.expect("initiate");

    assert_eq!(
        client
            .quick_connect_status(request.secret())
            .await
            .expect("first poll"),
        QuickConnectStatus::Pending
    );
    let error = client
        .authenticate_with_quick_connect(request.secret())
        .await
        .expect_err("not yet approved");
    assert!(
        matches!(error, ApiError::ReauthenticationRequired),
        "{error:?}"
    );
    server.shutdown().await;
}
