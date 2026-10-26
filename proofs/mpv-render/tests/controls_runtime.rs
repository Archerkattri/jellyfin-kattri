use std::{
    env, thread,
    time::{Duration, Instant},
};

use mpv_render_proof::{
    EventKind, MpvLibrary, MpvSession, PlaybackProperty, PlaybackPropertyValue,
};

#[test]
#[ignore = "requires checksum-verified libmpv and a local test-media path"]
fn playback_controls_change_observed_libmpv_state() {
    let library_path = env::var_os("KATTRI_TEST_LIBMPV")
        .expect("set KATTRI_TEST_LIBMPV to the verified libmpv DLL path");
    let media_path = env::var("KATTRI_TEST_MEDIA").expect("set KATTRI_TEST_MEDIA to a local clip");
    let library = MpvLibrary::load_from_path(library_path).expect("load selected libmpv");
    let session =
        MpvSession::new_software_null(&library).expect("initialize software/null session");
    let position_observation = session
        .observe_property(PlaybackProperty::TimePosition)
        .expect("observe playback position");
    let pause_observation = session
        .observe_property(PlaybackProperty::Paused)
        .expect("observe pause state");
    let speed_observation = session
        .observe_property(PlaybackProperty::Speed)
        .expect("observe playback speed");

    session
        .command(&["set", "pause", "no"])
        .expect("start software playback");
    session
        .command(&["loadfile", &media_path, "replace"])
        .expect("queue local sample");

    let load_deadline = Instant::now() + Duration::from_secs(20);
    let mut file_loaded = false;
    while Instant::now() < load_deadline && !file_loaded {
        if let Some(event) = session.try_event() {
            file_loaded = matches!(event.kind, EventKind::FileLoaded);
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(file_loaded, "libmpv did not load the local media");

    session.set_paused(true).expect("pause command accepted");
    session
        .set_playback_speed(1.5)
        .expect("speed command accepted");
    session
        .seek_absolute(8.0)
        .expect("absolute seek command accepted");

    let controls_deadline = Instant::now() + Duration::from_secs(10);
    let mut observed_position = None;
    let mut observed_paused = None;
    let mut observed_speed = None;
    while Instant::now() < controls_deadline {
        if let Some(event) = session.try_event()
            && let EventKind::PropertyChange(change) = event.kind
        {
            match (change.observation_id, change.property, change.value) {
                (
                    id,
                    PlaybackProperty::TimePosition,
                    PlaybackPropertyValue::TimePosition(value),
                ) if id == position_observation => observed_position = value,
                (id, PlaybackProperty::Paused, PlaybackPropertyValue::Paused(value))
                    if id == pause_observation =>
                {
                    observed_paused = value
                }
                (id, PlaybackProperty::Speed, PlaybackPropertyValue::Speed(value))
                    if id == speed_observation =>
                {
                    observed_speed = value
                }
                _ => {}
            }
        }

        if observed_paused == Some(true)
            && observed_speed.is_some_and(|speed| (speed - 1.5).abs() < 0.001)
            && observed_position.is_some_and(|position| (position - 8.0).abs() < 1.0)
        {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(observed_paused, Some(true), "pause state did not update");
    assert!(
        observed_speed.is_some_and(|speed| (speed - 1.5).abs() < 0.001),
        "speed property did not report 1.5x: {observed_speed:?}"
    );
    assert!(
        observed_position.is_some_and(|position| (position - 8.0).abs() < 1.0),
        "position property did not approach the requested seek: {observed_position:?}"
    );

    session
        .set_playback_speed(1.0)
        .expect("reset playback speed");
    session.set_paused(false).expect("resume playback");
    session
        .stop_playback()
        .expect("stop command queues speed reset before stop");
    println!(
        "control_smoke=passed;paused={observed_paused:?};speed={observed_speed:?};position={observed_position:?}"
    );
}
