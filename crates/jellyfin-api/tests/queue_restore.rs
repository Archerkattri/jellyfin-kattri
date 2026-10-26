//! M3-02 queue restoration uses comma-delimited `/Items?ids=` queries capped
//! at 50 IDs, and preserves the saved queue order across server results.

use jellyfin_api::{ClientConfig, ItemQuery, JellyfinClient, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn};
use serde_json::Value;
use url::form_urlencoded;

fn client(base_url: &str) -> JellyfinClient {
    JellyfinClient::new(
        ClientConfig::new(
            base_url,
            "Kattri test",
            "test device",
            "device-id",
            "test-version",
        )
        .expect("valid loopback config"),
    )
    .expect("valid Jellyfin client")
}

async fn signed_in(server: &RunningServer) -> (JellyfinClient, Session) {
    let client = client(&server.base_url());
    let session = client.authenticate("demo", "demo").await.expect("sign in");
    (client, session)
}

#[tokio::test]
async fn restores_two_hundred_tracks_in_four_bounded_batches_and_queue_order() {
    let server = spawn(MockConfig {
        audio_item_count: Some(200),
        ..MockConfig::default()
    })
    .await
    .expect("start mock Jellyfin with 200 tracks");
    let (client, session) = signed_in(&server).await;
    let original = client
        .list_items(
            &session,
            &ItemQuery {
                include_item_types: Some(vec!["Audio".into()]),
                limit: 200,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list the 200-track fixture")
        .items;
    assert_eq!(original.len(), 200);
    let requested_ids = original
        .iter()
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();

    let restored = client
        .list_items_by_ids(&session, &requested_ids)
        .await
        .expect("restore queue items in bounded batches");
    assert_eq!(
        restored
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>(),
        requested_ids
    );

    let request_log: Value = reqwest::get(format!("{}/__mock/requests", server.base_url()))
        .await
        .expect("read request log")
        .error_for_status()
        .expect("request log status")
        .json()
        .await
        .expect("parse request log");
    let batches = request_log
        .as_array()
        .expect("request entries")
        .iter()
        .filter_map(|entry| entry["Path"].as_str())
        .filter(|path| path.contains("ids="))
        .collect::<Vec<_>>();
    assert_eq!(batches.len(), 4, "200 IDs require four requests");
    for path in batches {
        let query = path
            .split_once('?')
            .map(|(_, query)| query)
            .expect("query string");
        let decoded = form_urlencoded::parse(query.as_bytes())
            .find(|(name, _)| name == "ids")
            .map(|(_, value)| value.into_owned())
            .expect("ids query value");
        assert_eq!(decoded.split(',').count(), 50);
    }
    server.shutdown().await;
}

#[tokio::test]
async fn album_children_are_scoped_to_the_album_and_return_audio_tracks() {
    let server = spawn(MockConfig::default())
        .await
        .expect("start mock Jellyfin");
    let (client, session) = signed_in(&server).await;
    let album = client
        .list_items(
            &session,
            &ItemQuery {
                include_item_types: Some(vec!["MusicAlbum".into()]),
                limit: 10,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("find a seeded album")
        .items
        .into_iter()
        .find(|item| item.name == "Tidewater Sessions")
        .expect("four-track album exists");

    let tracks = client
        .list_items(
            &session,
            &ItemQuery {
                recursive: false,
                parent_id: Some(album.id.clone()),
                include_item_types: Some(vec!["Audio".into()]),
                limit: 100,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list album tracks")
        .items;
    assert_eq!(tracks.len(), 4);
    assert!(
        tracks
            .iter()
            .all(|track| track.item_type.as_deref() == Some("Audio"))
    );
    let indexes = tracks
        .iter()
        .filter_map(|track| track.index_number)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(indexes, [1, 2, 3, 4].into_iter().collect());

    let log: Value = reqwest::get(format!("{}/__mock/requests", server.base_url()))
        .await
        .expect("read mock request log")
        .error_for_status()
        .expect("request log status")
        .json()
        .await
        .expect("decode request log");
    let album_query = log
        .as_array()
        .expect("request entries")
        .iter()
        .filter_map(|entry| entry["Path"].as_str())
        .find(|path| path.contains("parentId="))
        .expect("album child request is logged");
    assert!(album_query.contains("includeItemTypes=Audio"));
    assert!(album_query.contains(&format!("parentId={}", album.id)));
    assert!(album_query.contains("recursive=false"));

    server.shutdown().await;
}
