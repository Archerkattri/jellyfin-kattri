//! Deterministic seed catalogue. Every ID, tag and timestamp is derived from
//! fixed inputs, so repeated runs produce byte-identical responses.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

pub(crate) const SERVER_ID: &str = "00000000000000000000000000000005e7";
pub(crate) const DEMO_USER_ID: &str = "00000000000000000000000000000d01";
pub(crate) const MOCK_ACCESS_TOKEN: &str = "mock-access-token-0001";
pub(crate) const DEMO_USERNAME: &str = "demo";
pub(crate) const DEMO_PASSWORD: &str = "demo";
pub(crate) const SEEDED_PERSON_ID: &str = "00000000000000000000000000000a11";
pub(crate) const SEEDED_PERSON_NAME: &str = "Mara Vale";

const TICKS_PER_MINUTE: i64 = 600_000_000;

pub(crate) fn hex_id(value: u128) -> String {
    format!("{value:032x}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Movie,
    BoxSet,
    Series,
    Season,
    Episode,
    MusicAlbum,
    Audio,
    Photo,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Movie => "Movie",
            Kind::BoxSet => "BoxSet",
            Kind::Series => "Series",
            Kind::Season => "Season",
            Kind::Episode => "Episode",
            Kind::MusicAlbum => "MusicAlbum",
            Kind::Audio => "Audio",
            Kind::Photo => "Photo",
        }
    }

    /// `Video` or `Audio` for playable leaves, `None` for containers.
    pub(crate) fn media_type(self) -> Option<&'static str> {
        match self {
            Kind::Movie | Kind::Episode => Some("Video"),
            Kind::Audio => Some("Audio"),
            Kind::BoxSet | Kind::Series | Kind::Season | Kind::MusicAlbum | Kind::Photo => None,
        }
    }

    pub(crate) fn is_leaf(self) -> bool {
        self.media_type().is_some()
    }

    pub(crate) fn is_folder(self) -> bool {
        !self.is_leaf()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub parent_id: Option<String>,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub series_name: Option<String>,
    pub overview: String,
    pub genres: Vec<&'static str>,
    pub production_year: Option<i32>,
    pub run_time_ticks: Option<i64>,
    pub index_number: Option<i32>,
    pub parent_index_number: Option<i32>,
    pub has_backdrop: bool,
    pub played: bool,
    pub favorite: bool,
    pub community_rating: Option<u8>,
    pub position_ticks: i64,
    pub last_played: Option<String>,
    pub date_created: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Library {
    pub id: String,
    pub name: &'static str,
    pub collection_type: &'static str,
}

#[derive(Clone, Debug)]
pub(crate) struct Catalog {
    pub libraries: Vec<Library>,
    pub items: Vec<Item>,
    index: HashMap<String, usize>,
}

impl Catalog {
    pub(crate) fn get(&self, id: &str) -> Option<&Item> {
        self.index.get(id).map(|&position| &self.items[position])
    }

    /// Direct children match when `recursive` is false; any descendant
    /// matches when it is true.
    pub(crate) fn is_under(&self, item: &Item, ancestor: &str, recursive: bool) -> bool {
        if !recursive {
            return item.parent_id.as_deref() == Some(ancestor);
        }
        let mut current = item.parent_id.clone();
        while let Some(parent) = current {
            if parent == ancestor {
                return true;
            }
            current = self.get(&parent).and_then(|p| p.parent_id.clone());
        }
        false
    }

    fn rebuild_index(&mut self) {
        self.index = self
            .items
            .iter()
            .enumerate()
            .map(|(position, item)| (item.id.clone(), position))
            .collect();
    }
}

/// FNV-1a, used to derive stable colours and image tags from IDs.
pub(crate) fn fnv1a64(input: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in input.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn image_tag(item_id: &str, image_type: &str) -> String {
    format!("{:016x}", fnv1a64(&format!("{item_id}:{image_type}")))
}

#[derive(Clone, Copy)]
enum Progress {
    Unplayed,
    /// Minutes watched.
    Partway(i64),
    Played,
}

struct Builder {
    next: u128,
    items: Vec<Item>,
}

struct NewItem<'a> {
    name: &'a str,
    kind: Kind,
    parent_id: &'a str,
    series: Option<(&'a str, &'a str)>,
    season_id: Option<&'a str>,
    parent_index: Option<i32>,
    index: Option<i32>,
    year: Option<i32>,
    minutes: Option<i64>,
    genre: &'static str,
    has_backdrop: bool,
    progress: Progress,
}

impl Builder {
    fn push(&mut self, spec: NewItem<'_>) -> String {
        let id = hex_id(self.next);
        self.next += 1;
        let ordinal = self.items.len() as u32;
        let run_time_ticks = spec.minutes.map(|m| m * TICKS_PER_MINUTE);
        let (played, position_ticks) = match spec.progress {
            Progress::Unplayed => (false, 0),
            Progress::Partway(watched) => (false, watched * TICKS_PER_MINUTE),
            Progress::Played => (true, run_time_ticks.unwrap_or(0)),
        };
        let touched = played || position_ticks > 0;
        self.items.push(Item {
            id: id.clone(),
            name: spec.name.to_owned(),
            kind: spec.kind,
            parent_id: Some(spec.parent_id.to_owned()),
            series_id: spec.series.map(|(sid, _)| sid.to_owned()),
            season_id: spec.season_id.map(str::to_owned),
            series_name: spec.series.map(|(_, name)| name.to_owned()),
            overview: format!(
                "{} is a fictional {} title in the mock Jellyfin seed catalogue.",
                spec.name,
                spec.genre.to_lowercase()
            ),
            genres: vec![spec.genre],
            production_year: spec.year,
            run_time_ticks,
            index_number: spec.index,
            parent_index_number: spec.parent_index,
            has_backdrop: spec.has_backdrop,
            played,
            favorite: fnv1a64(spec.name).is_multiple_of(4),
            community_rating: Some((fnv1a64(spec.name) % 51 + 50) as u8),
            position_ticks,
            last_played: touched.then(|| {
                format!(
                    "2026-09-{:02}T18:{:02}:00.0000000Z",
                    1 + ordinal % 28,
                    ordinal % 60
                )
            }),
            date_created: format!(
                "2026-{:02}-{:02}T09:00:00.0000000Z",
                1 + ordinal % 9,
                1 + (ordinal * 5) % 28
            ),
        });
        id
    }
}

/// Build the fixed seed catalogue: 3 libraries and 43 items
/// (12 movies, 2 box sets, 2 series with 6 seasons and 12 episodes, 2 albums and 6 tracks).
pub(crate) fn catalog() -> Catalog {
    let libraries = vec![
        Library {
            id: hex_id(1),
            name: "Movies",
            collection_type: "movies",
        },
        Library {
            id: hex_id(2),
            name: "Shows",
            collection_type: "tvshows",
        },
        Library {
            id: hex_id(3),
            name: "Music",
            collection_type: "music",
        },
    ];
    let mut b = Builder {
        next: 0x100,
        items: Vec::new(),
    };
    let movies_lib = hex_id(1);
    let shows_lib = hex_id(2);
    let music_lib = hex_id(3);

    let movies: [(&str, i32, i64, &'static str, Progress, bool); 12] = [
        ("Night Harbor", 1994, 112, "Drama", Progress::Played, true),
        (
            "The Glass Orchard",
            2001,
            98,
            "Drama",
            Progress::Partway(25),
            true,
        ),
        (
            "Copperline",
            2007,
            121,
            "Thriller",
            Progress::Unplayed,
            true,
        ),
        (
            "Meridian Drift",
            2012,
            104,
            "Sci-Fi",
            Progress::Partway(40),
            false,
        ),
        ("Lantern Road", 2015, 93, "Family", Progress::Unplayed, true),
        (
            "Silent Quarry",
            1988,
            107,
            "Horror",
            Progress::Played,
            false,
        ),
        ("Ashgrove", 2003, 116, "Drama", Progress::Unplayed, true),
        ("Paper Tide", 2019, 89, "Comedy", Progress::Unplayed, false),
        (
            "Northbound",
            2010,
            126,
            "Adventure",
            Progress::Unplayed,
            true,
        ),
        (
            "The Long Winter",
            1999,
            133,
            "Drama",
            Progress::Played,
            true,
        ),
        (
            "Velvet Circuit",
            2021,
            101,
            "Thriller",
            Progress::Unplayed,
            true,
        ),
        (
            "Hollow Pines",
            2024,
            95,
            "Horror",
            Progress::Partway(10),
            false,
        ),
    ];
    for (name, year, minutes, genre, progress, backdrop) in movies {
        b.push(NewItem {
            name,
            kind: Kind::Movie,
            parent_id: &movies_lib,
            series: None,
            season_id: None,
            parent_index: None,
            index: None,
            year: Some(year),
            minutes: Some(minutes),
            genre,
            has_backdrop: backdrop,
            progress,
        });
    }

    // (series title, year, genre, seasons as (season number, title, episode minutes, progress))
    type SeasonSpec = (i32, &'static str, i64, Vec<Progress>);
    let shows: Vec<(&'static str, i32, &'static str, Vec<SeasonSpec>)> = vec![
        (
            "Harbor Lights",
            2019,
            "Drama",
            vec![
                (
                    1,
                    "Season 1",
                    44,
                    vec![Progress::Played, Progress::Partway(12)],
                ),
                (2, "Season 2", 44, vec![Progress::Unplayed; 2]),
                (0, "Specials", 30, vec![Progress::Unplayed; 2]),
            ],
        ),
        (
            "The Quiet Engine",
            2021,
            "Sci-Fi",
            vec![
                (
                    1,
                    "Season 1",
                    52,
                    vec![Progress::Played, Progress::Unplayed],
                ),
                (2, "Season 2", 52, vec![Progress::Unplayed; 2]),
                (0, "Specials", 30, vec![Progress::Unplayed; 2]),
            ],
        ),
    ];
    for (series_name, year, genre, seasons) in shows {
        let series_id = b.push(NewItem {
            name: series_name,
            kind: Kind::Series,
            parent_id: &shows_lib,
            series: None,
            season_id: None,
            parent_index: None,
            index: None,
            year: Some(year),
            minutes: None,
            genre,
            has_backdrop: true,
            progress: Progress::Unplayed,
        });
        for (season_number, season_name, episode_minutes, progresses) in seasons {
            let season_id = b.push(NewItem {
                name: season_name,
                kind: Kind::Season,
                parent_id: &series_id,
                series: Some((series_id.as_str(), series_name)),
                season_id: None,
                parent_index: None,
                index: Some(season_number),
                year: Some(year),
                minutes: None,
                genre,
                has_backdrop: false,
                progress: Progress::Unplayed,
            });
            for (episode_index, progress) in progresses.into_iter().enumerate() {
                let title = format!("Episode {}", episode_index + 1);
                b.push(NewItem {
                    name: &title,
                    kind: Kind::Episode,
                    parent_id: &season_id,
                    series: Some((series_id.as_str(), series_name)),
                    season_id: Some(&season_id),
                    parent_index: Some(season_number),
                    index: Some(episode_index as i32 + 1),
                    year: Some(year),
                    minutes: Some(episode_minutes),
                    genre,
                    has_backdrop: false,
                    progress,
                });
            }
        }
    }

    type AlbumSpec = (&'static str, i32, &'static [(&'static str, i64)]);
    let albums: [AlbumSpec; 2] = [
        (
            "Tidewater Sessions",
            2016,
            &[
                ("Low Tide", 4),
                ("Undertow", 3),
                ("Slack Water", 5),
                ("Harbor Bell", 3),
            ],
        ),
        (
            "Glass Hours",
            2022,
            &[("First Light", 4), ("Afterimage", 4)],
        ),
    ];
    for (album_name, year, tracks) in albums {
        let album_id = b.push(NewItem {
            name: album_name,
            kind: Kind::MusicAlbum,
            parent_id: &music_lib,
            series: None,
            season_id: None,
            parent_index: None,
            index: None,
            year: Some(year),
            minutes: None,
            genre: "Ambient",
            has_backdrop: false,
            progress: Progress::Unplayed,
        });
        for (track_index, (track, minutes)) in tracks.iter().enumerate() {
            let progress = if year == 2016 && track_index == 0 {
                Progress::Played
            } else {
                Progress::Unplayed
            };
            b.push(NewItem {
                name: track,
                kind: Kind::Audio,
                parent_id: &album_id,
                series: None,
                season_id: None,
                parent_index: Some(1),
                index: Some(track_index as i32 + 1),
                year: Some(year),
                minutes: Some(*minutes),
                genre: "Ambient",
                has_backdrop: false,
                progress,
            });
        }
    }

    for name in ["Coastal Anthology", "Winter Archive"] {
        b.push(NewItem {
            name,
            kind: Kind::BoxSet,
            parent_id: &movies_lib,
            series: None,
            season_id: None,
            parent_index: None,
            index: None,
            year: None,
            minutes: None,
            genre: "Collection",
            has_backdrop: false,
            progress: Progress::Unplayed,
        });
    }

    let photo_folder_id = hex_id(0xF0F0);
    b.push(NewItem {
        name: "Quiet Garden Photo",
        kind: Kind::Photo,
        parent_id: &photo_folder_id,
        series: None,
        season_id: None,
        parent_index: None,
        index: None,
        year: Some(2024),
        minutes: None,
        genre: "Fixture",
        has_backdrop: false,
        progress: Progress::Unplayed,
    });

    let index = b
        .items
        .iter()
        .enumerate()
        .map(|(position, item)| (item.id.clone(), position))
        .collect();
    Catalog {
        libraries,
        items: b.items,
        index,
    }
}

/// Extend the standard catalogue with deterministic fictional movie rows for
/// paging tests without changing the compact default fixture.
pub(crate) fn catalog_with_item_count(target_count: usize) -> Catalog {
    let mut catalog = catalog();
    let target_count = target_count.max(catalog.items.len());
    let movie_library = hex_id(1);
    for index in 0..target_count.saturating_sub(catalog.items.len()) {
        let name = format!("Catalog Fixture {index:03}");
        let ordinal = catalog.items.len() + index;
        let id = hex_id(0x1000 + index as u128);
        let year = 1980 + (index % 45) as i32;
        catalog.items.push(Item {
            id,
            name: name.clone(),
            kind: Kind::Movie,
            parent_id: Some(movie_library.clone()),
            series_id: None,
            season_id: None,
            series_name: None,
            overview: format!("{name} is a synthetic paging fixture."),
            genres: vec!["Fixture"],
            production_year: Some(year),
            run_time_ticks: Some(90 * TICKS_PER_MINUTE),
            index_number: None,
            parent_index_number: None,
            has_backdrop: false,
            played: false,
            favorite: index.is_multiple_of(5),
            community_rating: Some((index % 51 + 50) as u8),
            position_ticks: 0,
            last_played: None,
            date_created: format!(
                "2026-{:02}-{:02}T09:00:00.0000000Z",
                1 + (ordinal / 28) % 9,
                1 + (ordinal * 5) % 28
            ),
        });
    }
    catalog.index = catalog
        .items
        .iter()
        .enumerate()
        .map(|(position, item)| (item.id.clone(), position))
        .collect();
    catalog
}

/// Extend the standard catalogue with deterministic photos in its existing
/// photo folder without changing the default compact fixture.
pub(crate) fn catalog_with_photo_count(target_count: usize) -> Catalog {
    let mut catalog = catalog();
    let photo_folder_id = hex_id(0xF0F0);
    catalog.items.retain(|item| {
        item.kind != Kind::Photo || item.parent_id.as_deref() != Some(&photo_folder_id)
    });
    for index in 0..target_count {
        let name = format!("Photo Fixture {index:04}");
        let ordinal = catalog.items.len() + index;
        catalog.items.push(Item {
            id: hex_id(0xF10000 + index as u128),
            name: name.clone(),
            kind: Kind::Photo,
            parent_id: Some(photo_folder_id.clone()),
            series_id: None,
            season_id: None,
            series_name: None,
            overview: format!("{name} is a synthetic photo paging fixture."),
            genres: vec!["Fixture"],
            production_year: Some(2026),
            run_time_ticks: None,
            index_number: None,
            parent_index_number: None,
            has_backdrop: false,
            played: false,
            favorite: false,
            community_rating: None,
            position_ticks: 0,
            last_played: None,
            date_created: format!(
                "2026-{:02}-{:02}T09:00:00.0000000Z",
                1 + (ordinal / 28) % 9,
                1 + (ordinal * 5) % 28
            ),
        });
    }
    catalog.rebuild_index();
    catalog
}

/// Extend the standard catalogue with deterministic audio tracks for queue
/// restore tests without enlarging the compact default fixture.
pub(crate) fn catalog_with_audio_track_count(target_count: usize) -> Catalog {
    let mut catalog = catalog();
    let Some(template) = catalog
        .items
        .iter()
        .find(|item| item.kind == Kind::Audio)
        .cloned()
    else {
        return catalog;
    };
    let existing = catalog
        .items
        .iter()
        .filter(|item| item.kind == Kind::Audio)
        .count();
    let target_count = target_count.max(existing);
    for index in existing..target_count {
        let mut track = template.clone();
        track.id = hex_id(0xA0000 + index as u128);
        track.name = format!("Queue Fixture Track {index:03}");
        track.overview = format!("Synthetic audio track {index} for queue restore tests.");
        track.index_number = Some(i32::try_from(index).unwrap_or(i32::MAX));
        track.favorite = index.is_multiple_of(5);
        track.played = false;
        track.position_ticks = 0;
        track.last_played = None;
        track.date_created = format!("2026-10-{:02}T09:00:00.0000000Z", 1 + index % 28);
        catalog.items.push(track);
    }
    catalog.rebuild_index();
    catalog
}

/// Standard catalogue with a deterministic collection tree for M3-16.
pub(crate) fn catalog_with_collection_children() -> Catalog {
    let mut catalog = catalog();
    let Some(collection) = catalog
        .items
        .iter()
        .find(|item| item.kind == Kind::BoxSet && item.name == "Coastal Anthology")
        .map(|item| item.id.clone())
    else {
        return catalog;
    };
    let Some(movie_template) = catalog
        .items
        .iter()
        .find(|item| item.kind == Kind::Movie)
        .cloned()
    else {
        return catalog;
    };
    let Some(box_set_template) = catalog
        .items
        .iter()
        .find(|item| item.kind == Kind::BoxSet)
        .cloned()
    else {
        return catalog;
    };

    for index in 0..4 {
        let mut movie = movie_template.clone();
        movie.id = hex_id(0x9000 + index);
        movie.name = format!("Anthology Film {}", index + 1);
        movie.parent_id = Some(collection.clone());
        movie.index_number = Some(index as i32 + 1);
        movie.parent_index_number = None;
        movie.series_id = None;
        movie.season_id = None;
        movie.series_name = None;
        movie.favorite = false;
        movie.played = false;
        movie.position_ticks = 0;
        movie.last_played = None;
        catalog.items.push(movie);
    }

    let mut nested = box_set_template;
    nested.id = hex_id(0x9100);
    nested.name = "Coastal Shorts".to_owned();
    nested.parent_id = Some(collection);
    nested.index_number = Some(5);
    nested.favorite = false;
    nested.played = false;
    nested.position_ticks = 0;
    nested.last_played = None;
    catalog.items.push(nested);
    catalog.rebuild_index();
    catalog
}

/// Render an item as Jellyfin's `BaseItemDto` (PascalCase; absent when unknown).
pub(crate) fn item_json(item: &Item) -> Value {
    let mut map = Map::new();
    map.insert("Name".into(), json!(item.name));
    map.insert("ServerId".into(), json!(SERVER_ID));
    map.insert("Id".into(), json!(item.id));
    map.insert("Type".into(), json!(item.kind.as_str()));
    map.insert("IsFolder".into(), json!(item.kind.is_folder()));
    if matches!(item.kind, Kind::Movie | Kind::Episode) {
        map.insert("OfficialRating".into(), json!("PG-13"));
    }
    map.insert("Overview".into(), json!(item.overview));
    map.insert("Genres".into(), json!(item.genres));
    map.insert("DateCreated".into(), json!(item.date_created));
    map.insert(
        "ImageTags".into(),
        json!({ "Primary": image_tag(&item.id, "Primary") }),
    );
    if item.has_backdrop {
        map.insert(
            "BackdropImageTags".into(),
            json!([image_tag(&item.id, "Backdrop")]),
        );
    }
    if let Some(media_type) = item.kind.media_type() {
        map.insert("MediaType".into(), json!(media_type));
    }
    if let Some(parent) = &item.parent_id {
        map.insert("ParentId".into(), json!(parent));
    }
    if let Some(series_id) = &item.series_id {
        map.insert("SeriesId".into(), json!(series_id));
    }
    if let Some(season_id) = &item.season_id {
        map.insert("SeasonId".into(), json!(season_id));
    }
    if let Some(series_name) = &item.series_name {
        map.insert("SeriesName".into(), json!(series_name));
    }
    if let Some(year) = item.production_year {
        map.insert("ProductionYear".into(), json!(year));
        map.insert(
            "PremiereDate".into(),
            json!(format!("{year}-01-01T00:00:00.0000000Z")),
        );
    }
    if let Some(ticks) = item.run_time_ticks {
        map.insert("RunTimeTicks".into(), json!(ticks));
    }
    if let Some(index) = item.index_number {
        map.insert("IndexNumber".into(), json!(index));
    }
    if let Some(index) = item.parent_index_number {
        map.insert("ParentIndexNumber".into(), json!(index));
    }
    if item.kind.is_leaf() {
        let aspect = if item.kind == Kind::Audio {
            1.0
        } else {
            0.6667
        };
        map.insert("PrimaryImageAspectRatio".into(), json!(aspect));
    }
    map.insert("UserData".into(), user_data_json(item));
    if item.kind.media_type() == Some("Video") {
        map.insert("People".into(), Value::Array(people(item)));
    }
    if let Some(rating) = item.community_rating {
        map.insert("CommunityRating".into(), json!(f64::from(rating) / 10.0));
    }
    Value::Object(map)
}

pub(crate) fn people(item: &Item) -> Vec<Value> {
    if item.kind.media_type() != Some("Video") {
        return Vec::new();
    }
    vec![json!({
        "Name": SEEDED_PERSON_NAME,
        "Id": SEEDED_PERSON_ID,
        "Role": "Lead",
        "Type": "Actor",
        "PrimaryImageTag": image_tag(SEEDED_PERSON_ID, "Primary"),
    })]
}

pub(crate) fn is_person_name(name: &str) -> bool {
    name == SEEDED_PERSON_NAME
}

pub(crate) fn user_data_json(item: &Item) -> Value {
    let mut map = Map::new();
    map.insert("PlaybackPositionTicks".into(), json!(item.position_ticks));
    map.insert("PlayCount".into(), json!(i32::from(item.played)));
    map.insert("IsFavorite".into(), json!(item.favorite));
    map.insert("Played".into(), json!(item.played));
    map.insert("Key".into(), json!(item.id));
    let in_progress = item.position_ticks > 0 && !item.played;
    if let Some(runtime) = item.run_time_ticks.filter(|&r| r > 0 && in_progress) {
        let percent = item.position_ticks as f64 / runtime as f64 * 100.0;
        map.insert(
            "PlayedPercentage".into(),
            json!((percent * 10.0).round() / 10.0),
        );
    }
    if let Some(last) = &item.last_played {
        map.insert("LastPlayedDate".into(), json!(last));
    }
    Value::Object(map)
}

pub(crate) fn user_json() -> Value {
    json!({ "Id": DEMO_USER_ID, "Name": DEMO_USERNAME, "ServerId": SERVER_ID })
}

#[cfg(test)]
mod tests {
    use super::{Kind, catalog, catalog_with_audio_track_count};

    #[test]
    fn seed_counts_match_documentation() {
        let catalog = catalog();
        assert_eq!(catalog.libraries.len(), 3);
        assert_eq!(catalog.items.len(), 43);
        let count = |kind: Kind| {
            catalog
                .items
                .iter()
                .filter(|item| item.kind == kind)
                .count()
        };
        assert_eq!(count(Kind::Movie), 12);
        assert_eq!(count(Kind::BoxSet), 2);
        assert_eq!(count(Kind::Series), 2);
        assert_eq!(count(Kind::Season), 6);
        assert_eq!(count(Kind::Episode), 12);
        assert_eq!(count(Kind::MusicAlbum), 2);
        assert_eq!(count(Kind::Audio), 6);
        assert_eq!(count(Kind::Photo), 1);
    }

    #[test]
    fn queue_fixture_has_two_hundred_deterministic_audio_tracks() {
        let first = catalog_with_audio_track_count(200);
        let second = catalog_with_audio_track_count(200);
        let ids = |catalog: &super::Catalog| {
            catalog
                .items
                .iter()
                .filter(|item| item.kind == Kind::Audio)
                .map(|item| item.id.clone())
                .collect::<Vec<_>>()
        };
        let first_ids = ids(&first);
        assert_eq!(first_ids.len(), 200);
        assert_eq!(first_ids, ids(&second));
    }

    #[test]
    fn ids_are_unique_and_deterministic() {
        let first: Vec<String> = catalog().items.into_iter().map(|item| item.id).collect();
        let second: Vec<String> = catalog().items.into_iter().map(|item| item.id).collect();
        assert_eq!(first, second);
        let mut sorted = first.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 43);
        assert!(first.iter().all(|id| id.len() == 32));
    }

    #[test]
    fn official_rating_is_present_only_on_seeded_video_items() {
        let catalog = catalog();
        let movie = catalog
            .items
            .iter()
            .find(|item| item.kind == Kind::Movie)
            .expect("movie fixture exists");
        let episode = catalog
            .items
            .iter()
            .find(|item| item.kind == Kind::Episode)
            .expect("episode fixture exists");
        let box_set = catalog
            .items
            .iter()
            .find(|item| item.kind == Kind::BoxSet)
            .expect("box set fixture exists");

        assert_eq!(super::item_json(movie)["OfficialRating"], "PG-13");
        assert_eq!(super::item_json(episode)["OfficialRating"], "PG-13");
        assert!(super::item_json(box_set).get("OfficialRating").is_none());
    }
}
