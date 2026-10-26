//! SyncPlay fixture routes over real loopback HTTP. The mock returns canned responses and is
//! not a real Jellyfin server, so these tests check the mock's shape, not server behavior.

use mock_jellyfin::{MockConfig, RunningServer, spawn};
use reqwest::{Client, Method, StatusCode, header};
use serde_json::{Value, json};

const TOKEN: &str = "mock-access-token-0001";
const GROUP: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";
const OTHER_GROUP: &str = "00000000-0000-0000-0000-000000000001";
const ITEM_A: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3302";
const ITEM_B: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3303";

async fn start() -> RunningServer {
    spawn(MockConfig::default())
        .await
        .expect("spawn mock server")
}

fn auth_header() -> String {
    format!(
        "MediaBrowser Client=\"tests\", Device=\"ci\", DeviceId=\"ci-1\", Version=\"0\", Token=\"{TOKEN}\""
    )
}

async fn call(
    server: &RunningServer,
    method: Method,
    path: &str,
    body: Option<&str>,
    authorized: bool,
) -> reqwest::Response {
    let mut request = Client::new().request(method, format!("{}{path}", server.base_url()));
    if authorized {
        request = request.header(header::AUTHORIZATION, auth_header());
    }
    if let Some(text) = body {
        request = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(text.to_owned());
    }
    request.send().await.expect("request")
}

async fn status_of(
    server: &RunningServer,
    method: Method,
    path: &str,
    body: Option<&str>,
) -> StatusCode {
    call(server, method, path, body, true).await.status()
}

#[tokio::test]
async fn list_requires_a_token_and_returns_the_fixture_group() {
    let server = start().await;

    let refused = call(&server, Method::GET, "/SyncPlay/List", None, false).await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

    let response = call(&server, Method::GET, "/SyncPlay/List", None, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let groups: Value = response.json().await.expect("json array");
    let groups = groups.as_array().expect("array of groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["GroupId"], GROUP);
    assert_eq!(groups[0]["GroupName"], "Movie night");
    assert_eq!(groups[0]["State"], "Idle");
    assert_eq!(groups[0]["Participants"], json!(["demo"]));
    server.shutdown().await;
}

#[tokio::test]
async fn new_group_echoes_the_requested_name() {
    let server = start().await;

    let response = call(
        &server,
        Method::POST,
        "/SyncPlay/New",
        Some(r#"{"GroupName":"Friday"}"#),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let group: Value = response.json().await.expect("json object");
    assert_eq!(group["GroupName"], "Friday");
    assert_eq!(group["GroupId"], GROUP);

    assert_eq!(
        status_of(&server, Method::POST, "/SyncPlay/New", Some("not json")).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status_of(
            &server,
            Method::POST,
            "/SyncPlay/New",
            Some(r#"{"Name":"x"}"#)
        )
        .await,
        StatusCode::BAD_REQUEST,
        "GroupName is required"
    );
    let refused = call(
        &server,
        Method::POST,
        "/SyncPlay/New",
        Some(r#"{"GroupName":"Friday"}"#),
        false,
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    server.shutdown().await;
}

#[tokio::test]
async fn join_accepts_only_the_fixture_group() {
    let server = start().await;

    let joined = call(
        &server,
        Method::POST,
        "/SyncPlay/Join",
        Some(json!({ "GroupId": GROUP }).to_string().as_str()),
        true,
    )
    .await;
    assert_eq!(joined.status(), StatusCode::NO_CONTENT);

    assert_eq!(
        status_of(
            &server,
            Method::POST,
            "/SyncPlay/Join",
            Some(json!({ "GroupId": OTHER_GROUP }).to_string().as_str()),
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(&server, Method::POST, "/SyncPlay/Join", Some("{}")).await,
        StatusCode::BAD_REQUEST
    );
    server.shutdown().await;
}

#[tokio::test]
async fn leave_and_playback_messages_return_no_content() {
    let server = start().await;

    for path in [
        "/SyncPlay/Leave",
        "/SyncPlay/Pause",
        "/SyncPlay/Unpause",
        "/SyncPlay/Stop",
    ] {
        let response = call(&server, Method::POST, path, None, true).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "POST {path}");
    }
    let refused = call(&server, Method::POST, "/SyncPlay/Pause", None, false).await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    server.shutdown().await;
}

#[tokio::test]
async fn set_new_queue_requires_a_nonempty_guid_queue_and_valid_position() {
    let server = start().await;
    let body = json!({
        "PlayingQueue": [ITEM_A, ITEM_B],
        "PlayingItemPosition": 1,
        "StartPositionTicks": 1234567,
    })
    .to_string();
    assert_eq!(
        status_of(&server, Method::POST, "/SyncPlay/SetNewQueue", Some(&body)).await,
        StatusCode::NO_CONTENT
    );

    for body in [
        json!({"PlayingQueue": [], "PlayingItemPosition": 0, "StartPositionTicks": 0}),
        json!({"PlayingQueue": ["not-a-guid"], "PlayingItemPosition": 0, "StartPositionTicks": 0}),
        json!({"PlayingQueue": [ITEM_A], "PlayingItemPosition": 1, "StartPositionTicks": 0}),
        json!({"PlayingQueue": [ITEM_A], "PlayingItemPosition": 0, "StartPositionTicks": -1}),
    ] {
        assert_eq!(
            status_of(
                &server,
                Method::POST,
                "/SyncPlay/SetNewQueue",
                Some(body.to_string().as_str()),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
    let refused = call(
        &server,
        Method::POST,
        "/SyncPlay/SetNewQueue",
        Some(&body),
        false,
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    server.shutdown().await;
}

#[tokio::test]
async fn ready_and_buffering_require_the_playback_state_dto() {
    let server = start().await;
    let body = json!({
        "When": "2026-10-24T20:00:00.1250000Z",
        "PositionTicks": 1234567,
        "IsPlaying": true,
        "PlaylistItemId": ITEM_A,
    })
    .to_string();
    for path in ["/SyncPlay/Ready", "/SyncPlay/Buffering"] {
        assert_eq!(
            status_of(&server, Method::POST, path, Some(&body)).await,
            StatusCode::NO_CONTENT,
            "POST {path}"
        );
        assert_eq!(
            status_of(&server, Method::POST, path, Some(r#"{"When":"x"}"#)).await,
            StatusCode::BAD_REQUEST,
            "POST {path} rejects incomplete status"
        );
    }
    let refused = call(&server, Method::POST, "/SyncPlay/Ready", Some(&body), false).await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    server.shutdown().await;
}

#[tokio::test]
async fn seek_needs_a_non_negative_integer_position() {
    let server = start().await;

    assert_eq!(
        status_of(
            &server,
            Method::POST,
            "/SyncPlay/Seek",
            Some(r#"{"PositionTicks":1234500000}"#),
        )
        .await,
        StatusCode::NO_CONTENT
    );
    for body in [
        r#"{"PositionTicks":-1}"#,
        r#"{"PositionTicks":"10"}"#,
        "{}",
        "[]",
    ] {
        assert_eq!(
            status_of(&server, Method::POST, "/SyncPlay/Seek", Some(body)).await,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn ping_needs_an_integer_round_trip_value() {
    let server = start().await;

    assert_eq!(
        status_of(
            &server,
            Method::POST,
            "/SyncPlay/Ping",
            Some(r#"{"Ping":42}"#)
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        status_of(
            &server,
            Method::POST,
            "/SyncPlay/Ping",
            Some(r#"{"Ping":"42"}"#)
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    server.shutdown().await;
}

#[tokio::test]
async fn syncplay_calls_are_logged_without_the_token() {
    let server = start().await;
    let _ = call(&server, Method::GET, "/SyncPlay/List", None, true).await;
    let _ = call(
        &server,
        Method::POST,
        "/SyncPlay/Ping",
        Some(r#"{"Ping":1}"#),
        true,
    )
    .await;

    let log = call(&server, Method::GET, "/__mock/requests", None, false).await;
    let log_text = log.text().await.expect("log body");
    assert!(log_text.contains("/SyncPlay/List"));
    assert!(log_text.contains("/SyncPlay/Ping"));
    assert!(
        !log_text.contains(TOKEN),
        "token must not reach the request log"
    );
    server.shutdown().await;
}
