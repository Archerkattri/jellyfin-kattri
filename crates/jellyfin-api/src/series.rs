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
    /// Maximum number of episodes returned by one paged season request.
    pub const MAX_EPISODE_PAGE_SIZE: u32 = 48;

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

    /// List one bounded page of a season's episodes using
    /// `GET /Shows/{seriesId}/Episodes?seasonId=...&startIndex=...&limit=...`.
    ///
    /// A zero limit is invalid; larger limits are capped at
    /// [`JellyfinClient::MAX_EPISODE_PAGE_SIZE`]. A 404 from the dedicated endpoint falls back to
    /// the equivalent paged `/Items` query.
    pub async fn list_episodes_page(
        &self,
        session: &Session,
        series_id: &str,
        season_id: &str,
        start_index: u32,
        requested_limit: u32,
    ) -> Result<ItemsResponse, ApiError> {
        validate_id(series_id)?;
        validate_id(season_id)?;
        if requested_limit == 0 {
            return Err(ApiError::InvalidResponse);
        }
        let start_index = start_index.to_string();
        let limit = requested_limit.min(Self::MAX_EPISODE_PAGE_SIZE).to_string();
        let primary = self
            .get_series_items(
                session,
                &["Shows", series_id, "Episodes"],
                &[
                    ("seasonId", season_id.to_owned()),
                    ("userId", session.user.id.clone()),
                    ("enableUserData", "true".to_owned()),
                    ("fields", "Overview,PrimaryImageAspectRatio".to_owned()),
                    ("startIndex", start_index.clone()),
                    ("limit", limit.clone()),
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
                        ("startIndex", start_index),
                        ("limit", limit),
                        ("enableTotalRecordCount", "true".to_owned()),
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
