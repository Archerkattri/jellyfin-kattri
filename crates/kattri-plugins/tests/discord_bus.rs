//! The presence binding fed by a real playback EventBus, driven through the mock transport.
//! No Discord connection is made.

use std::time::Duration;

use kattri_client_core::events::{EventBus, PlaybackBusEvent};
use kattri_plugins::PluginManifest;
use kattri_plugins::discord::plugin::{PresencePlugin, RETRY_BACKOFF};
use kattri_plugins::discord::presence::PresenceOptions;
use kattri_plugins::discord::{HEADER_LEN, MockTransport, Opcode, PresenceBinding, decode_header};
use kattri_plugins::permission::{Permission, PermissionGrants};
use serde_json::Value;

const CLIENT_ID: &str = "123456789012345678";

fn secs(value: u64) -> Duration {
    Duration::from_secs(value)
}

fn binding_on_bus(transport: &MockTransport) -> (EventBus, PresenceBinding<MockTransport>) {
    let text = r#"{"id":"org.kattri.discord-presence","name":"Discord Presence","version":"0.1.0",
        "api_version":1,"entry":"src/lib.rs","permissions":["playback.read","presence.write"]}"#;
    let manifest = PluginManifest::from_json(text).expect("manifest");
    let mut grants = PermissionGrants::for_plugin(&manifest);
    grants.grant(Permission::PlaybackRead);
    grants.grant(Permission::PresenceWrite);
    let plugin = PresencePlugin::new(
        manifest,
        grants,
        transport.clone(),
        CLIENT_ID,
        PresenceOptions::default(),
    )
    .expect("plugin");

    let mut bus = EventBus::new(16, 4).expect("bus");
    let subscription = bus.subscribe().expect("subscription");
    (bus, PresenceBinding::new(subscription, plugin))
}

/// Publish at `at` on the bus, then let the binding drain its queue at the same instant.
fn publish(
    bus: &mut EventBus,
    binding: &mut PresenceBinding<MockTransport>,
    at: Duration,
    event: PlaybackBusEvent,
) -> usize {
    let report = bus.publish_at(at, event);
    binding.pump(at).expect("pump");
    report.delivered
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
fn bus_start_event_produces_the_playing_activity() {
    let transport = MockTransport::new();
    let (mut bus, mut binding) = binding_on_bus(&transport);

    let delivered = publish(&mut bus, &mut binding, secs(0), PlaybackBusEvent::Started);

    assert_eq!(delivered, 1);
    assert_eq!(transport.frames().len(), 1);
    let activity = last_activity(&transport);
    assert_eq!(activity["state"], "Playing 0:00");
    assert_eq!(activity["details"], "Watching");
}

#[test]
fn bus_stop_event_clears_the_activity() {
    let transport = MockTransport::new();
    let (mut bus, mut binding) = binding_on_bus(&transport);

    publish(&mut bus, &mut binding, secs(0), PlaybackBusEvent::Started);
    publish(&mut bus, &mut binding, secs(3), PlaybackBusEvent::Stopped);

    assert_eq!(transport.frames().len(), 2);
    assert_eq!(last_activity(&transport), Value::Null);
}

#[test]
fn bus_throttle_and_plugin_throttle_both_hold() {
    let transport = MockTransport::new();
    let (mut bus, mut binding) = binding_on_bus(&transport);

    publish(&mut bus, &mut binding, secs(0), PlaybackBusEvent::Started);
    assert_eq!(transport.frames().len(), 1);

    // The bus accepts the first position at 1 s and throttles 2 s, 3 s and 4 s.
    for at in 1..=4 {
        publish(
            &mut bus,
            &mut binding,
            secs(at),
            PlaybackBusEvent::Position {
                position_ms: at * 1_000,
            },
        );
    }
    assert_eq!(bus.throttled_total(), 3, "bus throttle window");
    assert_eq!(transport.frames().len(), 1, "plugin throttle window");

    // Five seconds after the accepted position the bus accepts again, and the plugin's
    // five-second window since its last frame has also closed.
    publish(
        &mut bus,
        &mut binding,
        secs(6),
        PlaybackBusEvent::Position { position_ms: 6_000 },
    );
    assert_eq!(transport.frames().len(), 2);
    assert_eq!(last_activity(&transport)["state"], "Playing 0:06");
}

#[test]
fn snapshot_joining_mid_playback_shows_the_paused_state() {
    let transport = MockTransport::new();
    let (mut bus, mut binding) = binding_on_bus(&transport);

    publish(
        &mut bus,
        &mut binding,
        secs(0),
        PlaybackBusEvent::Snapshot {
            position_ms: 90_000,
            duration_ms: Some(3_600_000),
            paused: true,
        },
    );
    assert_eq!(last_activity(&transport)["state"], "Paused 1:30");

    // A position report while paused keeps the paused state.
    publish(
        &mut bus,
        &mut binding,
        secs(6),
        PlaybackBusEvent::Position {
            position_ms: 95_000,
        },
    );
    assert_eq!(last_activity(&transport)["state"], "Paused 1:35");
}

#[test]
fn transport_connect_failure_does_not_stop_bus_delivery() {
    let transport = MockTransport::new();
    transport.fail_next_connects(1);
    let (mut bus, mut binding) = binding_on_bus(&transport);

    let started = publish(&mut bus, &mut binding, secs(0), PlaybackBusEvent::Started);
    let during_backoff = publish(
        &mut bus,
        &mut binding,
        secs(2),
        PlaybackBusEvent::Position { position_ms: 2_000 },
    );
    assert_eq!(started, 1);
    assert_eq!(during_backoff, 1, "bus delivers during the retry backoff");
    assert!(transport.frames().is_empty());
    assert!(!binding.plugin().is_connected());

    // After the backoff the next event reconnects and reaches the transport. Its
    // delivery proves the bus kept the subscriber.
    let after_backoff = publish(
        &mut bus,
        &mut binding,
        RETRY_BACKOFF + secs(1),
        PlaybackBusEvent::Position {
            position_ms: 16_000,
        },
    );
    assert_eq!(after_backoff, 1);
    assert_eq!(transport.connect_calls(), 2);
    assert_eq!(last_activity(&transport)["state"], "Playing 0:16");
}

#[test]
fn transport_write_failure_does_not_stop_bus_delivery() {
    let transport = MockTransport::new();
    let (mut bus, mut binding) = binding_on_bus(&transport);

    publish(&mut bus, &mut binding, secs(0), PlaybackBusEvent::Started);
    assert_eq!(transport.frames().len(), 1);

    transport.fail_next_writes(1);
    let stopped = publish(&mut bus, &mut binding, secs(1), PlaybackBusEvent::Stopped);
    assert_eq!(
        stopped, 1,
        "the stop is delivered even though its write fails"
    );
    assert_eq!(transport.frames().len(), 1);
    assert!(!binding.plugin().is_connected());

    // The next event after the backoff reconnects and sends the current state.
    let resumed = publish(
        &mut bus,
        &mut binding,
        RETRY_BACKOFF + secs(2),
        PlaybackBusEvent::Position {
            position_ms: 17_000,
        },
    );
    assert_eq!(resumed, 1);
    assert_eq!(last_activity(&transport)["state"], "Playing 0:17");
}

#[test]
fn shutdown_clears_the_activity_and_closes_the_connection() {
    let transport = MockTransport::new();
    let (mut bus, mut binding) = binding_on_bus(&transport);

    publish(&mut bus, &mut binding, secs(0), PlaybackBusEvent::Started);
    binding.shutdown(secs(2));

    assert_eq!(transport.frames().len(), 2);
    assert_eq!(last_activity(&transport), Value::Null);
    assert!(!transport.is_connected());

    // The bus still accepts events after shutdown, and the closed plugin sends nothing.
    let delivered = publish(&mut bus, &mut binding, secs(20), PlaybackBusEvent::Started);
    assert_eq!(delivered, 1);
    assert_eq!(transport.frames().len(), 2);
}
