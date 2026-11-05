//! Typed calls behind the library page tabs: the paged item lists Jellyfin
//! serves outside `/Items` (genres, studios, artists, upcoming episodes) and
//! the movie recommendation categories.

use reqwest::Method;
use serde::Deserialize;

use crate::{ApiError, BaseItemDto, ItemsResponse, JellyfinClient, Session, decode_response, home};

/// Most recommendation categories requested at once.
pub const MAX_RECOMMENDATION_CATEGORIES: u32 = 10;
/// Most items requested per recommendation category.
pub const MAX_RECOMMENDATION_ITEMS: u32 = 30;
/// Most upcoming episodes [`JellyfinClient::upcoming_episodes`] requests and returns.
pub const MAX_UPCOMING_EPISODES: u32 = 50;

/// A paged item list. Every variant answers with Jellyfin's
/// `BaseItemDtoQueryResult`, so [`JellyfinClient::list_items_at`] decodes them alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemsEndpoint {
    /// `GET /Items`.
    Items,
    /// `GET /Genres`; with `includeItemTypes=MusicAlbum` it lists music genres.
    Genres,
    /// `GET /Studios`.
    Studios,
    /// `GET /Artists`: every artist credited on a track or album.
    Artists,
    /// `GET /Artists/AlbumArtists`.
    AlbumArtists,
    /// `GET /Shows/Upcoming`: episodes that have not premiered yet.
    UpcomingEpisodes,
}

impl ItemsEndpoint {
    pub(crate) const fn path_segments(self) -> &'static [&'static str] {
        match self {
            Self::Items => &["Items"],
            Self::Genres => &["Genres"],
            Self::Studios => &["Studios"],
            Self::Artists => &["Artists"],
            Self::AlbumArtists => &["Artists", "AlbumArtists"],
            Self::UpcomingEpisodes => &["Shows", "Upcoming"],
        }
    }
}

/// Why Jellyfin recommends a category (`RecommendationType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecommendationKind {
    SimilarToRecentlyPlayed,
    SimilarToLikedItem,
    HasDirectorFromRecentlyPlayed,
    HasActorFromRecentlyPlayed,
    HasLikedDirector,
    HasLikedActor,
    /// A value this client does not know; the category is still shown.
    Other,
}

impl<'de> Deserialize<'de> for RecommendationKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "SimilarToRecentlyPlayed" => Self::SimilarToRecentlyPlayed,
            "SimilarToLikedItem" => Self::SimilarToLikedItem,
            "HasDirectorFromRecentlyPlayed" => Self::HasDirectorFromRecentlyPlayed,
            "HasActorFromRecentlyPlayed" => Self::HasActorFromRecentlyPlayed,
            "HasLikedDirector" => Self::HasLikedDirector,
            "HasLikedActor" => Self::HasLikedActor,
            _ => Self::Other,
        })
    }
}

/// One category from `GET /Movies/Recommendations`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct Recommendation {
    #[serde(default)]
    pub items: Vec<BaseItemDto>,
    #[serde(default = "other_kind")]
    pub recommendation_type: RecommendationKind,
    /// The watched or liked item, director, or actor the category is based on.
    #[serde(default)]
    pub baseline_item_name: Option<String>,
    #[serde(default)]
    pub category_id: Option<String>,
}

const fn other_kind() -> RecommendationKind {
    RecommendationKind::Other
}

impl JellyfinClient {
    /// Episodes that have not premiered yet (`GET /Shows/Upcoming`), in the
    /// server's order. The limit is clamped to `1..=MAX_UPCOMING_EPISODES` so one
    /// call stays bounded.
    pub async fn upcoming_episodes(
        &self,
        session: &Session,
        limit: u32,
    ) -> Result<Vec<BaseItemDto>, ApiError> {
        let limit = limit.clamp(1, MAX_UPCOMING_EPISODES).to_string();
        let request = self
            .http
            .request(
                Method::GET,
                self.endpoint(ItemsEndpoint::UpcomingEpisodes.path_segments()),
            )
            .query(&[
                ("userId", session.user.id.as_str()),
                ("limit", limit.as_str()),
            ]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let response: ItemsResponse = decode_response(response).await?;
        Ok(response.items)
    }

    /// Fetch movie recommendation categories (`GET /Movies/Recommendations`).
    /// Limits are clamped so one call stays bounded.
    pub async fn movie_recommendations(
        &self,
        session: &Session,
        parent_id: Option<&str>,
        category_limit: u32,
        item_limit: u32,
    ) -> Result<Vec<Recommendation>, ApiError> {
        let mut query = vec![
            ("userId", session.user.id.clone()),
            (
                "categoryLimit",
                category_limit
                    .clamp(1, MAX_RECOMMENDATION_CATEGORIES)
                    .to_string(),
            ),
            (
                "itemLimit",
                item_limit.clamp(1, MAX_RECOMMENDATION_ITEMS).to_string(),
            ),
            ("fields", "PrimaryImageAspectRatio".to_owned()),
        ];
        if let Some(parent_id) = parent_id {
            home::validate_id(parent_id)?;
            query.push(("parentId", parent_id.to_owned()));
        }
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Movies", "Recommendations"]))
            .query(&query);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{ItemsEndpoint, MAX_UPCOMING_EPISODES, Recommendation, RecommendationKind};
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

    fn request_url(request: &str) -> url::Url {
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("request target");
        url::Url::parse(&format!("http://localhost{target}")).expect("request URL")
    }

    fn params(url: &url::Url) -> HashMap<String, String> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[test]
    fn endpoints_map_to_jellyfin_paths() {
        assert_eq!(ItemsEndpoint::Items.path_segments(), ["Items"]);
        assert_eq!(ItemsEndpoint::Genres.path_segments(), ["Genres"]);
        assert_eq!(ItemsEndpoint::Studios.path_segments(), ["Studios"]);
        assert_eq!(ItemsEndpoint::Artists.path_segments(), ["Artists"]);
        assert_eq!(
            ItemsEndpoint::AlbumArtists.path_segments(),
            ["Artists", "AlbumArtists"]
        );
        assert_eq!(
            ItemsEndpoint::UpcomingEpisodes.path_segments(),
            ["Shows", "Upcoming"]
        );
    }

    #[test]
    fn recommendation_parses_known_and_unknown_kinds() {
        let parsed: Vec<Recommendation> = serde_json::from_value(serde_json::json!([
            {
                "Items": [{"Id": "m1", "Name": "Film", "Type": "Movie"}],
                "RecommendationType": "SimilarToRecentlyPlayed",
                "BaselineItemName": "Night Harbor",
                "CategoryId": "c1"
            },
            {"RecommendationType": "SomethingNew"},
            {}
        ]))
        .expect("recommendations parse");
        assert_eq!(
            parsed[0].recommendation_type,
            RecommendationKind::SimilarToRecentlyPlayed
        );
        assert_eq!(parsed[0].items.len(), 1);
        assert_eq!(
            parsed[0].baseline_item_name.as_deref(),
            Some("Night Harbor")
        );
        assert_eq!(parsed[1].recommendation_type, RecommendationKind::Other);
        assert_eq!(parsed[2].recommendation_type, RecommendationKind::Other);
        assert!(parsed[2].items.is_empty());
    }

    #[tokio::test]
    async fn upcoming_episodes_uses_bounded_route_and_query() {
        let body = r#"{"Items":[{"Id":"episode-1","Name":"Future","Type":"Episode"}],"TotalRecordCount":1}"#;
        for (requested, expected) in [(u32::MAX, "50"), (0, "1"), (12, "12")] {
            let (client, task) = serve_once(body.into()).await;
            let episodes = client
                .upcoming_episodes(&session(), requested)
                .await
                .expect("upcoming episodes");
            let request = task.await.expect("server task");
            let url = request_url(&request);
            assert_eq!(url.path(), "/jellyfin/Shows/Upcoming");
            assert_eq!(
                params(&url),
                HashMap::from([
                    ("userId".into(), "user-id".into()),
                    ("limit".into(), expected.into()),
                ])
            );
            assert_eq!(episodes.len(), 1);
            assert_eq!(episodes[0].id, "episode-1");
            let request_line = request.lines().next().unwrap_or_default();
            assert!(!request_line.contains("secret-token"));
        }
        assert_eq!(MAX_UPCOMING_EPISODES, 50);
    }

    #[tokio::test]
    async fn upcoming_episodes_preserves_server_errors() {
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
                .upcoming_episodes(&session(), 10)
                .await;
            server.await.expect("server task");
            assert!(matches!(result, Err(ApiError::HttpStatus(actual)) if actual == status));
        }
    }
}
