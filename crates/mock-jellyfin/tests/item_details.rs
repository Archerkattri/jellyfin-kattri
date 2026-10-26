//! HTTP-level coverage for the item detail DTO and its read-only behavior.

use mock_jellyfin::{MockConfig, RunningServer, spawn};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;

const TOKEN: &str = "mock-access-token-0001";
const USER_ID: &str = "00000000000000000000000000000d01";

async fn start() -> RunningServer {
    start_with(MockConfig::default()).await
}

async fn start_with(config: MockConfig) -> RunningServer {
    spawn(config).await.expect("spawn mock server")
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
        .expect("GET request");
    assert_eq!(response.status(), StatusCode::OK, "GET {path}");
    response.json().await.expect("JSON response")
}

async fn first_item_id(server: &RunningServer, item_type: &str) -> String {
    let body = get_json(
        server,
        &format!("/Items?recursive=true&includeItemTypes={item_type}&limit=1"),
    )
    .await;
    body["Items"][0]["Id"]
        .as_str()
        .expect("seeded item ID")
        .to_owned()
}

async fn get_image(server: &RunningServer, id: &str, image_type: &str) -> reqwest::Response {
    Client::new()
        .get(format!(
            "{}/Items/{id}/Images/{image_type}?imageIndex=0&format=jpg",
            server.base_url()
        ))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("GET image")
}

#[tokio::test]
async fn full_item_detail_contains_fields_used_by_detail_screen() {
    let server = start().await;
    let id = first_item_id(&server, "Episode").await;
    let item = get_json(&server, &format!("/Items/{id}?userId={USER_ID}")).await;

    assert_eq!(item["Id"], id);
    assert_eq!(item["Type"], "Episode");
    assert!(item["Name"].as_str().is_some_and(|name| !name.is_empty()));
    assert!(item["Overview"].is_string());
    assert!(item["OfficialRating"].is_string());
    assert!(item["Genres"].is_array());
    assert!(item["ProductionYear"].is_number());
    assert!(item["RunTimeTicks"].as_i64().is_some_and(|ticks| ticks > 0));
    assert!(item["SeriesId"].is_string());
    assert!(item["SeriesName"].is_string());
    assert!(item["SeasonId"].is_string());
    assert!(item["ParentIndexNumber"].is_number());
    assert!(item["IndexNumber"].is_number());
    assert!(item["ImageTags"]["Primary"].is_string());
    assert!(item["UserData"]["Played"].is_boolean());
    assert!(item["UserData"]["PlaybackPositionTicks"].is_number());

    let events = get_json(&server, "/__mock/sessions").await;
    assert_eq!(events.as_array().expect("session log").len(), 0);
    server.shutdown().await;
}

#[tokio::test]
async fn minimal_item_detail_keeps_title_and_omits_optional_media_metadata() {
    let server = start().await;
    let id = first_item_id(&server, "BoxSet").await;
    let item = get_json(&server, &format!("/Items/{id}")).await;

    assert_eq!(item["Id"], id);
    assert_eq!(item["Type"], "BoxSet");
    assert!(item["Name"].as_str().is_some_and(|name| !name.is_empty()));
    assert!(
        !item
            .as_object()
            .expect("item object")
            .contains_key("RunTimeTicks")
    );
    assert!(
        !item
            .as_object()
            .expect("item object")
            .contains_key("ProductionYear")
    );
    assert!(
        !item
            .as_object()
            .expect("item object")
            .contains_key("SeriesName")
    );
    let events = get_json(&server, "/__mock/sessions").await;
    assert_eq!(events.as_array().expect("session log").len(), 0);
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_item_detail_returns_404_without_playback_or_report_events() {
    let server = start().await;
    let response = Client::new()
        .get(format!(
            "{}/Items/ffffffffffffffffffffffffffffffff",
            server.base_url()
        ))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("GET unknown item");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let events = get_json(&server, "/__mock/sessions").await;
    assert_eq!(events.as_array().expect("session log").len(), 0);
    server.shutdown().await;
}

#[tokio::test]
async fn backdrop_image_route_serves_bounded_image_bytes() {
    let server = start().await;
    let id = first_item_id(&server, "Series").await;
    let item = get_json(&server, &format!("/Items/{id}")).await;
    assert!(
        item["BackdropImageTags"]
            .as_array()
            .is_some_and(|tags| !tags.is_empty())
    );

    let response = get_image(&server, &id, "Backdrop").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    let bytes = response.bytes().await.expect("image body");
    assert!(!bytes.is_empty());
    assert!(bytes.len() <= 4 * 1024);
    server.shutdown().await;
}

#[tokio::test]
async fn missing_backdrop_returns_404_so_primary_can_be_requested() {
    let server = start().await;
    let id = first_item_id(&server, "BoxSet").await;

    let backdrop = get_image(&server, &id, "Backdrop").await;
    assert_eq!(backdrop.status(), StatusCode::NOT_FOUND);
    let primary = get_image(&server, &id, "Primary").await;
    assert_eq!(primary.status(), StatusCode::OK);
    assert!(
        !primary
            .bytes()
            .await
            .expect("primary image body")
            .is_empty()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn primary_image_route_works_when_dto_omits_image_tags() {
    let server = start_with(MockConfig {
        omit_primary_image_tags: true,
        ..MockConfig::default()
    })
    .await;
    let id = first_item_id(&server, "BoxSet").await;
    let item = get_json(&server, &format!("/Items/{id}")).await;
    assert!(item.get("ImageTags").is_none());

    let backdrop = get_image(&server, &id, "Backdrop").await;
    assert_eq!(backdrop.status(), StatusCode::NOT_FOUND);
    let primary = get_image(&server, &id, "Primary").await;
    assert_eq!(primary.status(), StatusCode::OK);
    assert!(
        !primary
            .bytes()
            .await
            .expect("untagged primary body")
            .is_empty()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn absent_backdrop_and_primary_images_reach_missing_artwork_case() {
    let server = start_with(MockConfig {
        primary_images_unavailable: true,
        ..MockConfig::default()
    })
    .await;
    let id = first_item_id(&server, "BoxSet").await;

    assert_eq!(
        get_image(&server, &id, "Backdrop").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_image(&server, &id, "Primary").await.status(),
        StatusCode::NOT_FOUND
    );
    server.shutdown().await;
}
