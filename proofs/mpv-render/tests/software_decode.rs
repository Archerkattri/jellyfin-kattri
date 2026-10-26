use std::{
    env, thread,
    time::{Duration, Instant},
};

use mpv_render_proof::{
    CapabilitySnapshot, CapabilityValue, EndFileReason, EventKind, MpvLibrary, MpvSession,
    PlaybackProperty, PlaybackPropertyValue,
};

#[test]
#[ignore = "requires checksum-verified libmpv"]
fn missing_local_file_reports_the_actual_mpv_end_error() {
    let library_path = env::var_os("KATTRI_TEST_LIBMPV").expect("set KATTRI_TEST_LIBMPV");
    let library = MpvLibrary::load_from_path(library_path).expect("load selected libmpv");
    let session =
        MpvSession::new_software_null(&library).expect("initialize software/null session");
    let missing = env::temp_dir()
        .join(format!("kattri-missing-mpv-test-{}", std::process::id()))
        .join("missing.mp4");
    assert!(!missing.exists(), "test path unexpectedly exists");
    let path = missing.to_str().expect("Unicode temp path");
    session
        .command(&["loadfile", path, "replace"])
        .expect("queue missing file");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(event) = session.try_event()
            && let EventKind::EndFile(reason) = event.kind
        {
            assert_eq!(reason, EndFileReason::Error);
            assert!(
                event.error < 0,
                "missing file must carry a negative libmpv error"
            );
            println!("missing_file_end_error={}", event.error);
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("libmpv did not report the missing-file failure");
}

#[test]
#[ignore = "requires checksum-verified libmpv and a local test-media path"]
fn decodes_thirty_seconds_with_software_and_null_outputs() {
    let library_path = env::var_os("KATTRI_TEST_LIBMPV")
        .expect("set KATTRI_TEST_LIBMPV to the verified libmpv DLL path");
    let media_path = env::var("KATTRI_TEST_MEDIA").expect("set KATTRI_TEST_MEDIA to a local clip");
    let library = MpvLibrary::load_from_path(library_path).expect("load selected libmpv");
    let session =
        MpvSession::new_software_null(&library).expect("initialize software/null session");
    let started = Instant::now();
    session
        .command(&["set", "pause", "no"])
        .expect("queue unpaused software playback");
    session
        .command(&["loadfile", &media_path, "replace"])
        .expect("queue local sample");

    let deadline = started + Duration::from_secs(60);
    let mut file_loaded = false;
    let mut playback_restarted = false;
    let mut end_seen = false;
    let mut capability_request = None;
    let mut duration_request = None;
    let mut capabilities: Option<CapabilitySnapshot> = None;
    let mut duration = None;
    let mut decode_started = false;

    while Instant::now() < deadline {
        if let Some(event) = session.try_event() {
            match event.kind {
                EventKind::FileLoaded if !file_loaded => {
                    file_loaded = true;
                }
                EventKind::PlaybackRestart => playback_restarted = true,
                EventKind::EndFile(_) => end_seen = true,
                EventKind::CapabilitySnapshotReady
                    if Some(event.request_id) == capability_request =>
                {
                    capabilities = session.take_capability_snapshot(event.request_id);
                }
                EventKind::PropertyReadReply {
                    property: PlaybackProperty::Duration,
                    value: PlaybackPropertyValue::Duration(value),
                } if Some(event.request_id) == duration_request => {
                    duration = value;
                }
                _ => {}
            }
        }
        if file_loaded && capability_request.is_none() {
            capability_request = Some(
                session
                    .request_capability_snapshot()
                    .expect("queue active capability snapshot"),
            );
            duration_request = Some(
                session
                    .request_property(PlaybackProperty::Duration)
                    .expect("queue media duration"),
            );
        }
        if capabilities.is_some() && duration.is_some() {
            decode_started = true;
        }
        if end_seen && decode_started {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    assert!(file_loaded, "libmpv did not load the local media");
    assert!(playback_restarted, "libmpv did not start playback");
    assert!(
        decode_started,
        "libmpv did not provide a loaded-file snapshot"
    );
    assert!(
        end_seen,
        "software decode did not reach the 30-second end point"
    );
    let duration = duration.expect("libmpv reported media duration");
    assert!(
        duration >= 29.0,
        "test clip is shorter than the published ~30-second sample"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(30),
        "software decode did not run for at least 30 seconds"
    );
    let capabilities = capabilities.expect("runtime capability snapshot available");

    assert_eq!(
        capabilities.active_hwdec,
        CapabilityValue::Available("no".to_owned()),
        "software-only session must not use hardware decoding"
    );

    println!("software_decode_seconds={duration:.3}");
    println!(
        "elapsed_wall_seconds={:.3}",
        started.elapsed().as_secs_f64()
    );
    println!("video_codec=h264 (sample metadata; decoder name unavailable)");
    println!(
        "hwdec_current={};hwdec_interop={}",
        match &capabilities.active_hwdec {
            CapabilityValue::Available(value) => value,
            CapabilityValue::Unavailable(_) => "unavailable",
        },
        match &capabilities.hwdec_interop {
            CapabilityValue::Available(value) => value,
            CapabilityValue::Unavailable(_) => "unavailable",
        }
    );
    println!("presentation_metrics=not_applicable_for_vo_null");
}
