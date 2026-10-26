//! Media segment calls through the typed client against the in-process mock.
//! This validates the client against the mock only, not against a real server.

use jellyfin_api::{ApiError, ClientConfig, ItemQuery, JellyfinClient, MediaSegmentType, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn};
use reqwest::StatusCode;
use serde_json::Value;

fn client_for(base_url: &str) -> JellyfinClient {
    let config = ClientConfig::new(
        base_url,
        "Kattri test",
        "test device",
        "device-id",
        "test-version",
    )
    .expect("valid config");
    JellyfinClient::new(config).expect("loopback HTTP is allowed")
}

async fn signed_in(server: &RunningServer) -> (JellyfinClient, Session) {
    let client = client_for(&server.base_url());
    let session = client
        .authenticate("demo", "demo")
        .await
        .expect("demo sign-in");
    (client, session)
}

async fn first_id_of(client: &JellyfinClient, session: &Session, item_type: &str) -> String {
    client
        .list_items(
            session,
            &ItemQuery {
                recursive: true,
                include_item_types: Some(vec![item_type.to_owned()]),
                limit: 1,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list items")
        .items
        .first()
        .expect("seed has the item type")
        .id
        .clone()
}

async fn request_paths(server: &RunningServer) -> Vec<String> {
    let value: Value = reqwest::get(format!("{}/__mock/requests", server.base_url()))
        .await
        .expect("request log completes")
        .json()
        .await
        .expect("request log is JSON");
    value
        .as_array()
        .expect("request log is an array")
        .iter()
        .filter_map(|entry| entry["Path"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn episode_segments_are_typed_and_bounded() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let episode = first_id_of(&client, &session, "Episode").await;

    let response = client
        .get_media_segments(
            &session,
            &episode,
            &[MediaSegmentType::Intro, MediaSegmentType::Outro],
        )
        .await
        .expect("segments for an episode");

    assert!(
        !response.items.is_empty(),
        "the mock seeds episode segments"
    );
    for segment in &response.items {
        assert!(
            matches!(
                segment.segment_type,
                MediaSegmentType::Intro | MediaSegmentType::Outro
            ),
            "{:?}",
            segment.segment_type
        );
        assert_eq!(segment.item_id, episode);
        assert!(segment.start_ticks >= 0);
        assert!(segment.end_ticks > segment.start_ticks);
    }
    assert_eq!(
        response.total_record_count,
        i32::try_from(response.items.len()).ok()
    );
    server.shutdown().await;
}

/// Decoded query pairs for the last logged request to `/MediaSegments/{id}`.
fn logged_segment_query(paths: &[String], id: &str) -> Option<Vec<(String, String)>> {
    let prefix = format!("/MediaSegments/{id}");
    let path = paths.iter().rev().find(|path| path.starts_with(&prefix))?;
    let query = path.split_once('?').map_or("", |(_, query)| query);
    Some(
        url::form_urlencoded::parse(query.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect(),
    )
}

#[tokio::test]
async fn type_filter_is_sent_as_one_comma_separated_parameter() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let episode = first_id_of(&client, &session, "Episode").await;

    client
        .get_media_segments(
            &session,
            &episode,
            &[MediaSegmentType::Intro, MediaSegmentType::Outro],
        )
        .await
        .expect("filtered request");
    let filtered = logged_segment_query(&request_paths(&server).await, &episode)
        .expect("segment request logged");
    assert_eq!(
        filtered,
        vec![("includeSegmentTypes".to_owned(), "Intro,Outro".to_owned())]
    );

    client
        .get_media_segments(&session, &episode, &[])
        .await
        .expect("unfiltered request");
    let unfiltered = request_paths(&server).await;
    assert!(
        unfiltered
            .iter()
            .any(|path| path == &format!("/MediaSegments/{episode}")),
        "unfiltered request must omit the filter: {unfiltered:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn items_without_segments_return_an_empty_list() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let movie = first_id_of(&client, &session, "Movie").await;

    let response = client
        .get_media_segments(&session, &movie, &[])
        .await
        .expect("movie segments");
    assert!(response.items.is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_item_is_a_not_found_status() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;

    let result = client
        .get_media_segments(&session, "ffffffffffffffffffffffffffffffff", &[])
        .await;
    assert!(
        matches!(result, Err(ApiError::HttpStatus(StatusCode::NOT_FOUND))),
        "{result:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn malformed_item_id_is_refused_before_any_request() {
    let server = spawn(MockConfig::default()).await.expect("mock starts");
    let (client, session) = signed_in(&server).await;
    let before = request_paths(&server).await.len();

    let result = client.get_media_segments(&session, "../Users", &[]).await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(request_paths(&server).await.len(), before);
    server.shutdown().await;
}
