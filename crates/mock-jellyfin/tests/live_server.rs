//! Opt-in, read-only integration check against an existing Jellyfin server.
//! Run explicitly with `cargo test -p mock-jellyfin --test live_server -- --ignored --nocapture`.

use std::fs;
use std::path::PathBuf;
use std::{collections::BTreeMap, env, time::Duration};

use reqwest::{Client, Url, redirect::Policy};
use serde_json::{Value, json};
use zeroize::Zeroizing;

const DEFAULT_SERVER: &str = "http://127.0.0.1:8096/";
const MAX_ITEMS: usize = 12;

#[derive(Debug, PartialEq, Eq)]
struct SafeSourceSummary {
    container: String,
    video_codec: String,
    audio_codec: String,
    direct_play: bool,
    direct_stream: bool,
    transcode: bool,
    requires_headers: bool,
}

fn safe_base_url(value: &str) -> Result<Url, &'static str> {
    let mut url = Url::parse(value).map_err(|_| "invalid server URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("server URL must not contain credentials, query, or fragment");
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

fn safe_text(value: Option<&str>) -> String {
    value
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
        .unwrap_or("unknown")
        .to_ascii_lowercase()
}

fn credential_bearing_url(value: Option<&str>) -> bool {
    value.is_some_and(|text| {
        let text = text.to_ascii_lowercase();
        ["api_key=", "token=", "password=", "username="]
            .iter()
            .any(|needle| text.contains(needle))
    })
}

fn safe_source(source: &Value) -> Result<SafeSourceSummary, &'static str> {
    if credential_bearing_url(source.get("Path").and_then(Value::as_str))
        || credential_bearing_url(source.get("TranscodingUrl").and_then(Value::as_str))
        || credential_bearing_url(source.get("OpenToken").and_then(Value::as_str))
    {
        return Err("credential-bearing source URL suppressed");
    }
    let streams = source
        .get("MediaStreams")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let codec_for = |kind: &str| {
        streams
            .iter()
            .find(|stream| stream.get("Type").and_then(Value::as_str) == Some(kind))
            .and_then(|stream| stream.get("Codec").and_then(Value::as_str))
    };
    let has_headers = source
        .get("RequiredHttpHeaders")
        .and_then(Value::as_object)
        .is_some_and(|headers| !headers.is_empty());
    Ok(SafeSourceSummary {
        container: safe_text(source.get("Container").and_then(Value::as_str)),
        video_codec: safe_text(codec_for("Video")),
        audio_codec: safe_text(codec_for("Audio")),
        direct_play: source
            .get("SupportsDirectPlay")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        direct_stream: source
            .get("SupportsDirectStream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        transcode: source
            .get("SupportsTranscoding")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        requires_headers: has_headers,
    })
}

fn aggregate(rows: &[SafeSourceSummary]) -> BTreeMap<String, usize> {
    let mut summary = BTreeMap::new();
    for row in rows {
        *summary
            .entry(format!(
                "{} / {} / {}",
                row.container, row.video_codec, row.audio_codec
            ))
            .or_insert(0) += 1;
    }
    summary
}

fn read_device_profile(path: PathBuf) -> Result<Value, &'static str> {
    let bytes =
        fs::read(path).map_err(|_| "device profile could not be read; details suppressed")?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "device profile is invalid; details suppressed")?;
    let profile = value
        .get("DeviceProfile")
        .or_else(|| value.get("deviceProfile"))
        .cloned()
        .unwrap_or(value);
    let is_profile = profile.as_object().is_some_and(|object| {
        [
            "Name",
            "DirectPlayProfiles",
            "TranscodingProfiles",
            "CodecProfiles",
            "SubtitleProfiles",
        ]
        .iter()
        .any(|key| object.contains_key(*key))
    });
    is_profile
        .then_some(profile)
        .ok_or("file does not contain a Jellyfin DeviceProfile; details suppressed")
}

async fn get_json(
    client: &Client,
    url: Url,
    authorization: Option<&str>,
) -> Result<Value, &'static str> {
    let mut request = client.get(url);
    if let Some(header) = authorization {
        request = request.header(reqwest::header::AUTHORIZATION, header);
    }
    let response = request
        .send()
        .await
        .map_err(|_| "request failed; details suppressed")?;
    if !response.status().is_success() {
        return Err("server returned an unsuccessful response; details suppressed");
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| "server response was invalid; details suppressed")
}

fn endpoint(base: &Url, path: &str) -> Result<Url, &'static str> {
    base.join(path).map_err(|_| "invalid endpoint")
}

fn item_playback_endpoint(base: &Url, item_id: &str) -> Result<Url, &'static str> {
    let mut url = base.clone();
    let mut segments = url.path_segments_mut().map_err(|_| "invalid endpoint")?;
    segments.pop_if_empty();
    segments.extend(["Items", item_id, "PlaybackInfo"]);
    drop(segments);
    Ok(url)
}

#[tokio::test]
#[ignore = "requires explicit access to an existing Jellyfin server"]
async fn validates_live_server_without_mutating_state() {
    let base =
        safe_base_url(&env::var("JELLYFIN_TEST_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_owned()))
            .expect("safe configured URL");
    let client = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("HTTP client");

    let public = get_json(
        &client,
        endpoint(&base, "System/Info/Public").expect("public endpoint"),
        None,
    )
    .await
    .expect("anonymous public info; response details suppressed");
    let version = safe_text(public.get("Version").and_then(Value::as_str));
    println!("Anonymous public-info request: OK; server version {version}.");

    let (Ok(username), Ok(password)) = (
        env::var("JELLYFIN_TEST_USER"),
        env::var("JELLYFIN_TEST_PASSWORD"),
    ) else {
        println!(
            "Authenticated checks skipped: set both JELLYFIN_TEST_USER and JELLYFIN_TEST_PASSWORD."
        );
        return;
    };
    if username.is_empty() || password.is_empty() {
        println!(
            "Authenticated checks skipped: set both JELLYFIN_TEST_USER and JELLYFIN_TEST_PASSWORD."
        );
        return;
    }

    let username = Zeroizing::new(username);
    let password = Zeroizing::new(password);
    let auth_response = client
        .post(endpoint(&base, "Users/AuthenticateByName").expect("auth endpoint"))
        .json(&json!({ "Username": username.as_str(), "Pw": password.as_str() }))
        .send()
        .await
        .map_err(|_| "authentication request failed; details suppressed")
        .expect("authentication request; details suppressed");
    if !auth_response.status().is_success() {
        panic!("authentication failed; response details suppressed");
    }
    let auth: Value = auth_response
        .json()
        .await
        .map_err(|_| "authentication response invalid; details suppressed")
        .expect("authentication response; details suppressed");
    let token = auth
        .get("AccessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .expect("authentication token missing; value suppressed");
    let user_id = auth
        .get("User")
        .and_then(|user| user.get("Id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .expect("authenticated user identifier missing; value suppressed")
        .to_owned();
    let token = Zeroizing::new(token.to_owned());
    let authorization = Zeroizing::new(format!(
        "MediaBrowser Client=\"Kattri validator\", Device=\"validation\", DeviceId=\"kattri-validation\", Version=\"1\", Token=\"{}\"",
        token.as_str()
    ));
    drop(auth);
    drop(username);
    drop(password);

    let mut views_url = base.clone();
    let mut segments = views_url.path_segments_mut().expect("views endpoint path");
    segments.pop_if_empty();
    segments.extend(["Users", user_id.as_str(), "Views"]);
    drop(segments);
    let views = get_json(&client, views_url, Some(authorization.as_str()))
        .await
        .expect("library listing; details suppressed");
    let library_count = views
        .get("Items")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut items_url = endpoint(&base, "Items").expect("items endpoint");
    items_url
        .query_pairs_mut()
        .append_pair("userId", user_id.as_str())
        .append_pair("recursive", "true")
        .append_pair("startIndex", "0")
        .append_pair("limit", &MAX_ITEMS.to_string())
        .append_pair("Fields", "MediaSources");
    let items = get_json(&client, items_url, Some(&authorization))
        .await
        .expect("item listing; details suppressed");
    let sample = items
        .get("Items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let profile = env::var_os("JELLYFIN_TEST_DEVICE_PROFILE")
        .map(PathBuf::from)
        .map(read_device_profile)
        .transpose()
        .expect("device profile; details suppressed");
    if profile.is_none() {
        println!(
            "No generated device profile path supplied; PlaybackInfo used server-default negotiation."
        );
    }

    let mut summaries = Vec::new();
    for item in sample.iter().take(MAX_ITEMS) {
        let Some(item_id) = item.get("Id").and_then(Value::as_str) else {
            continue;
        };
        let playback_url = item_playback_endpoint(&base, item_id).expect("playback endpoint");
        let mut request_body = json!({
                "UserId": user_id.as_str(),
            "EnableDirectPlay": true,
            "EnableDirectStream": true,
            "EnableTranscoding": true
        });
        if let Some(profile) = &profile {
            request_body["DeviceProfile"] = profile.clone();
        }
        let response = client
            .post(playback_url)
            .header(reqwest::header::AUTHORIZATION, authorization.as_str())
            .json(&request_body)
            .send()
            .await
            .map_err(|_| "playback negotiation failed; details suppressed")
            .expect("playback negotiation; details suppressed");
        if !response.status().is_success() {
            eprintln!(
                "PlaybackInfo skipped one item due to a non-success response (details suppressed)."
            );
            continue;
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| "playback response invalid; details suppressed")
            .expect("playback response; details suppressed");
        if let Some(sources) = body.get("MediaSources").and_then(Value::as_array) {
            for source in sources {
                summaries
                    .push(safe_source(source).expect("safe playback source; details suppressed"));
            }
        }
    }
    let summary = aggregate(&summaries);
    let direct_play = summaries.iter().filter(|row| row.direct_play).count();
    let direct_stream = summaries.iter().filter(|row| row.direct_stream).count();
    let transcode = summaries.iter().filter(|row| row.transcode).count();
    let required_headers = summaries.iter().filter(|row| row.requires_headers).count();
    println!(
        "Read-only checks: libraries={library_count}, sampled_items={}, negotiated_sources={}, offers(direct_play={direct_play}, direct_stream={direct_stream}, transcode={transcode}), sources_requiring_headers={required_headers}.",
        sample.len(),
        summaries.len()
    );
    for (codec_container, count) in summary {
        println!("  {codec_container}: {count}");
    }
}

#[cfg(test)]
mod tests {
    use super::{SafeSourceSummary, aggregate, read_device_profile, safe_base_url, safe_source};
    use serde_json::json;

    #[test]
    fn server_url_rejects_embedded_credentials_and_query_tokens() {
        assert!(safe_base_url("https://user:pass@example.test/").is_err());
        assert!(safe_base_url("https://example.test/?api_key=secret").is_err());
        assert!(safe_base_url("https://example.test/#fragment").is_err());
    }

    #[test]
    fn source_summary_omits_item_identity_and_signed_urls() {
        let source = json!({
            "Id": "private-id",
            "Name": "private-title",
            "Path": "/library/private-title.mkv",
            "Container": "mkv",
            "SupportsDirectPlay": true,
            "SupportsDirectStream": false,
            "SupportsTranscoding": true,
            "RequiredHttpHeaders": { "X-Auth": "secret-value" },
            "MediaStreams": [
                { "Type": "Video", "Codec": "h264" },
                { "Type": "Audio", "Codec": "aac" }
            ]
        });
        let safe = safe_source(&source).expect("safe metadata");
        assert_eq!(safe.container, "mkv");
        assert_eq!(safe.video_codec, "h264");
        assert_eq!(safe.audio_codec, "aac");
        assert!(safe.direct_play);
        assert!(safe.requires_headers);
        let rendered = format!("{safe:?}");
        assert!(!rendered.contains("private-id"));
        assert!(!rendered.contains("private-title"));
        assert!(!rendered.contains("secret-value"));
    }

    #[test]
    fn credential_query_values_are_rejected_without_echoing_them() {
        let source = json!({
            "Path": "https://server.invalid/video?api_key=do-not-print",
            "Container": "mp4"
        });
        assert_eq!(
            safe_source(&source),
            Err("credential-bearing source URL suppressed")
        );
    }

    #[test]
    fn aggregate_is_limited_to_codec_and_container_dimensions() {
        let rows = vec![SafeSourceSummary {
            container: "mkv".into(),
            video_codec: "h264".into(),
            audio_codec: "aac".into(),
            direct_play: true,
            direct_stream: false,
            transcode: true,
            requires_headers: false,
        }];
        assert_eq!(aggregate(&rows).get("mkv / h264 / aac"), Some(&1));
    }

    #[test]
    fn device_profile_reader_accepts_jellyfin_shape_and_rejects_profile_provenance() {
        let path =
            std::env::temp_dir().join(format!("kattri-device-profile-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"DeviceProfile":{"Name":"Kattri","DirectPlayProfiles":[]}}"#,
        )
        .expect("write fixture");
        assert!(read_device_profile(path.clone()).is_ok());
        std::fs::write(&path, r#"{"softwareBaseline":{},"provenance":[]}"#)
            .expect("replace fixture");
        assert_eq!(
            read_device_profile(path.clone()),
            Err("file does not contain a Jellyfin DeviceProfile; details suppressed")
        );
        std::fs::remove_file(path).expect("remove fixture");
    }
}
