//! Drives the Live TV channel call against the in-process mock server over
//! loopback HTTP. This validates the client against the mock only, not against
//! a real Jellyfin server.

use std::collections::{HashMap, HashSet};

use jellyfin_api::{
    ApiError, ClientConfig, DeviceProfile, GuideWindow, GuideWindowError, JellyfinClient, Session,
};
use mock_jellyfin::{MockConfig, spawn};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// Lineup size of the mock's `/LiveTv/Channels` route.
const MOCK_CHANNEL_COUNT: usize = 150;

/// 2026-10-15T00:00:00Z and the same day's end, in Unix seconds.
const DAY_START_UNIX: i64 = 1_792_022_400;
const DAY_SECONDS: i64 = 86_400;

/// Mock channel IDs: channel N (1-based) has the ID `{N:032x}`.
fn mock_channel_id(number: u32) -> String {
    format!("{number:032x}")
}

/// Request paths the mock recorded for `/LiveTv/Programs`, in call order.
async fn recorded_program_paths(base_url: &str) -> Vec<String> {
    let log: serde_json::Value = reqwest::get(format!("{base_url}/__mock/requests"))
        .await
        .expect("request log")
        .json()
        .await
        .expect("request log is JSON");
    log.as_array()
        .expect("request log array")
        .iter()
        .filter_map(|entry| entry["Path"].as_str())
        .filter(|path| path.starts_with("/LiveTv/Programs"))
        .map(str::to_owned)
        .collect()
}

/// Decoded query parameters of a recorded path.
fn query_of(path: &str) -> HashMap<String, String> {
    let query = path.split_once('?').map_or("", |(_, query)| query);
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

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

async fn signed_in(base_url: &str) -> (JellyfinClient, Session) {
    let client = client_for(base_url);
    let session = client.authenticate("demo", "demo").await.expect("sign in");
    (client, session)
}

async fn read_http_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end;
    loop {
        let count = socket.read(&mut chunk).await.expect("read request");
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = index + 4;
            break;
        }
        assert!(bytes.len() <= 64 * 1024, "bounded request headers");
    }
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while bytes.len() < header_end.saturating_add(content_length) {
        let count = socket.read(&mut chunk).await.expect("read request body");
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 64 * 1024 + 256 * 1024);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn scripted_server(
    responses: Vec<(u16, &'static str)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let address = listener.local_addr().expect("fixture address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.expect("accept fixture request");
            requests.push(read_http_request(&mut socket).await);
            let reason = if status == 204 {
                "No Content"
            } else {
                "Fixture"
            };
            let response = if body.is_empty() {
                format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            } else {
                format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write fixture response");
        }
        requests
    });
    (format!("http://{address}/base"), task)
}

fn client(base_url: &str) -> JellyfinClient {
    JellyfinClient::new(
        ClientConfig::new(base_url, "Kattri", "Test", "device-id", "test-version")
            .expect("fixture client config"),
    )
    .expect("fixture client")
}

const AUTH_RESPONSE: &str = r#"{"AccessToken":"fixture-token","User":{"Id":"0123456789abcdef0123456789abcdef","Name":"Fixture"}}"#;

async fn fixture_session(client: &JellyfinClient) -> Session {
    client
        .authenticate("fixture", "password")
        .await
        .expect("authenticate")
}

#[tokio::test]
async fn open_live_stream_sends_validated_dto_and_parses_media_source() {
    let source = r#"{"MediaSource":{"Id":"media-id","Path":"/private/path","OpenToken":"private-open-token","LiveStreamId":"live-123","RequiresClosing":true,"SupportsTranscoding":true},"PlaySessionId":"session-123"}"#;
    let (base_url, server) = scripted_server(vec![(200, AUTH_RESPONSE), (200, source)]).await;
    let api = client(&base_url);
    let session = fixture_session(&api).await;
    let profile = DeviceProfile {
        name: Some("mpv".into()),
        ..DeviceProfile::default()
    };
    let protocols = vec!["File".to_owned(), "Http".to_owned()];
    let response = api
        .open_live_stream(
            &session,
            "0123456789abcdef0123456789abcdef",
            &profile,
            Some(8_000_000),
            Some(900),
            Some(1),
            Some(2),
            Some(6),
            Some(true),
            Some(true),
            Some(false),
            Some(&protocols),
        )
        .await
        .expect("open source");
    assert_eq!(
        response.media_source.live_stream_id.as_deref(),
        Some("live-123")
    );
    assert_eq!(
        response.media_source.open_token.as_deref(),
        Some("private-open-token")
    );
    assert_eq!(response.play_session_id.as_deref(), Some("session-123"));

    let requests = server.await.expect("fixture completed");
    let open = &requests[1];
    assert!(open.starts_with("POST /base/LiveStreams/Open HTTP/1.1"));
    assert!(
        open.to_ascii_lowercase()
            .contains("authorization: mediabrowser")
    );
    assert!(open.contains("Token=\"fixture-token\""));
    let body = open.split_once("\r\n\r\n").expect("body separator").1;
    let body: serde_json::Value = serde_json::from_str(body).expect("JSON body");
    assert_eq!(body["UserId"], "0123456789abcdef0123456789abcdef");
    assert_eq!(body["ItemId"], "0123456789abcdef0123456789abcdef");
    assert_eq!(body["DeviceProfile"]["Name"], "mpv");
    assert_eq!(body["MaxStreamingBitrate"], 8_000_000);
    assert_eq!(body["StartTimeTicks"], 900);
    assert_eq!(body["AudioStreamIndex"], 1);
    assert_eq!(body["SubtitleStreamIndex"], 2);
    assert_eq!(body["MaxAudioChannels"], 6);
    assert_eq!(body["EnableDirectPlay"], true);
    assert_eq!(body["EnableDirectStream"], true);
    assert_eq!(body["AlwaysBurnInSubtitleWhenTranscoding"], false);
    assert_eq!(
        body["DirectPlayProtocols"],
        serde_json::json!(["File", "Http"])
    );
    assert!(!open.contains("private-open-token"));
    assert!(!format!("{:?}", response.media_source.live_stream_id).contains("private-open-token"));
}

#[tokio::test]
async fn close_live_stream_query_encodes_id_and_accepts_204() {
    let (base_url, server) = scripted_server(vec![(200, AUTH_RESPONSE), (204, "")]).await;
    let api = client(&base_url);
    let session = fixture_session(&api).await;
    api.close_live_stream(&session, "stream id/&")
        .await
        .expect("close stream");
    let requests = server.await.expect("fixture completed");
    let close = &requests[1];
    assert!(close.starts_with("POST /base/LiveStreams/Close?liveStreamId="));
    assert!(!close.contains("stream id/&"));
    let target = close.split_whitespace().nth(1).expect("request target");
    let query = target.split_once('?').expect("query string").1;
    let params = url::form_urlencoded::parse(query.as_bytes()).collect::<HashMap<_, _>>();
    assert_eq!(
        params.get("liveStreamId").map(|value| value.as_ref()),
        Some("stream id/&")
    );
}

#[tokio::test]
async fn live_stream_auth_and_server_errors_are_typed() {
    let (base_url, server) = scripted_server(vec![(200, AUTH_RESPONSE), (401, "")]).await;
    let api = client(&base_url);
    let session = fixture_session(&api).await;
    let error = api
        .open_live_stream(
            &session,
            "0123456789abcdef0123456789abcdef",
            &DeviceProfile::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .err()
        .expect("401 response");
    assert!(matches!(error, ApiError::ReauthenticationRequired));
    server.await.expect("fixture completed");

    let (base_url, server) = scripted_server(vec![(200, AUTH_RESPONSE), (503, "")]).await;
    let api = client(&base_url);
    let session = fixture_session(&api).await;
    let error = api
        .close_live_stream(&session, "live-123")
        .await
        .expect_err("503 response");
    assert!(
        matches!(error, ApiError::Unavailable { status, .. } if status == reqwest::StatusCode::SERVICE_UNAVAILABLE)
    );
    server.await.expect("fixture completed");
}

#[tokio::test]
async fn live_stream_inputs_are_rejected_before_network_access() {
    let (base_url, server) = scripted_server(vec![(200, AUTH_RESPONSE)]).await;
    let api = client(&base_url);
    let session = fixture_session(&api).await;
    assert!(matches!(
        api.open_live_stream(
            &session,
            "not-a-guid",
            &DeviceProfile::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await,
        Err(ApiError::InvalidResponse)
    ));
    assert!(matches!(
        api.open_live_stream(
            &session,
            "0123456789abcdef0123456789abcdef",
            &DeviceProfile::default(),
            Some(2_000_000_001),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await,
        Err(ApiError::InvalidConfiguration)
    ));
    assert!(matches!(
        api.close_live_stream(&session, "bad\nstream").await,
        Err(ApiError::InvalidConfiguration)
    ));
    server.await.expect("auth-only fixture completed");
}

#[tokio::test]
async fn channel_list_pages_through_the_mock_lineup_in_two_pages() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let (client, session) = signed_in(&server.base_url()).await;

    let first = client
        .list_live_tv_channels(&session, 0, 100)
        .await
        .expect("first channel page");
    let second = client
        .list_live_tv_channels(&session, 100, 100)
        .await
        .expect("second channel page");

    assert_eq!(first.items.len(), 100);
    assert_eq!(second.items.len(), MOCK_CHANNEL_COUNT - 100);
    assert_eq!(first.total_record_count, Some(150));
    assert_eq!(second.start_index, 100);

    assert_eq!(first.items[0].item.name, "Mock Channel 001");
    assert_eq!(first.items[0].number.as_deref(), Some("1"));
    assert_eq!(
        first.items[0]
            .item
            .image_tags
            .as_ref()
            .and_then(|tags| tags.get("Primary"))
            .and_then(Option::as_deref),
        Some("channel-image-001")
    );
    assert_eq!(
        first.items[0]
            .item
            .user_data
            .as_ref()
            .and_then(|data| data.is_favorite),
        Some(true)
    );
    assert_eq!(second.items[0].item.name, "Mock Channel 101");
    assert_eq!(second.items[49].item.name, "Mock Channel 150");
    assert_eq!(second.items[49].number.as_deref(), Some("150"));

    let ids: HashSet<&str> = first
        .items
        .iter()
        .chain(&second.items)
        .map(|channel| channel.item.id.as_str())
        .collect();
    assert_eq!(ids.len(), MOCK_CHANNEL_COUNT, "page IDs are unique");

    let logo = client
        .get_live_tv_channel_logo(&session, &first.items[0].item.id, Some("channel-image-001"))
        .await
        .expect("fetch a channel logo from the generic item image route");
    assert!(!logo.is_empty());

    let request_log: serde_json::Value =
        reqwest::get(format!("{}/__mock/requests", server.base_url()))
            .await
            .expect("request log")
            .json()
            .await
            .expect("request log JSON");
    let paths = request_log
        .as_array()
        .expect("request log entries")
        .iter()
        .filter_map(|entry| entry["Path"].as_str())
        .collect::<Vec<_>>();
    assert!(paths.iter().any(|path| {
        path.starts_with("/Items/00000000000000000000000000000001/Images/Primary?")
            && path.contains("maxWidth=96")
            && path.contains("maxHeight=96")
            && path.contains("tag=channel-image-001")
    }));

    server.shutdown().await;
}

#[tokio::test]
async fn channel_logo_404_is_a_typed_optional_image_failure() {
    let server = spawn(MockConfig {
        primary_images_unavailable: true,
        ..MockConfig::default()
    })
    .await
    .expect("spawn mock server");
    let (client, session) = signed_in(&server.base_url()).await;
    assert!(matches!(
        client
            .get_live_tv_channel_logo(&session, &mock_channel_id(1), None)
            .await,
        Err(ApiError::HttpStatus(status)) if status == reqwest::StatusCode::NOT_FOUND
    ));
    server.shutdown().await;
}

#[tokio::test]
async fn channel_page_size_is_clamped_and_sent_with_user_and_current_program() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let (client, session) = signed_in(&server.base_url()).await;

    let page = client
        .list_live_tv_channels(&session, 0, 10_000)
        .await
        .expect("oversized limit is clamped, not refused");
    assert_eq!(page.items.len(), 100);

    let requests: serde_json::Value =
        reqwest::get(format!("{}/__mock/requests", server.base_url()))
            .await
            .expect("request log")
            .json()
            .await
            .expect("request log is JSON");
    let channel_path = requests
        .as_array()
        .expect("request log array")
        .iter()
        .filter_map(|entry| entry["Path"].as_str())
        .find(|path| path.starts_with("/LiveTv/Channels?"))
        .expect("channel request was recorded")
        .to_owned();
    assert!(channel_path.contains("startIndex=0"));
    assert!(channel_path.contains("limit=100"));
    assert!(channel_path.contains("addCurrentProgram=true"));
    assert!(channel_path.contains("enableImages=true"));
    assert!(channel_path.contains("enableUserData=true"));
    assert!(channel_path.contains(&format!("userId={}", session.user.id)));
    assert!(!channel_path.contains("token"));

    server.shutdown().await;
}

#[tokio::test]
async fn channel_page_past_the_end_is_empty_with_the_full_total() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let (client, session) = signed_in(&server.base_url()).await;

    let page = client
        .list_live_tv_channels(&session, 150, 100)
        .await
        .expect("page past the end");

    assert!(page.items.is_empty());
    assert_eq!(page.total_record_count, Some(150));
    assert_eq!(page.start_index, 150);

    server.shutdown().await;
}

#[tokio::test]
async fn guide_window_returns_hourly_programs_for_three_channels() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let (client, session) = signed_in(&server.base_url()).await;
    let window = GuideWindow::new(DAY_START_UNIX, DAY_START_UNIX + DAY_SECONDS)
        .expect("24-hour window is valid");
    let channels: Vec<String> = (1..=3).map(mock_channel_id).collect();

    // 3 channels x 24 hourly programs = 72 programs, all in one page.
    let full = client
        .list_live_tv_programs(&session, window, &channels, 0, 100)
        .await
        .expect("guide window");
    assert_eq!(full.total_record_count, Some(72));
    assert_eq!(full.items.len(), 72);
    assert_eq!(full.start_index, 0);
    for channel in &channels {
        let count = full
            .items
            .iter()
            .filter(|program| program.channel_id.as_ref() == Some(channel))
            .count();
        assert_eq!(count, 24, "24 programs for {channel}");
    }
    let first = &full.items[0];
    assert_eq!(first.item.name, "Mock Program 001 00:00");
    assert_eq!(first.start_date.as_deref(), Some("2026-10-15T00:00:00Z"));
    assert_eq!(first.end_date.as_deref(), Some("2026-10-15T01:00:00Z"));
    let last_start = full
        .items
        .iter()
        .filter_map(|p| p.start_date.as_deref())
        .max();
    assert_eq!(last_start, Some("2026-10-15T23:00:00Z"));

    // A later page of the same window comes back in order.
    let tail = client
        .list_live_tv_programs(&session, window, &channels, 70, 10)
        .await
        .expect("second page of the guide");
    assert_eq!(tail.items.len(), 2);
    assert_eq!(tail.start_index, 70);

    let paths = recorded_program_paths(&server.base_url()).await;
    assert_eq!(paths.len(), 2);
    let query = query_of(&paths[0]);
    assert_eq!(
        query.get("minStartDate").map(String::as_str),
        Some("2026-10-15T00:00:00Z")
    );
    assert_eq!(
        query.get("maxStartDate").map(String::as_str),
        Some("2026-10-16T00:00:00Z")
    );
    assert_eq!(
        query.get("channelIds").map(String::as_str),
        Some(
            "00000000000000000000000000000001,00000000000000000000000000000002,00000000000000000000000000000003"
        )
    );
    assert_eq!(
        query.get("userId").map(String::as_str),
        Some(session.user.id.as_str())
    );

    server.shutdown().await;
}

#[tokio::test]
async fn complete_guide_window_fetches_all_program_pages_under_the_bound() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let (client, session) = signed_in(&server.base_url()).await;
    let window = GuideWindow::new(DAY_START_UNIX, DAY_START_UNIX + DAY_SECONDS)
        .expect("24-hour window is valid");
    let channels: Vec<String> = (1..=5).map(mock_channel_id).collect();

    let page = client
        .list_live_tv_programs_for_window(&session, window, &channels)
        .await
        .expect("fetch every program page");

    assert_eq!(page.total_record_count, Some(120));
    assert_eq!(page.items.len(), 120);
    assert_eq!(
        page.items
            .first()
            .and_then(|item| item.channel_id.as_deref()),
        Some(channels[0].as_str())
    );
    assert_eq!(
        page.items
            .last()
            .and_then(|item| item.channel_id.as_deref()),
        Some(channels[4].as_str())
    );
    let paths = recorded_program_paths(&server.base_url()).await;
    assert_eq!(paths.len(), 2);
    assert!(
        query_of(&paths[0])
            .get("startIndex")
            .is_some_and(|value| value == "0")
    );
    assert!(
        query_of(&paths[1])
            .get("startIndex")
            .is_some_and(|value| value == "100")
    );

    server.shutdown().await;
}

#[tokio::test]
async fn complete_guide_window_batches_all_one_hundred_fifty_channels() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let (client, session) = signed_in(&server.base_url()).await;
    let window = GuideWindow::new(DAY_START_UNIX, DAY_START_UNIX + DAY_SECONDS)
        .expect("24-hour window is valid");
    let channels: Vec<String> = (1..=150).map(mock_channel_id).collect();

    let page = client
        .list_live_tv_programs_for_window(&session, window, &channels)
        .await
        .expect("fetch every program page for the paged lineup");

    assert_eq!(page.total_record_count, Some(3_600));
    assert_eq!(page.items.len(), 3_600);
    let paths = recorded_program_paths(&server.base_url()).await;
    assert_eq!(paths.len(), 36, "100 and 50 channel batches need 36 pages");
    let first_batch_ids = query_of(&paths[0])
        .get("channelIds")
        .cloned()
        .expect("first channel batch");
    assert_eq!(first_batch_ids.split(',').count(), 100);
    let last_batch_ids = query_of(paths.last().expect("request pages"))
        .get("channelIds")
        .cloned()
        .expect("second channel batch");
    assert_eq!(last_batch_ids.split(',').count(), 50);

    server.shutdown().await;
}

#[tokio::test]
async fn guide_window_with_no_programs_is_an_empty_page() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let (client, session) = signed_in(&server.base_url()).await;
    // 2026-10-20 is after the mock's program range.
    let window = GuideWindow::new(
        DAY_START_UNIX + 5 * DAY_SECONDS,
        DAY_START_UNIX + 6 * DAY_SECONDS,
    )
    .expect("window is valid");

    let page = client
        .list_live_tv_programs(&session, window, &[mock_channel_id(1)], 0, 100)
        .await
        .expect("empty guide window is not an error");

    assert!(page.items.is_empty());
    assert_eq!(page.total_record_count, Some(0));

    server.shutdown().await;
}

#[tokio::test]
async fn invalid_guide_windows_are_refused_before_any_request() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let (_client, _session) = signed_in(&server.base_url()).await;

    assert_eq!(
        GuideWindow::new(DAY_START_UNIX, DAY_START_UNIX),
        Err(GuideWindowError::NotIncreasing)
    );
    assert_eq!(
        GuideWindow::new(DAY_START_UNIX, DAY_START_UNIX - 1),
        Err(GuideWindowError::NotIncreasing)
    );
    assert_eq!(
        GuideWindow::new(DAY_START_UNIX, DAY_START_UNIX + DAY_SECONDS + 1),
        Err(GuideWindowError::TooLong)
    );

    assert!(recorded_program_paths(&server.base_url()).await.is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn guide_refuses_malformed_or_oversized_channel_lists_before_any_request() {
    let server = spawn(MockConfig::default())
        .await
        .expect("spawn mock server");
    let (client, session) = signed_in(&server.base_url()).await;
    let window =
        GuideWindow::new(DAY_START_UNIX, DAY_START_UNIX + DAY_SECONDS).expect("window is valid");

    // A comma inside one ID would split the joined query into two channels.
    let injected =
        vec!["00000000000000000000000000000001,0000000000000000000000000000000f".to_owned()];
    assert!(matches!(
        client
            .list_live_tv_programs(&session, window, &injected, 0, 10)
            .await,
        Err(ApiError::InvalidResponse)
    ));

    let too_many: Vec<String> = (1..=101).map(mock_channel_id).collect();
    assert!(matches!(
        client
            .list_live_tv_programs(&session, window, &too_many, 0, 10)
            .await,
        Err(ApiError::InvalidConfiguration)
    ));

    assert!(recorded_program_paths(&server.base_url()).await.is_empty());
    server.shutdown().await;
}
