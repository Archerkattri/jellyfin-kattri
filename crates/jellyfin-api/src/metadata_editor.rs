//! Administrator metadata management, following the Jellyfin Web metadata editor, item
//! identifier and image editor:
//!
//! - `GET /Items/{id}` and `POST /Items/{id}` (`UpdateItem`) edit an item's metadata.
//! - `POST /Items/RemoteSearch/{type}` and `POST /Items/RemoteSearch/Apply/{id}` identify an
//!   item against the server's metadata providers.
//! - `GET /Items/{id}/Images`, `GET /Items/{id}/RemoteImages`,
//!   `POST /Items/{id}/RemoteImages/Download` and `DELETE /Items/{id}/Images/{type}/{index}`
//!   manage its artwork.
//!
//! All of these need an administrator; the server answers 403 otherwise, returned as
//! [`ApiError::Forbidden`].

use std::time::Duration;

use reqwest::{Method, StatusCode, Url, header};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    ApiError, BaseItemPerson, JellyfinClient, MAX_PRIMARY_IMAGE_RESPONSE_BYTES, Session,
    decode_response, home, is_local_host, status_error, transport_error,
};

/// Most identify results or remote images the client keeps from one response.
pub const MAX_REMOTE_RESULTS: usize = 100;
/// Most current images listed for one item.
pub const MAX_ITEM_IMAGES: usize = 200;
/// Longest text the editor accepts for one field; longer values are cut.
const MAX_FIELD_CHARS: usize = 20_000;
/// Most entries in a genre, tag or studio list.
const MAX_LIST_ENTRIES: usize = 500;

/// Fields the editor can lock against provider refreshes (Jellyfin `MetadataField`).
pub const LOCKABLE_FIELDS: &[&str] = &[
    "Name",
    "Overview",
    "Genres",
    "OfficialRating",
    "Cast",
    "ProductionLocations",
    "Runtime",
    "Studios",
    "Tags",
];

/// Item types Jellyfin can identify against remote providers, as Jellyfin Web offers them.
pub const IDENTIFIABLE_TYPES: &[&str] = &[
    "Movie",
    "Trailer",
    "Series",
    "BoxSet",
    "Person",
    "Book",
    "MusicAlbum",
    "MusicArtist",
    "MusicVideo",
];

/// Whether `item_type` has a remote-search endpoint.
pub fn is_identifiable(item_type: &str) -> bool {
    IDENTIFIABLE_TYPES.contains(&item_type)
}

/// The editable part of an item. [`EditableItem`] keeps the rest of the server's DTO so a
/// save sends back every field the editor does not show unchanged.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetadataFields {
    pub name: String,
    /// `ForcedSortName`; empty lets the server derive the sort title.
    pub sort_name: String,
    pub original_title: String,
    pub overview: String,
    pub production_year: Option<i32>,
    /// `YYYY-MM-DD`, or empty for none.
    pub premiere_date: String,
    pub official_rating: String,
    pub community_rating: Option<f32>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub studios: Vec<String>,
    /// Lock the whole item against provider refreshes (`LockData`).
    pub lock_data: bool,
    /// Fields from [`LOCKABLE_FIELDS`] kept as they are on refresh.
    pub locked_fields: Vec<String>,
}

/// One item as the metadata editor loaded it.
#[derive(Clone, Debug, PartialEq)]
pub struct EditableItem {
    pub id: String,
    pub item_type: Option<String>,
    pub fields: MetadataFields,
    /// Credits, shown read-only.
    pub people: Vec<BaseItemPerson>,
    /// Provider ids such as `Tmdb` or `Imdb`, used to seed an identify search.
    pub provider_ids: Vec<(String, String)>,
    raw: Map<String, Value>,
}

impl EditableItem {
    /// Builds the editor view of a full `BaseItemDto` JSON object.
    pub fn from_json(value: Value) -> Result<Self, ApiError> {
        let Value::Object(raw) = value else {
            return Err(ApiError::InvalidResponse);
        };
        let id = raw
            .get("Id")
            .and_then(Value::as_str)
            .ok_or(ApiError::InvalidResponse)?
            .to_owned();
        home::validate_id(&id)?;
        let text = |key: &str| {
            raw.get(key)
                .and_then(Value::as_str)
                .map(|value| bounded(value.to_owned()))
                .unwrap_or_default()
        };
        let strings = |key: &str| -> Vec<String> {
            raw.get(key)
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .take(MAX_LIST_ENTRIES)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let studios = raw
            .get("Studios")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|studio| studio.get("Name").and_then(Value::as_str))
                    .take(MAX_LIST_ENTRIES)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let premiere_date = raw
            .get("PremiereDate")
            .and_then(Value::as_str)
            .and_then(|date| date.get(..10))
            .filter(|date| is_iso_date(date))
            .unwrap_or_default()
            .to_owned();
        let fields = MetadataFields {
            name: text("Name"),
            sort_name: text("ForcedSortName"),
            original_title: text("OriginalTitle"),
            overview: text("Overview"),
            production_year: raw
                .get("ProductionYear")
                .and_then(Value::as_i64)
                .and_then(|year| i32::try_from(year).ok()),
            premiere_date,
            official_rating: text("OfficialRating"),
            community_rating: raw
                .get("CommunityRating")
                .and_then(Value::as_f64)
                .map(|rating| rating as f32),
            genres: strings("Genres"),
            tags: strings("Tags"),
            studios,
            lock_data: raw
                .get("LockData")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            locked_fields: strings("LockedFields"),
        };
        let people = raw
            .get("People")
            .cloned()
            .and_then(|people| serde_json::from_value::<Vec<BaseItemPerson>>(people).ok())
            .unwrap_or_default();
        let provider_ids = raw
            .get("ProviderIds")
            .and_then(Value::as_object)
            .map(|ids| {
                ids.iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .filter(|(_, value)| !value.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            id,
            item_type: raw.get("Type").and_then(Value::as_str).map(str::to_owned),
            fields,
            people,
            provider_ids,
            raw,
        })
    }

    /// The DTO to post: the loaded item with the edited fields written over it.
    pub fn to_update_json(&self, fields: &MetadataFields) -> Value {
        let mut body = self.raw.clone();
        let optional_text = |value: &str| {
            let value = value.trim();
            if value.is_empty() {
                Value::Null
            } else {
                Value::String(value.to_owned())
            }
        };
        body.insert("Name".into(), Value::String(fields.name.trim().to_owned()));
        body.insert("ForcedSortName".into(), optional_text(&fields.sort_name));
        body.insert(
            "OriginalTitle".into(),
            optional_text(&fields.original_title),
        );
        body.insert("Overview".into(), optional_text(&fields.overview));
        body.insert(
            "OfficialRating".into(),
            optional_text(&fields.official_rating),
        );
        body.insert("ProductionYear".into(), json!(fields.production_year));
        body.insert(
            "CommunityRating".into(),
            fields
                .community_rating
                .filter(|rating| rating.is_finite())
                .map_or(Value::Null, |rating| json!(rating.clamp(0.0, 10.0))),
        );
        let date = fields.premiere_date.trim();
        body.insert(
            "PremiereDate".into(),
            if is_iso_date(date) {
                Value::String(format!("{date}T00:00:00.0000000Z"))
            } else {
                Value::Null
            },
        );
        body.insert("Genres".into(), json!(clean_list(&fields.genres)));
        body.insert("Tags".into(), json!(clean_list(&fields.tags)));
        // Keep a studio's id when its name is unchanged so the server does not relink it.
        let previous: Vec<&Map<String, Value>> = self
            .raw
            .get("Studios")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_object).collect())
            .unwrap_or_default();
        let studios: Vec<Value> = clean_list(&fields.studios)
            .into_iter()
            .map(|name| {
                previous
                    .iter()
                    .find(|studio| studio.get("Name").and_then(Value::as_str) == Some(&name))
                    .map_or_else(|| json!({ "Name": name }), |studio| json!(studio))
            })
            .collect();
        body.insert("Studios".into(), Value::Array(studios));
        body.insert("LockData".into(), Value::Bool(fields.lock_data));
        let locked: Vec<&str> = LOCKABLE_FIELDS
            .iter()
            .copied()
            .filter(|field| fields.locked_fields.iter().any(|locked| locked == field))
            .collect();
        body.insert("LockedFields".into(), json!(locked));
        Value::Object(body)
    }
}

fn bounded(mut value: String) -> String {
    if let Some((index, _)) = value.char_indices().nth(MAX_FIELD_CHARS) {
        value.truncate(index);
    }
    value
}

fn clean_list(values: &[String]) -> Vec<String> {
    let mut cleaned: Vec<String> = Vec::new();
    for value in values {
        let value = value.trim();
        if !value.is_empty() && !cleaned.iter().any(|existing| existing == value) {
            cleaned.push(value.to_owned());
        }
        if cleaned.len() >= MAX_LIST_ENTRIES {
            break;
        }
    }
    cleaned
}

/// `YYYY-MM-DD` with a plausible month and day.
pub fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| {
        value
            .get(range)
            .filter(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|part| part.parse::<u32>().ok())
    };
    matches!(
        (digits(0..4), digits(5..7), digits(8..10)),
        (Some(_), Some(1..=12), Some(1..=31))
    )
}

/// What to look for in an identify search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteSearchQuery {
    pub name: String,
    pub year: Option<i32>,
    /// Provider ids such as `("Tmdb", "603")`; empty values are left out.
    pub provider_ids: Vec<(String, String)>,
}

/// One identify match. The server's JSON is kept so Apply posts it back exactly.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteSearchResult {
    pub name: String,
    pub production_year: Option<i32>,
    pub provider_name: Option<String>,
    pub overview: Option<String>,
    /// Poster address on the provider; fetch it with [`JellyfinClient::get_remote_thumbnail`].
    pub image_url: Option<String>,
    raw: Value,
}

impl RemoteSearchResult {
    fn from_json(raw: Value) -> Option<Self> {
        let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            name: text("Name")?,
            production_year: raw
                .get("ProductionYear")
                .and_then(Value::as_i64)
                .and_then(|year| i32::try_from(year).ok()),
            provider_name: text("SearchProviderName"),
            overview: text("Overview").map(bounded),
            image_url: text("ImageUrl"),
            raw,
        })
    }
}

/// Jellyfin image types the image editor works with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageType {
    Primary,
    Art,
    Backdrop,
    Banner,
    Logo,
    Thumb,
    Disc,
    Box,
    Screenshot,
    Menu,
    BoxRear,
}

impl ImageType {
    /// Types offered when browsing remote images, in Jellyfin Web's order.
    pub const ALL: &[Self] = &[
        Self::Primary,
        Self::Art,
        Self::Backdrop,
        Self::Banner,
        Self::Box,
        Self::BoxRear,
        Self::Disc,
        Self::Logo,
        Self::Menu,
        Self::Screenshot,
        Self::Thumb,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "Primary",
            Self::Art => "Art",
            Self::Backdrop => "Backdrop",
            Self::Banner => "Banner",
            Self::Logo => "Logo",
            Self::Thumb => "Thumb",
            Self::Disc => "Disc",
            Self::Box => "Box",
            Self::Screenshot => "Screenshot",
            Self::Menu => "Menu",
            Self::BoxRear => "BoxRear",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }
}

/// One image the item has now (`ImageInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemImage {
    pub image_type: ImageType,
    pub index: u32,
    pub tag: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageInfoDto {
    image_type: String,
    #[serde(default)]
    image_index: Option<u32>,
    #[serde(default)]
    image_tag: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

/// One image a provider offers (`RemoteImageInfo`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteImage {
    #[serde(default)]
    pub provider_name: Option<String>,
    pub url: String,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub community_rating: Option<f64>,
    #[serde(default)]
    pub vote_count: Option<u32>,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteImageResultDto {
    #[serde(default)]
    images: Vec<RemoteImage>,
}

impl JellyfinClient {
    /// Load `item_id` with every field for the metadata editor (`GET /Items/{id}`).
    pub async fn get_item_metadata(
        &self,
        session: &Session,
        item_id: &str,
    ) -> Result<EditableItem, ApiError> {
        home::validate_id(item_id)?;
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Items", item_id]))
            .query(&[("userId", session.user.id.as_str())]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(transport_error)?;
        let value: Value = decode_response(response).await?;
        EditableItem::from_json(value)
    }

    /// Save `fields` onto `item` (`POST /Items/{id}`, `UpdateItem`).
    pub async fn update_item_metadata(
        &self,
        session: &Session,
        item: &EditableItem,
        fields: &MetadataFields,
    ) -> Result<(), ApiError> {
        home::validate_id(&item.id)?;
        if fields.name.trim().is_empty() {
            return Err(ApiError::InvalidConfiguration);
        }
        let request = self
            .http
            .request(Method::POST, self.endpoint(&["Items", &item.id]))
            .json(&item.to_update_json(fields));
        self.send_admin(session, request).await
    }

    /// Search the server's metadata providers for matches (`POST /Items/RemoteSearch/{type}`).
    pub async fn remote_search(
        &self,
        session: &Session,
        item_id: &str,
        item_type: &str,
        query: &RemoteSearchQuery,
    ) -> Result<Vec<RemoteSearchResult>, ApiError> {
        home::validate_id(item_id)?;
        if !is_identifiable(item_type) {
            return Err(ApiError::InvalidConfiguration);
        }
        let mut search_info = Map::new();
        let name = query.name.trim();
        if !name.is_empty() {
            search_info.insert("Name".into(), Value::String(name.to_owned()));
        }
        if let Some(year) = query.year {
            search_info.insert("Year".into(), json!(year));
        }
        let provider_ids: Map<String, Value> = query
            .provider_ids
            .iter()
            .filter(|(key, value)| !key.is_empty() && !value.trim().is_empty())
            .map(|(key, value)| (key.clone(), Value::String(value.trim().to_owned())))
            .collect();
        if name.is_empty() && provider_ids.is_empty() {
            return Err(ApiError::InvalidConfiguration);
        }
        search_info.insert("ProviderIds".into(), Value::Object(provider_ids));
        let body = json!({ "SearchInfo": search_info, "ItemId": item_id });
        let request = self
            .http
            .request(
                Method::POST,
                self.endpoint(&["Items", "RemoteSearch", item_type]),
            )
            .json(&body);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(transport_error)?;
        let values: Vec<Value> = decode_response(response).await?;
        Ok(values
            .into_iter()
            .filter_map(RemoteSearchResult::from_json)
            .take(MAX_REMOTE_RESULTS)
            .collect())
    }

    /// Apply an identify match to `item_id` (`POST /Items/RemoteSearch/Apply/{id}`).
    pub async fn apply_remote_search_result(
        &self,
        session: &Session,
        item_id: &str,
        result: &RemoteSearchResult,
        replace_all_images: bool,
    ) -> Result<(), ApiError> {
        home::validate_id(item_id)?;
        let request = self
            .http
            .request(
                Method::POST,
                self.endpoint(&["Items", "RemoteSearch", "Apply", item_id]),
            )
            .query(&[(
                "replaceAllImages",
                if replace_all_images { "true" } else { "false" },
            )])
            .json(&result.raw);
        self.send_admin(session, request).await
    }

    /// The images `item_id` has now (`GET /Items/{id}/Images`).
    pub async fn item_images(
        &self,
        session: &Session,
        item_id: &str,
    ) -> Result<Vec<ItemImage>, ApiError> {
        home::validate_id(item_id)?;
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Items", item_id, "Images"]));
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(transport_error)?;
        let images: Vec<ImageInfoDto> = decode_response(response).await?;
        Ok(images
            .into_iter()
            // Chapter and profile images are not managed from the editor.
            .filter_map(|image| {
                Some(ItemImage {
                    image_type: ImageType::parse(&image.image_type)?,
                    index: image.image_index.unwrap_or(0),
                    tag: image.image_tag,
                    width: image.width,
                    height: image.height,
                })
            })
            .take(MAX_ITEM_IMAGES)
            .collect())
    }

    /// Images the providers offer for `item_id` (`GET /Items/{id}/RemoteImages`).
    pub async fn remote_images(
        &self,
        session: &Session,
        item_id: &str,
        image_type: ImageType,
        include_all_languages: bool,
    ) -> Result<Vec<RemoteImage>, ApiError> {
        home::validate_id(item_id)?;
        let limit = MAX_REMOTE_RESULTS.to_string();
        let request = self
            .http
            .request(
                Method::GET,
                self.endpoint(&["Items", item_id, "RemoteImages"]),
            )
            .query(&[
                ("type", image_type.as_str()),
                ("startIndex", "0"),
                ("limit", limit.as_str()),
                (
                    "includeAllLanguages",
                    if include_all_languages {
                        "true"
                    } else {
                        "false"
                    },
                ),
            ]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(transport_error)?;
        let result: RemoteImageResultDto = decode_response(response).await?;
        Ok(result.images.into_iter().take(MAX_REMOTE_RESULTS).collect())
    }

    /// Ask the server to download `image_url` as the item's `image_type` image
    /// (`POST /Items/{id}/RemoteImages/Download`).
    pub async fn download_remote_image(
        &self,
        session: &Session,
        item_id: &str,
        image_type: ImageType,
        image_url: &str,
    ) -> Result<(), ApiError> {
        home::validate_id(item_id)?;
        if image_url.is_empty() || image_url.len() > 4096 || image_url.chars().any(char::is_control)
        {
            return Err(ApiError::InvalidConfiguration);
        }
        let request = self
            .http
            .request(
                Method::POST,
                self.endpoint(&["Items", item_id, "RemoteImages", "Download"]),
            )
            .query(&[("type", image_type.as_str()), ("imageUrl", image_url)]);
        self.send_admin(session, request).await
    }

    /// Delete one image (`DELETE /Items/{id}/Images/{type}/{index}`).
    pub async fn delete_item_image(
        &self,
        session: &Session,
        item_id: &str,
        image_type: ImageType,
        index: u32,
    ) -> Result<(), ApiError> {
        home::validate_id(item_id)?;
        let index = index.to_string();
        let request = self.http.request(
            Method::DELETE,
            self.endpoint(&["Items", item_id, "Images", image_type.as_str(), &index]),
        );
        self.send_admin(session, request).await
    }

    /// A bounded JPEG of one current image, for the image editor's grid.
    pub async fn get_item_image(
        &self,
        session: &Session,
        item_id: &str,
        image: &ItemImage,
    ) -> Result<Vec<u8>, ApiError> {
        home::validate_id(item_id)?;
        let index = image.index.to_string();
        self.get_primary_image_at(
            session,
            &[
                "Items",
                item_id,
                "Images",
                image.image_type.as_str(),
                &index,
            ],
            image.tag.as_deref(),
            480,
            480,
        )
        .await
    }

    /// Fetch a provider thumbnail (an identify poster or a remote image preview).
    ///
    /// These addresses point at third-party hosts, so no Jellyfin credentials are sent,
    /// redirects are not followed, plain HTTP is refused unless the host is local, and the
    /// body is capped like card artwork.
    pub async fn get_remote_thumbnail(&self, url: &str) -> Result<Vec<u8>, ApiError> {
        let url = Url::parse(url).map_err(|_| ApiError::InvalidConfiguration)?;
        let host = url.host_str().unwrap_or_default();
        let allowed = match url.scheme() {
            "https" => !host.is_empty(),
            "http" => is_local_host(host),
            _ => false,
        };
        if !allowed || !url.username().is_empty() || url.password().is_some() {
            return Err(ApiError::InsecureRemoteTransport);
        }
        let mut response = self
            .http
            .request(Method::GET, url)
            .header(header::ACCEPT, "image/jpeg, image/png")
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status();
        if status != StatusCode::OK {
            return Err(status_error(status, response.headers()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PRIMARY_IMAGE_RESPONSE_BYTES as u64)
        {
            return Err(ApiError::ImageTooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if body.len().saturating_add(chunk.len()) > MAX_PRIMARY_IMAGE_RESPONSE_BYTES {
                return Err(ApiError::ImageTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Err(ApiError::InvalidResponse);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie() -> Value {
        json!({
            "Id": "m1", "Name": "Film", "Type": "Movie", "Path": "/media/film.mkv",
            "ForcedSortName": "Film, The", "ProductionYear": 2001,
            "PremiereDate": "2001-05-04T00:00:00.0000000Z", "CommunityRating": 7.5,
            "Genres": ["Drama"], "Tags": ["tag"],
            "Studios": [{"Name": "Studio A", "Id": "s1"}],
            "People": [{"Name": "Ada", "Role": "Lead", "Type": "Actor"}],
            "ProviderIds": {"Tmdb": "603", "Imdb": ""},
            "LockedFields": ["Name"], "LockData": false, "FutureField": {"kept": true}
        })
    }

    #[test]
    fn editor_view_reads_the_editable_fields() {
        let item = EditableItem::from_json(movie()).expect("item");
        assert_eq!(item.fields.name, "Film");
        assert_eq!(item.fields.sort_name, "Film, The");
        assert_eq!(item.fields.premiere_date, "2001-05-04");
        assert_eq!(item.fields.community_rating, Some(7.5));
        assert_eq!(item.fields.studios, ["Studio A"]);
        assert_eq!(item.fields.locked_fields, ["Name"]);
        assert_eq!(item.people.len(), 1);
        assert_eq!(item.provider_ids, [("Tmdb".to_owned(), "603".to_owned())]);
    }

    #[test]
    fn update_keeps_unshown_fields_and_writes_the_edits() {
        let item = EditableItem::from_json(movie()).expect("item");
        let mut fields = item.fields.clone();
        fields.name = " New name ".into();
        fields.sort_name.clear();
        fields.premiere_date = "1999-12-31".into();
        fields.genres = vec!["Drama".into(), " ".into(), "Drama".into(), "Comedy".into()];
        fields.studios = vec!["Studio A".into(), "Studio B".into()];
        fields.locked_fields = vec!["Overview".into(), "Bogus".into()];
        fields.community_rating = Some(42.0);
        let body = item.to_update_json(&fields);
        assert_eq!(body["Name"], "New name");
        assert_eq!(body["ForcedSortName"], Value::Null);
        assert_eq!(body["PremiereDate"], "1999-12-31T00:00:00.0000000Z");
        assert_eq!(body["Genres"], json!(["Drama", "Comedy"]));
        assert_eq!(
            body["Studios"],
            json!([{"Name": "Studio A", "Id": "s1"}, {"Name": "Studio B"}])
        );
        assert_eq!(body["LockedFields"], json!(["Overview"]));
        assert_eq!(body["CommunityRating"], json!(10.0));
        assert_eq!(body["Path"], "/media/film.mkv");
        assert_eq!(body["FutureField"], json!({"kept": true}));
    }

    #[test]
    fn dates_must_be_plain_iso_days() {
        assert!(is_iso_date("2024-02-29"));
        assert!(!is_iso_date("2024-13-01"));
        assert!(!is_iso_date("2024-1-01"));
        assert!(!is_iso_date("24-01-011"));
    }

    #[test]
    fn image_types_round_trip_and_unknown_ones_are_dropped() {
        for kind in ImageType::ALL {
            assert_eq!(ImageType::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(ImageType::parse("Chapter"), None);
    }

    #[tokio::test]
    async fn remote_thumbnails_refuse_plain_http_to_public_hosts() {
        let client = JellyfinClient::new(
            crate::ClientConfig::new("http://127.0.0.1:1", "t", "t", "t", "t").expect("config"),
        )
        .expect("client");
        for url in [
            "http://image.example.com/poster.jpg",
            "ftp://image.example.com/poster.jpg",
            "https://user:pass@image.example.com/poster.jpg",
            "not a url",
        ] {
            assert!(
                client.get_remote_thumbnail(url).await.is_err(),
                "{url} should be refused"
            );
        }
    }
}
