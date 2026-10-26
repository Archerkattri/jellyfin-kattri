//! Drives the real `JellyfinClient` against the in-process mock server over
//! loopback HTTP. This validates the client against the mock only, not
//! against a real Jellyfin server.

use jellyfin_api::{ApiError, ClientConfig, ItemQuery, JellyfinClient};
use mock_jellyfin::{MockConfig, spawn};

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

#[tokio::test]
async fn home_calls_work_against_mock_server() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let client = client_for(&server.base_url());

    let info = client.public_system_info().await.expect("public info");
    assert_eq!(info.server_name.as_deref(), Some("Mock Jellyfin"));

    let session = client.authenticate("demo", "demo").await.expect("sign in");

    let views = client.user_views(&session).await.expect("user views");
    let names: Vec<&str> = views
        .iter()
        .map(|view| view.name.as_deref().unwrap_or("Untitled library"))
        .collect();
    assert_eq!(names, ["Movies", "Shows", "Music"]);

    let resume = client.resume_items(&session, 10).await.expect("resume");
    assert_eq!(resume.items.len(), 4);
    assert!(resume.items.iter().all(|item| {
        item.user_data
            .as_ref()
            .and_then(|data| data.playback_position_ticks)
            .is_some_and(|ticks| ticks > 0)
    }));

    let next = client.next_up(&session, 10).await.expect("next up");
    assert_eq!(next.items.len(), 2);
    assert!(next.items.iter().all(|item| item.series_name.is_some()));

    let latest = client
        .latest_items(&session, None, 5)
        .await
        .expect("latest");
    assert_eq!(latest.len(), 5);

    let music_library = &views[2].id;
    let scoped = client
        .latest_items(&session, Some(music_library), 50)
        .await
        .expect("scoped latest");
    assert_eq!(scoped.len(), 6);

    let first = &resume.items[0];
    let item = client
        .get_item(&session, &first.id)
        .await
        .expect("get item");
    assert_eq!(item.id, first.id);
    let tag = item
        .image_tags
        .as_ref()
        .and_then(|tags| tags.get("Primary"))
        .and_then(Clone::clone)
        .expect("primary image tag");
    let image = client
        .get_primary_image(&session, &item.id, Some(&tag))
        .await
        .expect("primary image");
    assert!(!image.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn wrong_password_maps_to_reauthentication_required() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let client = client_for(&server.base_url());
    let result = client.authenticate("demo", "not-the-password").await;
    assert!(matches!(result, Err(ApiError::ReauthenticationRequired)));
    server.shutdown().await;
}

#[tokio::test]
async fn loopback_sign_in_browses_libraries_and_items_without_logging_auth_headers() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let client = client_for(&server.base_url());
    let session = client.authenticate("demo", "demo").await.expect("sign in");
    let views = client.user_views(&session).await.expect("list libraries");
    let items = client
        .list_items(&session, &ItemQuery::default())
        .await
        .expect("list library items");
    assert_eq!(views.len(), 3);
    assert!(!items.items.is_empty());

    let log_url = format!("{}/__mock/requests", server.base_url());
    let requests: serde_json::Value = reqwest::get(log_url)
        .await
        .expect("read sanitized request log")
        .json()
        .await
        .expect("decode sanitized request log");
    let rendered = requests.to_string().to_ascii_lowercase();
    for forbidden in ["authorization", "mock-access-token", "password", "api_key"] {
        assert!(
            !rendered.contains(forbidden),
            "request log exposed {forbidden}"
        );
    }
    assert!(rendered.contains("/items"));
    server.shutdown().await;
}

#[tokio::test]
async fn mock_home_returns_two_views_and_two_box_sets_with_parent_filtering() {
    let server = spawn(MockConfig {
        user_view_limit: Some(2),
        ..MockConfig::default()
    })
    .await
    .expect("spawn mock server");
    let client = client_for(&server.base_url());
    let session = client.authenticate("demo", "demo").await.expect("sign in");

    let views = client.user_views(&session).await.expect("user views");
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].name.as_deref(), Some("Movies"));
    assert_eq!(views[1].name.as_deref(), Some("Shows"));

    let collections = client.box_sets(&session, 16).await.expect("box sets");
    assert_eq!(collections.len(), 2);
    assert!(
        collections
            .iter()
            .all(|collection| collection.item_type.as_deref() == Some("BoxSet"))
    );

    let children = client
        .list_items(
            &session,
            &ItemQuery {
                parent_id: Some(collections[0].id.clone()),
                ..ItemQuery::default()
            },
        )
        .await
        .expect("empty collection children");
    assert!(children.items.is_empty());

    server.shutdown().await;
}
