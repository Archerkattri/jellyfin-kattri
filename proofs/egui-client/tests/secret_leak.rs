//! End-to-end redaction checks for the client API and playback-relay boundary.
//! The mock's fixed access token and the transcode URL's fake auth query are sentinels.

use std::sync::{Arc, Mutex};

use jellyfin_api::{
    ClientConfig, ItemQuery, JellyfinClient, PlayMethod, PlaybackInfoRequest,
    PlaybackSourceRequestOptions,
};
use mock_jellyfin::{MockConfig, spawn};
use serde_json::json;
use tracing::field::Visit;

const SENTINEL_API_KEY: &str = "kattri-sentinel-api-key-do-not-log";
const SENTINEL_TOKEN: &str = "mock-access-token-0001";

#[derive(Clone, Default)]
struct CapturedEvents(Arc<Mutex<Vec<(String, String)>>>);

impl tracing::Subscriber for CapturedEvents {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        // Capture product diagnostics at every level; transport-library trace
        // events are intentionally outside the application's logging surface.
        [
            "jellyfin_api",
            "jellyfin_egui_client",
            "jellyfin_playback_relay",
        ]
        .iter()
        .any(|target| metadata.target().starts_with(target))
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write as _;
                let _ = write!(&mut self.0, "{}={value:?};", field.name());
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        if let Ok(mut events) = self.0.lock() {
            events.push((event.metadata().target().to_owned(), fields.0));
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
}

#[test]
fn connect_browse_and_playback_paths_do_not_leak_secrets_or_upstream_host() {
    runtime().block_on(async {
        let server = spawn(MockConfig::default())
            .await
            .expect("mock server starts");
        let base_url = server.base_url();
        let config = ClientConfig::new(
            &base_url,
            "Kattri secret-leak test",
            "test device",
            "secret-leak-test-device",
            "test-version",
        )
        .expect("test URL is valid");
        let client = JellyfinClient::new(config).expect("client initializes");

        let events = CapturedEvents::default();
        let dispatch = tracing::Dispatch::new(events.clone());
        let _subscriber = tracing::dispatcher::set_default(&dispatch);

        client.public_system_info().await.expect("public info");
        let session = client
            .authenticate("demo", "demo")
            .await
            .expect("mock sign-in");
        let views = client.user_views(&session).await.expect("library views");
        if let Some(tv_view) = views
            .iter()
            .find(|view| view.collection_type.as_deref() == Some("tvshows"))
            && let Some(series) = client
                .list_items(
                    &session,
                    &ItemQuery {
                        parent_id: Some(tv_view.id.clone()),
                        include_item_types: Some(vec!["Series".to_owned()]),
                        limit: 1,
                        ..ItemQuery::default()
                    },
                )
                .await
                .expect("browse TV library")
                .items
                .first()
        {
            let _ = client
                .list_seasons(&session, &series.id)
                .await
                .expect("browse series seasons");
        }
        let movie_view = views
            .iter()
            .find(|view| view.collection_type.as_deref() == Some("movies"))
            .expect("mock movie library");
        let movies = client
            .list_items(
                &session,
                &ItemQuery {
                    parent_id: Some(movie_view.id.clone()),
                    include_item_types: Some(vec!["Movie".to_owned()]),
                    limit: 1,
                    ..ItemQuery::default()
                },
            )
            .await
            .expect("browse movie library");
        let movie = movies.items.first().expect("mock movie item");
        let playback = client
            .playback_info(&session, &movie.id, &PlaybackInfoRequest::default())
            .await
            .expect("read PlaybackInfo");

        // The mock response is deliberately replaced in memory with an auth-bearing
        // server URL to exercise the production URL sanitizer without making a request.
        let source: jellyfin_api::MediaSourceInfo = serde_json::from_value(json!({
            "Container": "mkv",
            "SupportsTranscoding": true,
            "TranscodingUrl": format!(
                "{base_url}/Videos/{}/master.m3u8?api_key={SENTINEL_API_KEY}&token={SENTINEL_TOKEN}",
                movie.id
            )
        }))
        .expect("fixture source");
        assert!(!playback.media_sources.is_empty());
        let source_request = client
            .build_playback_source_request(
                &session,
                PlaybackSourceRequestOptions {
                    item_id: &movie.id,
                    media_source: &source,
                    play_method: PlayMethod::Transcode,
                    play_session_id: None,
                    audio_stream_index: None,
                    subtitle_stream_index: None,
                    start_time_ticks: None,
                },
            )
            .expect("sanitized source request");
        let source_debug = format!("{source_request:?}");
        assert!(!source_debug.contains(SENTINEL_TOKEN));
        assert!(!source_debug.contains(SENTINEL_API_KEY));
        let (relay_url, relay_handle) =
            jellyfin_playback_relay::start_relay(&base_url, source_request)
                .expect("loopback relay starts");
        let relay_url_debug = format!("{relay_url:?}");
        let relay_handle_debug = format!("{relay_handle:?}");
        assert!(!relay_url_debug.contains(SENTINEL_TOKEN));
        assert!(!relay_url_debug.contains(SENTINEL_API_KEY));
        assert!(!relay_handle_debug.contains(SENTINEL_TOKEN));
        assert!(!relay_handle_debug.contains(SENTINEL_API_KEY));
        relay_handle.shutdown().expect("relay shuts down");

        let debug_output = format!("{client:?}\n{session:?}\n{source_debug}\n{relay_url_debug}");
        assert!(!debug_output.contains(SENTINEL_TOKEN));
        assert!(!debug_output.contains(SENTINEL_API_KEY));

        {
            let captured = events.0.lock().expect("captured tracing events");
            let logs = captured
                .iter()
                .map(|(_, fields)| fields.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!captured.is_empty(), "the server-path diagnostic was not captured");
            assert!(!logs.contains(SENTINEL_TOKEN));
            assert!(!logs.contains(SENTINEL_API_KEY));
            let upstream_address = server.addr.to_string();
            let leaking_targets = captured
                .iter()
                .filter(|(_, fields)| fields.contains(&upstream_address))
                .map(|(target, _)| target.as_str())
                .collect::<Vec<_>>();
            assert!(
                leaking_targets.is_empty(),
                "upstream host appeared in log events from: {leaking_targets:?}"
            );
        }

        server.shutdown().await;
    });
}
