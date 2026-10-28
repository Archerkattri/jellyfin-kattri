//! Remote subtitle search and download, as in Jellyfin Web's subtitle editor:
//! `GET /Items/{id}/RemoteSearch/Subtitles/{language}` lists what the server's subtitle
//! providers offer, `POST /Items/{id}/RemoteSearch/Subtitles/{subtitleId}` has the server fetch
//! one and store it beside the video. The language picker reads `GET /Localization/Cultures`.
//!
//! Downloading writes to the server, so it needs an administrator or a user whose policy has
//! `EnableSubtitleManagement`; the server answers 403 otherwise ([`ApiError::Forbidden`]).

use reqwest::{Method, StatusCode};
use serde::Deserialize;

use crate::{ApiError, JellyfinClient, MediaSourceInfo, Session, home};

/// Most search results kept.
pub const MAX_REMOTE_SUBTITLES: usize = 200;
/// Most cultures kept for the language picker.
const MAX_CULTURES: usize = 600;
/// Largest subtitle file the client reads back from the server.
pub const MAX_SUBTITLE_FILE_BYTES: usize = 8 * 1024 * 1024;

/// One subtitle a provider offers for an item.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct RemoteSubtitleInfo {
    /// Provider-specific id passed back to download it.
    pub id: String,
    pub provider_name: Option<String>,
    pub name: Option<String>,
    /// File format, for example `srt`.
    pub format: Option<String>,
    pub author: Option<String>,
    pub comment: Option<String>,
    #[serde(rename = "ThreeLetterISOLanguageName")]
    pub language: Option<String>,
    pub community_rating: Option<f32>,
    pub download_count: Option<i64>,
    pub frame_rate: Option<f32>,
    pub is_hash_match: Option<bool>,
    pub forced: Option<bool>,
    pub hearing_impaired: Option<bool>,
    pub ai_translated: Option<bool>,
    pub machine_translated: Option<bool>,
}

/// A language the server knows, for the search language picker.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct CultureDto {
    pub display_name: String,
    #[serde(rename = "ThreeLetterISOLanguageName")]
    pub three_letter_code: String,
}

/// A subtitle stream of one of an item's media sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemSubtitleStream {
    pub media_source_id: String,
    pub index: i32,
    /// Lowercase codec, used as the file extension when the stream is read as a file.
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_external: bool,
    pub is_text: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct ItemMediaSources {
    media_sources: Vec<MediaSourceInfo>,
}

/// Whether `language` is a plain language code: 2 to 8 ASCII letters, digits or `-`.
fn valid_language(language: &str) -> bool {
    (2..=8).contains(&language.len())
        && language
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// A codec usable as a file extension: short, lowercase ASCII letters and digits.
fn file_format(codec: &str) -> Option<String> {
    let codec = codec.trim().to_ascii_lowercase();
    (!codec.is_empty() && codec.len() <= 12 && codec.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then_some(codec)
}

impl JellyfinClient {
    /// Subtitles the server's providers offer for `item_id` in `language` (a three-letter code
    /// such as `eng`), in the server's order (grouped by provider).
    pub async fn search_remote_subtitles(
        &self,
        session: &Session,
        item_id: &str,
        language: &str,
    ) -> Result<Vec<RemoteSubtitleInfo>, ApiError> {
        self.search_remote_subtitles_matching(session, item_id, language, None)
            .await
    }

    /// Search with Jellyfin's optional `isPerfectMatch` provider filter.
    pub async fn search_remote_subtitles_matching(
        &self,
        session: &Session,
        item_id: &str,
        language: &str,
        is_perfect_match: Option<bool>,
    ) -> Result<Vec<RemoteSubtitleInfo>, ApiError> {
        home::validate_id(item_id)?;
        if !valid_language(language) {
            return Err(ApiError::InvalidResponse);
        }
        let mut request = self.http.request(
            Method::GET,
            self.endpoint(&["Items", item_id, "RemoteSearch", "Subtitles", language]),
        );
        if let Some(is_perfect_match) = is_perfect_match {
            request = request.query(&[("isPerfectMatch", is_perfect_match)]);
        }
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let mut results: Vec<RemoteSubtitleInfo> = crate::decode_response(response).await?;
        results.retain(|result| home::validate_id(&result.id).is_ok());
        results.truncate(MAX_REMOTE_SUBTITLES);
        Ok(results)
    }

    /// Have the server download subtitle `subtitle_id` (from a search result) for `item_id`.
    pub async fn download_remote_subtitle(
        &self,
        session: &Session,
        item_id: &str,
        subtitle_id: &str,
    ) -> Result<(), ApiError> {
        home::validate_id(item_id)?;
        home::validate_id(subtitle_id)?;
        let request = self.http.request(
            Method::POST,
            self.endpoint(&["Items", item_id, "RemoteSearch", "Subtitles", subtitle_id]),
        );
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ApiError::ReauthenticationRequired);
        }
        if status == StatusCode::FORBIDDEN {
            return Err(ApiError::Forbidden);
        }
        if !status.is_success() {
            return Err(crate::status_error(status, response.headers()));
        }
        Ok(())
    }

    /// The server's languages, for the search language picker, sorted by display name.
    pub async fn cultures(&self, session: &Session) -> Result<Vec<CultureDto>, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Localization", "Cultures"]));
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let mut cultures: Vec<CultureDto> = crate::decode_response(response).await?;
        cultures.retain(|culture| {
            valid_language(&culture.three_letter_code) && !culture.display_name.trim().is_empty()
        });
        cultures.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        cultures.dedup_by(|a, b| a.three_letter_code == b.three_letter_code);
        cultures.truncate(MAX_CULTURES);
        Ok(cultures)
    }

    /// Subtitle streams of every media source of `item_id`, as the server lists them now.
    pub async fn item_subtitle_streams(
        &self,
        session: &Session,
        item_id: &str,
    ) -> Result<Vec<ItemSubtitleStream>, ApiError> {
        home::validate_id(item_id)?;
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Items", item_id]))
            .query(&[
                ("userId", session.user.id.as_str()),
                ("fields", "MediaSources,MediaStreams"),
            ]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let item: ItemMediaSources = crate::decode_response(response).await?;
        Ok(subtitle_streams(item))
    }

    /// The text of one subtitle stream, as a file in its own format
    /// (`GET /Videos/{id}/{mediaSourceId}/Subtitles/{index}/Stream.{format}`).
    pub async fn subtitle_stream_file(
        &self,
        session: &Session,
        item_id: &str,
        stream: &ItemSubtitleStream,
    ) -> Result<Vec<u8>, ApiError> {
        home::validate_id(item_id)?;
        home::validate_id(&stream.media_source_id)?;
        if stream.index < 0 {
            return Err(ApiError::InvalidResponse);
        }
        let format = file_format(&stream.codec).ok_or(ApiError::InvalidResponse)?;
        let index = stream.index.to_string();
        let file = format!("Stream.{format}");
        let request = self.http.request(
            Method::GET,
            self.endpoint(&[
                "Videos",
                item_id,
                &stream.media_source_id,
                "Subtitles",
                &index,
                &file,
            ]),
        );
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        crate::trickplay::read_bounded(response, MAX_SUBTITLE_FILE_BYTES, || {
            ApiError::JsonResponseTooLarge
        })
        .await
    }
}

fn subtitle_streams(item: ItemMediaSources) -> Vec<ItemSubtitleStream> {
    item.media_sources
        .into_iter()
        .filter_map(|source| Some((source.id?, source.media_streams.unwrap_or_default())))
        .flat_map(|(source_id, streams)| {
            streams.into_iter().filter_map(move |stream| {
                if stream.stream_type.as_deref() != Some("Subtitle") {
                    return None;
                }
                Some(ItemSubtitleStream {
                    media_source_id: source_id.clone(),
                    index: stream.index?,
                    codec: stream
                        .codec
                        .as_deref()
                        .and_then(file_format)
                        .unwrap_or_default(),
                    language: stream.language,
                    title: stream.display_title.or(stream.title),
                    is_external: stream.is_external,
                    is_text: stream.is_text_subtitle_stream,
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_results_read_the_fields_the_list_shows() {
        let results: Vec<RemoteSubtitleInfo> = serde_json::from_str(
            r#"[{"Id":"os-1","ProviderName":"Open Subtitles","Name":"Show.S01E01.srt",
            "Format":"srt","ThreeLetterISOLanguageName":"eng","CommunityRating":8.5,
            "DownloadCount":1200,"IsHashMatch":true,"HearingImpaired":false,"Extra":1}]"#,
        )
        .expect("results");
        let first = &results[0];
        assert_eq!(first.id, "os-1");
        assert_eq!(first.provider_name.as_deref(), Some("Open Subtitles"));
        assert_eq!(first.format.as_deref(), Some("srt"));
        assert_eq!(first.language.as_deref(), Some("eng"));
        assert_eq!(first.community_rating, Some(8.5));
        assert_eq!(first.download_count, Some(1200));
        assert_eq!(first.is_hash_match, Some(true));
    }

    #[test]
    fn subtitle_streams_keep_only_subtitles_with_an_index_and_a_source() {
        let item: ItemMediaSources = serde_json::from_str(
            r#"{"MediaSources":[{"Id":"src","MediaStreams":[
              {"Type":"Video","Index":0,"Codec":"h264"},
              {"Type":"Subtitle","Index":2,"Codec":"SRT","Language":"eng","DisplayTitle":"English",
               "IsExternal":true,"IsTextSubtitleStream":true},
              {"Type":"Subtitle","Codec":"ass"},
              {"Type":"Subtitle","Index":3,"Codec":"../x"}]},
              {"MediaStreams":[{"Type":"Subtitle","Index":1,"Codec":"srt"}]}]}"#,
        )
        .expect("item");
        let streams = subtitle_streams(item);
        assert_eq!(streams.len(), 2);
        assert_eq!(
            streams[0],
            ItemSubtitleStream {
                media_source_id: "src".into(),
                index: 2,
                codec: "srt".into(),
                language: Some("eng".into()),
                title: Some("English".into()),
                is_external: true,
                is_text: true,
            }
        );
        // A codec that is not a plain extension is kept as a stream but cannot be read back.
        assert_eq!(streams[1].codec, "");
    }

    #[test]
    fn languages_and_formats_are_plain_codes() {
        assert!(valid_language("eng"));
        assert!(valid_language("pt-BR"));
        assert!(!valid_language("e"));
        assert!(!valid_language("../eng"));
        assert_eq!(file_format(" SRT "), Some("srt".into()));
        assert_eq!(file_format("s/rt"), None);
    }
}
