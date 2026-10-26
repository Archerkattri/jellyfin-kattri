use mock_jellyfin::{MockConfig, RunningServer, spawn};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;

const TOKEN: &str = "mock-access-token-0001";
const PERSON_ID: &str = "00000000000000000000000000000a11";
const PERSON_NAME: &str = "Mara Vale";

async fn start(config: MockConfig) -> RunningServer {
    spawn(config).await.expect("spawn mock server")
}

async fn get(server: &RunningServer, path: &str) -> reqwest::Response {
    Client::new()
        .get(format!("{}{path}", server.base_url()))
        .header(
            header::AUTHORIZATION,
            format!("MediaBrowser Token=\"{TOKEN}\""),
        )
        .send()
        .await
        .expect("request")
}

async fn json(server: &RunningServer, path: &str) -> Value {
    let response = get(server, path).await;
    assert_eq!(response.status(), StatusCode::OK, "GET {path}");
    response.json().await.expect("JSON response")
}

async fn first_movie(server: &RunningServer) -> Value {
    json(
        server,
        "/Items?recursive=true&includeItemTypes=Movie&limit=1",
    )
    .await["Items"][0]
        .clone()
}

#[tokio::test]
async fn seeded_video_has_sdk_shaped_people_metadata() {
    let server = start(MockConfig::default()).await;
    let movie = first_movie(&server).await;
    let people = movie["People"].as_array().expect("People array");
    assert_eq!(people.len(), 1);
    assert_eq!(people[0]["Name"], PERSON_NAME);
    assert_eq!(people[0]["Id"], PERSON_ID);
    assert_eq!(people[0]["Role"], "Lead");
    assert_eq!(people[0]["Type"], "Actor");
    assert!(people[0]["PrimaryImageTag"].as_str().is_some());
    server.shutdown().await;
}

#[tokio::test]
async fn person_ids_filter_returns_seeded_filmography() {
    let server = start(MockConfig::default()).await;
    let response = json(
        &server,
        &format!("/Items?recursive=true&includeItemTypes=Movie&personIds={PERSON_ID}"),
    )
    .await;
    let items = response["Items"].as_array().expect("filtered items");
    assert!(!items.is_empty());
    assert!(items.iter().all(|item| {
        item["People"]
            .as_array()
            .is_some_and(|people| people.iter().any(|person| person["Id"] == PERSON_ID))
    }));
    server.shutdown().await;
}

#[tokio::test]
async fn similar_items_are_shaped_and_exclude_the_source_item() {
    let server = start(MockConfig::default()).await;
    let movie = first_movie(&server).await;
    let id = movie["Id"].as_str().expect("movie ID");
    let response = json(&server, &format!("/Items/{id}/Similar?limit=5")).await;
    let items = response["Items"].as_array().expect("similar items");
    assert!(!items.is_empty());
    assert!(items.iter().all(|item| item["Id"] != id));
    assert!(items.iter().all(|item| {
        item["People"]
            .as_array()
            .is_some_and(|people| people.iter().any(|person| person["Id"] == PERSON_ID))
    }));
    server.shutdown().await;
}

#[tokio::test]
async fn person_primary_image_uses_name_route_and_returns_png() {
    let server = start(MockConfig::default()).await;
    let response = get(&server, "/Persons/Mara%20Vale/Images/Primary").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    let image = json(&server, "/__mock/requests").await;
    assert!(
        image
            .as_array()
            .expect("request log")
            .iter()
            .any(|entry| { entry["Path"] == "/Persons/Mara Vale/Images/Primary" })
    );
    server.shutdown().await;
}

#[tokio::test]
async fn similar_endpoint_can_return_configured_server_error() {
    let server = start(MockConfig {
        similar_endpoint_status: Some(500),
        ..MockConfig::default()
    })
    .await;
    let movie = first_movie(&server).await;
    let id = movie["Id"].as_str().expect("movie ID");
    assert_eq!(
        get(&server, &format!("/Items/{id}/Similar")).await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    server.shutdown().await;
}
