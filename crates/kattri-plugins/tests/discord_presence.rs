//! Presence text checked through the frame encoder. Pure: no IPC and no Discord connection.

use kattri_plugins::discord::presence::{
    PlaybackState, PresenceOptions, activity_for, format_position,
};
use kattri_plugins::discord::{
    HEADER_LEN, MAX_PRESENCE_CHARS, Opcode, PresenceActivity, decode_header, encode_frame,
    set_activity_payload,
};
use serde_json::Value;

fn state(title: &str, paused: bool, position_ms: u64) -> PlaybackState {
    PlaybackState {
        title: title.to_owned(),
        paused,
        position_ms,
    }
}

fn frame_for(activity: &PresenceActivity) -> Vec<u8> {
    let payload = set_activity_payload(activity, 4242, "n-1").expect("payload");
    encode_frame(Opcode::Frame, &payload).expect("frame")
}

fn payload_of(frame: &[u8]) -> Value {
    let mut header = [0_u8; HEADER_LEN];
    header.copy_from_slice(&frame[..HEADER_LEN]);
    let (opcode, length) = decode_header(header).expect("header");
    assert_eq!(opcode, Opcode::Frame);
    assert_eq!(frame.len(), HEADER_LEN + length);
    serde_json::from_slice(&frame[HEADER_LEN..]).expect("json payload")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn playing_with_title_hidden_builds_the_generic_frame() {
    let activity = activity_for(
        &state("Kitchen sink", false, 61_000),
        PresenceOptions::default(),
    );
    let value = payload_of(&frame_for(&activity));
    assert_eq!(value["cmd"], "SET_ACTIVITY");
    assert_eq!(value["nonce"], "n-1");
    assert_eq!(value["args"]["pid"], 4242);
    assert_eq!(value["args"]["activity"]["details"], "Watching");
    assert_eq!(value["args"]["activity"]["state"], "Playing 1:01");
    assert!(value["args"]["activity"].get("assets").is_none());
}

#[test]
fn show_title_puts_the_title_in_details_and_paused_in_state() {
    let options = PresenceOptions { show_title: true };
    let activity = activity_for(&state("Kitchen sink, season 2", true, 5_000), options);
    let value = payload_of(&frame_for(&activity));
    assert_eq!(
        value["args"]["activity"]["details"],
        "Kitchen sink, season 2"
    );
    assert_eq!(value["args"]["activity"]["state"], "Paused 0:05");
}

#[test]
fn hidden_title_never_reaches_the_frame_bytes() {
    let activity = activity_for(&state("Kitchen sink", false, 0), PresenceOptions::default());
    assert!(!contains(&frame_for(&activity), b"Kitchen"));
}

#[test]
fn refused_titles_fall_back_to_the_generic_line() {
    let options = PresenceOptions { show_title: true };
    for title in [
        "https://media.example.org/Items/1",
        "http://10.0.0.5:8096/Items/1",
        "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",
        "Authorization: Bearer abc",
        "api_key=abc",
        "bad\u{7}bell",
        "",
    ] {
        let activity = activity_for(&state(title, false, 0), options);
        assert_eq!(
            activity.details.as_deref(),
            Some("Watching"),
            "title {title:?} was not refused"
        );
        assert!(activity.validate().is_ok(), "title {title:?}");
    }
}

#[test]
fn long_titles_are_clipped_to_the_presence_limit() {
    let options = PresenceOptions { show_title: true };
    let long = "x".repeat(MAX_PRESENCE_CHARS + 40);
    let activity = activity_for(&state(&long, false, 0), options);
    assert_eq!(
        activity.details.as_deref().map(|d| d.chars().count()),
        Some(MAX_PRESENCE_CHARS)
    );
    assert!(activity.validate().is_ok());
}

#[test]
fn format_position_uses_minutes_and_hours() {
    assert_eq!(format_position(0), "0:00");
    assert_eq!(format_position(59_999), "0:59");
    assert_eq!(format_position(61_000), "1:01");
    assert_eq!(format_position(3_723_000), "1:02:03");
}

#[test]
fn no_generated_frame_carries_a_url_origin_or_credential() {
    let titles = [
        "Kitchen sink",
        "https://media.example.org/x",
        "http://10.0.0.5:8096",
        "Authorization: Bearer abc",
        "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",
    ];
    let markers: [&[u8]; 6] = [
        b"://",
        b"api_key",
        b"token",
        b"Authorization",
        b"8096",
        b"localhost",
    ];
    for show_title in [false, true] {
        for paused in [false, true] {
            for title in titles {
                let options = PresenceOptions { show_title };
                let frame = frame_for(&activity_for(&state(title, paused, 9_999_000), options));
                for marker in markers {
                    assert!(
                        !contains(&frame, marker),
                        "marker {:?} found for title {title:?}, show_title {show_title}",
                        String::from_utf8_lossy(marker)
                    );
                }
            }
        }
    }
}

#[test]
fn clearing_frame_carries_a_null_activity() {
    let frame = frame_for(&PresenceActivity::default());
    let value = payload_of(&frame);
    assert_eq!(value["args"]["activity"], Value::Null);
}
