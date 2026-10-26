//! Read-only admin calls (M4-22) against `mock-jellyfin`, plus the 403 path
//! against a loopback stub. The mock is not a real Jellyfin server, so these
//! passes do not confirm compatibility with one.

use jellyfin_api::{ApiError, ClientConfig, JellyfinClient, Session, UserCounts};
use mock_jellyfin::{MockConfig, RunningServer, spawn};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

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

    let users = client.user_counts(&session).await.expect("user counts");
    assert_eq!(
        users,
        UserCounts {
            total: 3,
            enabled: 2,
            disabled: 1,
        }
    );

    let tasks = client.scheduled_tasks(&session).await.expect("tasks");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].name.as_deref(), Some("Mock Library Scan"));
    assert_eq!(tasks[0].state.as_deref(), Some("Idle"));
    assert_eq!(tasks[0].last_status.as_deref(), Some("Completed"));
    assert_eq!(tasks[1].last_status, None, "a task that never ran has none");

    let library_ids: Vec<&str> = folders
        .iter()
        .filter_map(|folder| folder.item_id.as_deref())
        .collect();
    let count = client
        .library_item_count(&session, library_ids[0])
        .await
        .expect("library item count");
    assert!(
        count.is_some_and(|count| count > 0),
        "the mock reports a total for a limited page: {count:?}"
    );

    let log = request_log(&server).await;
    for path in [
        "/System/Info",
        "/Library/VirtualFolders",
        "/Sessions",
        "/Users",
        "/ScheduledTasks",
    ] {
        assert!(
            log.iter()
                .any(|(method, logged)| method == "GET" && logged == path),
            "missing GET {path}: {log:?}"
        );
    }
    assert!(
        log.iter().any(|(method, logged)| method == "GET"
            && logged.starts_with("/Items?")
            && logged.contains("recursive=true")),
        "missing the counted library page: {log:?}"
    );
}

/// Serves recorded response bodies by request-path prefix. Loopback only; it
/// records nothing and has no write route.
async fn spawn_fixture_stub(routes: Vec<(&'static str, &'static str)>) -> (String, JoinHandle<()>) {
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
            let request = String::from_utf8_lossy(&received).into_owned();
            let target = request.split_whitespace().nth(1).unwrap_or_default();
            let body = routes
                .iter()
                .find(|(prefix, _)| target.starts_with(prefix))
                .map(|(_, body)| *body);
            let response = match body {
                Some(body) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_owned(),
            };
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), handle)
}

/// Recorded server bodies, with account names replaced by placeholders. This
/// exercises the exact shapes the live server returned on 2026-10-26.
#[tokio::test]
async fn recorded_admin_fixtures_decode_into_counts_and_task_rows() {
    const USERS: &str = include_str!("fixtures/admin/users.json");
    const TASKS: &str = include_str!("fixtures/admin/scheduled-tasks.json");
    const ITEMS: &str = include_str!("fixtures/admin/items-count.json");

    let (url, stub) = spawn_fixture_stub(vec![
        ("/Users", USERS),
        ("/ScheduledTasks", TASKS),
        ("/Items", ITEMS),
    ])
    .await;
    let mock = spawn(MockConfig::default()).await.expect("spawn mock");
    let (_, session) = signed_in(&mock).await;
    let stub_client = client(&url);

    let users = stub_client
        .user_counts(&session)
        .await
        .expect("user counts");
    assert_eq!(
        users,
        UserCounts {
            total: 3,
            enabled: 2,
            disabled: 1,
        }
    );

    let tasks = stub_client.scheduled_tasks(&session).await.expect("tasks");
    let rows: Vec<(Option<&str>, Option<&str>)> = tasks
        .iter()
        .map(|task| (task.state.as_deref(), task.last_status.as_deref()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (Some("Idle"), Some("Completed")),
            (Some("Idle"), Some("Aborted")),
            (Some("Running"), Some("Failed")),
            (Some("Idle"), None),
        ]
    );

    let count = stub_client
        .library_item_count(&session, "0f74e0b7a8b34f68a1b2c3d4e5f60718")
        .await
        .expect("library item count");
    assert_eq!(count, Some(74));

    stub.abort();
}

#[tokio::test]
async fn library_item_count_refuses_a_value_that_is_not_a_library_id() {
    const ITEMS: &str = include_str!("fixtures/admin/items-count.json");
    let (url, stub) = spawn_fixture_stub(vec![("/Items", ITEMS)]).await;
    let mock = spawn(MockConfig::default()).await.expect("spawn mock");
    let (_, session) = signed_in(&mock).await;
    let stub_client = client(&url);

    for library_id in [
        "",
        " ",
        "../../Users",
        "id with space",
        "<script>",
        &"a".repeat(65),
    ] {
        let result = stub_client.library_item_count(&session, library_id).await;
        assert!(
            matches!(result, Err(ApiError::InvalidConfiguration)),
            "library id {library_id:?} was not refused: {result:?}"
        );
    }

    stub.abort();
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
    let users = stub_client.user_counts(&session).await;
    assert!(
        matches!(users, Err(ApiError::Forbidden)),
        "users: {users:?}"
    );
    let tasks = stub_client.scheduled_tasks(&session).await;
    assert!(
        matches!(tasks, Err(ApiError::Forbidden)),
        "tasks: {tasks:?}"
    );
    let count = stub_client
        .library_item_count(&session, "0f74e0b7a8b34f68a1b2c3d4e5f60718")
        .await;
    assert!(
        matches!(count, Err(ApiError::Forbidden)),
        "library count: {count:?}"
    );

    stub.abort();
}
