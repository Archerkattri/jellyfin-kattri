use jellyfin_api::BaseItemDto;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Movie,
    Series,
    Episode,
    Audio,
    Photo,
    Playlist,
    Folder,
    /// Home videos and music videos: played like a movie.
    Video,
    /// A book the client opens or downloads instead of playing.
    Book,
    Other,
}

impl MediaKind {
    pub fn from_server_type(item_type: Option<&str>) -> Self {
        match item_type {
            Some("Movie") => Self::Movie,
            Some("Series") => Self::Series,
            Some("Episode") => Self::Episode,
            Some("Audio" | "MusicAlbum" | "AudioBook") => Self::Audio,
            Some("Video" | "MusicVideo") => Self::Video,
            Some("Book") => Self::Book,
            Some("Photo") => Self::Photo,
            Some("Playlist") => Self::Playlist,
            Some("Folder" | "CollectionFolder" | "BoxSet") => Self::Folder,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Movie => "Movie",
            Self::Series => "Series",
            Self::Episode => "Episode",
            Self::Audio => "Audio",
            Self::Photo => "Photo",
            Self::Playlist => "Playlist",
            Self::Folder => "Folder",
            Self::Video => "Video",
            Self::Book => "Book",
            Self::Other => "Jellyfin item",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ArtworkPalette {
    pub top: [u8; 3],
    pub bottom: [u8; 3],
    pub shape: [u8; 3],
    pub detail: [u8; 3],
    pub motif: ArtworkMotif,
}

#[derive(Clone, Copy, Debug)]
pub enum ArtworkMotif {
    Horizon,
    Orbit,
    Peaks,
    Arches,
    Sound,
}

#[derive(Clone)]
pub struct MediaItem {
    pub id: String,
    pub title: String,
    pub remote_type: Option<String>,
    pub overview: Option<String>,
    pub kind: MediaKind,
    pub artwork: ArtworkPalette,
    /// Item whose Primary image is shown for this entry: the item itself, or its series
    /// when an episode or season has no image of its own.
    pub artwork_id: String,
    /// Item whose backdrop represents this entry: the series for episodes and seasons, as
    /// Jellyfin's own clients show it, otherwise the item itself.
    pub backdrop_id: String,
    pub primary_image_tag: Option<String>,
    pub series_id: Option<String>,
    pub played: bool,
    pub favorite: bool,
    /// Has a saved position, so "Play from beginning" differs from "Play".
    pub resumable: bool,
    /// Release year, rating and runtime, when the server sent them. They are part of the
    /// default item payload, so listing pages get them without asking for extra fields.
    pub production_year: Option<i32>,
    pub official_rating: Option<String>,
    pub run_time_ticks: Option<i64>,
    pub artwork_texture: Option<egui::TextureHandle>,
}

impl MediaItem {
    pub fn from_api(item: BaseItemDto) -> Self {
        let kind = MediaKind::from_server_type(item.item_type.as_deref());
        let artwork = placeholder_artwork(&item.id);
        let own_tag = item
            .image_tags
            .as_ref()
            .and_then(|tags| tags.get("Primary"))
            .and_then(Option::as_deref)
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned);
        let series_artwork = item.series_id.as_deref().zip(
            item.series_primary_image_tag
                .as_deref()
                .map(str::trim)
                .filter(|tag| !tag.is_empty()),
        );
        let (artwork_id, primary_image_tag) = match (own_tag, series_artwork) {
            (Some(tag), _) => (item.id.clone(), Some(tag)),
            (None, Some((series_id, tag))) => (series_id.to_owned(), Some(tag.to_owned())),
            (None, None) => (item.id.clone(), None),
        };
        let backdrop_id = item
            .series_id
            .clone()
            .filter(|_| matches!(item.item_type.as_deref(), Some("Episode" | "Season")))
            .unwrap_or_else(|| item.id.clone());
        let user_data = item.user_data.as_ref();
        let played = user_data.and_then(|data| data.played).unwrap_or(false);
        let favorite = user_data.and_then(|data| data.is_favorite).unwrap_or(false);
        let resumable = user_data
            .and_then(|data| data.playback_position_ticks)
            .is_some_and(|ticks| ticks > 0);
        Self {
            series_id: item.series_id.clone(),
            played,
            favorite,
            resumable,
            production_year: item.production_year,
            official_rating: item
                .official_rating
                .as_deref()
                .map(str::trim)
                .filter(|rating| !rating.is_empty())
                .map(str::to_owned),
            run_time_ticks: item.run_time_ticks,
            artwork_id,
            backdrop_id,
            id: item.id,
            title: item.name,
            remote_type: item.item_type,
            overview: item.overview,
            kind,
            artwork,
            primary_image_tag,
            artwork_texture: None,
        }
    }

    /// An entry for a portrait poster grid. An episode's own image is a 16:9 still that a poster
    /// crops badly, so the grid shows its series poster when the server sent one.
    pub fn poster_from_api(item: BaseItemDto) -> Self {
        let series_poster = (item.item_type.as_deref() == Some("Episode"))
            .then(|| {
                item.series_id.clone().zip(
                    item.series_primary_image_tag
                        .as_deref()
                        .map(str::trim)
                        .filter(|tag| !tag.is_empty())
                        .map(str::to_owned),
                )
            })
            .flatten();
        let mut media = Self::from_api(item);
        if let Some((series_id, tag)) = series_poster {
            media.artwork_id = series_id;
            media.primary_image_tag = Some(tag);
        }
        media
    }

    pub fn subtitle(&self) -> &str {
        self.remote_type
            .as_deref()
            .filter(|item_type| !item_type.is_empty())
            .unwrap_or("Jellyfin item")
    }

    /// One muted line of facts for a list row: year, runtime and rating, in the same order the
    /// details page shows them. Empty when the server sent none of them.
    pub fn meta_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(year) = self.production_year.filter(|year| *year > 0) {
            parts.push(year.to_string());
        }
        if let Some(runtime) = crate::ui::detail::format_runtime(self.run_time_ticks) {
            parts.push(runtime);
        }
        if let Some(rating) = self.official_rating.as_deref() {
            parts.push(rating.to_owned());
        }
        parts.join(" · ")
    }
}

fn placeholder_artwork(id: &str) -> ArtworkPalette {
    const PALETTES: [ArtworkPalette; 5] = [
        ArtworkPalette {
            top: [28, 55, 49],
            bottom: [13, 31, 35],
            shape: [163, 179, 119],
            detail: [221, 192, 134],
            motif: ArtworkMotif::Peaks,
        },
        ArtworkPalette {
            top: [43, 105, 113],
            bottom: [16, 40, 59],
            shape: [100, 202, 175],
            detail: [217, 226, 162],
            motif: ArtworkMotif::Orbit,
        },
        ArtworkPalette {
            top: [67, 118, 116],
            bottom: [27, 52, 57],
            shape: [143, 194, 162],
            detail: [228, 213, 163],
            motif: ArtworkMotif::Horizon,
        },
        ArtworkPalette {
            top: [126, 79, 49],
            bottom: [37, 28, 25],
            shape: [222, 151, 94],
            detail: [244, 218, 178],
            motif: ArtworkMotif::Arches,
        },
        ArtworkPalette {
            top: [53, 95, 151],
            bottom: [19, 40, 83],
            shape: [90, 151, 209],
            detail: [195, 220, 228],
            motif: ArtworkMotif::Sound,
        },
    ];

    let index = id
        .bytes()
        .fold(0usize, |sum, byte| sum.wrapping_add(byte as usize))
        % PALETTES.len();
    PALETTES[index]
}

#[cfg(test)]
mod tests {
    use super::{MediaItem, MediaKind};

    fn item(json: serde_json::Value) -> MediaItem {
        MediaItem::from_api(serde_json::from_value(json).expect("item fixture"))
    }

    #[test]
    fn meta_line_lists_year_runtime_and_rating_in_details_order() {
        let movie = item(serde_json::json!({
            "Id": "m", "Name": "Film", "Type": "Movie",
            "ProductionYear": 2015, "RunTimeTicks": 84_600_000_000i64, "OfficialRating": "PG-13"
        }));
        assert_eq!(movie.meta_line(), "2015 · 2h 21m · PG-13");

        // Missing or blank facts are dropped instead of leaving separators behind.
        let bare = item(serde_json::json!({
            "Id": "b", "Name": "Bare", "Type": "Movie", "OfficialRating": "  "
        }));
        assert_eq!(bare.meta_line(), "");
        let rating_only = item(serde_json::json!({
            "Id": "r", "Name": "Rated", "Type": "Movie", "OfficialRating": "R"
        }));
        assert_eq!(rating_only.meta_line(), "R");
    }

    #[test]
    fn episode_without_its_own_image_uses_the_series_poster_and_backdrop() {
        let episode = item(serde_json::json!({
            "Id": "ep", "Name": "Pilot", "Type": "Episode",
            "SeriesId": "show", "SeriesPrimaryImageTag": "poster"
        }));
        assert_eq!(episode.artwork_id, "show");
        assert_eq!(episode.primary_image_tag.as_deref(), Some("poster"));
        assert_eq!(episode.backdrop_id, "show");
    }

    #[test]
    fn own_primary_image_wins_and_movies_keep_their_own_backdrop() {
        let episode = item(serde_json::json!({
            "Id": "ep", "Name": "Pilot", "Type": "Episode", "ImageTags": {"Primary": "still"},
            "SeriesId": "show", "SeriesPrimaryImageTag": "poster"
        }));
        assert_eq!(episode.artwork_id, "ep");
        assert_eq!(episode.primary_image_tag.as_deref(), Some("still"));
        let poster = MediaItem::poster_from_api(
            serde_json::from_value(serde_json::json!({
                "Id": "ep", "Name": "Pilot", "Type": "Episode", "ImageTags": {"Primary": "still"},
                "SeriesId": "show", "SeriesPrimaryImageTag": "poster"
            }))
            .expect("item fixture"),
        );
        assert_eq!(poster.artwork_id, "show");
        assert_eq!(poster.primary_image_tag.as_deref(), Some("poster"));
        let still_only = MediaItem::poster_from_api(
            serde_json::from_value(serde_json::json!({
                "Id": "ep", "Name": "Pilot", "Type": "Episode", "ImageTags": {"Primary": "still"},
                "SeriesId": "show"
            }))
            .expect("item fixture"),
        );
        assert_eq!(still_only.artwork_id, "ep");
        let movie = item(serde_json::json!({"Id": "film", "Name": "Film", "Type": "Movie"}));
        assert_eq!(movie.artwork_id, "film");
        assert_eq!(movie.backdrop_id, "film");
        assert!(movie.primary_image_tag.is_none());
    }

    #[test]
    fn new_library_item_types_map_to_their_display_kinds() {
        assert_eq!(
            item(serde_json::json!({"Id": "book", "Name": "Novel", "Type": "Book"})).kind,
            MediaKind::Book
        );
        assert_eq!(
            item(serde_json::json!({"Id": "audio", "Name": "Audiobook", "Type": "AudioBook"})).kind,
            MediaKind::Audio
        );
        for item_type in ["Video", "MusicVideo"] {
            assert_eq!(
                item(serde_json::json!({"Id": "video", "Name": "Clip", "Type": item_type})).kind,
                MediaKind::Video,
                "{item_type}"
            );
        }
    }
}
