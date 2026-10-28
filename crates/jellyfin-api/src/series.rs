//! Typed series, season, and episode list calls.
//!
//! The client prefers Jellyfin's dedicated `/Shows` operations and falls back
//! to `/Items` only when a server responds with 404. Response items use
//! [`crate::BaseItemDto`], including season/episode indices and user progress.

use std::sync::Once;

use reqwest::{Method, StatusCode};

use crate::{ApiError, ItemsResponse, JellyfinClient, Session, decode_response, home::validate_id};

static SEASONS_PRIMARY_LOGGED: Once = Once::new();
static SEASONS_FALLBACK_LOGGED: Once = Once::new();
static EPISODES_PRIMARY_LOGGED: Once = Once::new();
static EPISODES_FALLBACK_LOGGED: Once = Once::new();

impl JellyfinClient {
    /// List a series' seasons using `GET /Shows/{seriesId}/Seasons`.
    ///
    /// The authenticated user's `userId` and image flags are included so
    /// Jellyfin can return user-specific and image metadata. If the dedicated
    /// endpoint returns 404, this transparently falls back to
    /// `GET /Items?parentId={seriesId}&includeItemTypes=Season`.
    ///
    /// Season metadata is returned as [`ItemsResponse`]; each item is a
    /// [`crate::BaseItemDto`], with `index_number` carrying the season number.
    pub async fn list_seasons(
        &self,
        session: &Session,
        series_id: &str,
    ) -> Result<ItemsResponse, ApiError> {
        validate_id(series_id)?;
        let primary = self
            .get_series_items(
                session,
                &["Shows", series_id, "Seasons"],
                &[
                    ("userId", session.user.id.clone()),
                    ("enableUserData", "true".to_owned()),
                    ("enableImages", "true".to_owned()),
                    ("imageTypeLimit", "1".to_owned()),
                    ("enableImageTypes", "Primary,Backdrop".to_owned()),
                ],
            )
            .await;
        match primary {
            Ok(items) => {
                log_path_once(&SEASONS_PRIMARY_LOGGED, "GET /Shows/{seriesId}/Seasons");
                Ok(items)
            }
            Err(ApiError::HttpStatus(StatusCode::NOT_FOUND)) => {
                log_path_once(&SEASONS_FALLBACK_LOGGED, "GET /Items season fallback");
                self.get_series_items(
                    session,
                    &["Items"],
                    &[
                        ("userId", session.user.id.clone()),
                        ("parentId", series_id.to_owned()),
                        ("includeItemTypes", "Season".to_owned()),
                        ("recursive", "false".to_owned()),
                    ],
                )
                .await
            }
            Err(error) => Err(error),
        }
    }

    /// List a series' episodes in one season using
    /// `GET /Shows/{seriesId}/Episodes?seasonId=...`.
    ///
    /// The request includes the authenticated `userId` and metadata fields
    /// used by episode detail/progress views. If the dedicated endpoint
    /// returns 404, this falls back to
    /// `GET /Items?parentId={seasonId}&includeItemTypes=Episode`.
    pub async fn list_episodes(
        &self,
        session: &Session,
        series_id: &str,
        season_id: &str,
    ) -> Result<ItemsResponse, ApiError> {
        validate_id(series_id)?;
        validate_id(season_id)?;
        let primary = self
            .get_series_items(
                session,
                &["Shows", series_id, "Episodes"],
                &[
                    ("seasonId", season_id.to_owned()),
                    ("userId", session.user.id.clone()),
                    ("enableUserData", "true".to_owned()),
                    ("fields", "Overview,PrimaryImageAspectRatio".to_owned()),
                ],
            )
            .await;
        match primary {
            Ok(items) => {
                log_path_once(&EPISODES_PRIMARY_LOGGED, "GET /Shows/{seriesId}/Episodes");
                Ok(items)
            }
            Err(ApiError::HttpStatus(StatusCode::NOT_FOUND)) => {
                log_path_once(&EPISODES_FALLBACK_LOGGED, "GET /Items episode fallback");
                self.get_series_items(
                    session,
                    &["Items"],
                    &[
                        ("userId", session.user.id.clone()),
                        ("parentId", season_id.to_owned()),
                        ("includeItemTypes", "Episode".to_owned()),
                        ("recursive", "false".to_owned()),
                        ("fields", "Overview,PrimaryImageAspectRatio".to_owned()),
                    ],
                )
                .await
            }
            Err(error) => Err(error),
        }
    }

    /// The episodes before and after `episode_id` in series order, using
    /// `GET /Shows/{seriesId}/Episodes?adjacentTo={episodeId}`. Crosses season boundaries;
    /// specials are left out, as in Jellyfin Web's previous/next episode buttons.
    pub async fn adjacent_episodes(
        &self,
        session: &Session,
        series_id: &str,
        episode_id: &str,
    ) -> Result<AdjacentEpisodes, ApiError> {
        validate_id(series_id)?;
        validate_id(episode_id)?;
        let response = self
            .get_series_items(
                session,
                &["Shows", series_id, "Episodes"],
                &[
                    ("adjacentTo", episode_id.to_owned()),
                    ("userId", session.user.id.clone()),
                    ("enableUserData", "true".to_owned()),
                    ("fields", "Overview,PrimaryImageAspectRatio".to_owned()),
                ],
            )
            .await?;
        Ok(AdjacentEpisodes::from_items(response.items, episode_id))
    }

    /// The episodes of season number `season` of a series, using
    /// `GET /Shows/{seriesId}/Episodes?season=N`, for an episode page's "More from this season".
    pub async fn season_episodes_by_number(
        &self,
        session: &Session,
        series_id: &str,
        season: i32,
    ) -> Result<ItemsResponse, ApiError> {
        validate_id(series_id)?;
        self.get_series_items(
            session,
            &["Shows", series_id, "Episodes"],
            &[
                ("season", season.to_string()),
                ("userId", session.user.id.clone()),
                ("enableUserData", "true".to_owned()),
                ("fields", "Overview,PrimaryImageAspectRatio".to_owned()),
            ],
        )
        .await
    }

    /// The episode to play next in one series, using
    /// `GET /Shows/NextUp?seriesId={seriesId}`: the episode in progress, else the first
    /// unplayed one after the last played. `None` when the server has none to offer.
    pub async fn series_next_up(
        &self,
        session: &Session,
        series_id: &str,
    ) -> Result<Option<crate::BaseItemDto>, ApiError> {
        validate_id(series_id)?;
        let response = self
            .get_series_items(
                session,
                &["Shows", "NextUp"],
                &[
                    ("seriesId", series_id.to_owned()),
                    ("userId", session.user.id.clone()),
                    ("limit", "1".to_owned()),
                    ("enableUserData", "true".to_owned()),
                    ("fields", "Overview".to_owned()),
                ],
            )
            .await?;
        Ok(response.items.into_iter().next())
    }

    async fn get_series_items(
        &self,
        session: &Session,
        path: &[&str],
        query: &[(&str, String)],
    ) -> Result<ItemsResponse, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(path))
            .query(query);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }
}

/// The neighbours of one episode. Either side is `None` at the start or end of the series.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdjacentEpisodes {
    pub previous: Option<crate::BaseItemDto>,
    pub next: Option<crate::BaseItemDto>,
}

impl AdjacentEpisodes {
    /// Splits the server's `[previous, current, next]` window around `episode_id`. A response
    /// that does not contain the episode has no neighbours.
    pub fn from_items(mut items: Vec<crate::BaseItemDto>, episode_id: &str) -> Self {
        let Some(index) = items.iter().position(|item| item.id == episode_id) else {
            return Self::default();
        };
        let next = items.get(index + 1).cloned();
        items.truncate(index);
        Self {
            previous: items.pop(),
            next,
        }
    }
}

fn log_path_once(once: &Once, path: &'static str) {
    once.call_once(|| tracing::info!(endpoint = path, "series list endpoint selected"));
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use reqwest::StatusCode;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };
    use url::Url;

    use crate::{ApiError, ClientConfig, ItemQuery, JellyfinClient, Session, UserDto};
    use mock_jellyfin::{MockConfig, spawn as spawn_mock_jellyfin};

    fn client(base_url: &str) -> JellyfinClient {
        let config = ClientConfig::new(
            base_url,
            "Kattri test",
            "test device",
            "device-id",
            "test-version",
        )
        .expect("valid config");
        JellyfinClient::new(config).expect("client")
    }

    fn session() -> Session {
        Session {
            access_token: "unit-test-token".into(),
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

    async fn mock_server(responses: Vec<(u16, &'static str)>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("server address");
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut request = Vec::new();
                let mut buffer = [0; 2048];
                loop {
                    let read = stream.read(&mut buffer).await.expect("read request");
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request).into_owned());
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            requests
        });
        (format!("http://{address}/jellyfin"), task)
    }

    fn request_uri(request: &str) -> Url {
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("request target");
        Url::parse(&format!("http://localhost{target}")).expect("request URL")
    }

    fn params(url: &Url) -> HashMap<String, String> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[tokio::test]
    async fn list_seasons_serializes_route_user_and_image_parameters_and_parses_dto() {
        let body = r#"{"Items":[{"Id":"season-1","Name":"Season One","Type":"Season","IndexNumber":1}],"TotalRecordCount":1,"StartIndex":0}"#;
        let (base, task) = mock_server(vec![(200, body)]).await;
        let response = client(&base)
            .list_seasons(&session(), "series-1")
            .await
            .expect("seasons");
        let requests = task.await.expect("server task");
        let url = request_uri(&requests[0]);
        assert_eq!(url.path(), "/jellyfin/Shows/series-1/Seasons");
        assert_eq!(
            params(&url),
            HashMap::from([
                ("userId".into(), "user-id".into()),
                ("enableUserData".into(), "true".into()),
                ("enableImages".into(), "true".into()),
                ("imageTypeLimit".into(), "1".into()),
                ("enableImageTypes".into(), "Primary,Backdrop".into()),
            ])
        );
        assert_eq!(response.items[0].index_number, Some(1));
        assert_eq!(response.items[0].item_type.as_deref(), Some("Season"));
    }

    #[tokio::test]
    async fn list_episodes_serializes_route_season_user_and_fields_and_parses_progress() {
        let body = r#"{"Items":[{"Id":"episode-2","Name":"Second","Type":"Episode","IndexNumber":2,"RunTimeTicks":27000000000,"PremiereDate":"2025-01-02T00:00:00Z","UserData":{"PlayedPercentage":42.5,"PlaybackPositionTicks":100}}],"TotalRecordCount":1}"#;
        let (base, task) = mock_server(vec![(200, body)]).await;
        let response = client(&base)
            .list_episodes(&session(), "series-1", "season-1")
            .await
            .expect("episodes");
        let requests = task.await.expect("server task");
        let url = request_uri(&requests[0]);
        assert_eq!(url.path(), "/jellyfin/Shows/series-1/Episodes");
        assert_eq!(
            params(&url),
            HashMap::from([
                ("seasonId".into(), "season-1".into()),
                ("userId".into(), "user-id".into()),
                ("enableUserData".into(), "true".into()),
                ("fields".into(), "Overview,PrimaryImageAspectRatio".into()),
            ])
        );
        let episode = &response.items[0];
        assert_eq!(episode.index_number, Some(2));
        assert_eq!(episode.run_time_ticks, Some(27_000_000_000));
        assert_eq!(
            episode.premiere_date.as_deref(),
            Some("2025-01-02T00:00:00Z")
        );
        assert_eq!(
            episode
                .user_data
                .as_ref()
                .and_then(|data| data.played_percentage),
            Some(42.5)
        );
    }

    #[tokio::test]
    async fn adjacent_episodes_asks_for_the_window_and_splits_it() {
        let body = r#"{"Items":[{"Id":"e1","Name":"One","Type":"Episode"},{"Id":"e2","Name":"Two","Type":"Episode"},{"Id":"e3","Name":"Three","Type":"Episode"}]}"#;
        let (base, task) = mock_server(vec![(200, body)]).await;
        let adjacent = client(&base)
            .adjacent_episodes(&session(), "series-1", "e2")
            .await
            .expect("adjacent episodes");
        let requests = task.await.expect("server task");
        let url = request_uri(&requests[0]);
        assert_eq!(url.path(), "/jellyfin/Shows/series-1/Episodes");
        assert_eq!(
            params(&url).get("adjacentTo").map(String::as_str),
            Some("e2")
        );
        assert_eq!(
            params(&url).get("userId").map(String::as_str),
            Some("user-id")
        );
        assert_eq!(adjacent.previous.map(|item| item.id).as_deref(), Some("e1"));
        assert_eq!(adjacent.next.map(|item| item.id).as_deref(), Some("e3"));
    }

    #[tokio::test]
    async fn season_episodes_are_asked_for_by_season_number() {
        let body = r#"{"Items":[{"Id":"e1","Name":"One"}]}"#;
        let (base, task) = mock_server(vec![(200, body)]).await;
        let episodes = client(&base)
            .season_episodes_by_number(&session(), "series-1", 2)
            .await
            .expect("season");
        assert_eq!(episodes.items[0].id, "e1");
        let requests = task.await.expect("server task");
        let url = request_uri(&requests[0]);
        assert_eq!(url.path(), "/jellyfin/Shows/series-1/Episodes");
        assert_eq!(params(&url).get("season").map(String::as_str), Some("2"));
    }

    #[tokio::test]
    async fn series_next_up_asks_for_one_episode_of_the_series() {
        let body =
            r#"{"Items":[{"Id":"e5","Name":"Five","Type":"Episode","SeriesId":"series-1"}]}"#;
        let (base, task) = mock_server(vec![(200, body), (200, r#"{"Items":[]}"#)]).await;
        let client = client(&base);
        let next = client
            .series_next_up(&session(), "series-1")
            .await
            .expect("next up");
        assert_eq!(next.map(|item| item.id).as_deref(), Some("e5"));
        let none = client
            .series_next_up(&session(), "series-1")
            .await
            .expect("empty next up");
        assert!(none.is_none());
        let requests = task.await.expect("server task");
        let url = request_uri(&requests[0]);
        assert_eq!(url.path(), "/jellyfin/Shows/NextUp");
        let query = params(&url);
        assert_eq!(query.get("seriesId").map(String::as_str), Some("series-1"));
        assert_eq!(query.get("limit").map(String::as_str), Some("1"));
        assert_eq!(query.get("userId").map(String::as_str), Some("user-id"));
    }

    #[tokio::test]
    async fn genres_are_listed_by_name_for_one_library() {
        let body = r#"{"Items":[{"Id":"g1","Name":"Action"},{"Id":"g2","Name":" "},{"Id":"g3","Name":"Drama"}]}"#;
        let (base, task) = mock_server(vec![(200, body)]).await;
        let genres = client(&base)
            .list_genres(&session(), Some("library-1"), Some(&["Movie".to_owned()]))
            .await
            .expect("genres");
        assert_eq!(genres, ["Action", "Drama"]);
        let requests = task.await.expect("server task");
        let url = request_uri(&requests[0]);
        assert_eq!(url.path(), "/jellyfin/Genres");
        let query = params(&url);
        assert_eq!(query.get("parentId").map(String::as_str), Some("library-1"));
        assert_eq!(
            query.get("includeItemTypes").map(String::as_str),
            Some("Movie")
        );
        assert_eq!(query.get("userId").map(String::as_str), Some("user-id"));
    }

    #[test]
    fn adjacent_window_edges_and_a_missing_episode() {
        let item = |id: &str| -> crate::BaseItemDto {
            serde_json::from_value(serde_json::json!({"Id": id, "Name": id})).expect("dto")
        };
        let first = super::AdjacentEpisodes::from_items(vec![item("e1"), item("e2")], "e1");
        assert!(first.previous.is_none());
        assert_eq!(first.next.map(|item| item.id).as_deref(), Some("e2"));
        let last = super::AdjacentEpisodes::from_items(vec![item("e1"), item("e2")], "e2");
        assert_eq!(last.previous.map(|item| item.id).as_deref(), Some("e1"));
        assert!(last.next.is_none());
        assert_eq!(
            super::AdjacentEpisodes::from_items(vec![item("e1")], "other"),
            super::AdjacentEpisodes::default()
        );
    }

    #[tokio::test]
    async fn season_404_falls_back_to_items_with_parent_and_type() {
        let body = r#"{"Items":[{"Id":"season-1","Name":"Season One","IndexNumber":1}]}"#;
        let (base, task) = mock_server(vec![(404, "{}"), (200, body)]).await;
        let response = client(&base)
            .list_seasons(&session(), "series-1")
            .await
            .expect("fallback seasons");
        let requests = task.await.expect("server task");
        assert_eq!(requests.len(), 2);
        let url = request_uri(&requests[1]);
        assert_eq!(url.path(), "/jellyfin/Items");
        assert_eq!(
            params(&url),
            HashMap::from([
                ("userId".into(), "user-id".into()),
                ("parentId".into(), "series-1".into()),
                ("includeItemTypes".into(), "Season".into()),
                ("recursive".into(), "false".into()),
            ])
        );
        assert_eq!(response.items.len(), 1);
    }

    #[tokio::test]
    async fn episode_404_falls_back_to_items_and_non_404_is_preserved() {
        let body = r#"{"Items":[{"Id":"episode-1","Name":"Pilot","IndexNumber":1}]}"#;
        let (base, task) = mock_server(vec![(404, "{}"), (200, body)]).await;
        let response = client(&base)
            .list_episodes(&session(), "series-1", "season-1")
            .await
            .expect("fallback episodes");
        let requests = task.await.expect("server task");
        let url = request_uri(&requests[1]);
        assert_eq!(url.path(), "/jellyfin/Items");
        assert_eq!(
            params(&url).get("parentId").map(String::as_str),
            Some("season-1")
        );
        assert_eq!(
            params(&url).get("includeItemTypes").map(String::as_str),
            Some("Episode")
        );
        assert_eq!(response.items[0].index_number, Some(1));

        let (base, task) = mock_server(vec![(500, "{}")]).await;
        let error = client(&base)
            .list_seasons(&session(), "series-1")
            .await
            .expect_err("non-404 must not trigger fallback");
        assert!(matches!(
            error,
            ApiError::HttpStatus(StatusCode::INTERNAL_SERVER_ERROR)
        ));
        let requests = task.await.expect("server task");
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn list_methods_reject_invalid_identifiers_before_request() {
        let (base, _task) = mock_server(Vec::new()).await;
        let client = client(&base);
        assert!(matches!(
            client.list_seasons(&session(), "bad\nid").await,
            Err(ApiError::InvalidResponse)
        ));
        assert!(matches!(
            client.list_episodes(&session(), "series-1", "").await,
            Err(ApiError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn client_season_fallback_is_visible_in_mock_request_log() {
        const TOKEN: &str = "mock-access-token-0001";
        const USER_ID: &str = "00000000000000000000000000000d01";
        let server = spawn_mock_jellyfin(MockConfig {
            seasons_endpoint_not_found: true,
            ..MockConfig::default()
        })
        .await
        .expect("mock Jellyfin starts");
        let mut authenticated = session();
        authenticated.access_token = TOKEN.into();
        authenticated.user.id = USER_ID.into();
        let client = client(&server.base_url());
        let series = client
            .list_items(
                &authenticated,
                &ItemQuery {
                    limit: 1,
                    include_item_types: Some(vec!["Series".into()]),
                    ..ItemQuery::default()
                },
            )
            .await
            .expect("mock series listing");
        let series_id = series.items.first().expect("seeded series").id.clone();

        let seasons = client
            .list_seasons(&authenticated, &series_id)
            .await
            .expect("404 fallback returns seasons");
        assert_eq!(seasons.items.len(), 3);

        let request_log: serde_json::Value = reqwest::Client::new()
            .get(format!("{}/__mock/requests", server.base_url()))
            .send()
            .await
            .expect("read mock request log")
            .error_for_status()
            .expect("request log succeeds")
            .json()
            .await
            .expect("request log JSON");
        let requests = request_log.as_array().expect("request entries");
        assert!(requests.iter().any(|entry| {
            entry["Method"] == "GET"
                && entry["Path"]
                    .as_str()
                    .is_some_and(|path| path.starts_with(&format!("/Shows/{series_id}/Seasons?")))
        }));
        assert!(requests.iter().any(|entry| {
            entry["Method"] == "GET"
                && entry["Path"].as_str().is_some_and(|path| {
                    path.starts_with("/Items?")
                        && path.contains(&format!("parentId={series_id}"))
                        && path.contains("includeItemTypes=Season")
                })
        }));
        server.shutdown().await;
    }
}
