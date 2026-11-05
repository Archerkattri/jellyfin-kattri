//! Item theme media: `GET /Items/{itemId}/ThemeMedia`.
//!
//! Jellyfin splits theme videos and theme songs into two `BaseItemDtoQueryResult`
//! groups. A parent's theme is included when `inheritFromParent=true`.

use reqwest::Method;
use serde::Deserialize;

use crate::{ApiError, BaseItemDto, JellyfinClient, Session, decode_response, home};

/// Most theme videos and theme songs kept from one response.
pub const MAX_THEME_MEDIA_ITEMS: usize = 50;

/// Treat an explicit JSON `null` like a missing value for a `#[serde(default)]` field.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// One items collection in a theme-media response (`ThemeVideosResult` or
/// `ThemeSongsResult`). Unknown result fields, such as the paging counters, are ignored.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct ThemeMediaItems {
    #[serde(default, deserialize_with = "null_default")]
    pub items: Vec<BaseItemDto>,
}

/// Theme videos and songs for one item.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct ThemeMediaResult {
    #[serde(
        rename = "ThemeVideosResult",
        default,
        deserialize_with = "null_default"
    )]
    pub theme_videos: ThemeMediaItems,
    #[serde(
        rename = "ThemeSongsResult",
        default,
        deserialize_with = "null_default"
    )]
    pub theme_songs: ThemeMediaItems,
    /// The item that owns the returned theme, which may be a parent when inherited.
    #[serde(default)]
    pub owner_id: Option<String>,
}

impl JellyfinClient {
    /// Fetch the theme videos and songs for `item_id`, including an inherited
    /// parent theme (`GET /Items/{itemId}/ThemeMedia?inheritFromParent=true`).
    ///
    /// Each collection is truncated to [`MAX_THEME_MEDIA_ITEMS`] so a server
    /// cannot grow an unbounded list. The response body uses the shared JSON
    /// response cap ([`ApiError::JsonResponseTooLarge`]). A 404 or server error
    /// is returned as [`ApiError::HttpStatus`], like the other item calls.
    pub async fn theme_media(
        &self,
        session: &Session,
        item_id: &str,
    ) -> Result<ThemeMediaResult, ApiError> {
        home::validate_id(item_id)?;
        let request = self
            .http
            .request(
                Method::GET,
                self.endpoint(&["Items", item_id, "ThemeMedia"]),
            )
            .query(&[
                ("userId", session.user.id.as_str()),
                ("inheritFromParent", "true"),
            ]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let mut result: ThemeMediaResult = decode_response(response).await?;
        result.theme_videos.items.truncate(MAX_THEME_MEDIA_ITEMS);
        result.theme_songs.items.truncate(MAX_THEME_MEDIA_ITEMS);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{MAX_THEME_MEDIA_ITEMS, ThemeMediaResult};
    use crate::{ApiError, ClientConfig, JellyfinClient, Session, StatusCode, UserDto};

    fn client(base_url: &str) -> JellyfinClient {
        JellyfinClient::new(
            ClientConfig::new(base_url, "Kattri test", "test device", "device-id", "1.0")
                .expect("valid config"),
        )
        .expect("client")
    }

    fn session() -> Session {
        Session {
            access_token: "secret-token".into(),
            user: UserDto {
                policy: None,
                id: "user-id".into(),
                name: "Test user".into(),
                server_id: None,
            },
            server_id: None,
            session_info: None,
        }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 2048];
        loop {
            let count = socket.read(&mut chunk).await.expect("read request");
            assert_ne!(count, 0, "client closed before sending headers");
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return String::from_utf8_lossy(&request).into_owned();
            }
        }
    }

    async fn serve_once(body: String) -> (JellyfinClient, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let request = read_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            request
        });
        (client(&format!("http://{address}/jellyfin")), task)
    }

    fn request_uri(request: &str) -> url::Url {
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("request target");
        url::Url::parse(&format!("http://localhost{target}")).expect("request URL")
    }

    fn params(url: &url::Url) -> std::collections::HashMap<String, String> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[tokio::test]
    async fn requests_theme_media_for_the_authenticated_user_with_inheritance() {
        let (client, task) = serve_once(r#"{"ThemeVideosResult":{"Items":[]}}"#.into()).await;
        client
            .theme_media(&session(), "item/id")
            .await
            .expect("theme media");
        let request = task.await.expect("server task");
        let url = request_uri(&request);
        assert_eq!(url.path(), "/jellyfin/Items/item%2Fid/ThemeMedia");
        assert_eq!(
            params(&url),
            std::collections::HashMap::from([
                ("userId".into(), "user-id".into()),
                ("inheritFromParent".into(), "true".into()),
            ])
        );
        let request_line = request.lines().next().unwrap_or_default();
        assert!(!request_line.contains("secret-token"));
    }

    #[test]
    fn parses_minimal_theme_media_with_videos_songs_and_owner() {
        let parsed: ThemeMediaResult = serde_json::from_str(
            r#"{
                "ThemeVideosResult": {"Items": [{"Id": "video-1", "Name": "Opening", "Type": "Video"}]},
                "ThemeSongsResult": {"Items": [{"Id": "song-1", "Name": "Main Theme", "Type": "Audio"}]},
                "OwnerId": "owner-1"
            }"#,
        )
        .expect("minimal theme media");
        assert_eq!(parsed.theme_videos.items.len(), 1);
        assert_eq!(parsed.theme_videos.items[0].id, "video-1");
        assert_eq!(parsed.theme_songs.items.len(), 1);
        assert_eq!(parsed.theme_songs.items[0].id, "song-1");
        assert_eq!(parsed.owner_id.as_deref(), Some("owner-1"));
    }

    #[test]
    fn missing_result_groups_items_and_owner_default_to_empty() {
        for json in [
            "{}",
            r#"{"ThemeVideosResult":null,"ThemeSongsResult":null}"#,
            r#"{"ThemeVideosResult":{},"ThemeSongsResult":{},"OwnerId":null}"#,
            r#"{"ThemeVideosResult":{"Items":null},"ThemeSongsResult":{"TotalRecordCount":4}}"#,
        ] {
            let parsed: ThemeMediaResult =
                serde_json::from_str(json).expect("sparse theme media parses");
            assert!(parsed.theme_videos.items.is_empty(), "{json}");
            assert!(parsed.theme_songs.items.is_empty(), "{json}");
            assert_eq!(parsed.owner_id, None, "{json}");
        }
    }

    #[tokio::test]
    async fn truncates_each_theme_collection_to_the_bound() {
        let videos: Vec<String> = (0..MAX_THEME_MEDIA_ITEMS + 5)
            .map(|index| format!(r#"{{"Id":"video-{index}","Name":"Video"}}"#))
            .collect();
        let songs: Vec<String> = (0..MAX_THEME_MEDIA_ITEMS + 5)
            .map(|index| format!(r#"{{"Id":"song-{index}","Name":"Song"}}"#))
            .collect();
        let body = format!(
            r#"{{"ThemeVideosResult":{{"Items":[{}]}},"ThemeSongsResult":{{"Items":[{}]}}}}"#,
            videos.join(","),
            songs.join(",")
        );
        let (client, task) = serve_once(body).await;
        let result = client
            .theme_media(&session(), "item-id")
            .await
            .expect("theme media");
        task.await.expect("server task");
        assert_eq!(result.theme_videos.items.len(), MAX_THEME_MEDIA_ITEMS);
        assert_eq!(result.theme_songs.items.len(), MAX_THEME_MEDIA_ITEMS);
        assert_eq!(result.theme_videos.items[0].id, "video-0");
    }

    #[tokio::test]
    async fn preserves_not_found_and_server_errors() {
        for status in [StatusCode::NOT_FOUND, StatusCode::INTERNAL_SERVER_ERROR] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock server");
            let address = listener.local_addr().expect("mock server address");
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept client");
                let _ = read_request(&mut socket).await;
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write error response");
            });

            let result = client(&format!("http://{address}/jellyfin"))
                .theme_media(&session(), "item-id")
                .await;
            server.await.expect("server task");
            assert!(matches!(result, Err(ApiError::HttpStatus(actual)) if actual == status));
        }
    }

    #[tokio::test]
    async fn rejects_invalid_identifiers_before_request() {
        let client = client("http://127.0.0.1:1");
        for item_id in ["", ".", "..", "a\nb", &"x".repeat(129)] {
            assert!(matches!(
                client.theme_media(&session(), item_id).await,
                Err(ApiError::InvalidResponse)
            ));
        }
    }
}
