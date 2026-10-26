//! A transcode-only MKV played through the relay, end to end against the in-process mock:
//! sign in, negotiate, build the source request, start the relay, then read the master
//! playlist, the media playlist and every segment through the loopback relay URL only.
//!
//! This proves the client, relay and mock agree on the HLS flow for this scenario. It does not
//! prove behavior against a real Jellyfin server or that libmpv decodes the segments.

use jellyfin_api::{
    ClientConfig, ItemQuery, JellyfinClient, PlayMethod, PlaybackInfoRequest,
    PlaybackSourceRequestOptions,
};
use mock_jellyfin::{MockConfig, spawn};
use url::Url;

const MOCK_TOKEN: &str = "mock-access-token-0001";

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client")
}

#[tokio::test]
async fn transcode_only_mkv_plays_through_the_relay_without_exposing_the_server() {
    let server = spawn(MockConfig {
        transcode_only_mkv: true,
        ..MockConfig::default()
    })
    .await
    .expect("mock starts");
    let base_url = server.base_url();
    let client = JellyfinClient::new(
        ClientConfig::new(&base_url, "Kattri test", "device", "dev-1", "0.0.0").expect("config"),
    )
    .expect("client");
    let session = client.authenticate("demo", "demo").await.expect("sign in");

    let views = client.user_views(&session).await.expect("views");
    let movies = views
        .iter()
        .find(|view| view.collection_type.as_deref() == Some("movies"))
        .expect("movie library");
    let movie = client
        .list_items(
            &session,
            &ItemQuery {
                parent_id: Some(movies.id.clone()),
                include_item_types: Some(vec!["Movie".to_owned()]),
                limit: 1,
                ..ItemQuery::default()
            },
        )
        .await
        .expect("movies")
        .items
        .into_iter()
        .next()
        .expect("a movie");

    let info = client
        .playback_info(&session, &movie.id, &PlaybackInfoRequest::default())
        .await
        .expect("playback info");
    let source = info.media_sources.first().expect("one source");
    assert_eq!(source.container.as_deref(), Some("mkv"));
    assert!(!source.supports_direct_play && !source.supports_direct_stream);
    assert!(source.supports_transcoding);

    let request = client
        .build_playback_source_request(
            &session,
            PlaybackSourceRequestOptions {
                item_id: &movie.id,
                media_source: source,
                play_method: PlayMethod::Transcode,
                play_session_id: info.play_session_id.as_deref(),
                audio_stream_index: None,
                subtitle_stream_index: None,
                start_time_ticks: None,
            },
        )
        .expect("source request");
    let (relay_url, relay) =
        jellyfin_playback_relay::start_relay(&base_url, request).expect("relay starts");

    let master_url = Url::parse(relay_url.as_str()).expect("relay URL");
    assert_eq!(master_url.host_str(), Some("127.0.0.1"));
    assert_ne!(master_url.port(), Some(server.addr.port()));

    let master = http().get(master_url.clone()).send().await.expect("master");
    assert_eq!(master.status(), 200);
    let master_body = master.text().await.expect("master body");
    assert!(master_body.starts_with("#EXTM3U"), "{master_body}");

    let hidden = [
        MOCK_TOKEN,
        "main.m3u8",
        "MediaSourceId",
        &server.addr.to_string(),
    ];
    let variant = master_body
        .lines()
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .expect("variant line");
    let media_url = master_url.join(variant).expect("variant URL");
    assert_eq!(media_url.host_str(), Some("127.0.0.1"));
    assert_eq!(
        media_url.port(),
        master_url.port(),
        "variant stays on the relay"
    );
    for secret in hidden {
        assert!(
            !master_body.contains(secret),
            "{secret} leaked: {master_body}"
        );
    }

    let media = http()
        .get(media_url.clone())
        .send()
        .await
        .expect("media playlist");
    assert_eq!(media.status(), 200);
    let media_body = media.text().await.expect("media playlist body");
    assert!(media_body.contains("#EXT-X-ENDLIST"), "{media_body}");
    for secret in ["hls1", "runtimeTicks", ".ts?", MOCK_TOKEN] {
        assert!(
            !media_body.contains(secret),
            "{secret} leaked: {media_body}"
        );
    }
    let segments: Vec<&str> = media_body
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(segments.len(), 3, "{media_body}");

    for segment in segments {
        let url = media_url.join(segment).expect("segment URL");
        assert_eq!(url.port(), master_url.port());
        let response = http().get(url).send().await.expect("segment");
        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("video/mp2t")
        );
        assert_eq!(
            response.bytes().await.expect("segment body").len(),
            32 * 1024
        );
    }

    // The loopback URL is capability-gated: a wrong capability gets nothing.
    let mut wrong = master_url.clone();
    wrong.set_path("/not-the-capability/not-a-resource");
    assert_eq!(http().get(wrong).send().await.expect("wrong").status(), 404);

    relay.shutdown().expect("relay stops");
    server.shutdown().await;
}
