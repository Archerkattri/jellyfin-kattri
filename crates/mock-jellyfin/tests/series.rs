use mock_jellyfin::{MockConfig, RunningServer, spawn};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;

const TOKEN: &str = "mock-access-token-0001";
const USER_ID: &str = "00000000000000000000000000000d01";

async fn start(config: MockConfig) -> RunningServer {
    spawn(config).await.expect("spawn mock server")
}

fn auth_header() -> String {
    format!("MediaBrowser Token=\"{TOKEN}\"")
}

async fn get(server: &RunningServer, path: &str) -> reqwest::Response {
    Client::new()
        .get(format!("{}{path}", server.base_url()))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("request")
}

async fn json(server: &RunningServer, path: &str) -> Value {
    let response = get(server, path).await;
    assert_eq!(response.status(), StatusCode::OK, "GET {path}");
    response.json().await.expect("JSON response")
}

async fn series_id(server: &RunningServer) -> String {
    json(
        server,
        "/Items?recursive=true&includeItemTypes=Series&limit=1",
    )
    .await["Items"][0]["Id"]
        .as_str()
        .expect("series ID")
        .to_owned()
}

#[tokio::test]
async fn deterministic_series_include_three_seasons_and_two_episodes_each() {
    let server = start(MockConfig::default()).await;
    let series_id = series_id(&server).await;
    let seasons = json(
        &server,
        &format!("/Shows/{series_id}/Seasons?userId={USER_ID}"),
    )
    .await;
    let seasons = seasons["Items"].as_array().expect("seasons");
    assert_eq!(seasons.len(), 3);
    assert_eq!(
        seasons
            .iter()
            .filter_map(|s| s["IndexNumber"].as_i64())
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(
        seasons
            .iter()
            .find(|s| s["IndexNumber"] == 0)
            .expect("specials")["Name"],
        "Specials"
    );

    for season in seasons {
        let season_id = season["Id"].as_str().expect("season ID");
        let episodes = json(
            &server,
            &format!("/Shows/{series_id}/Episodes?seasonId={season_id}&userId={USER_ID}"),
        )
        .await;
        assert_eq!(episodes["TotalRecordCount"], 2);
        assert_eq!(
            episodes["Items"].as_array().expect("episode items").len(),
            2
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn missing_seasons_endpoint_allows_items_fallback_and_logs_both_requests() {
    let server = start(MockConfig {
        seasons_endpoint_not_found: true,
        ..MockConfig::default()
    })
    .await;
    let series_id = series_id(&server).await;

    let primary = get(
        &server,
        &format!("/Shows/{series_id}/Seasons?userId={USER_ID}"),
    )
    .await;
    assert_eq!(primary.status(), StatusCode::NOT_FOUND);

    let fallback_path =
        format!("/Items?parentId={series_id}&includeItemTypes=Season&userId={USER_ID}");
    let fallback = json(&server, &fallback_path).await;
    assert_eq!(fallback["TotalRecordCount"], 3);

    let request_log = json(&server, "/__mock/requests").await;
    let log = request_log.as_array().expect("request log");
    assert!(log.iter().any(|entry| {
        entry["Method"] == "GET"
            && entry["Path"] == format!("/Shows/{series_id}/Seasons?userId={USER_ID}")
    }));
    assert!(
        log.iter()
            .any(|entry| { entry["Method"] == "GET" && entry["Path"] == fallback_path })
    );
    server.shutdown().await;
}

#[tokio::test]
async fn episodes_endpoint_is_logged_with_season_and_user_query() {
    let server = start(MockConfig::default()).await;
    let series_id = series_id(&server).await;
    let season = json(
        &server,
        &format!("/Shows/{series_id}/Seasons?userId={USER_ID}"),
    )
    .await["Items"][1]["Id"]
        .as_str()
        .expect("season ID")
        .to_owned();
    let path = format!("/Shows/{series_id}/Episodes?seasonId={season}&userId={USER_ID}");
    let episodes = json(&server, &path).await;
    assert_eq!(episodes["TotalRecordCount"], 2);

    let log = json(&server, "/__mock/requests").await;
    assert!(
        log.as_array()
            .expect("request log")
            .iter()
            .any(|entry| { entry["Method"] == "GET" && entry["Path"] == path })
    );
    server.shutdown().await;
}

#[tokio::test]
async fn request_log_redacts_credential_query_values() {
    let server = start(MockConfig::default()).await;
    let response = get(
        &server,
        "/Items?includeItemTypes=Series&api_key=never-log-this-token",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let log = get(&server, "/__mock/requests")
        .await
        .json::<Value>()
        .await
        .expect("request log");
    let rendered = log.to_string();
    assert!(!rendered.contains("never-log-this-token"));
    assert!(rendered.contains("api_key=%5Bredacted%5D"));
    server.shutdown().await;
}
