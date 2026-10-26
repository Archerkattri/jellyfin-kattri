use std::{
    env, thread,
    time::{Duration, Instant},
};

use mpv_render_proof::{CapabilitySnapshot, CapabilityValue, EventKind, MpvLibrary, MpvSession};

#[test]
#[ignore = "requires an explicitly selected, checksum-verified libmpv runtime"]
fn reads_capability_properties_from_real_libmpv() {
    let library_path = env::var_os("KATTRI_TEST_LIBMPV")
        .expect("set KATTRI_TEST_LIBMPV to the verified libmpv DLL path");
    let library = MpvLibrary::load_from_path(library_path).expect("load selected libmpv");
    let session = MpvSession::new(&library).expect("initialize libmpv");
    let request_id = session
        .request_capability_snapshot()
        .expect("queue capability probe");
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        if let Some(event) = session.try_event()
            && event.request_id == request_id
            && event.kind == EventKind::CapabilitySnapshotReady
        {
            break;
        }
        assert!(Instant::now() < deadline, "capability probe timed out");
        thread::sleep(Duration::from_millis(5));
    }

    let snapshot = session
        .take_capability_snapshot(request_id)
        .expect("probe result must be retained");
    print_sanitized(&snapshot);
}

fn print_sanitized(snapshot: &CapabilitySnapshot) {
    println!("mpv-version={}", scalar(&snapshot.mpv_version));
    println!("ffmpeg-version={}", scalar(&snapshot.ffmpeg_version));
    println!("decoder-inventory={}", list(&snapshot.decoders));
    println!("demuxer-inventory={}", list(&snapshot.demuxers));
    println!("configured-hwdec={}", scalar(&snapshot.configured_hwdec));
    println!("active-hwdec-at-idle={}", scalar(&snapshot.active_hwdec));
    println!("hwdec-interop={}", scalar(&snapshot.hwdec_interop));
    println!(
        "audio-device-count={}",
        match &snapshot.audio_devices {
            CapabilityValue::Available(devices) => devices.len().to_string(),
            CapabilityValue::Unavailable(_) => "unavailable".to_owned(),
        }
    );
}

fn scalar(value: &CapabilityValue<String>) -> &str {
    match value {
        CapabilityValue::Available(value) => value,
        CapabilityValue::Unavailable(_) => "unavailable",
    }
}

fn list(value: &CapabilityValue<Vec<String>>) -> String {
    match value {
        CapabilityValue::Available(values) => values.join(","),
        CapabilityValue::Unavailable(_) => "unavailable".to_owned(),
    }
}
