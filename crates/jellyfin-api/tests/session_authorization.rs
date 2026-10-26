//! The session's `Authorization` value for the download layer, checked against the
//! in-process mock over loopback HTTP. This validates the header format against the mock
//! only, not against a real Jellyfin server.

use jellyfin_api::{ClientConfig, JellyfinClient, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn};

/// The mock's fixed access token (`seed::MOCK_ACCESS_TOKEN`, crate-private in mock-jellyfin).
const MOCK_TOKEN: &str = "mock-access-token-0001";

async fn start() -> RunningServer {
    spawn(MockConfig::default())
        .await
        .expect("spawn mock server")
}

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

async fn signed_in(server: &RunningServer) -> (JellyfinClient, Session) {
    let client = client_for(&server.base_url());
    let session = client
        .authenticate("demo", "demo")
        .await
        .expect("demo sign-in");
    (client, session)
}

#[tokio::test]
async fn authorization_value_carries_the_session_token_with_client_metadata() {
    let server = start().await;
    let (client, session) = signed_in(&server).await;

    let authorization = client.session_authorization(&session);
    let value = authorization.header_value();

    assert!(value.starts_with("MediaBrowser "), "{value}");
    assert!(
        value.contains(&format!("Token=\"{MOCK_TOKEN}\"")),
        "{value}"
    );
    assert!(value.contains("DeviceId=\"device-id\""), "{value}");
    // Spaces in metadata are percent-encoded, as the client does for every metadata field.
    assert!(value.contains("Client=\"Kattri%20test\""), "{value}");
}

#[tokio::test]
async fn debug_output_of_the_authorization_value_hides_the_token() {
    let server = start().await;
    let (client, session) = signed_in(&server).await;

    let authorization = client.session_authorization(&session);
    let debug = format!("{authorization:?}");

    assert_eq!(debug, "SessionAuthorization([REDACTED])");
    assert!(!debug.contains(MOCK_TOKEN), "{debug}");
    assert!(!debug.contains("MediaBrowser"), "{debug}");
}
