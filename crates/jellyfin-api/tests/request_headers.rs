//! Request-header security checks using loopback-only fixtures.

use jellyfin_api::{
    ApiError, ClientConfig, ExtraRequestHeader, JellyfinClient, MAX_EXTRA_REQUEST_HEADER_BYTES,
    MAX_EXTRA_REQUEST_HEADER_COUNT, MediaSourceInfo, PlayMethod, PlaybackSourceError,
    PlaybackSourceRequestOptions, RequestHeadersError,
};
use mock_jellyfin::{MockConfig, spawn};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

const TEST_SECRET: &str = "private-extra-header-value";

fn header(name: &str, value: impl Into<String>) -> ExtraRequestHeader {
    ExtraRequestHeader::new(name, value).expect("valid test header")
}

fn config(base_url: &str) -> ClientConfig {
    ClientConfig::new(
        base_url,
        "Kattri test",
        "test device",
        "device-id",
        "test-version",
    )
    .expect("valid test client config")
}

async fn read_request_headers(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let count = socket.read(&mut chunk).await.expect("read fixture request");
        assert_ne!(count, 0, "fixture client closed before sending headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 64 * 1024, "request headers are bounded");
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            return String::from_utf8_lossy(&bytes[..end + 4]).into_owned();
        }
    }
}

async fn spawn_http_fixture(response: Vec<u8>) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept fixture request");
        let request = read_request_headers(&mut socket).await;
        socket
            .write_all(&response)
            .await
            .expect("write fixture response");
        request
    });
    (format!("http://{address}"), task)
}

fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

#[test]
fn rejects_reserved_names_controls_duplicates_and_oversized_sets() {
    for name in ["", "Bad Header", "bad:name"] {
        assert_eq!(
            ExtraRequestHeader::new(name, "value").err(),
            Some(RequestHeadersError::InvalidName),
            "invalid header name {name:?}"
        );
    }

    for name in [
        "Authorization",
        "Cookie",
        "Set-Cookie",
        "X-Emby-Token",
        "X-MediaBrowser-Token",
        "X-MediaBrowser-Authorization",
        "X-Jellyfin-Token",
        "X-Api-Key",
        "X-Emby-Authorization",
        "X-Proxy-Identity",
        "Host",
        "Origin",
        "Referer",
        "Forwarded",
        "Via",
        "X-Forwarded-Host",
        "X-Original-URL",
        "Proxy-Authorization",
        "Proxy-Authenticate",
        "Proxy-Connection",
        "Connection",
        "Keep-Alive",
        "TE",
        "Trailer",
        "Upgrade",
        "Expect",
        "Transfer-Encoding",
        "Content-Length",
        "Content-Type",
        "Content-Encoding",
        "Content-Range",
        "Accept",
        "Accept-Ranges",
        "Range",
        "If-Range",
        "Accept-Encoding",
        "User-Agent",
        "X-HTTP-Method-Override",
        "X-Jellyfin-Authorization",
        "X-Auth-Token",
        "access_token",
        "api_key",
    ] {
        assert_eq!(
            ExtraRequestHeader::new(name, "value").err(),
            Some(RequestHeadersError::ForbiddenName),
            "reserved header name {name:?}"
        );
    }

    for value in [
        "line\nbreak",
        "line\rbreak",
        "nul\0byte",
        "del\u{7f}",
        "tab\tvalue",
    ] {
        assert_eq!(
            ExtraRequestHeader::new("X-Test", value).err(),
            Some(RequestHeadersError::InvalidValue),
            "control in value {value:?}"
        );
    }
    let secret_header = header("X-Test", TEST_SECRET);
    assert!(!format!("{secret_header:?}").contains(TEST_SECRET));
    let secret_error = ExtraRequestHeader::new("X-Test", format!("{TEST_SECRET}\r\ninvalid"));
    assert!(!format!("{:?}", secret_error.err()).contains(TEST_SECRET));

    let duplicate_config = config("https://jellyfin.example.test/")
        .with_extra_request_headers([header("X-Test", "first"), header("x-test", "second")]);
    assert_eq!(
        duplicate_config.err(),
        Some(RequestHeadersError::DuplicateName)
    );

    let too_many = (0..=MAX_EXTRA_REQUEST_HEADER_COUNT)
        .map(|index| header(&format!("X-Test-{index}"), "x"))
        .collect::<Vec<_>>();
    let count_config =
        config("https://jellyfin.example.test/").with_extra_request_headers(too_many);
    assert_eq!(
        count_config.err(),
        Some(RequestHeadersError::TooManyHeaders)
    );

    let too_large = vec![header("X-Test", "x".repeat(MAX_EXTRA_REQUEST_HEADER_BYTES))];
    let size_config =
        config("https://jellyfin.example.test/").with_extra_request_headers(too_large);
    assert_eq!(size_config.err(), Some(RequestHeadersError::TooLarge));
}

#[tokio::test]
async fn sends_custom_headers_to_the_configured_server_and_redacts_debug() {
    let (base_url, server) = spawn_http_fixture(http_response(
        "200 OK",
        "application/json",
        br#"{"ServerName":"fixture","Version":"1.0","Id":"fixture-id"}"#,
    ))
    .await;
    let config = config(&format!("{base_url}/jellyfin/"))
        .with_extra_request_headers([header("CF-Access-Client-Id", TEST_SECRET)])
        .expect("bounded extra headers");
    let config_debug = format!("{config:?}");
    assert!(!config_debug.contains(TEST_SECRET));

    let client = JellyfinClient::new(config).expect("loopback client");
    assert!(!format!("{client:?}").contains(TEST_SECRET));
    client.public_system_info().await.expect("fixture response");
    let request = server.await.expect("fixture task");
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("cf-access-client-id: {TEST_SECRET}"))
    );
    assert!(request.starts_with("GET /jellyfin/System/Info/Public "));
    assert!(
        !request
            .lines()
            .next()
            .unwrap_or_default()
            .contains(TEST_SECRET)
    );
}

#[tokio::test]
async fn custom_headers_are_not_sent_to_external_provider_images() {
    let (provider_url, provider) =
        spawn_http_fixture(http_response("200 OK", "image/png", b"png")).await;
    let config = config("http://127.0.0.1:9/")
        .with_loopback_provider_images()
        .with_extra_request_headers([header("CF-Access-Client-Id", TEST_SECRET)])
        .expect("bounded extra headers");
    let client = JellyfinClient::new(config).expect("loopback client");

    assert_eq!(
        client
            .get_remote_thumbnail(&format!("{provider_url}/preview.png"))
            .await
            .expect("provider image"),
        b"png"
    );
    let request = provider.await.expect("provider task");
    let lower = request.to_ascii_lowercase();
    assert!(!lower.contains("cf-access-client-id"));
    assert!(!lower.contains("authorization:"));
}

#[tokio::test]
async fn custom_headers_are_not_forwarded_across_redirects() {
    let target = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect target");
    let target_address = target.local_addr().expect("target address");
    let target_task = tokio::spawn(async move {
        let (mut socket, _) = target.accept().await.expect("accept redirect target");
        let request = read_request_headers(&mut socket).await;
        socket
            .write_all(&http_response("200 OK", "application/json", b"{}"))
            .await
            .expect("respond to redirected request");
        request
    });
    let (base_url, server) = spawn_http_fixture(
        format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/collect\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes(),
    )
    .await;
    let client = JellyfinClient::new(
        config(&base_url)
            .with_extra_request_headers([header("CF-Access-Client-Id", TEST_SECRET)])
            .expect("bounded extra headers"),
    )
    .expect("loopback client");

    assert!(matches!(
        client.public_system_info().await,
        Err(ApiError::HttpStatus(status)) if status.as_u16() == 302
    ));
    let request = server.await.expect("redirect fixture task");
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("cf-access-client-id: {TEST_SECRET}"))
    );
    assert!(!target_task.is_finished(), "redirect target was contacted");
    target_task.abort();
    let _ = target_task.await;
}

#[tokio::test]
async fn playback_source_handoff_preserves_only_same_origin_custom_headers() {
    let mock = spawn(MockConfig::default())
        .await
        .expect("spawn local mock Jellyfin");
    let client = JellyfinClient::new(
        config(&mock.base_url())
            .with_extra_request_headers([header("CF-Access-Client-Id", TEST_SECRET)])
            .expect("bounded extra headers"),
    )
    .expect("mock client");
    let session = client
        .authenticate("demo", "demo")
        .await
        .expect("mock sign-in");
    let source: MediaSourceInfo = serde_json::from_value(serde_json::json!({
        "Id": "source-id",
        "Container": "mkv",
        "SupportsDirectPlay": true,
        "RequiredHttpHeaders": { "X-Source-Key": "source-value" }
    }))
    .expect("media source fixture");
    let options = PlaybackSourceRequestOptions {
        item_id: "item-id",
        media_source: &source,
        play_method: PlayMethod::DirectPlay,
        play_session_id: None,
        audio_stream_index: None,
        subtitle_stream_index: None,
        start_time_ticks: None,
    };
    let playback = client
        .build_playback_source_request(&session, options)
        .expect("same-origin playback request");
    assert!(!format!("{playback:?}").contains(TEST_SECRET));
    let (url, headers) = playback.into_parts();
    let url = url::Url::parse(url.as_str()).expect("playback URL");
    assert_eq!(url.origin(), client.server_origin().origin());
    assert!(!url.as_str().contains(TEST_SECRET));
    let header_map = headers
        .iter()
        .map(|header| {
            let (name, value) = header.as_pair();
            (name.to_ascii_lowercase(), value)
        })
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(header_map.get("cf-access-client-id"), Some(&TEST_SECRET));
    assert_eq!(header_map.get("x-source-key"), Some(&"source-value"));
    assert!(!format!("{:?}", headers[2]).contains(TEST_SECRET));

    let conflicting: MediaSourceInfo = serde_json::from_value(serde_json::json!({
        "Id": "source-id",
        "Container": "mkv",
        "SupportsDirectPlay": true,
        "RequiredHttpHeaders": { "cf-access-client-id": "conflicting-value" }
    }))
    .expect("conflicting source fixture");
    assert!(matches!(
        client.build_playback_source_request(
            &session,
            PlaybackSourceRequestOptions {
                media_source: &conflicting,
                ..options
            }
        ),
        Err(PlaybackSourceError::DuplicateRequiredHeader)
    ));

    // Leave room for the configured extra, but not the client's authorization.
    // The complete handoff must respect the relay's 64-header bound.
    let crowded_headers = (0..63)
        .map(|index| (format!("X-Source-{index}"), serde_json::json!("x")))
        .collect::<serde_json::Map<_, _>>();
    let crowded: MediaSourceInfo = serde_json::from_value(serde_json::json!({
        "Id": "source-id",
        "Container": "mkv",
        "SupportsDirectPlay": true,
        "RequiredHttpHeaders": crowded_headers
    }))
    .expect("crowded source fixture");
    assert!(matches!(
        client.build_playback_source_request(
            &session,
            PlaybackSourceRequestOptions {
                media_source: &crowded,
                ..options
            }
        ),
        Err(PlaybackSourceError::InputTooLarge)
    ));

    let foreign: MediaSourceInfo = serde_json::from_value(serde_json::json!({
        "SupportsTranscoding": true,
        "TranscodingUrl": "https://images.example.test/stream.m3u8"
    }))
    .expect("foreign source fixture");
    let foreign_options = PlaybackSourceRequestOptions {
        media_source: &foreign,
        play_method: PlayMethod::Transcode,
        ..options
    };
    assert!(matches!(
        client.build_playback_source_request(&session, foreign_options),
        Err(PlaybackSourceError::CrossOriginTranscodingUrl)
    ));
    mock.shutdown().await;
}
