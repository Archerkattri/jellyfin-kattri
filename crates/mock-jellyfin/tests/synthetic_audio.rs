//! Opt-in local WAV fixtures for native audio queue acceptance.

use mock_jellyfin::{MockConfig, spawn};
use reqwest::{Client, StatusCode, header};
use serde_json::{Value, json};

const TOKEN: &str = "mock-access-token-0001";

fn auth_header() -> String {
    format!("MediaBrowser Token=\"{TOKEN}\"")
}

async fn first_audio_id(client: &Client, base_url: &str) -> String {
    let response = client
        .get(format!(
            "{base_url}/Items?recursive=true&includeItemTypes=Audio&limit=1"
        ))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("audio item request");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("audio item JSON");
    body["Items"][0]["Id"]
        .as_str()
        .expect("audio item id")
        .to_owned()
}

#[tokio::test]
async fn opt_in_audio_playback_serves_and_traces_valid_wav_without_secrets() {
    let server = spawn(MockConfig {
        audio_item_count: Some(8),
        synthetic_audio_wav: true,
        ..MockConfig::default()
    })
    .await
    .expect("mock server starts");
    let client = Client::new();
    let base_url = server.base_url();
    let id = first_audio_id(&client, &base_url).await;

    let playback_info = client
        .post(format!("{base_url}/Items/{id}/PlaybackInfo"))
        .header(header::AUTHORIZATION, auth_header())
        .json(&json!({
            "UserId": "00000000000000000000000000000d01",
            "EnableDirectPlay": true,
        }))
        .send()
        .await
        .expect("playback info request");
    assert_eq!(playback_info.status(), StatusCode::OK);
    let playback_info: Value = playback_info.json().await.expect("playback info JSON");
    let source = &playback_info["MediaSources"][0];
    assert_eq!(source["Container"], "wav");
    assert_eq!(source["Size"], 44 + 44_100 * 8 * 4);
    assert_eq!(source["SupportsDirectPlay"], true);
    assert_eq!(source["MediaStreams"][0]["Codec"], "pcm_s16le");
    assert_eq!(source["MediaStreams"][0]["SampleRate"], 44_100);

    let secret_query = "fixture-secret-for-redaction";
    let stream = client
        .get(format!(
            "{base_url}/Audio/{id}/stream.wav?api_key={secret_query}"
        ))
        .header(header::AUTHORIZATION, auth_header())
        .header(header::RANGE, "bytes=0-43")
        .send()
        .await
        .expect("audio range request");
    assert_eq!(stream.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(stream.headers()[header::CONTENT_TYPE], "audio/wav");
    assert_eq!(
        stream.headers()[header::CONTENT_RANGE],
        "bytes 0-43/1411244"
    );
    let wav_header = stream.bytes().await.expect("WAV header");
    assert_eq!(wav_header.len(), 44);
    assert_eq!(&wav_header[0..4], b"RIFF");
    assert_eq!(&wav_header[8..12], b"WAVE");
    assert_eq!(&wav_header[36..40], b"data");

    let requests: Value = client
        .get(format!("{base_url}/__mock/requests"))
        .send()
        .await
        .expect("request log")
        .json()
        .await
        .expect("request log JSON");
    let audio_request = requests
        .as_array()
        .expect("request log array")
        .iter()
        .find(|request| {
            request["Method"] == "GET"
                && request["Path"]
                    .as_str()
                    .is_some_and(|path| path.starts_with(&format!("/Audio/{id}/stream.wav")))
        })
        .expect("audio stream request recorded");
    let recorded_path = audio_request["Path"].as_str().expect("recorded path");
    assert!(!recorded_path.contains(secret_query));
    assert!(recorded_path.contains("api_key="));

    let reports: Value = client
        .get(format!("{base_url}/__mock/sessions"))
        .send()
        .await
        .expect("playback report log")
        .json()
        .await
        .expect("playback report JSON");
    assert_eq!(reports.as_array().map(Vec::len), Some(0));
    server.shutdown().await;
}

#[tokio::test]
async fn synthetic_audio_mode_leaves_default_placeholder_streams_unchanged() {
    let server = spawn(MockConfig::default())
        .await
        .expect("mock server starts");
    let client = Client::new();
    let base_url = server.base_url();
    let id = first_audio_id(&client, &base_url).await;
    let response = client
        .get(format!("{base_url}/Audio/{id}/stream"))
        .header(header::AUTHORIZATION, auth_header())
        .send()
        .await
        .expect("default audio stream request");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "audio/mpeg");
    assert_eq!(
        response.bytes().await.expect("placeholder stream").len(),
        32 * 1024
    );
    server.shutdown().await;
}
