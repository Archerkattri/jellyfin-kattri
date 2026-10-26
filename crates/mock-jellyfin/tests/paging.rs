//! Integration coverage for library paging and optional query capabilities.

use mock_jellyfin::{MockConfig, RunningServer, spawn};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use std::collections::HashSet;

const TOKEN: &str = "mock-access-token-0001";

async fn start(config: MockConfig) -> RunningServer {
    spawn(config).await.expect("spawn paging fixture")
}

fn auth_header() -> String {
    format!(
        "MediaBrowser Client=\"paging-tests\", Device=\"ci\", DeviceId=\"paging-ci\", Version=\"0\", Token=\"{TOKEN}\""
    )
}

async fn get_json(server: &RunningServer, path: &str) -> Value {
    let response = Client::new()
        .get(format!("{}{path}", server.base_url()))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("send mock request");
    assert_eq!(response.status(), StatusCode::OK, "GET {path}");
    response.json().await.expect("decode mock response")
}

#[tokio::test]
async fn catalog_pages_are_ordered_complete_and_duplicate_free() {
    let server = start(MockConfig {
        item_count: Some(120),
        ..MockConfig::default()
    })
    .await;

    let mut ids = HashSet::new();
    let mut previous_created: Option<String> = None;
    for (offset, expected_len) in [(0, 50), (50, 50), (100, 20)] {
        let path = format!(
            "/Items?recursive=true&startIndex={offset}&limit=50&sortBy=DateCreated&sortOrder=Ascending&enableTotalRecordCount=true"
        );
        let body = get_json(&server, &path).await;
        assert_eq!(body["TotalRecordCount"], 120);
        assert_eq!(body["StartIndex"], offset);
        let items = body["Items"].as_array().expect("Items array");
        assert_eq!(items.len(), expected_len);

        for item in items {
            let id = item["Id"].as_str().expect("item ID");
            assert!(ids.insert(id.to_owned()), "duplicate item ID: {id}");

            let created = item["DateCreated"]
                .as_str()
                .expect("DateCreated string")
                .to_owned();
            if let Some(previous) = previous_created.as_deref() {
                assert!(previous <= created.as_str(), "catalog order regressed");
            }
            previous_created = Some(created);
        }
    }
    assert_eq!(ids.len(), 120);

    server.shutdown().await;
}

#[tokio::test]
async fn total_count_can_be_omitted_and_unsupported_audio_filter_is_reported() {
    let server = start(MockConfig {
        item_count: Some(120),
        reject_unplayed_filter_for_audio: true,
        ..MockConfig::default()
    })
    .await;

    let without_total = get_json(
        &server,
        "/Items?recursive=true&startIndex=0&limit=50&enableTotalRecordCount=false",
    )
    .await;
    assert_eq!(
        without_total["Items"]
            .as_array()
            .expect("Items array")
            .len(),
        50
    );
    assert!(without_total.get("TotalRecordCount").is_none());

    let response = Client::new()
        .get(format!(
            "{}/Items?includeItemTypes=Audio,MusicAlbum&filters=IsUnplayed",
            server.base_url()
        ))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("send unsupported-filter request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("decode error response");
    assert_eq!(
        body["Message"],
        "The item filter 'IsUnplayed' is unsupported."
    );

    server.shutdown().await;
}
