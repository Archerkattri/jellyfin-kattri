//! `GET /GetUtcTime` against a loopback stub. The stub answers sign-in and this route and
//! records the request lines it receives. It is not a Jellyfin server, so these passes do
//! not confirm the route on a real one.

use std::sync::{Arc, Mutex};

use jellyfin_api::{ApiError, ClientConfig, JellyfinClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "test-token-value";
const UTC_BODY: &str = r#"{"RequestReceptionTime":"2026-10-22T10:00:00.1000000Z","ResponseTransmissionTime":"2026-10-22T10:00:00.2000000Z"}"#;
const AUTH_BODY: &str = r#"{"AccessToken":"test-token-value","ServerId":"stub-server","User":{"Id":"user-1","Name":"demo"}}"#;

/// One request the stub read: the request line and the header block.
#[derive(Clone, Debug)]
struct Seen {
    request_line: String,
    head: String,
}

async fn read_request(socket: &mut TcpStream) -> Option<Seen> {
    let mut data = Vec::new();
    let mut buf = [0_u8; 1024];
    let head_end = loop {
        if let Some(pos) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
        let read = socket.read(&mut buf).await.ok()?;
        if read == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..read]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while data.len() < head_end + content_length {
        let read = socket.read(&mut buf).await.ok()?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buf[..read]);
    }
    let request_line = head.lines().next().unwrap_or_default().to_owned();
    Some(Seen { request_line, head })
}

fn respond(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Spawn the stub. `utc_status` and `utc_body` set the answer to `GET /GetUtcTime`.
async fn spawn_stub(
    utc_status: &'static str,
    utc_body: &'static str,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback stub");
    let addr = listener.local_addr().expect("stub address");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let Some(request) = read_request(&mut socket).await else {
                continue;
            };
            let reply = if request
                .request_line
                .starts_with("POST /Users/AuthenticateByName")
            {
                respond("200 OK", AUTH_BODY)
            } else if request.request_line.starts_with("GET /GetUtcTime ") {
                respond(utc_status, utc_body)
            } else {
                respond("404 Not Found", "{}")
            };
            if let Ok(mut log) = log.lock() {
                log.push(request);
            }
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (format!("http://{addr}"), seen)
}

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

#[tokio::test]
async fn get_utc_time_sends_the_session_token_and_reads_both_server_times() {
    let (base_url, seen) = spawn_stub("200 OK", UTC_BODY).await;
    let client = client(&base_url);
    let session = client
        .authenticate("demo", "demo")
        .await
        .expect("sign in to the stub");

    let time = client
        .get_utc_time(&session)
        .await
        .expect("server time from the stub");

    assert_eq!(time.request_reception_time, "2026-10-22T10:00:00.1000000Z");
    assert_eq!(
        time.response_transmission_time,
        "2026-10-22T10:00:00.2000000Z"
    );

    let log = seen.lock().expect("request log").clone();
    let utc = log
        .iter()
        .find(|entry| entry.request_line.starts_with("GET /GetUtcTime "))
        .expect("the route was requested");
    assert!(
        utc.head.contains(TOKEN),
        "the call carries the session token"
    );
}

#[tokio::test]
async fn get_utc_time_maps_a_server_error_to_an_error_without_a_time() {
    let (base_url, _seen) = spawn_stub("500 Internal Server Error", "{}").await;
    let client = client(&base_url);
    let session = client
        .authenticate("demo", "demo")
        .await
        .expect("sign in to the stub");

    let result = client.get_utc_time(&session).await;

    assert!(
        matches!(result, Err(ApiError::HttpStatus(_))),
        "a 500 is an error, not an empty time: {result:?}"
    );
}
