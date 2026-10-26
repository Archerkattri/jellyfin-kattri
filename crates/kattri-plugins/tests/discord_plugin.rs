//! The presence plugin driven through the mock transport. No Discord connection is made.

use std::time::Duration;

use kattri_plugins::discord::plugin::{
    PROGRESS_INTERVAL, PlaybackNotice, PresencePlugin, RETRY_BACKOFF,
};
use kattri_plugins::discord::presence::PresenceOptions;
use kattri_plugins::discord::{HEADER_LEN, MockTransport, Opcode, decode_header};
use kattri_plugins::permission::{Permission, PermissionGrants};
use kattri_plugins::{PluginError, PluginManifest};
use serde_json::Value;

const CLIENT_ID: &str = "123456789012345678";

fn manifest(permissions: &str) -> PluginManifest {
    let text = format!(
        r#"{{"id":"org.kattri.discord-presence","name":"Discord Presence","version":"0.1.0",
            "api_version":1,"entry":"src/lib.rs","permissions":[{permissions}]}}"#
    );
    PluginManifest::from_json(&text).expect("manifest")
}

fn plugin_with(
    transport: &MockTransport,
    options: PresenceOptions,
    grant_read: bool,
    grant_write: bool,
) -> PresencePlugin<MockTransport> {
    let m = manifest(r#""playback.read","presence.write""#);
    let mut grants = PermissionGrants::for_plugin(&m);
    if grant_read {
        grants.grant(Permission::PlaybackRead);
    }
    if grant_write {
        grants.grant(Permission::PresenceWrite);
    }
    PresencePlugin::new(m, grants, transport.clone(), CLIENT_ID, options).expect("plugin")
}

fn granted(transport: &MockTransport) -> PresencePlugin<MockTransport> {
    plugin_with(transport, PresenceOptions::default(), true, true)
}

fn secs(value: u64) -> Duration {
    Duration::from_secs(value)
}

fn activity_of(frame: &[u8]) -> Value {
    let mut header = [0_u8; HEADER_LEN];
    header.copy_from_slice(&frame[..HEADER_LEN]);
    let (opcode, length) = decode_header(header).expect("header");
    assert_eq!(opcode, Opcode::Frame);
    assert_eq!(frame.len(), HEADER_LEN + length);
    let value: Value = serde_json::from_slice(&frame[HEADER_LEN..]).expect("json");
    assert_eq!(value["cmd"], "SET_ACTIVITY");
    value["args"]["activity"].clone()
}

fn last_activity(transport: &MockTransport) -> Value {
    let frames = transport.frames();
    activity_of(frames.last().expect("a frame was written"))
}

#[test]
fn start_sends_one_activity_frame() {
    let transport = MockTransport::new();
    let mut plugin = granted(&transport);
    plugin
        .handle(
            PlaybackNotice::ItemChanged {
                title: "Kitchen sink".into(),
            },
            secs(0),
        )
        .expect("item change");
    assert!(
        transport.frames().is_empty(),
        "no activity before playback starts"
    );

    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("start");
    assert_eq!(transport.frames().len(), 1);
    let activity = last_activity(&transport);
    assert_eq!(activity["state"], "Playing 0:00");
    assert_eq!(activity["details"], "Watching");
    assert_eq!(transport.connect_calls(), 1);
    assert!(plugin.is_connected());
}

#[test]
fn stop_clears_the_activity() {
    let transport = MockTransport::new();
    let mut plugin = granted(&transport);
    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("start");
    plugin
        .handle(PlaybackNotice::Stopped, secs(3))
        .expect("stop");
    assert_eq!(transport.frames().len(), 2);
    assert_eq!(last_activity(&transport), Value::Null);
}

#[test]
fn pause_and_resume_update_the_state_line_at_once() {
    let transport = MockTransport::new();
    let mut plugin = granted(&transport);
    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("start");
    plugin
        .handle(PlaybackNotice::Paused { position_ms: 5_000 }, secs(1))
        .expect("pause");
    assert_eq!(last_activity(&transport)["state"], "Paused 0:05");
    plugin
        .handle(PlaybackNotice::Resumed { position_ms: 5_000 }, secs(2))
        .expect("resume");
    assert_eq!(last_activity(&transport)["state"], "Playing 0:05");
    assert_eq!(transport.frames().len(), 3);
}

#[test]
fn progress_updates_at_most_once_per_five_seconds() {
    let transport = MockTransport::new();
    let mut plugin = granted(&transport);
    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("start");
    assert_eq!(transport.frames().len(), 1);

    let progress = |position_ms| PlaybackNotice::Progress {
        position_ms,
        paused: false,
    };
    plugin
        .handle(progress(1_000), secs(1))
        .expect("progress at 1 s");
    plugin
        .handle(progress(4_000), Duration::from_millis(4_900))
        .expect("progress at 4.9 s");
    assert_eq!(transport.frames().len(), 1, "inside the window: no update");

    plugin
        .handle(progress(6_000), PROGRESS_INTERVAL)
        .expect("progress at 5 s");
    assert_eq!(transport.frames().len(), 2);
    assert_eq!(last_activity(&transport)["state"], "Playing 0:06");

    plugin
        .handle(progress(7_000), secs(9))
        .expect("progress at 9 s");
    assert_eq!(
        transport.frames().len(),
        2,
        "window restarts at the last send"
    );
}

#[test]
fn progress_that_changes_nothing_sends_nothing() {
    let transport = MockTransport::new();
    let mut plugin = granted(&transport);
    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("start");
    plugin
        .handle(
            PlaybackNotice::Progress {
                position_ms: 0,
                paused: false,
            },
            secs(6),
        )
        .expect("progress");
    assert_eq!(transport.frames().len(), 1);
}

#[test]
fn transport_failure_does_not_stop_playback_reporting() {
    let transport = MockTransport::new();
    transport.fail_next_writes(1);
    let mut plugin = granted(&transport);
    plugin
        .handle(
            PlaybackNotice::ItemChanged {
                title: "Kitchen sink".into(),
            },
            secs(0),
        )
        .expect("item change");

    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("a failed write is absorbed");
    assert!(!plugin.is_connected());
    assert!(transport.frames().is_empty());

    plugin
        .handle(PlaybackNotice::Paused { position_ms: 4_000 }, secs(1))
        .expect("pause during backoff");
    plugin
        .handle(
            PlaybackNotice::Progress {
                position_ms: 5_000,
                paused: true,
            },
            secs(2),
        )
        .expect("progress during backoff");
    assert_eq!(
        transport.connect_calls(),
        1,
        "no reconnect before the backoff ends"
    );

    plugin
        .handle(
            PlaybackNotice::Resumed { position_ms: 9_000 },
            RETRY_BACKOFF,
        )
        .expect("resume after the backoff");
    assert!(plugin.is_connected());
    assert_eq!(transport.connect_calls(), 2);
    assert_eq!(last_activity(&transport)["state"], "Playing 0:09");
}

#[test]
fn failed_connect_is_retried_after_the_backoff() {
    let transport = MockTransport::new();
    transport.fail_next_connects(1);
    let mut plugin = granted(&transport);

    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("connect failure is absorbed");
    assert!(!plugin.is_connected());
    assert_eq!(transport.connect_calls(), 1);

    plugin
        .handle(
            PlaybackNotice::Progress {
                position_ms: 1_000,
                paused: false,
            },
            secs(1),
        )
        .expect("progress during backoff");
    assert_eq!(transport.connect_calls(), 1);
    assert!(transport.frames().is_empty());

    plugin
        .handle(
            PlaybackNotice::Progress {
                position_ms: 2_000,
                paused: false,
            },
            RETRY_BACKOFF,
        )
        .expect("progress after the backoff");
    assert_eq!(transport.connect_calls(), 2);
    assert_eq!(transport.frames().len(), 1);
    assert_eq!(last_activity(&transport)["state"], "Playing 0:02");
}

#[test]
fn presence_write_needs_a_grant() {
    let transport = MockTransport::new();
    let mut plugin = plugin_with(&transport, PresenceOptions::default(), true, false);
    let err = plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect_err("no presence.write grant");
    assert!(matches!(err, PluginError::PermissionDenied { .. }));
    assert!(transport.frames().is_empty());
    assert_eq!(transport.connect_calls(), 0, "a refused call opens nothing");
}

#[test]
fn playback_read_needs_a_grant() {
    let transport = MockTransport::new();
    let mut plugin = plugin_with(&transport, PresenceOptions::default(), false, true);
    let err = plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect_err("no playback.read grant");
    assert!(matches!(err, PluginError::PermissionDenied { .. }));
    assert!(transport.frames().is_empty());
}

#[test]
fn undeclared_presence_write_is_refused_even_when_granted() {
    let transport = MockTransport::new();
    let m = manifest(r#""playback.read""#);
    let mut grants = PermissionGrants::for_plugin(&m);
    grants.grant(Permission::PlaybackRead);
    grants.grant(Permission::PresenceWrite);
    let mut plugin = PresencePlugin::new(
        m,
        grants,
        transport.clone(),
        CLIENT_ID,
        PresenceOptions::default(),
    )
    .expect("plugin");
    let err = plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect_err("presence.write is not declared");
    assert!(matches!(err, PluginError::PermissionDenied { .. }));
    assert!(transport.frames().is_empty());
}

#[test]
fn show_title_option_puts_the_title_in_details_and_follows_item_changes() {
    let transport = MockTransport::new();
    let options = PresenceOptions { show_title: true };
    let mut plugin = plugin_with(&transport, options, true, true);
    plugin
        .handle(
            PlaybackNotice::ItemChanged {
                title: "Kitchen sink".into(),
            },
            secs(0),
        )
        .expect("item");
    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("start");
    assert_eq!(last_activity(&transport)["details"], "Kitchen sink");

    plugin
        .handle(
            PlaybackNotice::ItemChanged {
                title: "Next episode".into(),
            },
            secs(1),
        )
        .expect("item change");
    assert_eq!(last_activity(&transport)["details"], "Next episode");
    assert_eq!(transport.frames().len(), 2);
}

#[test]
fn shutdown_clears_the_activity_closes_the_connection_and_is_idempotent() {
    let transport = MockTransport::new();
    let mut plugin = granted(&transport);
    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("start");

    plugin.shutdown(secs(2));
    assert_eq!(last_activity(&transport), Value::Null);
    assert!(!transport.is_connected());
    assert_eq!(transport.frames().len(), 2);

    plugin.shutdown(secs(3));
    assert_eq!(transport.frames().len(), 2, "second shutdown sends nothing");
}

#[test]
fn shutdown_bypasses_the_backoff_to_clear_the_activity() {
    let transport = MockTransport::new();
    let mut plugin = granted(&transport);
    plugin
        .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
        .expect("start");
    assert_eq!(transport.frames().len(), 1);

    // The pause write fails, so the connection is down and the backoff runs until t=16.
    transport.fail_next_writes(1);
    plugin
        .handle(PlaybackNotice::Paused { position_ms: 1_000 }, secs(1))
        .expect("failed write is absorbed");
    assert!(!plugin.is_connected());
    assert_eq!(transport.connect_calls(), 1);

    // Exit ignores the backoff and reconnects once to clear the stale activity.
    plugin.shutdown(secs(2));
    assert_eq!(transport.connect_calls(), 2);
    assert_eq!(transport.frames().len(), 2);
    assert_eq!(last_activity(&transport), Value::Null);
}

#[test]
fn shutdown_with_nothing_shown_sends_nothing() {
    let transport = MockTransport::new();
    let mut plugin = granted(&transport);
    plugin.shutdown(secs(0));
    assert!(transport.frames().is_empty());
    assert_eq!(transport.connect_calls(), 0);
}

#[test]
fn dropping_the_plugin_clears_the_activity() {
    let transport = MockTransport::new();
    {
        let mut plugin = granted(&transport);
        plugin
            .handle(PlaybackNotice::Started { position_ms: 0 }, secs(0))
            .expect("start");
    }
    assert_eq!(transport.frames().len(), 2);
    assert_eq!(last_activity(&transport), Value::Null);
    assert!(!transport.is_connected());
}

#[test]
fn a_non_numeric_client_id_is_refused_at_setup() {
    let transport = MockTransport::new();
    let m = manifest(r#""playback.read","presence.write""#);
    let grants = PermissionGrants::for_plugin(&m);
    let err = PresencePlugin::new(
        m,
        grants,
        transport,
        "not-a-number",
        PresenceOptions::default(),
    )
    .err()
    .expect("client id refused");
    assert!(matches!(err, PluginError::Invalid { .. }));
}
