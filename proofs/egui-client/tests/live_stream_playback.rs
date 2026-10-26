//! Owner-authorized stream-only validation; deliberately no reporting client.
//! Only aggregate timings/methods are printed, never media identifiers or URLs.

use std::{
    env, thread,
    time::{Duration, Instant},
};

use jellyfin_api::{
    ClientConfig, ItemQuery, JellyfinClient, PlayMethod, PlaybackInfoRequest,
    PlaybackRequestOverrides, PlaybackSourceRequestOptions, baseline_device_profile,
};
use jellyfin_playback_relay::start_relay;
use kattri_client_core::playback_plan::{PlaybackMethod, select_playback_info_source_for_profile};
use mpv_render_proof::{
    EventKind, MpvLibrary, MpvSession, PlaybackProperty, PlaybackPropertyValue,
};
use zeroize::Zeroizing;

struct RunMetrics {
    startup_seconds: f64,
    elapsed_seconds: f64,
    position_seconds: f64,
    longest_progress_gap_seconds: f64,
}

fn decode_stream(library: &MpvLibrary, url: &str, seconds: u64) -> RunMetrics {
    let player = MpvSession::new_software_null(library).expect("initialize null-output player");
    player
        .observe_property(PlaybackProperty::TimePosition)
        .expect("observe progress");
    player
        .command(&["set", "pause", "no"])
        .expect("unpause local player");
    let started = Instant::now();
    player
        .command(&["loadfile", url, "replace"])
        .expect("queue loopback stream");
    let mut first_progress = None;
    let mut last_progress = started;
    let mut longest_gap = Duration::ZERO;
    let mut position = 0.0_f64;
    let mut ended = false;
    let mut next_summary = Duration::from_secs(30);
    while started.elapsed() < Duration::from_secs(seconds + 30) {
        if let Some(event) = player.try_event() {
            assert!(
                event.error >= 0,
                "libmpv returned an error; details suppressed"
            );
            match event.kind {
                EventKind::PropertyChange(change) => {
                    if let PlaybackPropertyValue::TimePosition(Some(value)) = change.value
                        && value > position + 0.001
                    {
                        let now = Instant::now();
                        if first_progress.is_some() {
                            longest_gap = longest_gap.max(now.duration_since(last_progress));
                        } else {
                            first_progress = Some(now);
                        }
                        last_progress = now;
                        position = value;
                    }
                }
                EventKind::EndFile { .. } | EventKind::Shutdown => {
                    ended = true;
                    break;
                }
                _ => {}
            }
        }
        if let Some(first) = first_progress {
            longest_gap = longest_gap.max(last_progress.elapsed());
            if first.elapsed() >= next_summary {
                println!(
                    "stream_progress wall_seconds={:.3} position_seconds={position:.3}",
                    first.elapsed().as_secs_f64()
                );
                next_summary += Duration::from_secs(30);
            }
            if first.elapsed() >= Duration::from_secs(seconds) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    let elapsed = first_progress
        .map(|first| first.elapsed().as_secs_f64())
        .unwrap_or_default();
    player
        .stop_playback()
        .expect("stop local decoding without server reporting");
    assert!(
        !ended,
        "stream ended before the observation window; details suppressed"
    );
    let first = first_progress.expect("no advancing media position within deadline");
    assert!(elapsed >= seconds as f64, "observation window incomplete");
    RunMetrics {
        startup_seconds: first.duration_since(started).as_secs_f64(),
        elapsed_seconds: elapsed,
        position_seconds: position,
        longest_progress_gap_seconds: longest_gap.as_secs_f64(),
    }
}

fn direct_stream_probe_profile(
    baseline: &jellyfin_api::DeviceProfile,
    source: &jellyfin_api::MediaSourceInfo,
) -> Option<jellyfin_api::DeviceProfile> {
    let source_container = source.container.as_deref()?;
    let mut profile = baseline.clone();
    let mut matched_video = false;
    for entry in profile.direct_play_profiles.as_mut()?.iter_mut() {
        if entry.profile_type.as_deref() != Some("Video") {
            continue;
        }
        let containers = entry.container.as_deref()?;
        let filtered = containers
            .split(',')
            .map(str::trim)
            .filter(|container| !container.eq_ignore_ascii_case(source_container))
            .collect::<Vec<_>>();
        if filtered.len() != containers.split(',').count() {
            entry.container = Some(filtered.join(","));
            matched_video = true;
        }
    }
    matched_video.then_some(profile)
}

#[test]
#[ignore = "requires owner-approved streaming, saved credentials and verified libmpv"]
fn stream_only_relay_playback_never_reports_sessions() {
    assert_eq!(
        env::var("KATTRI_ALLOW_LIVE_PLAYBACK").as_deref(),
        Ok("1"),
        "explicit streaming opt-in required"
    );
    let library = MpvLibrary::load_from_path(
        env::var_os("KATTRI_TEST_LIBMPV").expect("verified libmpv path required"),
    )
    .expect("load verified libmpv");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let base = env::var("JELLYFIN_TEST_URL").unwrap_or_else(|_| "http://127.0.0.1:8096/".to_owned());
        let client = JellyfinClient::new(ClientConfig::new(&base, "Kattri stream validator", "validation", "kattri-stream-m1-11", "1")
            .expect("server configuration")).expect("HTTP client");
        let username = Zeroizing::new(env::var("JELLYFIN_TEST_USER").expect("test account required"));
        let password = Zeroizing::new(env::var("JELLYFIN_TEST_PASSWORD").expect("test password required"));
        let session = client.authenticate(&username, &password).await.unwrap_or_else(|_| panic!("authentication failed; details suppressed"));
        let items = client.list_items(&session, &ItemQuery { recursive: true, limit: 50,
            include_item_types: Some(vec!["Movie".to_owned()]), ..ItemQuery::default() })
            .await.unwrap_or_else(|_| panic!("movie listing failed; details suppressed"));
        let baseline = baseline_device_profile();
        let mut chosen = None;
        for item in items.items.iter().take(12) {
            let request = PlaybackInfoRequest::for_mpv_profile(baseline.clone(), PlaybackRequestOverrides::default());
            let Ok(response) = client.playback_info(&session, &item.id, &request).await else { continue; };
            if let Some(selection) = select_playback_info_source_for_profile(&response, &baseline)
                && selection.method == PlaybackMethod::DirectPlay
                && selection.media_source.run_time_ticks.is_some_and(|ticks| ticks > 660 * 10_000_000)
            { chosen = Some((item.id.clone(), response)); break; }
        }
        let (item_id, direct_response) = chosen.expect("no long profile-compatible movie found; nothing streamed");
        let direct_selection = select_playback_info_source_for_profile(&direct_response, &baseline)
            .expect("selected item no longer matches the baseline profile");
        let direct_stream_profile = direct_stream_probe_profile(&baseline, direct_selection.media_source);
        let direct_stream_response = if let Some(profile) = direct_stream_profile.as_ref() {
            let request = PlaybackInfoRequest::for_mpv_profile(
                profile.clone(),
                PlaybackRequestOverrides::default(),
            );
            match client.playback_info(&session, &item_id, &request).await {
                Ok(response) => Some(response),
                Err(_) => {
                    println!("stream_negotiation method=DirectStream outcome=unavailable");
                    None
                }
            }
        } else {
            None
        };
        // Same single item, deliberately outside the test profile's direct-play entries.
        let mut restricted = baseline.clone();
        restricted.direct_play_profiles = Some(Vec::new());
        let request = PlaybackInfoRequest::for_mpv_profile(restricted.clone(), PlaybackRequestOverrides {
            max_streaming_bitrate: Some(2_000_000), force_transcode: true });
        let transcode_response = client.playback_info(&session, &item_id, &request).await
            .unwrap_or_else(|_| panic!("transcode negotiation failed; details suppressed"));
        let mut runs = vec![(&transcode_response, &restricted, PlaybackMethod::Transcode, 30)];
        if let (Some(response), Some(profile)) =
            (direct_stream_response.as_ref(), direct_stream_profile.as_ref())
        {
            let selection = select_playback_info_source_for_profile(response, profile);
            if selection.is_some_and(|selected| selected.method == PlaybackMethod::DirectStream) {
                runs.push((response, profile, PlaybackMethod::DirectStream, 30));
            } else {
                println!("stream_negotiation method=DirectStream outcome=not_offered");
            }
        }
        runs.push((&direct_response, &baseline, PlaybackMethod::DirectPlay, 600));
        let mut metrics = Vec::new();
        for (response, profile, expected, seconds) in runs {
            let selection = select_playback_info_source_for_profile(response, profile).expect("no profile-compatible source");
            assert_eq!(selection.method, expected, "negotiated method mismatch");
            let source = selection.media_source;
            let method = match expected { PlaybackMethod::DirectPlay => PlayMethod::DirectPlay,
                PlaybackMethod::DirectStream => PlayMethod::DirectStream, PlaybackMethod::Transcode => PlayMethod::Transcode };
            let request = client.build_playback_source_request(&session, PlaybackSourceRequestOptions {
                item_id: &item_id, media_source: source, play_method: method,
                play_session_id: response.play_session_id.as_deref(),
                audio_stream_index: source.default_audio_stream_index,
                subtitle_stream_index: source.default_subtitle_stream_index, start_time_ticks: None,
            }).expect("safe source request");
            let (url, relay) = start_relay(client.server_origin().as_str(), request).expect("start isolated relay");
            println!("stream_mode method={expected:?} player=software-null hwdec=no vo=null ao=null");
            assert!(url.as_str().starts_with("http://127.0.0.1:"), "only loopback reaches libmpv");
            println!("stream_start method={expected:?} duration_seconds={seconds} reporting=disabled watched_updates=disabled");
            let run = decode_stream(&library, url.as_str(), seconds);
            relay.shutdown().expect("stop relay");
            println!("stream_result method={expected:?} startup_seconds={:.3} elapsed_seconds={:.3} position_seconds={:.3} longest_progress_gap_seconds={:.3}",
                run.startup_seconds, run.elapsed_seconds, run.position_seconds, run.longest_progress_gap_seconds);
            metrics.push((expected, run));
        }
        for (method, run) in metrics {
            assert!(run.longest_progress_gap_seconds < 2.0, "observed playback-progress stall");
            if method == PlaybackMethod::DirectPlay { assert!(run.startup_seconds <= 3.0, "direct-play startup exceeded three seconds"); }
        }
        println!("stream_only_summary PASS session_reporting_calls=0 watched_state_calls=0 presentation=unvalidated");
    });
}
