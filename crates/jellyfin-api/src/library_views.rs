//! Typed calls behind the library page tabs: the paged item lists Jellyfin
//! serves outside `/Items` (genres, studios, artists, upcoming episodes) and
//! the movie recommendation categories.

use reqwest::Method;
use serde::Deserialize;

use crate::{ApiError, BaseItemDto, JellyfinClient, Session, decode_response, home};

/// Most recommendation categories requested at once.
pub const MAX_RECOMMENDATION_CATEGORIES: u32 = 10;
/// Most items requested per recommendation category.
pub const MAX_RECOMMENDATION_ITEMS: u32 = 30;

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
    use super::{ItemsEndpoint, Recommendation, RecommendationKind};

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
}
