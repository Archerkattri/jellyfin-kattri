//! Search (M3-17) and admin-lite read-only calls (M4-22) through the typed
//! client, against `mock-jellyfin`. The mock is not a real Jellyfin server, so
//! passing here does not confirm compatibility with one.

use jellyfin_api::{
    ClientConfig, JellyfinClient, SEARCH_ITEM_TYPES, SearchError, SearchGroup, Session,
    group_by_type,
};
use mock_jellyfin::{MockConfig, RunningServer, spawn};
use serde_json::Value;

fn client(base_url: &str) -> JellyfinClient {
    let config = ClientConfig::new(
        base_url,
        "Kattri test",
        "test device",
        "device-id",
        "test-version",
    )
    .expect("valid client configuration");
    JellyfinClient::new(config).expect("configured Jellyfin client")
}

async fn signed_in(server: &RunningServer) -> (JellyfinClient, Session) {
    let client = client(&server.base_url());
    let session = client.authenticate("demo", "demo").await.expect("sign in");
    (client, session)
}

/// Sanitized request-log entries (`Path` includes the query string).
async fn request_paths(server: &RunningServer) -> Vec<String> {
    reqwest::Client::new()
        .get(format!("{}/__mock/requests", server.base_url()))
        .send()
        .await
        .expect("read request log")
        .error_for_status()
        .expect("request log status")
        .json::<Value>()
        .await
        .expect("request log JSON")
        .as_array()
        .expect("request entries")
        .iter()
        .filter_map(|entry| entry["Path"].as_str().map(str::to_owned))
        .collect()
}

fn item_search_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| path.starts_with("/Items?") && path.contains("searchTerm="))
        .cloned()
        .collect()
}

#[tokio::test]
async fn search_matches_across_audio_and_album_types_in_one_group() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;

    // "Tide" matches the track "Low Tide" (Audio) and the album
    // "Tidewater Sessions" (MusicAlbum) in the mock's seed.
    let response = client
        .search_items(&session, "tide", 0, 50)
        .await
        .expect("search");
    let types: Vec<&str> = response
        .items
        .iter()
        .filter_map(|item| item.item_type.as_deref())
        .collect();
    assert!(types.contains(&"Audio"), "types: {types:?}");
    assert!(types.contains(&"MusicAlbum"), "types: {types:?}");

    let groups = group_by_type(&response.items);
    let order: Vec<SearchGroup> = groups.iter().map(|group| group.group).collect();
    assert_eq!(
        order,
        vec![SearchGroup::Music],
        "empty groups must be hidden"
    );
    assert_eq!(groups[0].items.len(), response.items.len());

    let paths = item_search_paths(&request_paths(&server).await);
    assert_eq!(paths.len(), 1, "paths: {paths:?}");
    let params: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(paths[0].split_once('?').map_or("", |(_, q)| q).as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
    assert_eq!(
        params.get("includeItemTypes").map(String::as_str),
        Some(SEARCH_ITEM_TYPES.join(",").as_str())
    );
    assert_eq!(params.get("searchTerm").map(String::as_str), Some("tide"));
}

#[tokio::test]
async fn channel_search_filters_the_paged_live_tv_lineup_and_groups_tv_channels() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;

    // The deterministic 150-channel mock requires two API pages. Matching is
    // local because Jellyfin's GetLiveTvChannels operation has no searchTerm.
    let page = client
        .search_live_tv_channels(&session, "channel 1", 0, 3)
        .await
        .expect("search channels");
    assert_eq!(page.total_record_count, Some(51));
    assert_eq!(page.start_index, 0);
    assert_eq!(
        page.items
            .iter()
            .map(|channel| channel.item.name.as_str())
            .collect::<Vec<_>>(),
        ["Mock Channel 100", "Mock Channel 101", "Mock Channel 102"]
    );
    assert!(
        page.items
            .iter()
            .all(|channel| { channel.item.item_type.as_deref() == Some("TvChannel") })
    );

    let channel_items = page
        .items
        .iter()
        .map(|channel| channel.item.clone())
        .collect::<Vec<_>>();
    let grouped = group_by_type(&channel_items);
    assert_eq!(grouped.len(), 1);
    assert_eq!(grouped[0].group, SearchGroup::Channels);

    let requests = request_paths(&server).await;
    let channel_paths: Vec<_> = requests
        .iter()
        .filter(|path| path.starts_with("/LiveTv/Channels?"))
        .collect();
    assert_eq!(channel_paths.len(), 2, "requests: {channel_paths:?}");
    for path in &channel_paths {
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(path.split_once('?').map_or("", |(_, q)| q).as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
        assert!(!params.contains_key("searchTerm"));
        assert_eq!(params.get("limit").map(String::as_str), Some("100"));
    }
    assert!(channel_paths[0].contains("startIndex=0"));
    assert!(channel_paths[1].contains("startIndex=100"));

    let later_page = client
        .search_live_tv_channels(&session, "channel 1", 3, 3)
        .await
        .expect("search channel page 2");
    assert_eq!(later_page.total_record_count, Some(51));
    assert_eq!(later_page.start_index, 3);
    assert_eq!(later_page.items[0].item.name, "Mock Channel 103");
}

#[tokio::test]
async fn empty_channel_search_is_refused_without_a_live_tv_request() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;

    let before = request_paths(&server)
        .await
        .iter()
        .filter(|path| path.starts_with("/LiveTv/Channels?"))
        .count();
    assert!(matches!(
        client.search_live_tv_channels(&session, "  ", 0, 10).await,
        Err(SearchError::EmptyQuery)
    ));
    let after = request_paths(&server)
        .await
        .iter()
        .filter(|path| path.starts_with("/LiveTv/Channels?"))
        .count();
    assert_eq!(before, after);
}

#[tokio::test]
async fn search_pages_return_the_same_sequence_as_one_page() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;

    let all = client
        .search_items(&session, "tide", 0, 100)
        .await
        .expect("full search");
    let total = all.total_record_count.unwrap_or(0) as usize;
    assert_eq!(total, all.items.len());
    assert!(all.items.len() >= 2, "need at least two matches to page");

    let mut paged = Vec::new();
    let mut start = 0_u32;
    loop {
        let page = client
            .search_items(&session, "tide", start, 1)
            .await
            .expect("page");
        if page.items.is_empty() {
            break;
        }
        assert_eq!(page.items.len(), 1);
        paged.extend(page.items);
        start += 1;
    }
    assert_eq!(paged, all.items);
}

#[tokio::test]
async fn empty_or_blank_query_is_refused_before_any_request() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;
    let before = item_search_paths(&request_paths(&server).await).len();

    for term in ["", "   ", "\t\n"] {
        let result = client.search_items(&session, term, 0, 10).await;
        assert!(
            matches!(result, Err(SearchError::EmptyQuery)),
            "term {term:?} was not refused: {result:?}"
        );
    }

    let after = item_search_paths(&request_paths(&server).await).len();
    assert_eq!(before, after, "a refused query must not reach the server");
}

#[tokio::test]
async fn server_info_reads_identity_from_the_mock() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;

    let info = client.server_info(&session).await.expect("server info");
    assert_eq!(info.server_name.as_deref(), Some("Mock Jellyfin"));
    assert_eq!(info.version.as_deref(), Some("0.0.0-mock"));
    assert_eq!(info.startup_wizard_completed, Some(true));

    let paths = request_paths(&server).await;
    assert!(
        paths.iter().any(|path| path == "/System/Info"),
        "paths: {paths:?}"
    );
}

#[tokio::test]
async fn virtual_folders_lists_the_mock_libraries_in_server_order() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;

    let folders = client
        .virtual_folders(&session)
        .await
        .expect("virtual folders");
    let names: Vec<&str> = folders
        .iter()
        .filter_map(|folder| folder.name.as_deref())
        .collect();
    assert_eq!(names, vec!["Movies", "Shows", "Music"]);
    assert_eq!(folders[0].collection_type.as_deref(), Some("movies"));
    assert_eq!(folders[1].collection_type.as_deref(), Some("tvshows"));
    assert_eq!(folders[2].collection_type.as_deref(), Some("music"));
    assert!(folders.iter().all(|folder| folder.item_id.is_some()));
    assert!(folders.iter().all(|folder| folder.locations.len() == 1));

    let paths = request_paths(&server).await;
    assert!(
        paths.iter().any(|path| path == "/Library/VirtualFolders"),
        "paths: {paths:?}"
    );
}

/// M4-22 acceptance: the admin module exposes no mutating method. This is a
/// text check of the module source, as the M4 spec allows.
#[test]
fn admin_module_source_contains_no_mutating_method() {
    let source = include_str!("../src/admin.rs");
    for forbidden in [
        "POST", "PUT", "DELETE", "PATCH", ".post(", ".put(", ".delete(", ".patch(",
    ] {
        assert!(
            !source.contains(forbidden),
            "admin.rs must not contain {forbidden:?}"
        );
    }
    for (index, _) in source.match_indices("Method::") {
        let rest = &source[index + "Method::".len()..];
        assert!(
            rest.starts_with("GET"),
            "admin.rs uses a non-GET method near: {:?}",
            &rest[..rest.len().min(20)]
        );
    }
}
