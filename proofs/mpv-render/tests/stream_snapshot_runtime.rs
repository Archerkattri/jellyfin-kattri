//! Reads the media info properties (M6-14) from a real libmpv through `StreamSnapshot`.
//!
//! Ignored by default: it needs a checksum-verified libmpv (`KATTRI_TEST_LIBMPV`). The media
//! defaults to the committed 32-second H.264/AAC fixture; `KATTRI_TEST_MEDIA` overrides it.

use std::{
    env, thread,
    time::{Duration, Instant},
};

use mpv_render_proof::{EventKind, MpvLibrary, MpvSession, StreamSnapshot, available_text};

fn fixture_path() -> String {
    env::var("KATTRI_TEST_MEDIA").unwrap_or_else(|_| {
        format!(
            "{}/tests/fixtures/clip-h264-aac.mkv",
            env!("CARGO_MANIFEST_DIR")
        )
    })
}

fn wait_for(session: &MpvSession, timeout: Duration, mut done: impl FnMut(EventKind, u64) -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(event) = session.try_event()
            && done(event.kind, event.request_id)
        {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("libmpv did not reach the expected state within {timeout:?}");
}

fn snapshot(session: &MpvSession) -> StreamSnapshot {
    let request = session
        .request_stream_snapshot()
        .expect("queue the stream snapshot read");
    wait_for(session, Duration::from_secs(10), |kind, id| {
        matches!(kind, EventKind::StreamSnapshotReady) && id == request
    });
    session
        .take_stream_snapshot(request)
        .expect("the completed snapshot is stored for its request")
}

#[test]
#[ignore = "requires checksum-verified libmpv"]
fn the_stream_snapshot_reads_the_playing_clip() {
    let library_path = env::var_os("KATTRI_TEST_LIBMPV")
        .expect("set KATTRI_TEST_LIBMPV to the verified libmpv library path");
    let library = MpvLibrary::load_from_path(library_path).expect("load selected libmpv");
    let session =
        MpvSession::new_software_null(&library).expect("initialize software/null session");

    // Nothing is loaded yet: every property is unavailable, and the read still completes.
    let idle = snapshot(&session);
    assert_eq!(available_text(&idle.video_codec), None);

    session
        .command(&["set", "pause", "no"])
        .expect("start software playback");
    session
        .command(&["loadfile", &fixture_path(), "replace"])
        .expect("queue the clip");
    wait_for(&session, Duration::from_secs(20), |kind, _| {
        matches!(kind, EventKind::PlaybackRestart)
    });

    // Decoder and output properties settle after the first frames.
    let deadline = Instant::now() + Duration::from_secs(10);
    let playing = loop {
        let current = snapshot(&session);
        if available_text(&current.pixel_format).is_some()
            && available_text(&current.audio_channel_count).is_some()
        {
            break current;
        }
        assert!(
            Instant::now() < deadline,
            "video and audio params never appeared"
        );
        thread::sleep(Duration::from_millis(100));
    };

    println!("{playing:#?}");
    if fixture_path().ends_with("clip-h264-aac.mkv") {
        // Known properties of the committed fixture.
        assert_eq!(available_text(&playing.container), Some("mkv"));
        assert_eq!(available_text(&playing.video_codec), Some("h264"));
        assert_eq!(available_text(&playing.width), Some("160"));
        assert_eq!(available_text(&playing.height), Some("90"));
        assert_eq!(available_text(&playing.pixel_format), Some("yuv420p"));
        assert_eq!(available_text(&playing.audio_codec), Some("aac"));
        assert_eq!(available_text(&playing.audio_channel_count), Some("2"));
        let fps: f64 = available_text(&playing.frame_rate)
            .and_then(|text| text.parse().ok())
            .expect("container-fps is a number");
        assert!((fps - 10.0).abs() < 0.01, "frame rate {fps}");
    }
    // The software/null session decodes on the CPU and plays to the null output.
    assert_eq!(available_text(&playing.hardware_decoder), Some("no"));
    assert_eq!(available_text(&playing.audio_output), Some("null"));
    // No subtitle track is selected.
    assert_eq!(available_text(&playing.subtitle_codec), None);

    // The same values feed the media info panel without leaking a path.
    let values = playing.stream_values();
    let info = kattri_client_core::media_info::MediaInfo::from_input(
        &kattri_client_core::media_info_mpv::media_input_from_mpv(&values, &[], &[]),
    );
    let text = info.diagnostics_text();
    println!("{text}");
    assert!(text.contains("Hardware decode: Software"));
    assert!(!text.contains(&fixture_path()));
}
