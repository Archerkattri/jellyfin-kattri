//! Loopback checks for the mock remote-command harness (`/__mock/remote/*`). The harness is a
//! test hook. It is not the Jellyfin WebSocket transport, so these tests check only the
//! queue order, the envelope check, and the token check.

use mock_jellyfin::{MockConfig, RunningServer, spawn};
use reqwest::{Client, StatusCode, header::AUTHORIZATION};
use serde_json::{Value, json};

const TOKEN: &str = "mock-access-token-0001";

async fn start() -> RunningServer {
    spawn(MockConfig::default()).await.expect("spawn mock")
}

fn auth_value() -> String {
    format!(
        "MediaBrowser Client=\"tests\", Device=\"ci\", DeviceId=\"ci-1\", Version=\"0\", Token=\"{TOKEN}\""
    )
}

#[tokio::test]
async fn queued_commands_come_back_in_the_order_they_were_posted() {
    let server = start().await;
    let base = server.base_url();
    let http = Client::new();
    let pause = json!({"MessageType": "Playstate", "Data": {"Command": "Pause"}});
    let volume = json!({"MessageType": "GeneralCommand", "Data": {"Name": "SetVolume", "Arguments": {"Volume": "20"}}});

    for envelope in [&pause, &volume] {
        let posted = http
            .post(format!("{base}/__mock/remote/commands"))
            .json(envelope)
            .send()
            .await
            .expect("post");
        assert_eq!(posted.status(), StatusCode::NO_CONTENT);
    }

    for expected in [&pause, &volume] {
        let received = http
            .get(format!("{base}/__mock/remote/next"))
            .header(AUTHORIZATION, auth_value())
            .send()
            .await
            .expect("poll");
        assert_eq!(received.status(), StatusCode::OK);
        let body: Value = received.json().await.expect("json");
        assert_eq!(&body, expected);
    }

    let empty = http
        .get(format!("{base}/__mock/remote/next"))
        .header(AUTHORIZATION, auth_value())
        .send()
        .await
        .expect("poll empty");
    assert_eq!(empty.status(), StatusCode::NO_CONTENT);
    server.shutdown().await;
}

#[tokio::test]
async fn poll_without_the_session_token_is_refused_and_keeps_the_message() {
    let server = start().await;
    let base = server.base_url();
    let http = Client::new();
    let envelope = json!({"MessageType": "Playstate", "Data": {"Command": "Stop"}});
    http.post(format!("{base}/__mock/remote/commands"))
        .json(&envelope)
        .send()
        .await
        .expect("post");

    let refused = http
        .get(format!("{base}/__mock/remote/next"))
        .send()
        .await
        .expect("poll without token");
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

    let kept = http
        .get(format!("{base}/__mock/remote/next"))
        .header(AUTHORIZATION, auth_value())
        .send()
        .await
        .expect("poll with token");
    assert_eq!(kept.status(), StatusCode::OK);
    server.shutdown().await;
}

#[tokio::test]
async fn envelopes_with_the_wrong_shape_are_rejected_at_the_sender() {
    let server = start().await;
    let base = server.base_url();
    let http = Client::new();
    let bad_bodies = [
        "not json".to_owned(),
        json!({"MessageType": "Sessions", "Data": {}}).to_string(),
        json!({"MessageType": "Playstate"}).to_string(),
        json!({"MessageType": "Playstate", "Data": "Pause"}).to_string(),
        json!({"Data": {"Command": "Pause"}}).to_string(),
    ];
    for body in bad_bodies {
        let response = http
            .post(format!("{base}/__mock/remote/commands"))
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await
            .expect("post");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }

    let empty = http
        .get(format!("{base}/__mock/remote/next"))
        .header(AUTHORIZATION, auth_value())
        .send()
        .await
        .expect("poll");
    assert_eq!(
        empty.status(),
        StatusCode::NO_CONTENT,
        "bad envelopes must not be queued"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn each_server_has_its_own_queue() {
    let first = start().await;
    let second = start().await;
    let http = Client::new();
    let envelope = json!({"MessageType": "Playstate", "Data": {"Command": "Pause"}});
    http.post(format!("{}/__mock/remote/commands", first.base_url()))
        .json(&envelope)
        .send()
        .await
        .expect("post");

    let other = http
        .get(format!("{}/__mock/remote/next", second.base_url()))
        .header(AUTHORIZATION, auth_value())
        .send()
        .await
        .expect("poll other server");
    assert_eq!(other.status(), StatusCode::NO_CONTENT);
    first.shutdown().await;
    second.shutdown().await;
}
