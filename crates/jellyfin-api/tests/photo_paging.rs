//! Exercises Jellyfin `/Items` photo-folder paging with a large deterministic mock catalogue.

use std::collections::HashSet;

use jellyfin_api::{ClientConfig, ItemQuery, JellyfinClient};
use mock_jellyfin::{MockConfig, spawn};
use serde_json::Value;

const PHOTO_FOLDER_ID: &str = "0000000000000000000000000000f0f0";
const PHOTO_COUNT: usize = 1_000;
const PAGE_SIZE: u32 = 100;

fn client_for(base_url: &str) -> JellyfinClient {
    JellyfinClient::new(
        ClientConfig::new(
            base_url,
            "Kattri photo paging test",
            "test device",
            "photo-paging-test",
            "test-version",
        )
        .expect("valid loopback config"),
    )
    .expect("create client")
}

#[tokio::test]
async fn photo_folder_of_one_thousand_items_is_fetched_in_ten_unique_pages() {
    let server = spawn(MockConfig {
        photo_item_count: Some(PHOTO_COUNT),
        ..MockConfig::default()
    })
    .await
    .expect("start mock Jellyfin with 1,000 photos");
    let client = client_for(&server.base_url());
    let session = client
        .authenticate("demo", "demo")
        .await
        .expect("authenticate with mock server");

    let mut ids = HashSet::with_capacity(PHOTO_COUNT);
    let mut fetched = 0usize;
    for page_index in 0..10u32 {
        let page = client
            .list_items(
                &session,
                &ItemQuery {
                    parent_id: Some(PHOTO_FOLDER_ID.to_owned()),
                    include_item_types: Some(vec!["Photo".to_owned()]),
                    start_index: page_index * PAGE_SIZE,
                    limit: PAGE_SIZE,
                    ..ItemQuery::default()
                },
            )
            .await
            .expect("fetch photo page");

        assert_eq!(page.items.len(), PAGE_SIZE as usize);
        assert_eq!(page.total_record_count, Some(PHOTO_COUNT as u32));
        assert_eq!(page.start_index, page_index * PAGE_SIZE);
        for item in page.items {
            assert!(ids.insert(item.id), "duplicate photo ID across pages");
            fetched += 1;
        }
    }

    assert_eq!(fetched, PHOTO_COUNT);
    assert_eq!(ids.len(), PHOTO_COUNT);

    let requests: Value = reqwest::get(format!("{}/__mock/requests", server.base_url()))
        .await
        .expect("read mock request log")
        .error_for_status()
        .expect("request log status")
        .json()
        .await
        .expect("parse request log");
    let item_paths = requests
        .as_array()
        .expect("request log is an array")
        .iter()
        .filter_map(|entry| entry["Path"].as_str())
        .filter(|path| path.starts_with("/Items?"))
        .collect::<Vec<_>>();
    assert_eq!(item_paths.len(), 10, "exactly ten `/Items` page requests");
    for (page_index, path) in item_paths.into_iter().enumerate() {
        assert!(path.contains(&format!("parentId={PHOTO_FOLDER_ID}")));
        assert!(path.contains("includeItemTypes=Photo"));
        assert!(path.contains("limit=100"));
        assert!(path.contains(&format!("startIndex={}", page_index * PAGE_SIZE as usize)));
    }

    server.shutdown().await;
}
