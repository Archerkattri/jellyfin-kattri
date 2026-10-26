//! Resuming a session from a saved token, against the in-process mock over loopback. This
//! checks the request shape and the 401 handling against the mock only, not a real server.

use jellyfin_api::{ApiError, ClientConfig, ItemQuery, JellyfinClient};
use mock_jellyfin::{MockConfig, spawn};

/// The mock's fixed access token (crate-private in mock-jellyfin).
const MOCK_TOKEN: &str = "mock-access-token-0001";

fn client_for(base_url: &str) -> JellyfinClient {
    let config = ClientConfig::new(base_url, "Kattri test", "device", "device-id", "0.0.0")
        .expect("valid config");
    JellyfinClient::new(config).expect("loopback HTTP is allowed")
}

#[tokio::test]
async fn a_saved_token_resumes_the_same_user_and_can_browse() {
    let server = spawn(MockConfig::default()).await.expect("mock");
    let client = client_for(&server.base_url());

    let signed_in = client.authenticate("demo", "demo").await.expect("sign in");
    let saved = signed_in.token_for_credential_store();
    assert_eq!(saved.as_str(), MOCK_TOKEN);

    let resumed = client.resume_session(&saved).await.expect("resume");
    assert_eq!(resumed.user, signed_in.user);
    assert_eq!(resumed.server_id, signed_in.server_id);
    let page = client
        .list_items(&resumed, &ItemQuery::default())
        .await
        .expect("a resumed session browses");
    assert!(!page.items.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn a_revoked_or_malformed_token_asks_for_a_new_sign_in() {
    let server = spawn(MockConfig::default()).await.expect("mock");
    let client = client_for(&server.base_url());

    for token in ["revoked-token", "", "bad\ntoken"] {
        let result = client.resume_session(token).await;
        assert!(
            matches!(result, Err(ApiError::ReauthenticationRequired)),
            "{token:?}: {result:?}"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn a_session_debug_never_shows_the_exported_token() {
    let server = spawn(MockConfig::default()).await.expect("mock");
    let client = client_for(&server.base_url());
    let session = client.resume_session(MOCK_TOKEN).await.expect("resume");
    assert!(!format!("{session:?}").contains(MOCK_TOKEN));
    server.shutdown().await;
}
