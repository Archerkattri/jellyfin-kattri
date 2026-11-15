//! Two-client SyncPlay transport tests against a stateful loopback-only fixture.
//! These exercise the Rust HTTP/WebSocket client path, not Jellyfin server compatibility.

#[path = "../src/stateful_syncplay.rs"]
mod stateful_syncplay;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use jellyfin_api::{
    ClientConfig, JellyfinClient, SyncPlayPlaybackStatus,
    websocket::{
        Inbound, SessionSocket, SyncPlayGroupUpdate, SyncPlayPlaybackAction, WebSocketError,
    },
};
use mock_jellyfin::{MockConfig, RunningServer, spawn as spawn_auth_server};
use stateful_syncplay::{
    StatefulSyncPlayServer, SyncPlayTransportEvent, SyncPlayTransportOperation,
};
use tokio::sync::Mutex;

const GROUP_ID: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";
const ITEM_A: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3302";
const ITEM_B: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3303";
const WHEN: &str = "2026-11-14T20:00:00.0000000Z";

// SessionSocket uses a bounded resolver worker shared by this process.
static SOCKET_TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn client(base_url: &str, device_id: &str) -> JellyfinClient {
    let config = ClientConfig::new(base_url, "SyncPlay fixture test", device_id, device_id, "0")
        .expect("loopback client config");
    JellyfinClient::new(config).expect("loopback HTTP client")
}

async fn authenticate(server: &RunningServer, device_id: &str) -> jellyfin_api::Session {
    client(&server.base_url(), device_id)
        .authenticate("demo", "demo")
        .await
        .expect("fixture account sign-in")
}

fn receive_matching(
    socket: &mut SessionSocket,
    mut matches: impl FnMut(&Inbound) -> bool,
) -> Inbound {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let incoming = socket.receive().expect("session socket receive");
        if matches(&incoming) {
            return incoming;
        }
        assert_ne!(
            incoming,
            Inbound::Closed,
            "session socket closed before the expected message"
        );
        assert!(
            Instant::now() < deadline,
            "bounded wait for expected SyncPlay message expired; last message: {incoming:?}"
        );
    }
}

fn receive_state(socket: &mut SessionSocket, expected: jellyfin_api::GroupState) {
    let incoming = receive_matching(socket, |message| {
        matches!(
            message,
            Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::StateUpdate { state, .. })
                if *state == expected
        )
    });
    assert!(matches!(
        incoming,
        Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::StateUpdate { state, .. })
            if state == expected
    ));
}

fn receive_command(socket: &mut SessionSocket, expected: SyncPlayPlaybackAction) {
    let incoming = receive_matching(socket, |message| {
        matches!(
            message,
            Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::PlaybackCommand(command))
                if command.action == expected
        )
    });
    assert!(matches!(
        incoming,
        Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::PlaybackCommand(command))
            if command.action == expected && command.group_id.as_deref() == Some(GROUP_ID)
    ));
}

fn api_operations(events: &[SyncPlayTransportEvent]) -> Vec<(String, SyncPlayTransportOperation)> {
    events
        .iter()
        .filter(|event| {
            !matches!(
                event.operation,
                SyncPlayTransportOperation::SocketConnected
                    | SyncPlayTransportOperation::SocketDisconnected
            )
        })
        .map(|event| (event.client_id.clone(), event.operation))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_share_group_queue_and_ordered_playback_over_http_and_websocket() {
    let _serial = SOCKET_TEST_LOCK.lock().await;
    let auth_server = spawn_auth_server(MockConfig::default())
        .await
        .expect("spawn authentication mock");
    let server = StatefulSyncPlayServer::spawn()
        .await
        .expect("spawn stateful SyncPlay fixture");
    let client_a = client(&server.base_url(), "syncplay-client-a");
    let client_b = client(&server.base_url(), "syncplay-client-b");
    let socket_client_a = client(&server.socket_base_url(), "syncplay-client-a");
    let socket_client_b = client(&server.socket_base_url(), "syncplay-client-b");
    let session_a = authenticate(&auth_server, "syncplay-client-a").await;
    let session_b = authenticate(&auth_server, "syncplay-client-b").await;

    let mut socket_a =
        SessionSocket::connect(&socket_client_a, &session_a).expect("connect client A");
    assert!(server.wait_for_events(1, Duration::from_secs(2)).await);
    let group = client_a
        .create_sync_play_group(&session_a, "Loopback watch party")
        .await
        .expect("create shared group");
    assert_eq!(group.group_id, GROUP_ID);
    assert_eq!(group.participants, vec!["syncplay-client-a".to_owned()]);
    let created = receive_matching(&mut socket_a, |message| {
        matches!(
            message,
            Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::Joined(_))
        )
    });
    assert!(matches!(
        created,
        Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::Joined(group))
            if group.group_id == GROUP_ID
    ));

    let mut socket_b =
        SessionSocket::connect(&socket_client_b, &session_b).expect("connect client B");
    assert!(server.wait_for_events(3, Duration::from_secs(2)).await);
    let listed = client_b
        .list_sync_play_groups(&session_b)
        .await
        .expect("list host group");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].participants, vec!["syncplay-client-a".to_owned()]);
    client_b
        .join_sync_play_group(&session_b, &group.group_id)
        .await
        .expect("join host group");
    for socket in [&mut socket_a, &mut socket_b] {
        let incoming = receive_matching(socket, |message| {
            matches!(
                message,
                Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::UserJoined { .. })
            )
        });
        assert!(matches!(
            incoming,
            Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::UserJoined { user_name, .. })
                if user_name == "syncplay-client-b"
        ));
    }

    let playlist = vec![ITEM_A.to_owned(), ITEM_B.to_owned()];
    client_a
        .set_sync_play_new_queue(&session_a, &playlist, 0, 12_000_000)
        .await
        .expect("replace the shared queue");
    let queue_updates = [&mut socket_a, &mut socket_b].map(|socket| {
        receive_matching(socket, |message| {
            matches!(
                message,
                Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::PlayQueue { .. })
            )
        })
    });
    let playlist_item_id = queue_updates
        .iter()
        .find_map(|message| match message {
            Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::PlayQueue { update, .. }) => {
                Some(update.playlist[0].playlist_item_id.clone())
            }
            _ => None,
        })
        .expect("queue update contains a current playlist entry");
    assert!(queue_updates.iter().all(|message| matches!(
        message,
        Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::PlayQueue { update, .. })
            if update.playlist.len() == 2 && update.start_position_ticks == 12_000_000
    )));
    receive_state(&mut socket_a, jellyfin_api::GroupState::Waiting);
    receive_state(&mut socket_b, jellyfin_api::GroupState::Waiting);

    let status = |position_ticks| SyncPlayPlaybackStatus {
        when: WHEN.to_owned(),
        position_ticks,
        is_playing: true,
        playlist_item_id: playlist_item_id.clone(),
    };
    client_a
        .sync_play_ready(&session_a, &status(12_000_000))
        .await
        .expect("client A reports ready");
    receive_state(&mut socket_a, jellyfin_api::GroupState::Waiting);
    client_b
        .sync_play_ready(&session_b, &status(12_500_000))
        .await
        .expect("client B reports ready");
    receive_state(&mut socket_b, jellyfin_api::GroupState::Playing);

    client_b
        .sync_play_buffering(&session_b, &status(15_000_000))
        .await
        .expect("client B reports buffering");
    receive_state(&mut socket_a, jellyfin_api::GroupState::Waiting);
    client_b
        .sync_play_ready(&session_b, &status(16_000_000))
        .await
        .expect("client B recovers and reports ready");
    receive_state(&mut socket_a, jellyfin_api::GroupState::Playing);
    receive_state(&mut socket_b, jellyfin_api::GroupState::Playing);
    client_a
        .sync_play_pause(&session_a)
        .await
        .expect("pause shared playback");
    receive_state(&mut socket_b, jellyfin_api::GroupState::Paused);
    receive_command(&mut socket_b, SyncPlayPlaybackAction::Pause);
    client_a
        .sync_play_seek(&session_a, 30_000_000)
        .await
        .expect("seek shared playback");
    receive_command(&mut socket_b, SyncPlayPlaybackAction::Seek);
    client_b
        .sync_play_unpause(&session_b)
        .await
        .expect("unpause shared playback");
    receive_state(&mut socket_a, jellyfin_api::GroupState::Playing);
    receive_command(&mut socket_a, SyncPlayPlaybackAction::Unpause);

    let prior_events = server.events().await.len();
    socket_b.close().expect("close client B session socket");
    assert!(
        server
            .wait_for_events(prior_events + 1, Duration::from_secs(2))
            .await
    );
    let disconnected = server.events().await;
    assert!(disconnected.iter().any(|event| {
        event.client_id == "syncplay-client-b"
            && event.operation == SyncPlayTransportOperation::SocketDisconnected
    }));

    let prior_events = disconnected.len();
    let mut socket_b =
        SessionSocket::connect(&socket_client_b, &session_b).expect("reconnect client B");
    assert!(
        server
            .wait_for_events(prior_events + 1, Duration::from_secs(2))
            .await
    );
    let rejoined = receive_matching(&mut socket_b, |message| {
        matches!(
            message,
            Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::Updated(_))
        )
    });
    assert!(matches!(
        rejoined,
        Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::Updated(group))
            if group.group_id == GROUP_ID
                && group.state == jellyfin_api::GroupState::Playing
                && group.participants.len() == 2
    ));

    client_b
        .leave_sync_play_group(&session_b)
        .await
        .expect("leave shared group");
    let left = receive_matching(&mut socket_a, |message| {
        matches!(
            message,
            Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::UserLeft { .. })
        )
    });
    assert!(matches!(
        left,
        Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::UserLeft { user_name, .. })
            if user_name == "syncplay-client-b"
    ));
    let remaining = client_b
        .list_sync_play_groups(&session_b)
        .await
        .expect("list group after leave");
    assert_eq!(
        remaining[0].participants,
        vec!["syncplay-client-a".to_owned()]
    );

    let events = server.events().await;
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    assert_eq!(
        api_operations(&events),
        vec![
            (
                "syncplay-client-a".into(),
                SyncPlayTransportOperation::GroupCreated
            ),
            (
                "syncplay-client-b".into(),
                SyncPlayTransportOperation::GroupListed
            ),
            (
                "syncplay-client-b".into(),
                SyncPlayTransportOperation::GroupJoined
            ),
            (
                "syncplay-client-a".into(),
                SyncPlayTransportOperation::QueueChanged
            ),
            (
                "syncplay-client-a".into(),
                SyncPlayTransportOperation::Ready
            ),
            (
                "syncplay-client-b".into(),
                SyncPlayTransportOperation::Ready
            ),
            (
                "syncplay-client-b".into(),
                SyncPlayTransportOperation::Buffering
            ),
            (
                "syncplay-client-b".into(),
                SyncPlayTransportOperation::Ready
            ),
            (
                "syncplay-client-a".into(),
                SyncPlayTransportOperation::Paused
            ),
            (
                "syncplay-client-a".into(),
                SyncPlayTransportOperation::Seeked
            ),
            (
                "syncplay-client-b".into(),
                SyncPlayTransportOperation::Unpaused
            ),
            ("syncplay-client-b".into(), SyncPlayTransportOperation::Left),
            (
                "syncplay-client-b".into(),
                SyncPlayTransportOperation::GroupListed
            ),
        ]
    );
    assert!(events.iter().any(|event| {
        event.operation == SyncPlayTransportOperation::QueueChanged && event.detail == Some(2)
    }));
    assert!(events.iter().any(|event| {
        event.operation == SyncPlayTransportOperation::Seeked && event.detail == Some(30_000_000)
    }));

    socket_b.close().expect("close reconnected client B socket");
    socket_a.close().expect("close client A socket");
    server.shutdown().await;
    auth_server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_socket_setup_cancellation_finishes_within_a_bound() {
    let _serial = SOCKET_TEST_LOCK.lock().await;
    let auth_server = spawn_auth_server(MockConfig::default())
        .await
        .expect("spawn authentication mock");
    let server = StatefulSyncPlayServer::spawn_with_upgrade_delay(Duration::from_millis(700))
        .await
        .expect("spawn delayed WebSocket fixture");
    let client = client(&server.socket_base_url(), "syncplay-cancel-client");
    let session = authenticate(&auth_server, "syncplay-cancel-client").await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let connect_cancelled = Arc::clone(&cancelled);
    let started = Instant::now();
    let connect = tokio::task::spawn_blocking(move || {
        SessionSocket::connect_with_cancel(&client, &session, connect_cancelled)
    });

    tokio::time::sleep(Duration::from_millis(60)).await;
    cancelled.store(true, Ordering::Release);
    let result = tokio::time::timeout(Duration::from_secs(1), connect)
        .await
        .expect("cancelled socket setup returns within one second")
        .expect("socket setup task joins");
    assert!(matches!(result, Err(WebSocketError::SetupCancelled)));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        !server
            .events()
            .await
            .iter()
            .any(|event| event.operation == SyncPlayTransportOperation::SocketConnected)
    );

    server.shutdown().await;
    auth_server.shutdown().await;
}
