//! SyncPlay group calls through the typed client, against `mock-jellyfin`.
//! The mock is not a real Jellyfin server, so passing here does not confirm
//! compatibility with one.
//!
//! These tests cover input refusal: values that are not valid GUIDs or names
//! are rejected by the client before any request is sent.

use jellyfin_api::{ApiError, ClientConfig, JellyfinClient, MAX_GROUP_NAME_LENGTH, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn};

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

#[tokio::test]
async fn join_refuses_invalid_group_id_before_any_request() {
    let server = spawn(MockConfig::default()).await.expect("mock server");
    let (client, session) = signed_in(&server).await;

    // The mock answers unknown routes with 404. A refusal of InvalidResponse
    // therefore shows that no request reached the server.
    for value in ["", "not-a-guid", "3f2504e0-4f89-11d3-9a0c-0305e82c33zz"] {
        let result = client.join_sync_play_group(&session, value).await;
        assert!(
            matches!(result, Err(ApiError::InvalidResponse)),
            "{value:?} must be refused locally"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn create_refuses_overlong_group_name_before_any_request() {
    let server = spawn(MockConfig::default()).await.expect("mock server");
    let (client, session) = signed_in(&server).await;

    let overlong = "a".repeat(MAX_GROUP_NAME_LENGTH + 1);
    let result = client.create_sync_play_group(&session, &overlong).await;
    assert!(matches!(result, Err(ApiError::InvalidResponse)));
    server.shutdown().await;
}
