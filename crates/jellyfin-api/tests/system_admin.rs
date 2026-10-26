//! Read-only admin calls (M4-22) against `mock-jellyfin`, plus the 403 path
//! against a loopback stub. The mock is not a real Jellyfin server, so these
//! passes do not confirm compatibility with one.

use jellyfin_api::{ApiError, ClientConfig, JellyfinClient, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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

/// Sanitized request-log entries as `(method, path)` pairs.
async fn request_log(server: &RunningServer) -> Vec<(String, String)> {
    reqwest::Client::new()
        .get(format!("{}/__mock/requests", server.base_url()))
        .send()
        .await
        .expect("read request log")
        .error_for_status()
        .expect("request log status")
        .json::<Value>()
        .await
        .expect("request log JSON")
        .as_array()
        .expect("request entries")
        .iter()
        .filter_map(|entry| {
            Some((
                entry["Method"].as_str()?.to_owned(),
                entry["Path"].as_str()?.to_owned(),
            ))
        })
        .collect()
}

#[tokio::test]
async fn each_admin_read_returns_its_mock_fixture_with_get() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;

    let info = client.server_info(&session).await.expect("server info");
    assert_eq!(info.server_name.as_deref(), Some("Mock Jellyfin"));
    assert_eq!(info.version.as_deref(), Some("0.0.0-mock"));

    let folders = client
        .virtual_folders(&session)
        .await
        .expect("virtual folders");
    let names: Vec<&str> = folders
        .iter()
        .filter_map(|folder| folder.name.as_deref())
        .collect();
    assert_eq!(names, vec!["Movies", "Shows", "Music"]);

    let active = client
        .active_session_count(&session)
        .await
        .expect("active session count");
    assert_eq!(active, 1, "the fixture has one session marked active");

    let log = request_log(&server).await;
    for path in ["/System/Info", "/Library/VirtualFolders", "/Sessions"] {
        assert!(
            log.iter()
                .any(|(method, logged)| method == "GET" && logged == path),
            "missing GET {path}: {log:?}"
        );
    }
}

/// Reads a request until the blank line that ends its headers, then answers
/// every connection with `403`. Loopback only.
async fn spawn_forbidden_stub() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback stub");
    let addr = listener.local_addr().expect("stub address");
    let handle = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut received = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => received.extend_from_slice(&chunk[..read]),
                }
            }
            let _ = socket
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn forbidden_admin_reads_map_to_forbidden_not_a_panic() {
    // The session comes from the mock; the 403 comes from a loopback stub that
    // stands in for a server that revoked admin rights mid-session.
    let mock = spawn(MockConfig::default()).await.expect("spawn mock");
    let (_, session) = signed_in(&mock).await;

    let (stub_url, stub) = spawn_forbidden_stub().await;
    let stub_client = client(&stub_url);

    let folders = stub_client.virtual_folders(&session).await;
    assert!(
        matches!(folders, Err(ApiError::Forbidden)),
        "virtual folders: {folders:?}"
    );
    let sessions = stub_client.active_session_count(&session).await;
    assert!(
        matches!(sessions, Err(ApiError::Forbidden)),
        "sessions: {sessions:?}"
    );

    stub.abort();
}
