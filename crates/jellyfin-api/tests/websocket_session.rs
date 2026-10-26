//! Loopback tests for the M4-13 session socket. The server is a std thread that runs the
//! tungstenite handshake on 127.0.0.1. No real Jellyfin server is contacted; the session
//! token comes from the in-process mock.

use std::{net::TcpListener, sync::mpsc, thread};

use jellyfin_api::remote_command::RemoteCommand;
use jellyfin_api::websocket::{Inbound, MAX_MESSAGE_BYTES, SessionSocket, WebSocketError};
use jellyfin_api::{ClientConfig, JellyfinClient, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn as spawn_mock_jellyfin};
use tokio::sync::Mutex;
use tungstenite::{
    Message, accept_hdr,
    handshake::server::{Request, Response},
    http::header::AUTHORIZATION,
};

// SessionSocket shares a deliberately bounded resolver worker. These loopback cases need
// exclusive access so unrelated test connections do not consume its single queued slot.
static SOCKET_TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// What the socket server saw in the opening handshake request.
struct Seen {
    uri: String,
    authorization: Option<String>,
}

struct SocketServer {
    base_url: String,
    seen: mpsc::Receiver<Seen>,
}

/// Start a loopback socket server that sends `messages` to the first client, then holds
/// the connection until the client goes away.
#[allow(
    clippy::result_large_err,
    reason = "tungstenite's handshake Callback fixes the ErrorResponse type"
)]
fn serve(messages: Vec<Message>) -> SocketServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let (seen_tx, seen_rx) = mpsc::channel();
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let callback = move |request: &Request, response: Response| {
            let authorization = request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let _ = seen_tx.send(Seen {
                uri: request.uri().to_string(),
                authorization,
            });
            Ok(response)
        };
        let Ok(mut socket) = accept_hdr(stream, callback) else {
            return;
        };
        for message in messages {
            if socket.send(message).is_err() {
                return;
            }
        }
        while socket.read().is_ok() {}
    });
    SocketServer {
        base_url: format!("http://{addr}"),
        seen: seen_rx,
    }
}

async fn signed_in(base_url: &str) -> (JellyfinClient, Session, RunningServer) {
    let mock = spawn_mock_jellyfin(MockConfig::default())
        .await
        .expect("spawn mock");
    let config = ClientConfig::new(base_url, "Kattri test", "test device", "device-id", "0")
        .expect("valid config");
    let client = JellyfinClient::new(config).expect("loopback HTTP is allowed");
    let session = JellyfinClient::new(
        ClientConfig::new(
            &mock.base_url(),
            "Kattri test",
            "test device",
            "device-id",
            "0",
        )
        .expect("valid config"),
    )
    .expect("mock client")
    .authenticate("demo", "demo")
    .await
    .expect("demo sign-in");
    (client, session, mock)
}

/// Skip idle polls and return the first real result.
fn next(socket: &mut SessionSocket) -> Inbound {
    loop {
        match socket.receive().expect("receive") {
            Inbound::Idle => continue,
            other => return other,
        }
    }
}

#[tokio::test]
async fn playstate_pause_arrives_as_the_pause_command() {
    let _serial = SOCKET_TEST_LOCK.lock().await;
    let server = serve(vec![Message::text(
        r#"{"MessageType":"Playstate","Data":{"Command":"Pause"}}"#,
    )]);
    let (client, session, mock) = signed_in(&server.base_url).await;

    let mut socket = SessionSocket::connect(&client, &session).expect("connect");
    assert_eq!(next(&mut socket), Inbound::Command(RemoteCommand::Pause));
    mock.shutdown().await;
}

#[tokio::test]
async fn set_volume_general_command_arrives_as_its_command() {
    let _serial = SOCKET_TEST_LOCK.lock().await;
    let server = serve(vec![Message::text(
        r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetVolume","Arguments":{"Volume":"40"}}}"#,
    )]);
    let (client, session, mock) = signed_in(&server.base_url).await;

    let mut socket = SessionSocket::connect(&client, &session).expect("connect");
    assert_eq!(
        next(&mut socket),
        Inbound::Command(RemoteCommand::SetVolume { volume: 40 })
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn other_message_types_and_unsupported_commands_are_ignored() {
    let _serial = SOCKET_TEST_LOCK.lock().await;
    let server = serve(vec![
        Message::text(r#"{"MessageType":"ForceKeepAlive","Data":60}"#),
        Message::text(r#"{"MessageType":"GeneralCommand","Data":{"Name":"Mute","Arguments":{}}}"#),
        Message::text(r#"{"MessageType":"Playstate","Data":{"Command":"Pause"}}"#),
    ]);
    let (client, session, mock) = signed_in(&server.base_url).await;

    let mut socket = SessionSocket::connect(&client, &session).expect("connect");
    assert_eq!(next(&mut socket), Inbound::Ignored);
    assert_eq!(next(&mut socket), Inbound::Ignored);
    assert_eq!(next(&mut socket), Inbound::Command(RemoteCommand::Pause));
    mock.shutdown().await;
}

#[tokio::test]
async fn message_at_the_limit_is_accepted_and_one_byte_over_is_refused() {
    let _serial = SOCKET_TEST_LOCK.lock().await;
    let prefix = r#"{"MessageType":"Sessions","Data":""#;
    let suffix = r#""}"#;
    let padding = |total: usize| "x".repeat(total - prefix.len() - suffix.len());
    let at_limit = format!("{prefix}{}{suffix}", padding(MAX_MESSAGE_BYTES));
    let over_limit = format!("{prefix}{}{suffix}", padding(MAX_MESSAGE_BYTES + 1));
    assert_eq!(at_limit.len(), MAX_MESSAGE_BYTES);
    assert_eq!(over_limit.len(), MAX_MESSAGE_BYTES + 1);

    let server = serve(vec![Message::text(at_limit), Message::text(over_limit)]);
    let (client, session, mock) = signed_in(&server.base_url).await;

    let mut socket = SessionSocket::connect(&client, &session).expect("connect");
    assert_eq!(next(&mut socket), Inbound::Ignored);
    assert!(matches!(
        socket.receive(),
        Err(WebSocketError::MessageTooLarge)
    ));
    mock.shutdown().await;
}

#[tokio::test]
async fn token_is_sent_in_the_header_and_never_in_the_opened_url() {
    let _serial = SOCKET_TEST_LOCK.lock().await;
    let server = serve(Vec::new());
    let (client, session, mock) = signed_in(&server.base_url).await;

    let header = client
        .session_authorization(&session)
        .header_value()
        .to_owned();
    let token = header
        .split("Token=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("token in authorization header");
    assert!(!token.is_empty());

    let _socket = SessionSocket::connect(&client, &session).expect("connect");
    let seen = server.seen.recv().expect("handshake observed");

    assert_eq!(seen.uri, "/socket");
    assert!(!seen.uri.contains(token), "token leaked into the URL");
    assert!(!seen.uri.to_ascii_lowercase().contains("api_key"));
    assert_eq!(seen.authorization.as_deref(), Some(header.as_str()));
    mock.shutdown().await;
}
