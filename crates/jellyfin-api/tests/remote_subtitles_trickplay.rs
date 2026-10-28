//! Remote subtitle search and download, and trickplay manifest and tiles, through the typed
//! client against the in-process mock. This validates the client against the mock only; the
//! subtitle download writes to the server, so it is never run against a real one here.

use jellyfin_api::{
    ApiError, ClientConfig, ItemQuery, ItemSubtitleStream, JellyfinClient, Session,
};
use mock_jellyfin::{MockConfig, RunningServer, spawn};

async fn signed_in(server: &RunningServer) -> (JellyfinClient, Session) {
    let config = ClientConfig::new(
        &server.base_url(),
        "Kattri test",
        "test device",
        "device-id",
        "test-version",
    )
    .expect("valid config");
    let client = JellyfinClient::new(config).expect("loopback HTTP is allowed");
    let session = client
        .authenticate("demo", "demo")
        .await
        .expect("demo sign-in");
    (client, session)
}

async fn first_episode(client: &JellyfinClient, session: &Session) -> String {
    client
        .list_items(
            session,
            &ItemQuery {
                recursive: true,
                include_item_types: Some(vec!["Episode".to_owned()]),
                limit: 1,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("list items")
        .items
        .first()
        .expect("seed has an episode")
        .id
        .clone()
}

#[tokio::test]
async fn searching_and_downloading_a_subtitle_adds_an_external_stream() {
    let server = spawn(MockConfig::default()).await.expect("mock");
    let (client, session) = signed_in(&server).await;
    let episode = first_episode(&client, &session).await;

    let results = client
        .search_remote_subtitles(&session, &episode, "eng")
        .await
        .expect("search");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, "mock-eng-1");
    assert_eq!(results[0].provider_name.as_deref(), Some("Mock Subtitles"));
    assert_eq!(results[0].download_count, Some(1200));
    assert_eq!(
        client
            .search_remote_subtitles_matching(&session, &episode, "eng", Some(true))
            .await
            .expect("perfect-match search")
            .len(),
        2
    );
    assert!(
        client
            .search_remote_subtitles(&session, &episode, "zxx")
            .await
            .expect("empty search")
            .is_empty()
    );
    assert!(matches!(
        client
            .search_remote_subtitles(&session, &episode, "../eng")
            .await,
        Err(ApiError::InvalidResponse)
    ));

    let before = client
        .item_subtitle_streams(&session, &episode)
        .await
        .expect("streams");
    assert!(before.is_empty());

    client
        .download_remote_subtitle(&session, &episode, "mock-eng-1")
        .await
        .expect("download");
    assert!(matches!(
        client
            .download_remote_subtitle(&session, &episode, "mock-eng-9")
            .await,
        Err(ApiError::HttpStatus(status)) if status == reqwest::StatusCode::NOT_FOUND
    ));

    let after = client
        .item_subtitle_streams(&session, &episode)
        .await
        .expect("streams");
    assert_eq!(after.len(), 1);
    let stream = &after[0];
    assert_eq!(stream.media_source_id, episode);
    assert_eq!(stream.codec, "srt");
    assert_eq!(stream.language.as_deref(), Some("eng"));
    assert!(stream.is_external && stream.is_text);

    let file = client
        .subtitle_stream_file(&session, &episode, stream)
        .await
        .expect("subtitle file");
    assert!(String::from_utf8_lossy(&file).contains("Mock eng subtitle"));

    let cultures = client.cultures(&session).await.expect("cultures");
    assert_eq!(
        cultures
            .iter()
            .map(|culture| culture.three_letter_code.as_str())
            .collect::<Vec<_>>(),
        ["eng", "fre", "ger"]
    );
}

#[tokio::test]
async fn a_video_lists_its_trickplay_resolution_and_serves_its_sheets() {
    let server = spawn(MockConfig::default()).await.expect("mock");
    let (client, session) = signed_in(&server).await;
    let episode = first_episode(&client, &session).await;

    let resolutions = client
        .trickplay_resolutions(&session, &episode, Some(&episode))
        .await
        .expect("manifest");
    assert_eq!(resolutions.len(), 1);
    let info = resolutions[0];
    assert_eq!(
        (info.width, info.tile_width, info.interval),
        (64, 10, 10_000)
    );

    let sheet = client
        .trickplay_tile(&session, &episode, Some(&episode), info.width, 0)
        .await
        .expect("sheet");
    assert!(!sheet.is_empty());
    assert!(matches!(
        client
            .trickplay_tile(&session, &episode, Some(&episode), 999, 0)
            .await,
        Err(ApiError::HttpStatus(status)) if status == reqwest::StatusCode::NOT_FOUND
    ));
    // Another source id has no manifest, so trickplay is simply absent.
    assert!(
        client
            .trickplay_resolutions(&session, &episode, Some("other-source"))
            .await
            .expect("manifest")
            .is_empty()
    );
}

#[tokio::test]
async fn subtitle_and_trickplay_routes_reject_invalid_ids_and_out_of_range_route_values() {
    let server = spawn(MockConfig::default()).await.expect("mock");
    let (client, session) = signed_in(&server).await;

    for result in [
        client
            .search_remote_subtitles(&session, "bad\nid", "eng")
            .await
            .map(|_| ()),
        client
            .download_remote_subtitle(&session, "item", "bad\nid")
            .await,
        client
            .trickplay_tile(&session, "item", None, i32::MAX as u32 + 1, 0)
            .await
            .map(|_| ()),
        client
            .trickplay_tile(&session, "item", None, 320, i32::MAX as u32 + 1)
            .await
            .map(|_| ()),
        client
            .trickplay_tile(&session, "bad\nid", None, 320, 0)
            .await
            .map(|_| ()),
        client
            .subtitle_stream_file(
                &session,
                "item",
                &ItemSubtitleStream {
                    media_source_id: "source".to_owned(),
                    index: -1,
                    codec: "srt".to_owned(),
                    language: None,
                    title: None,
                    is_external: false,
                    is_text: true,
                },
            )
            .await
            .map(|_| ()),
    ] {
        assert!(matches!(result, Err(ApiError::InvalidResponse)));
    }
}
