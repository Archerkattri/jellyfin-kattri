//! Outbound SyncPlay playback messages (pause, unpause, seek) through the typed client,
//! against `mock-jellyfin`. The mock is not a real Jellyfin server, so these tests show
//! the request shape the client sends, not compatibility with a live server.

use jellyfin_api::{ApiError, ClientConfig, JellyfinClient, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn};
use serde_json::Value;

const SEEK_PATH: &str = "/SyncPlay/Seek";

fn client(base_url: &str) -> JellyfinClient {
    let config = ClientConfig::new(
        base_url,
        "Kattri test",
        "test device",
        "device-id",
        "test-version",
    )
    .expect("valid client configuration");
    JellyfinClient::new(config).expect("configured Jellyfin client")
}

async fn signed_in(server: &RunningServer) -> (JellyfinClient, Session) {
    let client = client(&server.base_url());
    let session = client.authenticate("demo", "demo").await.expect("sign in");
    (client, session)
}

/// Method and path pairs the mock recorded, in arrival order.
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
                entry["Method"].as_str().unwrap_or_default().to_owned(),
                entry["Path"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn syncplay_posts(requests: &[(String, String)]) -> Vec<&str> {
    requests
        .iter()
        .filter(|(method, path)| method == "POST" && path.starts_with("/SyncPlay/"))
        .map(|(_, path)| path.as_str())
        .collect()
}

#[tokio::test]
async fn pause_unpause_and_seek_reach_the_group_routes() {
    let server = spawn(MockConfig::default()).await.expect("mock server");
    let (client, session) = signed_in(&server).await;

    client
        .sync_play_pause(&session)
        .await
        .expect("pause accepted");
    client
        .sync_play_seek(&session, 1_234_500_000)
        .await
        .expect("seek accepted");
    client
        .sync_play_unpause(&session)
        .await
        .expect("unpause accepted");

    // The mock answers 400 to a seek body it cannot read and 404 to an unknown route,
    // so a successful result also shows the body shape and the route were right.
    let requests = recorded_requests(&server).await;
    assert_eq!(
        syncplay_posts(&requests),
        vec!["/SyncPlay/Pause", SEEK_PATH, "/SyncPlay/Unpause"]
    );
    server.shutdown().await;
}

#[tokio::test]
async fn seek_at_int64_maximum_is_sent_and_accepted() {
    let server = spawn(MockConfig::default()).await.expect("mock server");
    let (client, session) = signed_in(&server).await;

    client
        .sync_play_seek(&session, i64::MAX as u64)
        .await
        .expect("largest valid tick count is accepted");
    let requests = recorded_requests(&server).await;
    assert_eq!(syncplay_posts(&requests), vec![SEEK_PATH]);
    server.shutdown().await;
}

#[tokio::test]
async fn seek_above_int64_maximum_is_refused_before_any_request() {
    let server = spawn(MockConfig::default()).await.expect("mock server");
    let (client, session) = signed_in(&server).await;

    let result = client.sync_play_seek(&session, i64::MAX as u64 + 1).await;
    assert!(matches!(result, Err(ApiError::InvalidResponse)));

    let requests = recorded_requests(&server).await;
    assert!(
        syncplay_posts(&requests).is_empty(),
        "no SyncPlay request may be sent for an invalid seek"
    );
    server.shutdown().await;
}
