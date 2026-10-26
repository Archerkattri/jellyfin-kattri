use jellyfin_api::{ApiError, ClientConfig, ItemQuery, JellyfinClient};
use mock_jellyfin::{MockConfig, spawn};
use serde_json::Value;

const TOKEN: &str = "mock-access-token-0001";

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

async fn request_log(server: &mock_jellyfin::RunningServer) -> Value {
    reqwest::Client::new()
        .get(format!("{}/__mock/requests", server.base_url()))
        .send()
        .await
        .expect("read request log")
        .error_for_status()
        .expect("request log status")
        .json()
        .await
        .expect("request log JSON")
}

#[tokio::test]
async fn mock_people_endpoints_work_through_the_typed_client() {
    let server = spawn(MockConfig::default())
        .await
        .expect("mock Jellyfin starts");
    let client = client(&server.base_url());
    let session = client.authenticate("demo", "demo").await.expect("sign in");
    let movies = client
        .list_items(
            &session,
            &ItemQuery {
                limit: 1,
                include_item_types: Some(vec!["Movie".into()]),
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list seeded movie");
    let movie = movies.items.first().expect("seeded movie");
    let person = movie
        .people
        .as_deref()
        .and_then(|people| people.first())
        .expect("seeded cast credit");
    let person_id = person.id.as_deref().expect("person ID");
    let person_name = person.name.as_deref().expect("person name");

    let similar = client
        .list_similar(&session, &movie.id, 12)
        .await
        .expect("similar movies");
    assert!(!similar.items.is_empty());
    assert!(similar.items.iter().all(|item| item.id != movie.id));

    let filmography = client
        .list_person_items(&session, person_id, 50)
        .await
        .expect("person filmography");
    assert!(filmography.items.iter().any(|item| item.id == movie.id));

    let portrait = client
        .get_person_image(&session, person_name, person.primary_image_tag.as_deref())
        .await
        .expect("person portrait");
    assert!(!portrait.is_empty());

    let log = request_log(&server).await;
    let requests = log.as_array().expect("request entries");
    assert!(requests.iter().any(|entry| {
        entry["Path"]
            .as_str()
            .is_some_and(|path| path.starts_with("/Items/") && path.contains("/Similar?"))
    }));
    assert!(requests.iter().any(|entry| {
        entry["Path"]
            .as_str()
            .is_some_and(|path| path.starts_with("/Items?") && path.contains("personIds="))
    }));
    assert!(requests.iter().any(|entry| {
        entry["Path"]
            .as_str()
            .is_some_and(|path| path.starts_with("/Persons/") && path.contains("/Images/Primary"))
    }));
    assert!(!log.to_string().contains(TOKEN));
    server.shutdown().await;
}

#[tokio::test]
async fn optional_similar_endpoint_server_error_is_typed_for_the_ui_to_hide() {
    let server = spawn(MockConfig {
        similar_endpoint_status: Some(500),
        ..MockConfig::default()
    })
    .await
    .expect("mock Jellyfin starts");
    let client = client(&server.base_url());
    let session = client.authenticate("demo", "demo").await.expect("sign in");
    let result = client
        .list_similar(&session, "00000000000000000000000000000101", 12)
        .await;
    assert!(matches!(
        result,
        Err(ApiError::HttpStatus(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ))
    ));
    server.shutdown().await;
}
