//! Verifies bounded photo image requests against the in-process Jellyfin mock.

use jellyfin_api::{
    ApiError, ClientConfig, ItemQuery, JellyfinClient, PHOTO_FULL_SIZE_MAX_DIMENSION,
    PHOTO_THUMBNAIL_MAX_HEIGHT, PHOTO_THUMBNAIL_MAX_WIDTH,
};
use mock_jellyfin::{MockConfig, spawn};

fn client_for(base_url: &str) -> JellyfinClient {
    let config = ClientConfig::new(
        base_url,
        "Kattri test",
        "test device",
        "test-device",
        "test-version",
    )
    .expect("valid loopback config");
    JellyfinClient::new(config).expect("create client")
}

async fn request_paths(base_url: &str) -> Vec<String> {
    reqwest::get(format!("{base_url}/__mock/requests"))
        .await
        .expect("read mock request log")
        .json::<Vec<serde_json::Value>>()
        .await
        .expect("request log JSON")
        .into_iter()
        .filter_map(|entry| entry["Path"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn photo_image_calls_use_bounded_primary_image_routes_and_tags() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let base_url = server.base_url();
    let client = client_for(&base_url);
    let session = client
        .authenticate("demo", "demo")
        .await
        .expect("mock sign-in");
    let photos = client
        .list_items(
            &session,
            &ItemQuery {
                include_item_types: Some(vec!["Photo".into()]),
                limit: 100,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list mock photos");
    let photo_id = photos.items.first().expect("seeded photo").id.as_str();

    let thumbnail = client
        .get_photo_thumbnail(&session, photo_id, Some("thumb-tag"))
        .await
        .expect("fetch thumbnail");
    let full_size = client
        .get_photo_full_size_image(&session, photo_id, Some("full-tag"))
        .await
        .expect("fetch full-size image");
    assert!(thumbnail.starts_with(b"\x89PNG"));
    assert!(full_size.starts_with(b"\x89PNG"));

    let paths = request_paths(&base_url).await;
    let image_paths = paths
        .iter()
        .filter(|path| path.starts_with(&format!("/Items/{photo_id}/Images/Primary?")))
        .collect::<Vec<_>>();
    assert_eq!(image_paths.len(), 2);
    let thumbnail_path = image_paths
        .iter()
        .find(|path| path.contains("tag=thumb-tag"))
        .expect("thumbnail request path");
    let full_size_path = image_paths
        .iter()
        .find(|path| path.contains("tag=full-tag"))
        .expect("full-size request path");
    assert!(thumbnail_path.contains(&format!("maxWidth={PHOTO_THUMBNAIL_MAX_WIDTH}")));
    assert!(thumbnail_path.contains(&format!("maxHeight={PHOTO_THUMBNAIL_MAX_HEIGHT}")));
    assert!(full_size_path.contains(&format!("maxWidth={PHOTO_FULL_SIZE_MAX_DIMENSION}")));
    assert!(full_size_path.contains(&format!("maxHeight={PHOTO_FULL_SIZE_MAX_DIMENSION}")));
    server.shutdown().await;
}

#[tokio::test]
async fn photo_image_apis_reject_invalid_item_ids_before_network_access() {
    let client = client_for("http://127.0.0.1:1");
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let session = client_for(&server.base_url())
        .authenticate("demo", "demo")
        .await
        .expect("mock sign-in");

    assert!(matches!(
        client.get_photo_thumbnail(&session, "", None).await,
        Err(ApiError::InvalidResponse)
    ));
    assert!(matches!(
        client
            .get_photo_full_size_image(&session, "bad\nid", None)
            .await,
        Err(ApiError::InvalidResponse)
    ));
    server.shutdown().await;
}
