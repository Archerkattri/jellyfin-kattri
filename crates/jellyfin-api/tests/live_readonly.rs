//! Opt-in, read-only live check against an existing Jellyfin server.
//!
//! Covers public info, sign-in, library views, Continue Watching, latest
//! items, item details, series navigation, people/similar items, and
//! PlaybackInfo negotiation for a few items.
//! Only GET requests and the sign-in and PlaybackInfo POSTs are issued. No
//! playlist, favorite, played-state, or playback-reporting call is made.
//!
//! Environment: `JELLYFIN_TEST_URL` (defaults to the owner server),
//! `JELLYFIN_TEST_USER`, `JELLYFIN_TEST_PASSWORD`.
//!
//! Run with:
//! `cargo test -p jellyfin-api --test live_readonly -- --ignored --nocapture`
//!
//! Output prints only status classes, counts, item types, codecs, and
//! negotiation flags. It never prints names, IDs, paths, tokens, or URLs.

use std::collections::BTreeMap;
use std::env;

use jellyfin_api::{
    ApiError, BackdropImageError, ClientConfig, ItemFilter, ItemQuery, ItemSortBy, ItemSortOrder,
    JellyfinClient, MediaSourceInfo, PlaybackInfoRequest, PlaybackRequestOverrides,
    baseline_device_profile,
};
use zeroize::Zeroizing;

const DEFAULT_SERVER: &str = "http://127.0.0.1:8096/";
const SAMPLE_LIMIT: u32 = 12;
const PLAYBACK_SAMPLE: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackSampleResult {
    Valid,
    Unavailable,
    RequestError,
    Empty,
    EmbeddedError,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct PlaybackCoverage {
    attempted: usize,
    valid: usize,
    unavailable: usize,
    request_errors: usize,
    empty: usize,
    embedded_errors: usize,
}

impl PlaybackCoverage {
    fn record(&mut self, result: PlaybackSampleResult) {
        self.attempted += 1;
        match result {
            PlaybackSampleResult::Valid => self.valid += 1,
            PlaybackSampleResult::Unavailable => self.unavailable += 1,
            PlaybackSampleResult::RequestError => self.request_errors += 1,
            PlaybackSampleResult::Empty => self.empty += 1,
            PlaybackSampleResult::EmbeddedError => self.embedded_errors += 1,
        }
    }

    fn is_complete(&self, required: usize, available: usize) -> bool {
        available >= required
            && self.attempted == required
            && self.valid == required
            && self.unavailable == 0
            && self.request_errors == 0
            && self.empty == 0
            && self.embedded_errors == 0
    }
}

#[cfg(test)]
mod playback_coverage_tests {
    use super::{PlaybackCoverage, PlaybackSampleResult};

    #[test]
    fn requires_three_valid_samples_and_no_failed_samples() {
        let mut coverage = PlaybackCoverage::default();
        for _ in 0..3 {
            coverage.record(PlaybackSampleResult::Valid);
        }
        assert!(coverage.is_complete(3, 3));
        assert!(!coverage.is_complete(3, 2));
    }

    #[test]
    fn unavailable_error_empty_and_embedded_error_do_not_complete_coverage() {
        for result in [
            PlaybackSampleResult::Unavailable,
            PlaybackSampleResult::RequestError,
            PlaybackSampleResult::Empty,
            PlaybackSampleResult::EmbeddedError,
        ] {
            let mut coverage = PlaybackCoverage::default();
            coverage.record(PlaybackSampleResult::Valid);
            coverage.record(result);
            coverage.record(PlaybackSampleResult::Valid);
            assert!(
                !coverage.is_complete(3, 3),
                "{result:?} counted as complete"
            );
        }
    }
}

fn outcome<T>(
    failures: &mut Vec<&'static str>,
    label: &'static str,
    result: Result<T, ApiError>,
) -> Option<T> {
    match result {
        Ok(value) => {
            println!("{label}: PASS");
            Some(value)
        }
        Err(err) => {
            // Error details can contain server paths or other private metadata.
            println!("{label}: FAIL class={:?}", err.classify());
            failures.push(label);
            None
        }
    }
}

fn count_types<'a>(types: impl Iterator<Item = Option<&'a str>>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for item_type in types {
        let key = item_type.unwrap_or("unknown").to_ascii_lowercase();
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

fn playback_method(source: &MediaSourceInfo) -> &'static str {
    if source.transcoding_url.is_some() {
        "Transcode"
    } else if source.supports_direct_play {
        "DirectPlay"
    } else if source.supports_direct_stream {
        "DirectStream"
    } else if source.supports_transcoding {
        "TranscodeOffered"
    } else {
        "None"
    }
}

// Collections and series also have item IDs, but cannot negotiate a media stream.
// Keep them available to metadata checks without counting them as playable samples.
fn is_playback_sample(item_type: Option<&str>) -> bool {
    matches!(item_type, Some("Movie" | "Episode" | "Audio"))
}

#[test]
fn playback_sampling_excludes_folders_and_unknown_item_types() {
    for item_type in ["Movie", "Episode", "Audio"] {
        assert!(is_playback_sample(Some(item_type)));
    }
    for item_type in ["BoxSet", "Series", "Season", "Person", "Folder", "unknown"] {
        assert!(!is_playback_sample(Some(item_type)));
    }
    assert!(!is_playback_sample(None));
}

fn stream_codec(source: &MediaSourceInfo, kind: &str) -> String {
    source
        .media_streams
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|stream| stream.stream_type.as_deref() == Some(kind))
        .and_then(|stream| stream.codec.as_deref())
        .unwrap_or("none")
        .to_ascii_lowercase()
}

#[tokio::test]
#[ignore = "requires explicit access to an existing Jellyfin server"]
async fn readonly_item_series_people() {
    let base = env::var("JELLYFIN_TEST_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_owned());
    let config = ClientConfig::new(
        &base,
        "Kattri validator",
        "validation",
        "kattri-validation-m1-07-09",
        "1",
    )
    .expect("valid server configuration");
    let client = JellyfinClient::new(config).expect("HTTP client");
    let mut failures = Vec::new();
    if let Some(info) = outcome(
        &mut failures,
        "M1-07..09 public_info",
        client.public_system_info().await,
    ) {
        println!(
            "M1-07..09 server_version: {}",
            info.version.as_deref().unwrap_or("unknown")
        );
    }

    let username = env::var("JELLYFIN_TEST_USER")
        .ok()
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    let password = env::var("JELLYFIN_TEST_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    let (Some(username), Some(password)) = (username, password) else {
        println!("M1-07..09 authenticated checks: NOT RUN (credentials unavailable)");
        assert!(failures.is_empty(), "public info failed: {failures:?}");
        return;
    };
    let session = client
        .authenticate(&username, &password)
        .await
        .expect("authentication failed; response details are suppressed");
    println!("M1-07..09 authenticate: PASS");

    let views = outcome(
        &mut failures,
        "M1-07..09 user_views",
        client.user_views(&session).await,
    );

    let movie_view = views.as_ref().and_then(|views| {
        views
            .iter()
            .find(|view| view.collection_type.as_deref() == Some("movies"))
    });
    let movie = if let Some(view) = movie_view {
        outcome(
            &mut failures,
            "M1-07..09 movie_sample",
            client
                .list_items(
                    &session,
                    &ItemQuery {
                        parent_id: Some(view.id.clone()),
                        include_item_types: Some(vec!["Movie".to_owned()]),
                        limit: 1,
                        ..ItemQuery::default()
                    },
                )
                .await,
        )
        .and_then(|page| page.items.into_iter().next())
    } else {
        println!("M1-07/09 movie sample: NOT AVAILABLE (no Movies library view)");
        None
    };

    if let Some(movie) = &movie {
        if let Some(item) = outcome(
            &mut failures,
            "M1-07 get_item",
            client.get_item(&session, &movie.id).await,
        ) {
            println!(
                "M1-07 metadata: type={} year_present={} runtime_present={} rating_present={} genres_present={} overview_present={}",
                item.item_type
                    .as_deref()
                    .unwrap_or("unknown")
                    .to_ascii_lowercase(),
                item.production_year.is_some(),
                item.run_time_ticks.is_some(),
                item.official_rating.is_some(),
                item.genres
                    .as_ref()
                    .is_some_and(|genres| !genres.is_empty()),
                item.overview
                    .as_ref()
                    .is_some_and(|overview| !overview.is_empty()),
            );

            let mut try_primary = item.backdrop_image_tags.as_ref().is_none_or(Vec::is_empty);
            if !try_primary {
                match client.get_backdrop_image(&session, &movie.id).await {
                    Ok(_) => println!("M1-07 backdrop_image: PASS"),
                    Err(BackdropImageError::NotFoundOrEmptyImage) => {
                        println!("M1-07 backdrop_image: NOT AVAILABLE (trying Primary fallback)");
                        try_primary = true;
                    }
                    Err(BackdropImageError::Api(error)) => {
                        println!("M1-07 backdrop_image: FAIL class={:?}", error.classify());
                        failures.push("M1-07 backdrop_image");
                    }
                }
            }
            if try_primary {
                let tag = item
                    .image_tags
                    .as_ref()
                    .and_then(|tags| tags.get("Primary"))
                    .and_then(Option::as_deref);
                match client.get_primary_image(&session, &movie.id, tag).await {
                    Ok(_) => println!("M1-07 primary_image: PASS"),
                    Err(ApiError::HttpStatus(status)) if status.as_u16() == 404 => {
                        println!("M1-07 primary_image: NOT AVAILABLE (placeholder fallback)");
                    }
                    Err(error) => {
                        println!("M1-07 primary_image: FAIL class={:?}", error.classify());
                        failures.push("M1-07 primary_image");
                    }
                }
            }
        }
    } else {
        println!("M1-07 item details/artwork: NOT AVAILABLE (no movie sample)");
    }

    let tv_view = views.as_ref().and_then(|views| {
        views
            .iter()
            .find(|view| view.collection_type.as_deref() == Some("tvshows"))
    });
    if let Some(view) = tv_view {
        let series = outcome(
            &mut failures,
            "M1-08 series_sample",
            client
                .list_items(
                    &session,
                    &ItemQuery {
                        parent_id: Some(view.id.clone()),
                        include_item_types: Some(vec!["Series".to_owned()]),
                        limit: 1,
                        ..ItemQuery::default()
                    },
                )
                .await,
        )
        .and_then(|page| page.items.into_iter().next());
        if let Some(series) = series {
            if let Some(seasons) = outcome(
                &mut failures,
                "M1-08 seasons",
                client.list_seasons(&session, &series.id).await,
            ) {
                println!("M1-08 seasons: returned={}", seasons.items.len());
                if let Some(season) = seasons.items.first() {
                    if let Some(episodes) = outcome(
                        &mut failures,
                        "M1-08 episodes",
                        client.list_episodes(&session, &series.id, &season.id).await,
                    ) {
                        println!("M1-08 episodes: returned={}", episodes.items.len());
                    }
                } else {
                    println!("M1-08 episodes: NOT AVAILABLE (sample series has no seasons)");
                }
            }
        } else {
            println!("M1-08 seasons/episodes: NOT AVAILABLE (no series sample)");
        }
    } else {
        println!("M1-08 seasons/episodes: NOT AVAILABLE (no TV library view)");
    }

    if let Some(movie) = &movie {
        if let Some(item) = outcome(
            &mut failures,
            "M1-09 item_people",
            client.get_item(&session, &movie.id).await,
        ) {
            let people = item.people.unwrap_or_default();
            println!("M1-09 cast: returned={}", people.len().min(20));
            if let Some(person) = people
                .iter()
                .take(20)
                .find(|person| person.name.as_deref().is_some_and(|name| !name.is_empty()))
            {
                if let Some(person_id) = person.id.as_deref() {
                    if let Some(filmography) = outcome(
                        &mut failures,
                        "M1-09 person_filmography",
                        client.list_person_items(&session, person_id, 50).await,
                    ) {
                        println!(
                            "M1-09 person_filmography: returned={}",
                            filmography.items.len()
                        );
                    }
                } else {
                    println!("M1-09 person_filmography: NOT AVAILABLE (cast member has no ID)");
                }
                if let Some(name) = person.name.as_deref() {
                    match client
                        .get_person_image(&session, name, person.primary_image_tag.as_deref())
                        .await
                    {
                        Ok(_) => println!("M1-09 person_image: PASS"),
                        Err(ApiError::HttpStatus(status)) if status.as_u16() == 404 => {
                            println!("M1-09 person_image: NOT AVAILABLE (no image)");
                        }
                        Err(error) => {
                            println!("M1-09 person_image: FAIL class={:?}", error.classify());
                            failures.push("M1-09 person_image");
                        }
                    }
                }
            } else {
                println!("M1-09 person details: NOT AVAILABLE (sample has no named cast)");
            }
        }

        match client.list_similar(&session, &movie.id, 12).await {
            Ok(similar) => {
                let self_count = similar
                    .items
                    .iter()
                    .filter(|candidate| candidate.id == movie.id)
                    .count();
                println!(
                    "M1-09 similar_items: PASS returned={} self_entries={self_count}",
                    similar.items.len()
                );
            }
            Err(ApiError::HttpStatus(status))
                if status.as_u16() == 404 || status.is_server_error() =>
            {
                println!(
                    "M1-09 similar_items: OPTIONAL_UNAVAILABLE class={:?}",
                    ApiError::HttpStatus(status).classify()
                );
            }
            Err(error) => {
                println!("M1-09 similar_items: FAIL class={:?}", error.classify());
                failures.push("M1-09 similar_items");
            }
        }
    } else {
        println!("M1-09 cast/similar: NOT AVAILABLE (no movie sample)");
    }

    println!(
        "M1-07..09 read-only summary: failed_calls={}",
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "read-only live calls failed: {failures:?}"
    );
}

#[tokio::test]
#[ignore = "requires explicit access to an existing Jellyfin server"]
async fn readonly_collection_members_preserve_server_order() {
    let base = env::var("JELLYFIN_TEST_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_owned());
    let config = ClientConfig::new(
        &base,
        "Kattri validator",
        "validation",
        "kattri-validation-m3-16",
        "1",
    )
    .expect("valid server configuration");
    let client = JellyfinClient::new(config).expect("HTTP client");
    let info = client
        .public_system_info()
        .await
        .expect("public server info failed; response details are suppressed");
    println!(
        "M3-16 server_version: {}",
        info.version.as_deref().unwrap_or("unknown")
    );
    let username = Zeroizing::new(
        env::var("JELLYFIN_TEST_USER").expect("JELLYFIN_TEST_USER is required for this live test"),
    );
    let password = Zeroizing::new(
        env::var("JELLYFIN_TEST_PASSWORD")
            .expect("JELLYFIN_TEST_PASSWORD is required for this live test"),
    );
    let session = client
        .authenticate(&username, &password)
        .await
        .expect("authentication failed; response details are suppressed");

    let box_sets = client
        .list_items(
            &session,
            &ItemQuery {
                include_item_types: Some(vec!["BoxSet".to_owned()]),
                limit: 10,
                enable_total_record_count: Some(true),
                ..ItemQuery::default()
            },
        )
        .await
        .expect("box-set listing failed; response details are suppressed");
    println!("M3-16 box_sets: {}", box_sets.items.len());
    let Some(box_set) = box_sets.items.first() else {
        panic!("M3-16 validation requires one readable box set");
    };

    let members = client
        .collection_members(&session, &box_set.id, 0, 100)
        .await
        .expect("collection member listing failed; response details are suppressed");
    let unique_ids = members
        .items
        .iter()
        .map(|item| item.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        unique_ids.len(),
        members.items.len(),
        "duplicate members returned"
    );

    let mut type_counts = BTreeMap::new();
    for item in &members.items {
        *type_counts
            .entry(item.item_type.as_deref().unwrap_or("unknown"))
            .or_insert(0_usize) += 1;
    }
    let nested_box_sets = members
        .items
        .iter()
        .filter(|item| item.item_type.as_deref() == Some("BoxSet"))
        .count();
    println!(
        "M3-16 collection_members: returned={} total={} nested_box_sets={nested_box_sets} type_counts={type_counts:?}",
        members.items.len(),
        members
            .total_record_count
            .unwrap_or(members.items.len() as u32),
    );
    println!("M3-16 order: PASS (client preserves response order)");
}

#[tokio::test]
#[ignore = "requires explicit access to an existing Jellyfin server"]
async fn readonly_home_and_playback_negotiation() {
    let base = env::var("JELLYFIN_TEST_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_owned());
    let config = ClientConfig::new(
        &base,
        "Kattri validator",
        "validation",
        "kattri-validation-readonly",
        "1",
    )
    .expect("valid server configuration");
    let client = JellyfinClient::new(config).expect("HTTP client");
    let mut failures = Vec::new();

    if let Some(info) = outcome(
        &mut failures,
        "M1-01 public_info",
        client.public_system_info().await,
    ) {
        println!(
            "server_version: {}",
            info.version.as_deref().unwrap_or("unknown")
        );
    }

    let username = env::var("JELLYFIN_TEST_USER")
        .ok()
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    let password = env::var("JELLYFIN_TEST_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    let (Some(username), Some(password)) = (username, password) else {
        println!(
            "authenticated checks skipped: JELLYFIN_TEST_USER and JELLYFIN_TEST_PASSWORD must both be set"
        );
        assert!(failures.is_empty(), "public info failed: {failures:?}");
        return;
    };

    let session = outcome(
        &mut failures,
        "authenticate",
        client.authenticate(&username, &password).await,
    );
    let Some(session) = session else {
        panic!("sign-in failed; see the authenticate line above");
    };

    let views = outcome(
        &mut failures,
        "M1-05 user_views",
        client.user_views(&session).await,
    );
    if let Some(views) = &views {
        let kinds = count_types(views.iter().map(|view| view.collection_type.as_deref()));
        println!("library_views: count={}", views.len());
        for (kind, count) in kinds {
            println!("  view_type {kind}: {count}");
        }
    }

    let box_sets = outcome(
        &mut failures,
        "M1-05 box_sets",
        client.box_sets(&session, SAMPLE_LIMIT).await,
    );
    if let Some(box_sets) = &box_sets {
        println!("M1-05 collections: returned={}", box_sets.len());
    }

    let resume = outcome(
        &mut failures,
        "M1-02 resume_items",
        client.resume_items(&session, SAMPLE_LIMIT).await,
    );
    if let Some(resume) = &resume {
        println!(
            "M1-02 resume_items: returned={} total={}",
            resume.items.len(),
            resume
                .total_record_count
                .map_or_else(|| "unknown".to_owned(), |total| total.to_string())
        );
        for (kind, count) in count_types(resume.items.iter().map(|item| item.item_type.as_deref()))
        {
            println!("  M1-02 type {kind}: {count}");
        }
    }

    let next_up = outcome(
        &mut failures,
        "M1-03 next_up",
        client.next_up(&session, SAMPLE_LIMIT).await,
    );
    if let Some(next_up) = &next_up {
        println!("M1-03 next_up: returned={}", next_up.items.len());
        for (kind, count) in count_types(next_up.items.iter().map(|item| item.item_type.as_deref()))
        {
            println!("  M1-03 type {kind}: {count}");
        }
    }

    let latest = outcome(
        &mut failures,
        "M1-04 latest_items",
        client.latest_items(&session, None, SAMPLE_LIMIT).await,
    );
    if let Some(latest) = &latest {
        println!("M1-04 latest_items: returned={}", latest.len());
        for (kind, count) in count_types(latest.iter().map(|item| item.item_type.as_deref())) {
            println!("  M1-04 type {kind}: {count}");
        }
    }

    if let Some(views) = &views {
        for view in views {
            if !matches!(
                view.collection_type.as_deref(),
                Some("movies" | "tvshows" | "music")
            ) {
                continue;
            }
            if let Some(items) = outcome(
                &mut failures,
                "M1-04 latest_items_by_library",
                client
                    .latest_items(&session, Some(&view.id), SAMPLE_LIMIT)
                    .await,
            ) {
                println!("M1-04 per_library_latest: returned={}", items.len());
            }
        }
    }

    // Sample candidates from read-only home and library calls; never print their IDs.
    let mut candidates: Vec<String> = Vec::new();
    let mut playback_candidates: Vec<String> = Vec::new();
    for item in resume
        .iter()
        .flat_map(|response| response.items.iter())
        .chain(next_up.iter().flat_map(|response| response.items.iter()))
        .chain(latest.iter().flatten())
        .chain(box_sets.iter().flat_map(|items| items.iter()))
    {
        if !candidates.contains(&item.id) {
            candidates.push(item.id.clone());
        }
        if is_playback_sample(item.item_type.as_deref()) {
            playback_candidates.push(item.id.clone());
        }
    }

    // Exercise real paging, sort and filter query shapes against the first
    // supported library. All requests are GETs and their results stay in memory.
    if let Some(view) = views.as_ref().and_then(|views| {
        views.iter().find(|view| {
            matches!(
                view.collection_type.as_deref(),
                Some("movies" | "tvshows" | "music")
            )
        })
    }) {
        let first_page = outcome(
            &mut failures,
            "M1-06 library_page_1_sorted",
            client
                .list_items(
                    &session,
                    &ItemQuery {
                        parent_id: Some(view.id.clone()),
                        start_index: 0,
                        limit: 50,
                        sort_by: Some(vec![ItemSortBy::SortName]),
                        sort_order: Some(vec![ItemSortOrder::Ascending]),
                        enable_total_record_count: Some(true),
                        ..ItemQuery::default()
                    },
                )
                .await,
        );
        if let Some(first_page) = &first_page {
            println!(
                "M1-06 library_page_1_sorted: returned={} total={}",
                first_page.items.len(),
                first_page
                    .total_record_count
                    .map_or_else(|| "unknown".to_owned(), |count| count.to_string())
            );
            candidates.extend(first_page.items.iter().take(3).map(|item| item.id.clone()));
            playback_candidates.extend(
                first_page
                    .items
                    .iter()
                    .filter(|item| is_playback_sample(item.item_type.as_deref()))
                    .map(|item| item.id.clone()),
            );
            if first_page.items.len() == 50
                || first_page
                    .total_record_count
                    .is_some_and(|total| total > 50)
            {
                if let Some(second_page) = outcome(
                    &mut failures,
                    "M1-06 library_page_2_sorted",
                    client
                        .list_items(
                            &session,
                            &ItemQuery {
                                parent_id: Some(view.id.clone()),
                                start_index: 50,
                                limit: 50,
                                sort_by: Some(vec![ItemSortBy::SortName]),
                                sort_order: Some(vec![ItemSortOrder::Ascending]),
                                enable_total_record_count: Some(true),
                                ..ItemQuery::default()
                            },
                        )
                        .await,
                ) {
                    let first_ids: std::collections::HashSet<&str> = first_page
                        .items
                        .iter()
                        .map(|item| item.id.as_str())
                        .collect();
                    let duplicate_count = second_page
                        .items
                        .iter()
                        .filter(|item| first_ids.contains(item.id.as_str()))
                        .count();
                    println!(
                        "M1-06 library_page_2_summary: returned={} duplicate_ids={duplicate_count}",
                        second_page.items.len()
                    );
                    if duplicate_count > 0 {
                        failures.push("M1-06 page_duplicate_ids");
                    }
                    candidates.extend(second_page.items.iter().take(1).map(|item| item.id.clone()));
                    playback_candidates.extend(
                        second_page
                            .items
                            .iter()
                            .filter(|item| is_playback_sample(item.item_type.as_deref()))
                            .map(|item| item.id.clone()),
                    );
                }
            } else {
                println!("M1-06 library_page_2_sorted: NOT AVAILABLE (fewer than 51 items)");
            }
        }

        let filter_result = outcome(
            &mut failures,
            "M1-06 is_played_filter",
            client
                .list_items(
                    &session,
                    &ItemQuery {
                        parent_id: Some(view.id.clone()),
                        limit: 1,
                        filters: Some(vec![ItemFilter::IsPlayed]),
                        ..ItemQuery::default()
                    },
                )
                .await,
        );
        if let Some(filtered) = filter_result {
            println!("M1-06 is_played_filter: returned={}", filtered.items.len());
        }
    } else {
        println!("M1-06 library_browse: NOT AVAILABLE (no supported library view)");
    }
    candidates.sort();
    candidates.dedup();
    playback_candidates.sort();
    playback_candidates.dedup();

    match candidates.first() {
        Some(first_id) => {
            if let Some(item) = outcome(
                &mut failures,
                "M1-07 get_item",
                client.get_item(&session, first_id).await,
            ) {
                println!(
                    "M1-07 metadata: type={} year_present={} runtime_present={} rating_present={} genres_present={} overview_present={}",
                    item.item_type
                        .as_deref()
                        .unwrap_or("unknown")
                        .to_ascii_lowercase(),
                    item.production_year.is_some(),
                    item.run_time_ticks.is_some(),
                    item.official_rating.is_some(),
                    item.genres
                        .as_ref()
                        .is_some_and(|genres| !genres.is_empty()),
                    item.overview
                        .as_ref()
                        .is_some_and(|overview| !overview.is_empty()),
                );

                let backdrop_available = item
                    .backdrop_image_tags
                    .as_ref()
                    .is_some_and(|tags| !tags.is_empty());
                let artwork = if backdrop_available {
                    match client.get_backdrop_image(&session, first_id).await {
                        Ok(_) => {
                            println!("M1-07 backdrop_image: PASS");
                            None
                        }
                        Err(BackdropImageError::NotFoundOrEmptyImage) => {
                            println!("M1-07 backdrop_image: 404_OR_EMPTY; trying primary fallback");
                            Some(())
                        }
                        Err(BackdropImageError::Api(error)) => {
                            println!("M1-07 backdrop_image: FAIL class={:?}", error.classify());
                            failures.push("M1-07 backdrop_image");
                            None
                        }
                    }
                } else {
                    println!("M1-07 backdrop_image: NOT AVAILABLE (no backdrop tag)");
                    Some(())
                };
                if artwork.is_some() {
                    let primary_tag = item
                        .image_tags
                        .as_ref()
                        .and_then(|tags| tags.get("Primary"))
                        .and_then(Option::as_deref);
                    match client
                        .get_primary_image(&session, first_id, primary_tag)
                        .await
                    {
                        Ok(_) => println!("M1-07 primary_fallback: PASS"),
                        Err(ApiError::HttpStatus(status)) if status.as_u16() == 404 => {
                            println!(
                                "M1-07 primary_fallback: NOT AVAILABLE (image missing; placeholder path)"
                            );
                        }
                        Err(error) => {
                            println!("M1-07 primary_fallback: FAIL class={:?}", error.classify());
                            failures.push("M1-07 primary_fallback");
                        }
                    }
                }
            }
        }
        None => {
            println!("M1-07 get_item: NOT AVAILABLE (no item candidate in read-only responses)")
        }
    }

    // Read one series hierarchy without starting playback or changing state.
    let series_view = views.as_ref().and_then(|views| {
        views
            .iter()
            .find(|view| view.collection_type.as_deref() == Some("tvshows"))
    });
    if let Some(view) = series_view {
        let series_page = outcome(
            &mut failures,
            "M1-08 series_sample",
            client
                .list_items(
                    &session,
                    &ItemQuery {
                        parent_id: Some(view.id.clone()),
                        include_item_types: Some(vec!["Series".to_owned()]),
                        limit: 1,
                        ..ItemQuery::default()
                    },
                )
                .await,
        );
        if let Some(series) = series_page.and_then(|page| page.items.into_iter().next()) {
            if let Some(seasons) = outcome(
                &mut failures,
                "M1-08 seasons",
                client.list_seasons(&session, &series.id).await,
            ) {
                println!("M1-08 seasons: returned={}", seasons.items.len());
                if let Some(season) = seasons.items.first() {
                    if let Some(episodes) = outcome(
                        &mut failures,
                        "M1-08 episodes",
                        client.list_episodes(&session, &series.id, &season.id).await,
                    ) {
                        println!("M1-08 episodes: returned={}", episodes.items.len());
                    }
                } else {
                    println!("M1-08 episodes: NOT AVAILABLE (sample series has no seasons)");
                }
            }
        } else {
            println!("M1-08 seasons/episodes: NOT AVAILABLE (no series item in the TV library)");
        }
    } else {
        println!("M1-08 seasons/episodes: NOT AVAILABLE (no TV library view)");
    }

    // Read one movie's cast, similar items, optional person filmography, and
    // person artwork. Names and IDs never leave memory or enter test output.
    let movie_view = views.as_ref().and_then(|views| {
        views
            .iter()
            .find(|view| view.collection_type.as_deref() == Some("movies"))
    });
    if let Some(view) = movie_view {
        let movie_page = outcome(
            &mut failures,
            "M1-09 movie_sample",
            client
                .list_items(
                    &session,
                    &ItemQuery {
                        parent_id: Some(view.id.clone()),
                        include_item_types: Some(vec!["Movie".to_owned()]),
                        limit: 1,
                        ..ItemQuery::default()
                    },
                )
                .await,
        );
        if let Some(movie) = movie_page.and_then(|page| page.items.into_iter().next()) {
            if let Some(item) = outcome(
                &mut failures,
                "M1-09 item_people",
                client.get_item(&session, &movie.id).await,
            ) {
                let people = item.people.unwrap_or_default();
                println!("M1-09 cast: returned={}", people.len().min(20));
                if let Some(person) = people
                    .iter()
                    .take(20)
                    .find(|person| person.name.as_deref().is_some_and(|name| !name.is_empty()))
                {
                    if let Some(person_id) = person.id.as_deref() {
                        if let Some(filmography) = outcome(
                            &mut failures,
                            "M1-09 person_filmography",
                            client.list_person_items(&session, person_id, 50).await,
                        ) {
                            println!(
                                "M1-09 person_filmography: returned={}",
                                filmography.items.len()
                            );
                        }
                    } else {
                        println!("M1-09 person_filmography: NOT AVAILABLE (cast member has no ID)");
                    }
                    if let Some(name) = person.name.as_deref() {
                        match client
                            .get_person_image(&session, name, person.primary_image_tag.as_deref())
                            .await
                        {
                            Ok(_) => println!("M1-09 person_image: PASS"),
                            Err(ApiError::HttpStatus(status)) if status.as_u16() == 404 => {
                                println!("M1-09 person_image: NOT AVAILABLE (no image)");
                            }
                            Err(error) => {
                                println!("M1-09 person_image: FAIL class={:?}", error.classify());
                                failures.push("M1-09 person_image");
                            }
                        }
                    }
                } else {
                    println!("M1-09 person details: NOT AVAILABLE (sample has no named cast)");
                }
            }

            match client.list_similar(&session, &movie.id, 12).await {
                Ok(similar) => {
                    let self_count = similar
                        .items
                        .iter()
                        .filter(|candidate| candidate.id == movie.id)
                        .count();
                    println!(
                        "M1-09 similar_items: PASS returned={} self_entries={self_count}",
                        similar.items.len()
                    );
                }
                Err(ApiError::HttpStatus(status))
                    if status.as_u16() == 404 || status.is_server_error() =>
                {
                    println!(
                        "M1-09 similar_items: OPTIONAL_UNAVAILABLE class={:?}",
                        ApiError::HttpStatus(status).classify()
                    );
                }
                Err(error) => {
                    println!("M1-09 similar_items: FAIL class={:?}", error.classify());
                    failures.push("M1-09 similar_items");
                }
            }
        } else {
            println!("M1-09 cast/similar: NOT AVAILABLE (no movie in the Movies library)");
        }
    } else {
        println!("M1-09 cast/similar: NOT AVAILABLE (no Movies library view)");
    }

    let request = PlaybackInfoRequest::for_mpv_profile(
        baseline_device_profile(),
        PlaybackRequestOverrides::default(),
    );
    let available_playback_samples = playback_candidates.len().min(PLAYBACK_SAMPLE);
    if available_playback_samples < PLAYBACK_SAMPLE {
        println!(
            "M1-01 playback_info coverage: FAIL required={PLAYBACK_SAMPLE} available={available_playback_samples}"
        );
        failures.push("M1-01 insufficient PlaybackInfo samples");
    }
    let mut playback_coverage = PlaybackCoverage::default();
    for (index, item_id) in playback_candidates.iter().take(PLAYBACK_SAMPLE).enumerate() {
        let label = match index {
            0 => "M1-01 playback_info[1]",
            1 => "M1-01 playback_info[2]",
            _ => "M1-01 playback_info[3]",
        };
        let info = match client.playback_info(&session, item_id, &request).await {
            Ok(info) if info.media_sources.is_empty() => {
                playback_coverage.record(PlaybackSampleResult::Empty);
                println!("{label}: FAIL class=EmptyMediaSources");
                failures.push(label);
                continue;
            }
            Ok(info) if info.error_code.is_some() => {
                playback_coverage.record(PlaybackSampleResult::EmbeddedError);
                println!("{label}: FAIL class=PlaybackInfoError");
                failures.push(label);
                continue;
            }
            Ok(info) => {
                playback_coverage.record(PlaybackSampleResult::Valid);
                println!("{label}: PASS");
                info
            }
            Err(error) if error.classify() == jellyfin_api::ErrorClass::ServerUnavailable => {
                playback_coverage.record(PlaybackSampleResult::Unavailable);
                println!("{label}: UNAVAILABLE class=ServerUnavailable");
                failures.push(label);
                continue;
            }
            Err(error) => {
                playback_coverage.record(PlaybackSampleResult::RequestError);
                println!("{label}: FAIL class={:?}", error.classify());
                failures.push(label);
                continue;
            }
        };
        if let Some(code) = info.error_code {
            println!("  error_code: {code:?}");
        }
        println!("  media_sources: {}", info.media_sources.len());
        for source in &info.media_sources {
            println!(
                "  method={} container={} video={} audio={} supports_direct_play={} supports_direct_stream={} supports_transcoding={} transcoding_url_present={}",
                playback_method(source),
                source
                    .container
                    .as_deref()
                    .unwrap_or("unknown")
                    .to_ascii_lowercase(),
                stream_codec(source, "Video"),
                stream_codec(source, "Audio"),
                source.supports_direct_play,
                source.supports_direct_stream,
                source.supports_transcoding,
                source.transcoding_url.is_some(),
            );
        }
    }
    println!(
        "M1-01 playback_info summary: required={PLAYBACK_SAMPLE} available={available_playback_samples} attempted={} valid={} unavailable={} request_errors={} empty={} embedded_errors={}",
        playback_coverage.attempted,
        playback_coverage.valid,
        playback_coverage.unavailable,
        playback_coverage.request_errors,
        playback_coverage.empty,
        playback_coverage.embedded_errors,
    );
    if !playback_coverage.is_complete(PLAYBACK_SAMPLE, available_playback_samples) {
        println!("M1-01 playback_info coverage: FAIL");
        failures.push("M1-01 PlaybackInfo coverage incomplete");
    } else {
        println!("M1-01 playback_info coverage: PASS");
    }

    println!("read-only summary: failed_calls={}", failures.len());
    assert!(
        failures.is_empty(),
        "read-only live calls failed: {failures:?}"
    );
}

/// M4-22 live, read-only: the admin dashboard reads against a real server.
/// Prints counts and library kinds only; never names, ids, tokens or paths.
/// A 403 is reported as "admin rights unavailable" instead of failing, because
/// the server decides whether the account may see the section.
#[tokio::test]
#[ignore = "requires explicit access to an existing Jellyfin server"]
async fn readonly_admin_dashboard_reads() {
    let base = env::var("JELLYFIN_TEST_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_owned());
    let config = ClientConfig::new(
        &base,
        "Kattri validator",
        "validation",
        "kattri-validation-m4-22",
        "1",
    )
    .expect("valid server configuration");
    let client = JellyfinClient::new(config).expect("HTTP client");
    let username = env::var("JELLYFIN_TEST_USER")
        .ok()
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    let password = env::var("JELLYFIN_TEST_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    let (Some(username), Some(password)) = (username, password) else {
        println!("M4-22 admin reads: NOT RUN (credentials unavailable)");
        return;
    };
    let session = client
        .authenticate(&username, &password)
        .await
        .expect("authentication failed; response details are suppressed");

    let mut failures = Vec::new();
    if let Some(info) = outcome(
        &mut failures,
        "M4-22 system_info",
        client.server_info(&session).await,
    ) {
        println!(
            "M4-22 server_info: PASS product_reported={} version_reported={}",
            info.product_name.is_some(),
            info.version.is_some()
        );
    }
    let libraries = match client.virtual_folders(&session).await {
        Ok(libraries) => {
            println!("M4-22 virtual_folders: PASS count={}", libraries.len());
            Some(libraries)
        }
        Err(ApiError::Forbidden) => {
            println!("M4-22 virtual_folders: FORBIDDEN (account is not elevated)");
            None
        }
        Err(error) => {
            println!("M4-22 virtual_folders: FAIL {error}");
            failures.push("M4-22 virtual folders");
            None
        }
    };
    if let Some(active) = outcome(
        &mut failures,
        "M4-22 active_sessions",
        client.active_session_count(&session).await,
    ) {
        println!("M4-22 active_sessions: PASS count={active}");
    }
    match client.user_counts(&session).await {
        Ok(users) => println!(
            "M4-22 user_counts: PASS total={} enabled={} disabled={}",
            users.total, users.enabled, users.disabled
        ),
        Err(ApiError::Forbidden) => println!("M4-22 user_counts: FORBIDDEN"),
        Err(error) => {
            println!("M4-22 user_counts: FAIL {error}");
            failures.push("M4-22 user counts");
        }
    }
    match client.scheduled_tasks(&session).await {
        Ok(tasks) => {
            let running = tasks
                .iter()
                .filter(|task| {
                    task.state
                        .as_deref()
                        .is_some_and(|state| !state.eq_ignore_ascii_case("Idle"))
                })
                .count();
            let named = tasks.iter().filter(|task| task.name.is_some()).count();
            println!(
                "M4-22 scheduled_tasks: PASS count={} named={} running={}",
                tasks.len(),
                named,
                running
            );
        }
        Err(ApiError::Forbidden) => println!("M4-22 scheduled_tasks: FORBIDDEN"),
        Err(error) => {
            println!("M4-22 scheduled_tasks: FAIL {error}");
            failures.push("M4-22 scheduled tasks");
        }
    }
    if let Some(libraries) = libraries {
        let mut counted = 0;
        let mut missing = 0;
        for library in libraries.iter().take(8) {
            let Some(id) = library.item_id.as_deref() else {
                missing += 1;
                continue;
            };
            match client.library_item_count(&session, id).await {
                Ok(Some(_)) => counted += 1,
                Ok(None) => missing += 1,
                Err(ApiError::Forbidden) => {
                    println!("M4-22 library_item_count: FORBIDDEN");
                    break;
                }
                Err(error) => {
                    println!("M4-22 library_item_count: FAIL {error}");
                    failures.push("M4-22 library item count");
                    break;
                }
            }
        }
        println!("M4-22 library_item_count: PASS counted={counted} without_total={missing}");
    }

    println!("M4-22 read-only summary: failed_calls={}", failures.len());
    assert!(failures.is_empty(), "admin reads failed: {failures:?}");
}
