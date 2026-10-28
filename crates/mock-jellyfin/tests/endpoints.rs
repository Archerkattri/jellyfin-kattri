//! In-process integration tests: spawn the mock on an ephemeral loopback port
//! and exercise every endpoint over real HTTP with reqwest.

use mock_jellyfin::{MockConfig, RunningServer, spawn};
use reqwest::{Client, Response, StatusCode, header};
use serde_json::{Value, json};
use std::collections::HashSet;

const TOKEN: &str = "mock-access-token-0001";

async fn start() -> RunningServer {
    spawn(MockConfig::default())
        .await
        .expect("spawn mock server")
}

fn auth_header() -> String {
    format!(
        "MediaBrowser Client=\"tests\", Device=\"ci\", DeviceId=\"ci-1\", Version=\"0\", Token=\"{TOKEN}\""
    )
}

async fn get_json(server: &RunningServer, path: &str) -> Value {
    let response = Client::new()
        .get(format!("{}{path}", server.base_url()))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK, "GET {path}");
    response.json().await.expect("json body")
}

async fn get_status(server: &RunningServer, path: &str, auth: Option<&str>) -> StatusCode {
    let mut request = Client::new().get(format!("{}{path}", server.base_url()));
    if let Some(value) = auth {
        request = request.header(header::AUTHORIZATION, value);
    }
    request.send().await.expect("request").status()
}

fn item_names(body: &Value) -> Vec<String> {
    body["Items"]
        .as_array()
        .expect("Items array")
        .iter()
        .map(|item| item["Name"].as_str().expect("name").to_owned())
        .collect()
}

async fn first_item_id(server: &RunningServer, query: &str) -> String {
    get_json(server, &format!("/Items?{query}&limit=1")).await["Items"][0]["Id"]
        .as_str()
        .expect("id")
        .to_owned()
}

#[tokio::test]
async fn public_system_info_is_anonymous() {
    let server = start().await;
    let response = Client::new()
        .get(format!("{}/System/Info/Public", server.base_url()))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ServerName"], "Mock Jellyfin");
    assert_eq!(body["Id"], "00000000000000000000000000000005e7");
    assert!(body["Version"].is_string());
    server.shutdown().await;
}

#[tokio::test]
async fn authenticate_accepts_demo_and_rejects_everything_else() {
    let server = start().await;
    let client = Client::new();
    let url = format!("{}/Users/AuthenticateByName", server.base_url());

    let ok: Value = client
        .post(&url)
        .json(&json!({"Username": "demo", "Pw": "demo"}))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(ok["AccessToken"], TOKEN);
    assert_eq!(ok["User"]["Name"], "demo");
    assert_eq!(ok["User"]["Id"], "00000000000000000000000000000d01");
    assert_eq!(ok["ServerId"], "00000000000000000000000000000005e7");

    for body in [
        json!({"Username": "demo", "Pw": "wrong"}),
        json!({"Username": "other", "Pw": "demo"}),
        json!({"Username": "demo"}),
    ] {
        let status = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "body {body}");
    }
    let malformed = client
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .body("{not json")
        .send()
        .await
        .expect("request")
        .status();
    assert_eq!(malformed, StatusCode::UNAUTHORIZED);
    server.shutdown().await;
}

#[tokio::test]
async fn authenticated_routes_require_a_valid_token() {
    let server = start().await;
    let path = "/UserViews";
    assert_eq!(
        get_status(&server, path, None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get_status(&server, path, Some("MediaBrowser Token=\"wrong\"")).await,
        StatusCode::UNAUTHORIZED
    );

    let via_authorization = format!("MediaBrowser Token=\"{TOKEN}\"");
    assert_eq!(
        get_status(&server, path, Some(&via_authorization)).await,
        StatusCode::OK
    );

    let via_emby_header = Client::new()
        .get(format!("{}{path}", server.base_url()))
        .header(
            "X-Emby-Authorization",
            format!("Emby Client=\"x\", Token=\"{TOKEN}\""),
        )
        .send()
        .await
        .expect("request")
        .status();
    assert_eq!(via_emby_header, StatusCode::OK);
    server.shutdown().await;
}

#[tokio::test]
async fn user_views_lists_three_libraries() {
    let server = start().await;
    let body = get_json(
        &server,
        "/UserViews?userId=00000000000000000000000000000d01",
    )
    .await;
    let views = body["Items"].as_array().expect("items");
    let names: Vec<&str> = views.iter().map(|v| v["Name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Movies", "Shows", "Music"]);
    let collections: Vec<&str> = views
        .iter()
        .map(|v| v["CollectionType"].as_str().unwrap())
        .collect();
    assert_eq!(collections, ["movies", "tvshows", "music"]);
    assert_eq!(body["TotalRecordCount"], 3);
    server.shutdown().await;
}

#[tokio::test]
async fn items_search_type_filter_and_paging() {
    let server = start().await;

    let all = get_json(&server, "/Items?recursive=true").await;
    assert_eq!(all["TotalRecordCount"], 43);
    assert_eq!(all["Items"].as_array().unwrap().len(), 43);

    let movies = get_json(&server, "/Items?recursive=true&includeItemTypes=Movie").await;
    assert_eq!(movies["TotalRecordCount"], 12);

    let search = get_json(&server, "/Items?recursive=true&searchTerm=HARBOR").await;
    let mut names = item_names(&search);
    names.sort();
    assert_eq!(names, ["Harbor Bell", "Harbor Lights", "Night Harbor"]);

    let page = get_json(
        &server,
        "/Items?recursive=true&includeItemTypes=Movie&startIndex=10&limit=5",
    )
    .await;
    assert_eq!(page["TotalRecordCount"], 12);
    assert_eq!(page["StartIndex"], 10);
    assert_eq!(page["Items"].as_array().unwrap().len(), 2);

    let by_year = get_json(&server, "/Items?recursive=true&includeItemTypes=Movie&sortBy=ProductionYear&sortOrder=Descending&limit=1").await;
    assert_eq!(by_year["Items"][0]["Name"], "Hollow Pines");
    server.shutdown().await;
}

#[tokio::test]
async fn catalog_fixture_pages_120_items_without_duplicates_and_supports_filters() {
    let server = spawn(MockConfig {
        item_count: Some(120),
        ..MockConfig::default()
    })
    .await
    .expect("spawn 120-item fixture");
    let mut ids = HashSet::new();
    let mut first_created = None;
    let mut previous_created: Option<String> = None;

    for start in [0, 50, 100] {
        let body = get_json(
            &server,
            &format!(
                "/Items?recursive=true&startIndex={start}&limit=50&sortBy=DateCreated&sortOrder=Ascending&enableTotalRecordCount=true"
            ),
        )
        .await;
        assert_eq!(body["TotalRecordCount"], 120);
        let items = body["Items"].as_array().expect("item page");
        assert_eq!(items.len(), if start == 100 { 20 } else { 50 });
        for item in items {
            let id = item["Id"].as_str().expect("item ID");
            assert!(
                ids.insert(id.to_owned()),
                "item repeated across pages: {id}"
            );
            let created = item["DateCreated"].as_str().expect("created date");
            if first_created.is_none() {
                first_created = Some(created.to_owned());
            }
            if let Some(previous) = &previous_created {
                assert!(previous.as_str() <= created);
            }
            previous_created = Some(created.to_owned());
        }
    }
    assert_eq!(ids.len(), 120);
    assert!(first_created.is_some());

    let no_count = get_json(
        &server,
        "/Items?recursive=true&startIndex=0&limit=50&enableTotalRecordCount=false",
    )
    .await;
    assert_eq!(no_count["Items"].as_array().unwrap().len(), 50);
    assert!(no_count.get("TotalRecordCount").is_none());

    let played = get_json(&server, "/Items?recursive=true&filters=IsPlayed&limit=50").await;
    assert!(!played["Items"].as_array().unwrap().is_empty());
    assert!(
        played["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| { item["UserData"]["Played"].as_bool() == Some(true) })
    );
    let favorites = get_json(&server, "/Items?recursive=true&filters=IsFavorite&limit=50").await;
    assert!(!favorites["Items"].as_array().unwrap().is_empty());
    assert!(
        favorites["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| { item["UserData"]["IsFavorite"].as_bool() == Some(true) })
    );
    let played_favorites = get_json(
        &server,
        "/Items?recursive=true&filters=IsPlayed&filters=IsFavorite&limit=50",
    )
    .await;
    assert!(!played_favorites["Items"].as_array().unwrap().is_empty());
    assert!(
        played_favorites["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| {
                item["UserData"]["Played"].as_bool() == Some(true)
                    && item["UserData"]["IsFavorite"].as_bool() == Some(true)
            })
    );
    server.shutdown().await;
}

#[tokio::test]
async fn unsupported_audio_filter_returns_a_server_error_for_ui_fallback_tests() {
    let server = spawn(MockConfig {
        reject_unplayed_filter_for_audio: true,
        ..MockConfig::default()
    })
    .await
    .expect("spawn filter-rejection fixture");
    let response = Client::new()
        .get(format!(
            "{}/Items?includeItemTypes=Audio,MusicAlbum&filters=IsUnplayed",
            server.base_url()
        ))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error response");
    assert_eq!(
        body["Message"],
        "The item filter 'IsUnplayed' is unsupported."
    );
    server.shutdown().await;
}

#[tokio::test]
async fn items_parent_filter_respects_recursive_flag() {
    let server = start().await;
    let movies_lib = "00000000000000000000000000000001";
    let shows_lib = "00000000000000000000000000000002";

    let direct = get_json(&server, &format!("/Items?parentId={movies_lib}")).await;
    assert_eq!(direct["TotalRecordCount"], 14);

    let shows_recursive = get_json(
        &server,
        &format!("/Items?parentId={shows_lib}&recursive=true"),
    )
    .await;
    assert_eq!(shows_recursive["TotalRecordCount"], 20);

    let shows_direct = get_json(
        &server,
        &format!("/Items?parentId={shows_lib}&recursive=false"),
    )
    .await;
    assert_eq!(shows_direct["TotalRecordCount"], 2);
    server.shutdown().await;
}

#[tokio::test]
async fn favorite_override_changes_item_data_and_favorite_filter() {
    let server = start().await;
    let movie_id = first_item_id(&server, "includeItemTypes=Movie").await;
    let url = format!(
        "{}/UserFavoriteItems/{movie_id}?userId=demo-user",
        server.base_url()
    );

    let favorited = Client::new()
        .post(&url)
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("favorite request");
    assert_eq!(favorited.status(), StatusCode::OK);
    assert_eq!(
        favorited.json::<Value>().await.expect("favorite DTO")["IsFavorite"],
        true
    );

    let favorites = get_json(&server, "/Items?filters=IsFavorite&limit=100").await;
    assert!(
        favorites["Items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| { item["Id"] == movie_id && item["UserData"]["IsFavorite"] == true })
    );

    let unfavorited = Client::new()
        .delete(&url)
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("unfavorite request");
    assert_eq!(unfavorited.status(), StatusCode::OK);
    assert_eq!(
        unfavorited.json::<Value>().await.expect("unfavorite DTO")["IsFavorite"],
        false
    );
    let favorites = get_json(&server, "/Items?filters=IsFavorite&limit=100").await;
    assert!(
        !favorites["Items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["Id"] == movie_id)
    );
    server.shutdown().await;
}

#[tokio::test]
async fn photo_fixture_is_listable_for_favorite_api_tests() {
    let server = start().await;
    let photos = get_json(&server, "/Items?includeItemTypes=Photo&limit=10").await;
    assert_eq!(photos["TotalRecordCount"], 1);
    assert_eq!(photos["Items"][0]["Name"], "Quiet Garden Photo");
    assert_eq!(photos["Items"][0]["Type"], "Photo");
    server.shutdown().await;
}

#[tokio::test]
async fn get_item_returns_seed_fields_and_404_for_unknown() {
    let server = start().await;
    let id = first_item_id(&server, "includeItemTypes=Episode&recursive=true").await;
    let item = get_json(&server, &format!("/Items/{id}")).await;
    assert_eq!(item["Type"], "Episode");
    assert!(item["SeriesName"].is_string());
    assert!(item["RunTimeTicks"].as_i64().unwrap() > 0);
    assert!(item["Overview"].is_string());
    assert!(item["ImageTags"]["Primary"].is_string());
    assert!(item["UserData"]["PlaybackPositionTicks"].is_i64());

    let missing = get_status(
        &server,
        "/Items/ffffffffffffffffffffffffffffffff",
        Some(&auth_header()),
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
    server.shutdown().await;
}

#[tokio::test]
async fn resume_returns_in_progress_videos_only() {
    let server = start().await;
    let body = get_json(
        &server,
        "/UserItems/Resume?userId=00000000000000000000000000000d01&limit=10&mediaTypes=Video",
    )
    .await;
    let items = body["Items"].as_array().unwrap();
    let mut names: Vec<&str> = items.iter().map(|i| i["Name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "Episode 2",
            "Hollow Pines",
            "Meridian Drift",
            "The Glass Orchard"
        ]
    );
    for item in items {
        assert!(item["UserData"]["PlaybackPositionTicks"].as_i64().unwrap() > 0);
        assert!(item["UserData"]["PlayedPercentage"].as_f64().unwrap() > 0.0);
        assert_eq!(item["UserData"]["Played"], false);
    }

    let limited = get_json(&server, "/UserItems/Resume?limit=2").await;
    assert_eq!(limited["Items"].as_array().unwrap().len(), 2);
    server.shutdown().await;
}

#[tokio::test]
async fn next_up_picks_next_unplayed_episode_per_series() {
    let server = start().await;
    let body = get_json(
        &server,
        "/Shows/NextUp?userId=00000000000000000000000000000d01&limit=10",
    )
    .await;
    let items = body["Items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let pairs: Vec<(String, String, i64)> = items
        .iter()
        .map(|i| {
            (
                i["SeriesName"].as_str().unwrap().to_owned(),
                i["Name"].as_str().unwrap().to_owned(),
                i["ParentIndexNumber"].as_i64().unwrap(),
            )
        })
        .collect();
    assert!(pairs.contains(&("Harbor Lights".into(), "Episode 2".into(), 1)));
    assert!(pairs.contains(&("The Quiet Engine".into(), "Episode 2".into(), 1)));
    server.shutdown().await;
}

#[tokio::test]
async fn latest_is_a_bare_array_of_leaf_items() {
    let server = start().await;
    let client = Client::new();
    let response = client
        .get(format!("{}/Items/Latest?limit=5", server.base_url()))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let array = body.as_array().expect("bare array, not an object");
    assert_eq!(array.len(), 5);
    assert!(
        array
            .iter()
            .all(|item| matches!(item["Type"].as_str(), Some("Movie" | "Episode" | "Audio")))
    );

    let scoped = get_json(
        &server,
        "/Items/Latest?parentId=00000000000000000000000000000003&limit=50",
    )
    .await;
    let scoped = scoped.as_array().unwrap();
    assert_eq!(scoped.len(), 6);
    assert!(scoped.iter().all(|item| item["Type"] == "Audio"));
    server.shutdown().await;
}

#[tokio::test]
async fn playback_info_offers_direct_play_and_transcode() {
    let server = start().await;
    let id = first_item_id(&server, "includeItemTypes=Movie&recursive=true").await;
    let response = Client::new()
        .post(format!("{}/Items/{id}/PlaybackInfo", server.base_url()))
        .header(header::AUTHORIZATION, auth_header())
        .json(&json!({"UserId": "00000000000000000000000000000d01", "EnableDirectPlay": true}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let sources = body["MediaSources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    let source = &sources[0];
    assert_eq!(source["Id"], id);
    assert_eq!(source["Container"], "mp4");
    assert_eq!(source["SupportsDirectPlay"], true);
    assert_eq!(source["SupportsTranscoding"], true);
    let transcode = source["TranscodingUrl"].as_str().unwrap();
    assert!(transcode.starts_with(&format!("/Videos/{id}/master.m3u8?")));
    assert!(!transcode.contains(TOKEN), "token must not appear in URLs");
    assert!(body["PlaySessionId"].is_string());

    let missing = Client::new()
        .post(format!(
            "{}/Items/ffffffffffffffffffffffffffffffff/PlaybackInfo",
            server.base_url()
        ))
        .header(header::AUTHORIZATION, auth_header())
        .json(&json!({}))
        .send()
        .await
        .expect("request")
        .status();
    assert_eq!(missing, StatusCode::NOT_FOUND);
    server.shutdown().await;
}

#[tokio::test]
async fn images_are_png_deterministic_per_id() {
    let server = start().await;
    let id = first_item_id(&server, "includeItemTypes=Movie&recursive=true").await;
    let fetch = |path: String| {
        let url = format!("{}{path}", server.base_url());
        async move {
            Client::new()
                .get(url)
                .header(header::AUTHORIZATION, auth_header())
                .send()
                .await
                .expect("request")
        }
    };
    let first: Response = fetch(format!("/Items/{id}/Images/Primary")).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()[header::CONTENT_TYPE], "image/png");
    let first_bytes = first.bytes().await.expect("bytes");
    assert_eq!(&first_bytes[..8], b"\x89PNG\r\n\x1a\n");

    let second = fetch(format!("/Items/{id}/Images/Primary"))
        .await
        .bytes()
        .await
        .expect("bytes");
    assert_eq!(first_bytes, second, "same id must yield identical bytes");

    let other_id = first_item_id(
        &server,
        "includeItemTypes=Movie&recursive=true&startIndex=1",
    )
    .await;
    let other = fetch(format!("/Items/{other_id}/Images/Primary"))
        .await
        .bytes()
        .await
        .expect("bytes");
    assert_ne!(
        first_bytes, other,
        "different ids should get different colours"
    );

    // Backdrops exist only for items flagged with them (Night Harbor has one).
    let backdrop_id = first_item_id(&server, "searchTerm=Night%20Harbor&recursive=true").await;
    assert_eq!(
        fetch(format!("/Items/{backdrop_id}/Images/Backdrop"))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        fetch(format!("/Items/{id}/Images/Thumb")).await.status(),
        StatusCode::NOT_FOUND
    );
    server.shutdown().await;
}

#[tokio::test]
async fn stream_supports_full_partial_and_unsatisfiable_ranges() {
    let server = start().await;
    let id = first_item_id(&server, "includeItemTypes=Movie&recursive=true").await;
    let client = Client::new();
    let url = format!("{}/Videos/{id}/stream.mp4?static=true", server.base_url());

    let full = client
        .get(&url)
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("request");
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(full.headers()[header::ACCEPT_RANGES], "bytes");
    let full_bytes = full.bytes().await.expect("bytes");
    assert_eq!(full_bytes.len(), 32 * 1024);

    let partial = client
        .get(&url)
        .header(header::AUTHORIZATION, auth_header())
        .header(header::RANGE, "bytes=100-199")
        .send()
        .await
        .expect("request");
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        partial.headers()[header::CONTENT_RANGE],
        "bytes 100-199/32768"
    );
    let partial_bytes = partial.bytes().await.expect("bytes");
    assert_eq!(partial_bytes.len(), 100);
    assert_eq!(partial_bytes[..], full_bytes[100..200]);

    let suffix = client
        .get(&url)
        .header(header::AUTHORIZATION, auth_header())
        .header(header::RANGE, "bytes=-10")
        .send()
        .await
        .expect("request");
    assert_eq!(suffix.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(suffix.bytes().await.expect("bytes").len(), 10);

    let unsatisfiable = client
        .get(&url)
        .header(header::AUTHORIZATION, auth_header())
        .header(header::RANGE, "bytes=99999999-")
        .send()
        .await
        .expect("request");
    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        unsatisfiable.headers()[header::CONTENT_RANGE],
        "bytes */32768"
    );

    let unauthenticated = client.get(&url).send().await.expect("request").status();
    assert_eq!(unauthenticated, StatusCode::UNAUTHORIZED);
    server.shutdown().await;
}

#[tokio::test]
async fn master_playlist_is_a_stub_playlist() {
    let server = start().await;
    let id = first_item_id(&server, "includeItemTypes=Movie&recursive=true").await;
    let response = Client::new()
        .get(format!("{}/Videos/{id}/master.m3u8", server.base_url()))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.expect("text");
    assert!(text.starts_with("#EXTM3U"));
    assert!(text.contains("#EXT-X-STREAM-INF"));

    let audio_id = first_item_id(&server, "includeItemTypes=Audio&recursive=true").await;
    let audio = Client::new()
        .get(format!(
            "{}/Videos/{audio_id}/master.m3u8",
            server.base_url()
        ))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("request")
        .status();
    assert_eq!(audio, StatusCode::BAD_REQUEST);
    server.shutdown().await;
}

#[tokio::test]
async fn playback_reports_are_recorded_in_order() {
    let server = start().await;
    let client = Client::new();
    let item = "00000000000000000000000000000100";
    let body = json!({"ItemId": item, "PositionTicks": 0, "IsPaused": false});
    for path in [
        "/Sessions/Playing",
        "/Sessions/Playing/Progress",
        "/Sessions/Playing/Stopped",
    ] {
        let status = client
            .post(format!("{}{path}", server.base_url()))
            .header(header::AUTHORIZATION, auth_header())
            .json(&body)
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(status, StatusCode::NO_CONTENT, "POST {path}");
    }

    let log = get_json(&server, "/__mock/sessions").await;
    let events = log.as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["Event"], "Playing");
    assert_eq!(events[1]["Event"], "Progress");
    assert_eq!(events[2]["Event"], "Stopped");
    assert_eq!(events[2]["Sequence"], 3);
    assert_eq!(events[0]["Body"]["ItemId"], item);

    let unauthenticated = client
        .post(format!("{}/Sessions/Playing", server.base_url()))
        .json(&body)
        .send()
        .await
        .expect("request")
        .status();
    assert_eq!(unauthenticated, StatusCode::UNAUTHORIZED);

    let malformed = client
        .post(format!("{}/Sessions/Playing/Progress", server.base_url()))
        .header(header::AUTHORIZATION, auth_header())
        .body("{oops")
        .send()
        .await
        .expect("request")
        .status();
    assert_eq!(malformed, StatusCode::BAD_REQUEST);
    assert_eq!(
        get_json(&server, "/__mock/sessions")
            .await
            .as_array()
            .unwrap()
            .len(),
        3
    );
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_routes_return_404_and_seed_is_deterministic() {
    let server = start().await;
    assert_eq!(
        get_status(&server, "/no/such/route", Some(&auth_header())).await,
        StatusCode::NOT_FOUND
    );

    let other = start().await;
    let first = get_json(&server, "/Items?recursive=true&sortBy=DateCreated").await;
    let second = get_json(&other, "/Items?recursive=true&sortBy=DateCreated").await;
    assert_eq!(first, second, "two servers must serve identical seed data");
    server.shutdown().await;
    other.shutdown().await;
}

#[tokio::test]
async fn bind_uses_loopback_and_requested_port() {
    let free = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let port = free.local_addr().expect("addr").port();
    drop(free);
    let server = spawn(MockConfig {
        port,
        ..MockConfig::default()
    })
    .await
    .expect("spawn on fixed port");
    assert!(server.addr.ip().is_loopback());
    assert_eq!(server.addr.port(), port);
    server.shutdown().await;
}

#[tokio::test]
async fn extra_libraries_expose_views_and_typed_items() {
    let server = spawn(MockConfig {
        extra_libraries: true,
        ..MockConfig::default()
    })
    .await
    .expect("spawn mock server");
    let views = get_json(&server, "/UserViews").await;
    let types: Vec<&str> = views["Items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|view| view["CollectionType"].as_str().expect("type"))
        .collect();
    assert_eq!(
        types,
        [
            "movies",
            "tvshows",
            "music",
            "boxsets",
            "books",
            "homevideos",
            "musicvideos",
            "livetv"
        ]
    );
    let books = get_json(&server, "/Items?recursive=true&includeItemTypes=Book").await;
    let mut names = item_names(&books);
    names.sort();
    assert_eq!(names, ["Field Notes", "The Harbor Almanac"]);
    assert_eq!(books["Items"][0]["IsFolder"], false);
    let videos = get_json(&server, "/Items?recursive=true&includeItemTypes=Video").await;
    assert_eq!(videos["TotalRecordCount"], 2);
    assert_eq!(videos["Items"][0]["MediaType"], "Video");
    let music_videos = get_json(&server, "/Items?recursive=true&includeItemTypes=MusicVideo").await;
    assert_eq!(music_videos["TotalRecordCount"], 2);
    let box_sets = get_json(&server, "/Items?recursive=true&includeItemTypes=BoxSet").await;
    assert_eq!(box_sets["TotalRecordCount"], 2);
    server.shutdown().await;
}
