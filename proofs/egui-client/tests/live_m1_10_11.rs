//! Opt-in read-only validation of search and profile-driven playback negotiation.
//!
//! The search term is derived from one live title but is never printed. Playback
//! validation stops at PlaybackInfo: no media URL is fetched and no playback or
//! reporting endpoint is called.

use std::{collections::HashSet, env};

use jellyfin_api::{
    ClientConfig, ItemQuery, ItemSortBy, ItemSortOrder, JellyfinClient, PlaybackInfoRequest,
    PlaybackRequestOverrides, baseline_device_profile,
};
use kattri_client_core::{
    playback_plan::{PlaybackMethod, select_playback_info_source_for_profile},
    search::group_search_results,
};
use zeroize::Zeroizing;

const DEFAULT_SERVER: &str = "http://127.0.0.1:8096/";
const SEARCH_TYPES: [&str; 8] = [
    "Movie",
    "Series",
    "Episode",
    "Audio",
    "MusicAlbum",
    "Playlist",
    "Photo",
    "Person",
];
const PLAYBACK_SAMPLE_LIMIT: usize = 8;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
}

fn video_codec<'a>(source: &'a jellyfin_api::MediaSourceInfo, kind: &str) -> &'a str {
    source
        .media_streams
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|stream| stream.stream_type.as_deref() == Some(kind))
        .and_then(|stream| stream.codec.as_deref())
        .unwrap_or("none")
}

fn direct_stream_probe_profile(
    profile: &jellyfin_api::DeviceProfile,
    source: &jellyfin_api::MediaSourceInfo,
) -> Option<jellyfin_api::DeviceProfile> {
    let container = source.container.as_deref()?;
    let mut narrowed = profile.clone();
    let mut changed = false;
    for entry in narrowed.direct_play_profiles.as_mut()?.iter_mut() {
        if entry.profile_type.as_deref() != Some("Video") {
            continue;
        }
        let containers = entry.container.as_deref()?;
        let allowed = containers
            .split(',')
            .map(str::trim)
            .filter(|candidate| !candidate.eq_ignore_ascii_case(container))
            .collect::<Vec<_>>();
        if allowed.len() != containers.split(',').count() {
            entry.container = Some(allowed.join(","));
            changed = true;
        }
    }
    changed.then_some(narrowed)
}

#[test]
#[ignore = "requires explicit access to an existing Jellyfin server"]
fn live_search_and_profile_playback_negotiation_are_read_only() {
    runtime().block_on(async {
        let base = env::var("JELLYFIN_TEST_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_owned());
        let config = ClientConfig::new(
            &base,
            "Kattri validator",
            "validation",
            "kattri-validation-m1-10-11",
            "1",
        )
        .expect("valid server configuration");
        let client = JellyfinClient::new(config).expect("HTTP client");

        let public_info = client
            .public_system_info()
            .await
            .expect("public info request failed");
        println!(
            "M1-10/11 public_info: PASS server_version={}",
            public_info.version.as_deref().unwrap_or("unknown")
        );

        let username = env::var("JELLYFIN_TEST_USER")
            .ok()
            .filter(|value| !value.is_empty())
            .map(Zeroizing::new);
        let password = env::var("JELLYFIN_TEST_PASSWORD")
            .ok()
            .filter(|value| !value.is_empty())
            .map(Zeroizing::new);
        let (Some(username), Some(password)) = (username, password) else {
            println!("M1-10/11 authenticated checks: NOT RUN (credentials unavailable)");
            return;
        };

        let session = match client.authenticate(&username, &password).await {
            Ok(session) => session,
            Err(error) => {
                println!("M1-10/11 authenticate: FAIL class={:?}", error.classify());
                panic!("authentication failed; response details suppressed");
            }
        };
        println!("M1-10/11 authenticate: PASS");

        let latest = client
            .latest_items(&session, None, 12)
            .await
            .expect("latest items request failed");
        let anchor = latest
            .iter()
            .find(|item| {
                matches!(
                    item.item_type.as_deref(),
                    Some("Movie" | "Series" | "Episode" | "Audio" | "Person")
                ) && item.name.split_whitespace().any(|word| word.chars().count() >= 2)
            })
            .expect("server returned no searchable M1 item");
        let search_term = anchor
            .name
            .split_whitespace()
            .find(|word| word.chars().count() >= 2)
            .expect("anchor has a searchable word");
        let anchor_id = anchor.id.clone();
        let search_results = client
            .list_items(
                &session,
                &ItemQuery {
                    recursive: true,
                    limit: 30,
                    search_term: Some(search_term.to_owned()),
                    include_item_types: Some(SEARCH_TYPES.iter().map(|kind| (*kind).to_owned()).collect()),
                    sort_by: Some(vec![ItemSortBy::SortName]),
                    sort_order: Some(vec![ItemSortOrder::Ascending]),
                    enable_total_record_count: Some(true),
                    name_starts_with: None,
                    name_less_than: None,
                    studio_ids: None,
                    ..ItemQuery::default()
                },
            )
            .await
            .expect("search request failed");
        assert!(
            search_results.items.iter().any(|item| item.id == anchor_id),
            "search did not return the title used to derive its private probe term"
        );
        let groups = group_search_results(&search_results.items);
        println!(
            "M1-10 search: PASS results={} groups={}",
            search_results.items.len(),
            groups.len()
        );
        for group in groups {
            println!("  group {:?}: {}", group.kind, group.items.len());
        }

        match client
            .search_live_tv_channels(&session, search_term, 0, 10)
            .await
        {
            Ok(page) => println!(
                "M3-17 channel search: PASS returned={} total={}",
                page.items.len(),
                page.total_record_count.unwrap_or(0)
            ),
            Err(jellyfin_api::SearchError::Api(error))
                if error.classify() == jellyfin_api::ErrorClass::NotFound =>
            {
                println!("M3-17 channel search: NOT CONFIGURED (channel group hidden)");
            }
            Err(jellyfin_api::SearchError::Api(error)) => panic!(
                "M3-17 channel search failed: class={:?}",
                error.classify()
            ),
            Err(jellyfin_api::SearchError::EmptyQuery) => {
                panic!("M3-17 channel search refused a non-empty query")
            }
        }

        let mut candidate_ids = Vec::new();
        let mut seen_ids = HashSet::new();
        for item in latest.iter().chain(search_results.items.iter()) {
            if matches!(item.item_type.as_deref(), Some("Movie" | "Episode"))
                && seen_ids.insert(item.id.as_str())
            {
                candidate_ids.push(item.id.as_str());
            }
            if candidate_ids.len() >= PLAYBACK_SAMPLE_LIMIT {
                break;
            }
        }
        assert!(
            !candidate_ids.is_empty(),
            "server returned no movie or episode for PlaybackInfo"
        );

        let profile = baseline_device_profile();
        let request = PlaybackInfoRequest::for_mpv_profile(
            profile.clone(),
            PlaybackRequestOverrides::default(),
        );
        let mut successful_calls = 0usize;
        let mut unavailable_calls = 0usize;
        let mut selected = [0usize; 3];
        let mut direct_stream_probes = 0usize;
        let mut direct_stream_offers = 0usize;
        let mut direct_stream_probe_errors = 0usize;
        for (index, item_id) in candidate_ids.iter().enumerate() {
            match client.playback_info(&session, item_id, &request).await {
                Ok(response) => {
                    successful_calls += 1;
                    match select_playback_info_source_for_profile(&response, &profile) {
                        Some(selection) => {
                            let method_index = match selection.method {
                                PlaybackMethod::DirectPlay => 0,
                                PlaybackMethod::DirectStream => 1,
                                PlaybackMethod::Transcode => 2,
                            };
                            selected[method_index] += 1;
                            println!(
                                "M1-11 sample[{}]: method={:?} container={} video={} audio={}",
                                index + 1,
                                selection.method,
                                selection
                                    .media_source
                                    .container
                                    .as_deref()
                                    .unwrap_or("unknown")
                                    .to_ascii_lowercase(),
                                video_codec(selection.media_source, "Video").to_ascii_lowercase(),
                                video_codec(selection.media_source, "Audio").to_ascii_lowercase(),
                            );
                            if selection.method == PlaybackMethod::DirectPlay
                                && let Some(probe_profile) =
                                    direct_stream_probe_profile(&profile, selection.media_source)
                            {
                                direct_stream_probes += 1;
                                let probe_request = PlaybackInfoRequest::for_mpv_profile(
                                    probe_profile.clone(),
                                    PlaybackRequestOverrides::default(),
                                );
                                match client
                                    .playback_info(&session, item_id, &probe_request)
                                    .await
                                {
                                    Ok(probe) => {
                                        if select_playback_info_source_for_profile(
                                            &probe,
                                            &probe_profile,
                                        )
                                        .is_some_and(|candidate| {
                                            candidate.method == PlaybackMethod::DirectStream
                                        }) {
                                            direct_stream_offers += 1;
                                        }
                                    }
                                    Err(_) => direct_stream_probe_errors += 1,
                                }
                            }
                        }
                        None => println!(
                            "M1-11 sample[{}]: NO_PROFILE_SUPPORTED_SOURCE sources={}",
                            index + 1,
                            response.media_sources.len()
                        ),
                    }
                }
                Err(error) if error.classify() == jellyfin_api::ErrorClass::ServerUnavailable => {
                    unavailable_calls += 1;
                    println!(
                        "M1-11 sample[{}]: UNAVAILABLE class={:?}",
                        index + 1,
                        error.classify()
                    );
                }
                Err(error) => {
                    println!(
                        "M1-11 sample[{}]: FAIL class={:?}",
                        index + 1,
                        error.classify()
                    );
                    panic!("PlaybackInfo request failed; details suppressed");
                }
            }
        }
        assert!(successful_calls > 0, "all PlaybackInfo samples failed");
        assert!(selected.iter().sum::<usize>() > 0, "no source matched the MPV profile");
        println!(
            "M1-11 summary: successful={} unavailable={} direct_play={} direct_stream={} transcode={}",
            successful_calls,
            unavailable_calls,
            selected[0],
            selected[1],
            selected[2]
        );
        println!(
            "M1-11 DirectStream profile probe: tried={} offered={} failed={}",
            direct_stream_probes, direct_stream_offers, direct_stream_probe_errors
        );
        println!("M1-10/11 read-only summary: PASS");
    });
}
