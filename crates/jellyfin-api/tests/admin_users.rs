use std::net::SocketAddr;

use jellyfin_api::{
    AdminUserPolicy, AdminUserPolicyChanges, ApiError, ClientConfig, JellyfinClient, Session,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use zeroize::Zeroizing;

const USER_ID: &str = "0000000000000000000000000000a002";

struct FixtureResponse {
    status: u16,
    body: Vec<u8>,
}

fn json_response(status: u16, body: &str) -> FixtureResponse {
    FixtureResponse {
        status,
        body: body.as_bytes().to_vec(),
    }
}

fn empty_response(status: u16) -> FixtureResponse {
    FixtureResponse {
        status,
        body: Vec::new(),
    }
}

/// Every listener and target in this file is loopback-only; no Jellyfin server is contacted.
async fn serve(responses: Vec<FixtureResponse>) -> (String, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address: SocketAddr = listener.local_addr().expect("loopback address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept loopback request");
            requests.push(read_request(&mut socket).await);
            let status_text = match response.status {
                200 => "OK",
                204 => "No Content",
                400 => "Bad Request",
                401 => "Unauthorized",
                403 => "Forbidden",
                404 => "Not Found",
                _ => "Fixture Response",
            };
            let headers = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.status,
                status_text,
                response.body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write response headers");
            socket
                .write_all(&response.body)
                .await
                .expect("write response body");
            socket.shutdown().await.expect("close fixture connection");
        }
        requests
    });
    (format!("http://{address}"), task)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = socket.read(&mut buffer).await.expect("read request");
        assert_ne!(count, 0, "client closed before sending a complete request");
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 64 * 1024, "fixture request exceeded cap");

        let Some(headers_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..headers_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or(0);
        if bytes.len() >= headers_end + 4 + content_length {
            return bytes;
        }
    }
}

fn client(base_url: &str) -> JellyfinClient {
    let config = ClientConfig::new(base_url, "Kattri test", "test device", "device-id", "test")
        .expect("loopback client config");
    JellyfinClient::new(config).expect("client")
}

async fn session() -> Session {
    let (base, task) = serve(vec![json_response(200,
        r#"{"AccessToken":"loopback-test-token","User":{"Id":"0000000000000000000000000000a001","Name":"admin"}}"#,
    )]).await;
    let session = client(&base)
        .authenticate("admin", "fixture-password")
        .await
        .expect("authenticate against loopback fixture");
    task.await.expect("authentication fixture exits");
    session
}

#[tokio::test]
async fn signed_in_quick_connect_authorizes_only_a_valid_code() {
    let session = session().await;
    let (base, fixture) = serve(vec![
        json_response(200, "true"),
        json_response(200, "false"),
    ])
    .await;
    let client = client(&base);
    for bad in ["12345", "1234567", "12&345", "１２３４５６"] {
        assert!(matches!(
            client.authorize_quick_connect(&session, bad).await,
            Err(ApiError::InvalidResponse)
        ));
    }
    assert!(
        client
            .authorize_quick_connect(&session, " 012345 ")
            .await
            .expect("authorized")
    );
    assert!(
        !client
            .authorize_quick_connect(&session, "654321")
            .await
            .expect("not authorized")
    );
    let requests = fixture.await.expect("fixture exits");
    for (request, code) in requests.iter().zip(["012345", "654321"]) {
        assert_eq!(
            request_line(request),
            format!("POST /QuickConnect/Authorize?code={code} HTTP/1.1")
        );
        assert!(
            request_header(request, "Authorization")
                .is_some_and(|value| value.contains("loopback-test-token"))
        );
    }
}

#[tokio::test]
async fn quick_connect_authorization_preserves_failure_classes() {
    let session = session().await;
    let (base, fixture) = serve(vec![empty_response(401), empty_response(503)]).await;
    let client = client(&base);
    for expected in [
        jellyfin_api::ErrorClass::AuthExpired,
        jellyfin_api::ErrorClass::ServerUnavailable,
    ] {
        let error = client
            .authorize_quick_connect(&session, "012345")
            .await
            .expect_err("authorization request failed");
        assert_eq!(error.classify(), expected);
    }
    assert_eq!(fixture.await.expect("fixture exits").len(), 2);
}

#[tokio::test]
async fn account_preferences_are_read_only_and_bound_to_the_current_account() {
    let session = session().await;
    let (base, fixture) = serve(vec![
        json_response(200, r#"{"Id":"0000000000000000000000000000a001","Configuration":{"AudioLanguagePreference":"jpn","SubtitleMode":"OnlyForced","HidePlayedInLatest":true,"FutureField":1}}"#),
        json_response(200, r#"{"Id":"another-account","Configuration":{}}"#),
    ]).await;
    let client = client(&base);
    let prefs = client
        .account_preferences(&session)
        .await
        .expect("load preferences")
        .expect("server configuration");
    assert_eq!(prefs.audio_language_preference.as_deref(), Some("jpn"));
    assert_eq!(prefs.subtitle_mode.as_deref(), Some("OnlyForced"));
    assert_eq!(prefs.hide_played_in_latest, Some(true));
    assert_eq!(prefs.display_missing_episodes, None);
    assert!(matches!(
        client.account_preferences(&session).await,
        Err(ApiError::InvalidResponse)
    ));
    for request in fixture.await.expect("fixture exits") {
        assert_eq!(request_line(&request), "GET /Users/Me HTTP/1.1");
    }
}

#[tokio::test]
async fn updating_latest_preference_preserves_unrelated_fields() {
    let session = session().await;
    let (base, fixture) = serve(vec![
        json_response(200, r#"{"Id":"0000000000000000000000000000a001","Configuration":{"HidePlayedInLatest":false,"FutureField":{"Keep":true}}}"#),
        empty_response(204),
    ]).await;
    client(&base)
        .set_account_preferences(
            &session,
            &jellyfin_api::AccountPreferences {
                hide_played_in_latest: Some(true),
                ..jellyfin_api::AccountPreferences::default()
            },
        )
        .await
        .expect("fixture update succeeds");
    let requests = fixture.await.expect("fixture exits");
    assert_eq!(request_line(&requests[0]), "GET /Users/Me HTTP/1.1");
    assert_eq!(
        request_line(&requests[1]),
        "POST /Users/Configuration?userId=0000000000000000000000000000a001 HTTP/1.1"
    );
    assert_eq!(
        request_json(&requests[1]),
        json!({"HidePlayedInLatest":true,"FutureField":{"Keep":true}})
    );
}

#[tokio::test]
async fn unchanged_latest_preference_does_not_submit_a_write() {
    let session = session().await;
    let (base, fixture) = serve(vec![json_response(
        200,
        r#"{"Id":"0000000000000000000000000000a001","Configuration":{"HidePlayedInLatest":true}}"#,
    )])
    .await;
    client(&base)
        .set_account_preferences(
            &session,
            &jellyfin_api::AccountPreferences {
                hide_played_in_latest: Some(true),
                ..jellyfin_api::AccountPreferences::default()
            },
        )
        .await
        .expect("unchanged preference");
    let requests = fixture.await.expect("fixture exits");
    assert_eq!(requests.len(), 1);
    assert_eq!(request_line(&requests[0]), "GET /Users/Me HTTP/1.1");
}

fn request_line(request: &[u8]) -> &str {
    std::str::from_utf8(request)
        .expect("request bytes")
        .lines()
        .next()
        .expect("request line")
}

fn request_header<'a>(request: &'a [u8], name: &str) -> Option<&'a str> {
    std::str::from_utf8(request)
        .expect("request bytes")
        .split("\r\n\r\n")
        .next()?
        .lines()
        .skip(1)
        .find_map(|line| {
            let (header_name, value) = line.split_once(':')?;
            header_name.eq_ignore_ascii_case(name).then(|| value.trim())
        })
}

fn request_json(request: &[u8]) -> Value {
    let body = request
        .split_once_bytes(b"\r\n\r\n")
        .expect("request body separator")
        .1;
    serde_json::from_slice(body).expect("request JSON")
}

trait SplitOnceBytes {
    fn split_once_bytes(&self, needle: &[u8]) -> Option<(&[u8], &[u8])>;
}

impl SplitOnceBytes for [u8] {
    fn split_once_bytes(&self, needle: &[u8]) -> Option<(&[u8], &[u8])> {
        let index = self
            .windows(needle.len())
            .position(|window| window == needle)?;
        Some((&self[..index], &self[index + needle.len()..]))
    }
}

#[tokio::test]
async fn lists_typed_users_without_retaining_unmodeled_secret_fields() {
    let (base, task) = serve(vec![json_response(
        200,
        &format!(
            r#"[{{"Id":"{USER_ID}","Name":"member","Password":"fixture-only-secret","Policy":{{"IsAdministrator":false,"IsDisabled":true,"SyncPlayAccess":"CreateAndJoinGroups"}}}}]"#
        ),
    )])
    .await;

    let users = client(&base)
        .list_admin_users(&session().await)
        .await
        .expect("user list");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].id, USER_ID);
    assert_eq!(users[0].name, "member");
    assert_eq!(
        users[0].policy.as_ref().and_then(|p| p.is_disabled),
        Some(true)
    );
    assert!(!format!("{:?}", users[0]).contains("fixture-only-secret"));

    let requests = task.await.expect("fixture server");
    assert_eq!(request_line(&requests[0]), "GET /Users HTTP/1.1");
    assert!(
        request_header(&requests[0], "authorization")
            .expect("authorization")
            .contains("loopback-test-token")
    );
}

#[tokio::test]
async fn creates_user_with_typed_name_and_zeroizing_optional_password() {
    let (base, task) = serve(vec![json_response(
        200,
        &format!(r#"{{"Id":"{USER_ID}","Name":"new member"}}"#),
    )])
    .await;

    let created = client(&base)
        .create_user(
            &session().await,
            "new member",
            Some(Zeroizing::new("fixture-password-only".to_owned())),
        )
        .await
        .expect("created user");
    assert_eq!(created.id, USER_ID);
    assert_eq!(created.name, "new member");

    let requests = task.await.expect("fixture server");
    assert!(request_line(&requests[0]).starts_with("POST /Users/New "));
    let body = request_json(&requests[0]);
    assert_eq!(body["Name"], "new member");
    assert_eq!(body["Password"], "fixture-password-only");
}

#[tokio::test]
async fn renames_user_while_preserving_server_user_configuration() {
    let current = format!(
        r#"{{"Id":"{USER_ID}","Name":"before","Configuration":{{"AudioLanguagePreference":"en","RememberAudioSelections":true}},"Policy":{{"IsAdministrator":false}}}}"#
    );
    let (base, task) = serve(vec![json_response(200, &current), empty_response(204)]).await;

    client(&base)
        .update_user_name(&session().await, USER_ID, "after")
        .await
        .expect("renamed user");

    let requests = task.await.expect("fixture server");
    assert_eq!(
        request_line(&requests[0]),
        format!("GET /Users/{USER_ID} HTTP/1.1")
    );
    assert_eq!(
        request_line(&requests[1]),
        format!("POST /Users?userId={USER_ID} HTTP/1.1")
    );
    let body = request_json(&requests[1]);
    assert_eq!(body["Name"], "after");
    assert_eq!(body["Configuration"]["AudioLanguagePreference"], "en");
    assert_eq!(body["Configuration"]["RememberAudioSelections"], true);
}

#[tokio::test]
async fn policy_update_changes_selected_flags_and_round_trips_unmodeled_fields() {
    let current = format!(
        r#"{{"Id":"{USER_ID}","Name":"member","Policy":{{"IsAdministrator":false,"IsHidden":false,"IsDisabled":false,"EnableMediaPlayback":true,"EnableRemoteAccess":true,"EnabledFolders":["0000000000000000000000000000f001"],"AuthenticationProviderId":"provider-id","PasswordResetProviderId":"reset-provider-id","SyncPlayAccess":"CreateAndJoinGroups"}}}}"#
    );
    let (base, task) = serve(vec![json_response(200, &current), empty_response(204)]).await;

    client(&base)
        .update_user_policy(
            &session().await,
            USER_ID,
            &AdminUserPolicyChanges {
                is_disabled: Some(true),
                enable_remote_access: Some(false),
                ..AdminUserPolicyChanges::default()
            },
        )
        .await
        .expect("policy updated");

    let requests = task.await.expect("fixture server");
    assert_eq!(
        request_line(&requests[0]),
        format!("GET /Users/{USER_ID} HTTP/1.1")
    );
    assert_eq!(
        request_line(&requests[1]),
        format!("POST /Users/{USER_ID}/Policy HTTP/1.1")
    );
    let body = request_json(&requests[1]);
    assert_eq!(body["IsDisabled"], true);
    assert_eq!(body["EnableRemoteAccess"], false);
    assert_eq!(body["EnableMediaPlayback"], true);
    assert_eq!(
        body["EnabledFolders"][0],
        "0000000000000000000000000000f001"
    );
    assert_eq!(body["AuthenticationProviderId"], "provider-id");
    assert_eq!(body["PasswordResetProviderId"], "reset-provider-id");
    assert_eq!(body["SyncPlayAccess"], "CreateAndJoinGroups");
}

#[tokio::test]
async fn delete_targets_only_the_explicit_user_and_redacts_server_error_text() {
    let (base, task) = serve(vec![empty_response(204)]).await;
    client(&base)
        .delete_user(&session().await, USER_ID)
        .await
        .expect("deleted user");
    let requests = task.await.expect("fixture server");
    assert_eq!(
        request_line(&requests[0]),
        format!("DELETE /Users/{USER_ID} HTTP/1.1")
    );

    let (base, _task) = serve(vec![json_response(403, "server-secret-message")]).await;
    let error = client(&base)
        .delete_user(&session().await, USER_ID)
        .await
        .expect_err("server denied delete");
    assert!(matches!(error, ApiError::Forbidden));
    assert!(!error.to_string().contains("server-secret-message"));
    assert!(!format!("{error:?}").contains("server-secret-message"));
}

#[tokio::test]
async fn malformed_target_id_is_rejected_before_any_request() {
    let client = client("http://127.0.0.1:9");
    assert!(matches!(
        client.delete_user(&session().await, "not-a-user-id").await,
        Err(ApiError::InvalidConfiguration)
    ));
    assert!(matches!(
        client
            .update_user_policy(
                &session().await,
                "../other-user",
                &AdminUserPolicyChanges::default()
            )
            .await,
        Err(ApiError::InvalidConfiguration)
    ));
}

#[test]
fn policy_debug_redacts_opaque_server_fields_but_serialization_preserves_them() {
    let policy: AdminUserPolicy = serde_json::from_value(json!({
        "IsAdministrator": false,
        "AuthenticationProviderId": "provider-id",
        "PasswordResetProviderId": "reset-provider-id",
        "SyncPlayAccess": "CreateAndJoinGroups",
        "OpaqueSecretSentinel": "fixture-only-secret"
    }))
    .expect("policy DTO");
    assert!(!format!("{policy:?}").contains("fixture-only-secret"));
    let round_trip = serde_json::to_value(policy).expect("policy serialization");
    assert_eq!(round_trip["SyncPlayAccess"], "CreateAndJoinGroups");
    assert_eq!(round_trip["OpaqueSecretSentinel"], "fixture-only-secret");
}
