//! Administrator metadata editing, identify and image management through the typed client,
//! against `mock-jellyfin`. The mock is not a real Jellyfin server, so passing here does not
//! confirm compatibility with one.

use jellyfin_api::{ApiError, ClientConfig, ImageType, JellyfinClient, RemoteSearchQuery, Session};
use mock_jellyfin::{MockConfig, RunningServer, spawn};
use serde_json::Value;

fn client(base_url: &str) -> JellyfinClient {
    let config = ClientConfig::new(
        base_url,
        "Kattri test",
        "test device",
        "device-id",
        "test-version",
    )
    .expect("valid client configuration")
    // The mock serves provider thumbnails from its own loopback address.
    .with_loopback_provider_images();
    JellyfinClient::new(config).expect("configured Jellyfin client")
}

async fn signed_in(server: &RunningServer) -> (JellyfinClient, Session) {
    let client = client(&server.base_url());
    let session = client.authenticate("demo", "demo").await.expect("sign in");
    (client, session)
}

async fn request_paths(server: &RunningServer) -> Vec<String> {
    reqwest::Client::new()
        .get(format!("{}/__mock/requests", server.base_url()))
        .send()
        .await
        .expect("read request log")
        .json::<Value>()
        .await
        .expect("request log JSON")
        .as_array()
        .expect("request entries")
        .iter()
        .filter_map(|entry| {
            Some(format!(
                "{} {}",
                entry["Method"].as_str()?,
                entry["Path"].as_str()?
            ))
        })
        .collect()
}

/// The first movie in the mock catalog.
async fn first_movie(client: &JellyfinClient, session: &Session) -> String {
    let query = jellyfin_api::ItemQuery {
        include_item_types: Some(vec!["Movie".into()]),
        recursive: true,
        limit: 1,
        ..Default::default()
    };
    client
        .list_items(session, &query)
        .await
        .expect("movies")
        .items
        .into_iter()
        .next()
        .expect("a movie")
        .id
}

#[tokio::test]
async fn edited_metadata_is_saved_and_read_back() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;
    let id = first_movie(&client, &session).await;

    let item = client
        .get_item_metadata(&session, &id)
        .await
        .expect("load metadata");
    assert_eq!(item.item_type.as_deref(), Some("Movie"));
    let mut fields = item.fields.clone();
    fields.name = "Edited title".into();
    fields.sort_name = "Edited".into();
    fields.overview = "New overview".into();
    fields.premiere_date = "1999-12-31".into();
    fields.production_year = Some(1999);
    fields.community_rating = Some(8.5);
    fields.genres = vec!["Drama".into(), "Comedy".into()];
    fields.tags = vec!["kept".into()];
    fields.studios = vec!["Mock Studio".into()];
    fields.locked_fields = vec!["Name".into()];
    fields.lock_data = true;
    client
        .update_item_metadata(&session, &item, &fields)
        .await
        .expect("save metadata");

    let reloaded = client
        .get_item_metadata(&session, &id)
        .await
        .expect("reload metadata");
    assert_eq!(reloaded.fields, fields);
    let paths = request_paths(&server).await;
    assert!(paths.contains(&format!("POST /Items/{id}")), "{paths:?}");

    let mut empty = fields.clone();
    empty.name = "  ".into();
    assert!(matches!(
        client.update_item_metadata(&session, &item, &empty).await,
        Err(ApiError::InvalidConfiguration)
    ));
}

#[tokio::test]
async fn identify_searches_and_applies_a_match() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;
    let id = first_movie(&client, &session).await;

    let results = client
        .remote_search(
            &session,
            &id,
            "Movie",
            &RemoteSearchQuery {
                name: "The Matrix".into(),
                year: Some(1999),
                provider_ids: vec![("Tmdb".into(), " ".into())],
            },
        )
        .await
        .expect("remote search");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].name, "The Matrix");
    assert_eq!(results[0].production_year, Some(1999));
    assert_eq!(results[0].provider_name.as_deref(), Some("MockDb"));
    let poster = results[0].image_url.clone().expect("poster url");
    let bytes = client
        .get_remote_thumbnail(&poster)
        .await
        .expect("poster bytes");
    assert!(bytes.starts_with(b"\x89PNG"));

    client
        .apply_remote_search_result(&session, &id, &results[0], true)
        .await
        .expect("apply match");
    let item = client.get_item_metadata(&session, &id).await.expect("item");
    assert_eq!(item.fields.name, "The Matrix");
    assert_eq!(
        item.provider_ids,
        [("MockDb".to_owned(), "mock-1".to_owned())]
    );
    let paths = request_paths(&server).await;
    assert!(
        paths.contains(&format!(
            "POST /Items/RemoteSearch/Apply/{id}?replaceAllImages=true"
        )),
        "{paths:?}"
    );

    // Neither a name nor an id: refused before any request.
    assert!(matches!(
        client
            .remote_search(&session, &id, "Movie", &RemoteSearchQuery::default())
            .await,
        Err(ApiError::InvalidConfiguration)
    ));
    assert!(matches!(
        client
            .remote_search(
                &session,
                &id,
                "Episode",
                &RemoteSearchQuery {
                    name: "x".into(),
                    ..Default::default()
                }
            )
            .await,
        Err(ApiError::InvalidConfiguration)
    ));
}

#[tokio::test]
async fn images_are_listed_downloaded_and_deleted() {
    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let (client, session) = signed_in(&server).await;
    let id = first_movie(&client, &session).await;

    let images = client.item_images(&session, &id).await.expect("images");
    let primary = images
        .iter()
        .find(|image| image.image_type == ImageType::Primary)
        .expect("primary image");
    let bytes = client
        .get_item_image(&session, &id, primary)
        .await
        .expect("primary bytes");
    assert!(!bytes.is_empty());

    let remote = client
        .remote_images(&session, &id, ImageType::Backdrop, false)
        .await
        .expect("remote images");
    assert_eq!(remote.len(), 3);
    let thumbnail = remote[0].thumbnail_url.clone().expect("thumbnail url");
    client
        .get_remote_thumbnail(&thumbnail)
        .await
        .expect("thumbnail bytes");

    let backdrops = |images: &[jellyfin_api::ItemImage]| {
        images
            .iter()
            .filter(|image| image.image_type == ImageType::Backdrop)
            .count()
    };
    let before = backdrops(&images);
    client
        .download_remote_image(&session, &id, ImageType::Backdrop, &remote[0].url)
        .await
        .expect("download");
    let after_download = client.item_images(&session, &id).await.expect("images");
    assert_eq!(backdrops(&after_download), before + 1);

    client
        .delete_item_image(&session, &id, ImageType::Backdrop, 0)
        .await
        .expect("delete");
    let after_delete = client.item_images(&session, &id).await.expect("images");
    assert_eq!(backdrops(&after_delete), before);

    let paths = request_paths(&server).await;
    assert!(
        paths
            .iter()
            .any(|path| path.starts_with(&format!("GET /Items/{id}/RemoteImages?type=Backdrop"))),
        "{paths:?}"
    );
    assert!(
        paths.iter().any(|path| path.starts_with(&format!(
            "POST /Items/{id}/RemoteImages/Download?type=Backdrop&imageUrl="
        ))),
        "{paths:?}"
    );
    assert!(
        paths.contains(&format!("DELETE /Items/{id}/Images/Backdrop/0")),
        "{paths:?}"
    );

    // Deleting an image that is not there reports the server's 404.
    assert!(matches!(
        client
            .delete_item_image(&session, &id, ImageType::Banner, 0)
            .await,
        Err(ApiError::HttpStatus(status)) if status == reqwest::StatusCode::NOT_FOUND
    ));
}
