//! Playlist and collection calls through the typed client, against
//! `mock-jellyfin`. The mock is not a real Jellyfin server, so passing here does
//! not confirm compatibility with one.

use jellyfin_api::{
    ApiError, ClientConfig, ItemQuery, JellyfinClient, MAX_PLAYLIST_IDS_PER_REQUEST, Session,
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

async fn request_log(server: &RunningServer) -> Vec<Value> {
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
        .clone()
}

/// Requests logged as `"{method} {path}"`, including the sanitized query string.
fn logged(log: &[Value]) -> Vec<String> {
    log.iter()
        .map(|entry| {
            format!(
                "{} {}",
                entry["Method"].as_str().unwrap_or_default(),
                entry["Path"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

async fn first_movie_ids(client: &JellyfinClient, session: &Session, count: u32) -> Vec<String> {
    client
        .list_items(
            session,
            &ItemQuery {
                limit: count,
                include_item_types: Some(vec!["Movie".into()]),
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list seeded movies")
        .items
        .into_iter()
        .map(|item| item.id)
        .collect()
}

const MISSING_PLAYLIST_ID: &str = "00000000000000000000000000000fff";

#[tokio::test]
async fn playlist_lifecycle_keeps_order_and_entry_ids_through_the_mock() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let movies = first_movie_ids(&client, &session, 2).await;
    let (first, second) = (movies[0].clone(), movies[1].clone());

    let playlist_id = client
        .create_playlist(&session, "Road trip", std::slice::from_ref(&first))
        .await
        .expect("create playlist");
    let listed_playlists = client
        .list_playlists(&session, 20)
        .await
        .expect("list playlists through Items");
    assert!(listed_playlists.items.iter().any(|item| {
        item.id == playlist_id
            && item.name == "Road trip"
            && item.item_type.as_deref() == Some("Playlist")
    }));

    // Duplicates are allowed: the first movie is appended again.
    client
        .add_playlist_items(&session, &playlist_id, &[second.clone(), first.clone()])
        .await
        .expect("add items");

    let page = client
        .list_playlist_items(&session, &playlist_id, 0, 10)
        .await
        .expect("list entries");
    let ids: Vec<&str> = page.items.iter().map(|e| e.item.id.as_str()).collect();
    assert_eq!(ids, [first.as_str(), second.as_str(), first.as_str()]);
    assert_eq!(page.total_record_count, Some(3));
    let entry_ids: Vec<String> = page
        .items
        .iter()
        .map(|e| e.playlist_item_id.clone().expect("entry ID"))
        .collect();
    assert_ne!(
        entry_ids[0], entry_ids[2],
        "duplicates have distinct entries"
    );

    // Move the last entry to the front; only the order changes.
    client
        .move_playlist_entry(&session, &playlist_id, &entry_ids[2], 0)
        .await
        .expect("move entry");
    let moved = client
        .list_playlist_items(&session, &playlist_id, 0, 10)
        .await
        .expect("list after move");
    let moved_entries: Vec<&str> = moved
        .items
        .iter()
        .map(|e| e.playlist_item_id.as_deref().expect("entry ID"))
        .collect();
    assert_eq!(
        moved_entries,
        [
            entry_ids[2].as_str(),
            entry_ids[0].as_str(),
            entry_ids[1].as_str()
        ]
    );

    // Remove the first occurrence of the duplicated movie by entry ID.
    client
        .remove_playlist_entries(&session, &playlist_id, std::slice::from_ref(&entry_ids[0]))
        .await
        .expect("remove one occurrence");
    let after_remove = client
        .list_playlist_items(&session, &playlist_id, 0, 10)
        .await
        .expect("list after remove");
    let remaining: Vec<&str> = after_remove
        .items
        .iter()
        .map(|e| e.playlist_item_id.as_deref().expect("entry ID"))
        .collect();
    assert_eq!(remaining, [entry_ids[2].as_str(), entry_ids[1].as_str()]);

    // Paging returns the requested window.
    let window = client
        .list_playlist_items(&session, &playlist_id, 1, 1)
        .await
        .expect("paged entries");
    assert_eq!(window.items.len(), 1);
    assert_eq!(window.start_index, 1);
    assert_eq!(
        window.items[0].playlist_item_id.as_deref(),
        Some(entry_ids[1].as_str())
    );

    client
        .rename_playlist(&session, &playlist_id, "Weekend")
        .await
        .expect("rename playlist");
    let dto = client
        .get_playlist(&session, &playlist_id)
        .await
        .expect("playlist metadata");
    // After removing the first duplicate, the remaining order is [first, second].
    assert_eq!(dto.item_ids, [first.clone(), second.clone()]);

    client
        .delete_playlist(&session, &playlist_id)
        .await
        .expect("delete playlist");
    let gone = client.get_playlist(&session, &playlist_id).await;
    assert!(matches!(
        gone,
        Err(ApiError::HttpStatus(status)) if status == reqwest::StatusCode::NOT_FOUND
    ));

    let calls = logged(&request_log(&server).await);
    let has = |prefix: String| calls.iter().any(|call| call.starts_with(&prefix));
    assert!(
        calls.iter().any(|call| call == "POST /Playlists"),
        "{calls:?}"
    );
    assert!(
        has(format!("POST /Playlists/{playlist_id}/Items?")),
        "{calls:?}"
    );
    assert!(
        has(format!("GET /Playlists/{playlist_id}/Items?")),
        "{calls:?}"
    );
    assert!(
        has(format!(
            "POST /Playlists/{playlist_id}/Items/{}/Move/0",
            entry_ids[2]
        )),
        "{calls:?}"
    );
    assert!(
        has(format!("DELETE /Playlists/{playlist_id}/Items?")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&format!("POST /Playlists/{playlist_id}")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&format!("DELETE /Items/{playlist_id}")),
        "{calls:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn long_id_lists_are_split_into_bounded_requests() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let movie = first_movie_ids(&client, &session, 1).await.remove(0);
    let playlist_id = client
        .create_playlist(&session, "Many", &[])
        .await
        .expect("create empty playlist");

    let ids = vec![movie.clone(); MAX_PLAYLIST_IDS_PER_REQUEST + 50];
    client
        .add_playlist_items(&session, &playlist_id, &ids)
        .await
        .expect("add in chunks");

    let page = client
        .list_playlist_items(&session, &playlist_id, 0, 100)
        .await
        .expect("first page");
    assert_eq!(page.total_record_count, Some(150));
    assert_eq!(page.items.len(), 100);

    let calls = logged(&request_log(&server).await);
    let adds = calls
        .iter()
        .filter(|call| call.starts_with(&format!("POST /Playlists/{playlist_id}/Items?")))
        .count();
    assert_eq!(adds, 2);

    server.shutdown().await;
}

#[tokio::test]
async fn invalid_ids_are_refused_before_any_request() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let before = request_log(&server).await.len();

    let bad = client.get_playlist(&session, "abc,def").await;
    assert!(matches!(bad, Err(ApiError::InvalidResponse)));
    let bad_add = client
        .add_playlist_items(&session, MISSING_PLAYLIST_ID, &["x y".to_owned()])
        .await;
    assert!(matches!(bad_add, Err(ApiError::InvalidResponse)));

    assert_eq!(request_log(&server).await.len(), before);
    server.shutdown().await;
}

#[tokio::test]
async fn missing_playlist_reports_not_found_and_mutations_do_not_create_it() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;

    let read = client.get_playlist(&session, MISSING_PLAYLIST_ID).await;
    assert!(matches!(
        read,
        Err(ApiError::HttpStatus(status)) if status == reqwest::StatusCode::NOT_FOUND
    ));
    let rename = client
        .rename_playlist(&session, MISSING_PLAYLIST_ID, "Ghost")
        .await;
    assert!(matches!(
        rename,
        Err(ApiError::HttpStatus(status)) if status == reqwest::StatusCode::NOT_FOUND
    ));
    let delete = client.delete_playlist(&session, MISSING_PLAYLIST_ID).await;
    assert!(matches!(
        delete,
        Err(ApiError::HttpStatus(status)) if status == reqwest::StatusCode::NOT_FOUND
    ));

    server.shutdown().await;
}

#[tokio::test]
async fn deleting_a_library_item_through_the_playlist_route_is_refused() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let movie = first_movie_ids(&client, &session, 1).await.remove(0);

    let result = client.delete_playlist(&session, &movie).await;
    assert!(matches!(result, Err(ApiError::Forbidden)), "{result:?}");
    server.shutdown().await;
}

#[tokio::test]
async fn list_playlists_requests_the_playlist_item_type() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;

    // The mock's /Items route does not return playlists (see the mock module
    // docs), so this asserts the request shape and a successful response only.
    client
        .list_playlists(&session, 50)
        .await
        .expect("list playlists");

    let calls = request_log(&server).await;
    let query = calls
        .iter()
        .filter_map(|entry| entry["Path"].as_str())
        .find(|path| path.starts_with("/Items?") && path.contains("includeItemTypes=Playlist"))
        .expect("playlist list request logged");
    assert!(query.contains("recursive=true"), "{query}");
    server.shutdown().await;
}

#[tokio::test]
async fn collection_members_request_is_scoped_to_the_box_set() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let box_set = client
        .list_items(
            &session,
            &ItemQuery {
                limit: 1,
                include_item_types: Some(vec!["BoxSet".into()]),
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list box sets")
        .items
        .remove(0);

    // Seeded box sets have no child items in the mock, so the member list is empty.
    let members = client
        .collection_members(&session, &box_set.id, 0, 50)
        .await
        .expect("collection members");
    assert!(members.items.is_empty());
    assert_eq!(members.total_record_count, Some(0));

    let log = request_log(&server).await;
    let query = log
        .iter()
        .filter_map(|entry| entry["Path"].as_str())
        .find(|path| path.contains(&format!("parentId={}", box_set.id)))
        .expect("parent-scoped request logged");
    assert!(query.contains("recursive=false"), "{query}");
    server.shutdown().await;
}

#[tokio::test]
async fn seeded_collection_has_four_ordered_items_and_a_nested_box_set() {
    let server = spawn(MockConfig {
        collection_children: true,
        ..MockConfig::default()
    })
    .await
    .expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let collections = client
        .list_items(
            &session,
            &ItemQuery {
                recursive: true,
                include_item_types: Some(vec!["BoxSet".to_owned()]),
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list box sets");
    let collection = collections
        .items
        .iter()
        .find(|item| item.name == "Coastal Anthology")
        .expect("seeded collection exists");
    let members = client
        .collection_members(&session, &collection.id, 0, 100)
        .await
        .expect("list collection children");

    assert_eq!(members.total_record_count, Some(5));
    assert_eq!(
        members
            .items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Anthology Film 1",
            "Anthology Film 2",
            "Anthology Film 3",
            "Anthology Film 4",
            "Coastal Shorts"
        ]
    );
    assert_eq!(
        members
            .items
            .last()
            .and_then(|item| item.item_type.as_deref()),
        Some("BoxSet")
    );
    server.shutdown().await;
}
