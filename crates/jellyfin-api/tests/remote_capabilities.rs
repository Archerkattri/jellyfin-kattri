//! Capability advertisement through the typed client against the in-process mock.
//! This validates the client against the mock only, not against a real server.

use jellyfin_api::capabilities::{ClientCapabilities, GeneralCommandType, PlayableMediaType};
use jellyfin_api::{ClientConfig, JellyfinClient, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn};
use serde_json::{Value, json};

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

async fn recorded_capability_bodies(server: &RunningServer) -> Vec<Value> {
    reqwest::get(format!("{}/__mock/capabilities", server.base_url()))
        .await
        .expect("capability log completes")
        .json()
        .await
        .expect("capability log is JSON")
}

#[tokio::test]
async fn advertised_body_reaches_the_server_with_media_control_and_types() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let capabilities = ClientCapabilities::remote_controllable(
        vec![PlayableMediaType::Video, PlayableMediaType::Audio],
        vec![GeneralCommandType::SetVolume],
    );

    client
        .advertise_capabilities(&session, &capabilities)
        .await
        .expect("server accepts the capabilities");

    let bodies = recorded_capability_bodies(&server).await;
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert_eq!(
        bodies[0],
        json!({
            "PlayableMediaTypes": ["Video", "Audio"],
            "SupportedCommands": ["SetVolume"],
            "SupportsMediaControl": true,
            "SupportsPersistentIdentifier": false
        })
    );
    server.shutdown().await;
}

#[tokio::test]
async fn advertisement_uses_the_full_capabilities_route() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    client
        .advertise_capabilities(
            &session,
            &ClientCapabilities::remote_controllable(Vec::new(), Vec::new()),
        )
        .await
        .expect("empty capabilities are still valid");

    let value: Value = reqwest::get(format!("{}/__mock/requests", server.base_url()))
        .await
        .expect("request log completes")
        .json()
        .await
        .expect("request log is JSON");
    let logged = value
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|entry| entry["Path"].as_str())
        .any(|path| path == "/Sessions/Capabilities/Full");
    assert!(logged, "{value}");
    server.shutdown().await;
}
