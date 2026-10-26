//! Loopback tests for inbound SyncPlay commands on the M4-13 session socket. The server is a
//! std thread that runs the tungstenite handshake on 127.0.0.1. No real Jellyfin server is
//! contacted; the session token comes from the in-process mock.

use std::{net::TcpListener, thread};

use jellyfin_api::websocket::{Inbound, SessionSocket, SyncPlayMessage, SyncPlayWireCommand};
use jellyfin_api::{ClientConfig, JellyfinClient, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn as spawn_mock_jellyfin};
use tokio::sync::Mutex;
use tungstenite::{Message, accept};

// SessionSocket shares a deliberately bounded resolver worker. Keep these loopback connections
// from racing for its single queued slot when the integration test harness runs in parallel.
static SOCKET_TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// Start a loopback socket server that sends `messages` to the first client, then holds the
/// connection until the client goes away.
fn serve(messages: Vec<Message>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let Ok(mut socket) = accept(stream) else {
            return;
        };
        for message in messages {
            if socket.send(message).is_err() {
                return;
            }
        }
        while socket.read().is_ok() {}
    });
    format!("http://{addr}")
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

const PAUSE: &str = r#"{"MessageType":"SyncPlayCommand","Data":{"GroupId":"group-1","Command":"Pause","PositionTicks":600000000,"When":"1970-01-01T00:16:40.0000000Z","EmittedAt":"1970-01-01T00:16:40.0000000Z"}}"#;

#[tokio::test]
async fn sync_play_pause_arrives_with_its_position_and_server_time() {
    let _serial = SOCKET_TEST_LOCK.lock().await;
    let base_url = serve(vec![Message::text(PAUSE)]);
    let (client, session, mock) = signed_in(&base_url).await;

    let mut socket = SessionSocket::connect(&client, &session).expect("connect");
    assert_eq!(
        next(&mut socket),
        Inbound::SyncPlay(SyncPlayMessage {
            command: SyncPlayWireCommand::Pause,
            position_ticks: 600_000_000,
            when: "1970-01-01T00:16:40.0000000Z".into(),
        })
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn unsupported_sync_play_command_is_ignored_and_the_next_one_arrives() {
    let _serial = SOCKET_TEST_LOCK.lock().await;
    let stop = r#"{"MessageType":"SyncPlayCommand","Data":{"GroupId":"group-1","Command":"Stop","PositionTicks":0,"When":"1970-01-01T00:16:40.0000000Z"}}"#;
    let base_url = serve(vec![Message::text(stop), Message::text(PAUSE)]);
    let (client, session, mock) = signed_in(&base_url).await;

    let mut socket = SessionSocket::connect(&client, &session).expect("connect");
    assert_eq!(next(&mut socket), Inbound::Ignored);
    assert!(matches!(
        next(&mut socket),
        Inbound::SyncPlay(SyncPlayMessage {
            command: SyncPlayWireCommand::Pause,
            ..
        })
    ));
    mock.shutdown().await;
}
