//! Media segment data for one item, such as intro, outro, and recap spans that
//! a client can offer to skip.
//!
//! The OpenAPI operation is `GetItemSegments`: `GET /MediaSegments/{itemId}`
//! with an optional `includeSegmentTypes` filter. Segment boundaries are
//! Jellyfin ticks (100-nanosecond units), the same unit used by playback
//! position fields.

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::{ApiError, JellyfinClient, Session, home};

/// The kind of content a media segment covers (`MediaSegmentType` in the
/// OpenAPI). Values the client does not recognize decode as `Unknown`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaSegmentType {
    Commercial,
    Preview,
    Recap,
    Outro,
    Intro,
    #[default]
    #[serde(other)]
    Unknown,
}

impl MediaSegmentType {
    /// The API string used in the `includeSegmentTypes` query parameter.
    pub const fn as_api_value(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Commercial => "Commercial",
            Self::Preview => "Preview",
            Self::Recap => "Recap",
            Self::Outro => "Outro",
            Self::Intro => "Intro",
        }
    }
}

/// One segment of an item, with boundaries in Jellyfin ticks.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSegment {
    pub id: String,
    pub item_id: String,
    #[serde(rename = "Type", default)]
    pub segment_type: MediaSegmentType,
    pub start_ticks: i64,
    pub end_ticks: i64,
}

/// The `MediaSegmentDtoQueryResult` envelope.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSegmentsResponse {
    #[serde(default)]
    pub items: Vec<MediaSegment>,
    #[serde(default)]
    pub total_record_count: Option<i32>,
    #[serde(default)]
    pub start_index: Option<i32>,
}

impl JellyfinClient {
    /// Fetch the media segments for one item, optionally limited to some types.
    ///
    /// An empty `include` slice omits the filter, so the server returns every
    /// segment type it has. An item with no segments yields an empty list.
    /// A 404 means the item is not on this server.
    pub async fn get_media_segments(
        &self,
        session: &Session,
        item_id: &str,
        include: &[MediaSegmentType],
    ) -> Result<MediaSegmentsResponse, ApiError> {
        validate_item_id(item_id)?;
        let url = self.endpoint(&["MediaSegments", item_id]);
        let mut request = self.http.request(Method::GET, url);
        if !include.is_empty() {
            let types = include
                .iter()
                .map(|segment_type| segment_type.as_api_value())
                .collect::<Vec<_>>()
                .join(",");
            request = request.query(&[("includeSegmentTypes", types.as_str())]);
        }
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        super::decode_response(response).await
    }
}

fn validate_item_id(item_id: &str) -> Result<(), ApiError> {
    home::validate_id(item_id)?;
    if item_id
        .chars()
        .any(|character| matches!(character, '/' | '\\'))
        || matches!(item_id, "." | "..")
    {
        return Err(ApiError::InvalidResponse);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MediaSegmentType, MediaSegmentsResponse, validate_item_id};

    #[test]
    fn item_identifiers_refuse_path_separators_and_dot_segments() {
        for value in ["../Users", "folder/item", "folder\\item", ".", ".."] {
            assert!(validate_item_id(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn parses_segments_from_the_openapi_shape() {
        let body = r#"{
            "Items": [
                {
                    "Id": "0a0b0c0d-0000-4000-8000-000000000001",
                    "ItemId": "1a1b1c1d-0000-4000-8000-000000000002",
                    "Type": "Intro",
                    "StartTicks": 0,
                    "EndTicks": 900000000
                },
                {
                    "Id": "0a0b0c0d-0000-4000-8000-000000000003",
                    "ItemId": "1a1b1c1d-0000-4000-8000-000000000002",
                    "Type": "Outro",
                    "StartTicks": 24000000000,
                    "EndTicks": 26000000000
                }
            ],
            "TotalRecordCount": 2,
            "StartIndex": 0
        }"#;
        let response: MediaSegmentsResponse = serde_json::from_str(body).expect("valid fixture");
        assert_eq!(response.total_record_count, Some(2));
        assert_eq!(response.start_index, Some(0));
        assert_eq!(response.items.len(), 2);
        let intro = &response.items[0];
        assert_eq!(intro.segment_type, MediaSegmentType::Intro);
        assert_eq!(intro.start_ticks, 0);
        assert_eq!(intro.end_ticks, 900_000_000);
        assert_eq!(response.items[1].segment_type, MediaSegmentType::Outro);
    }

    #[test]
    fn unrecognized_segment_types_decode_as_unknown() {
        let body =
            r#"{"Items":[{"Id":"x","ItemId":"y","Type":"Credits","StartTicks":1,"EndTicks":2}]}"#;
        let response: MediaSegmentsResponse = serde_json::from_str(body).expect("valid fixture");
        assert_eq!(response.items[0].segment_type, MediaSegmentType::Unknown);
    }

    #[test]
    fn missing_items_and_totals_are_empty_not_errors() {
        let response: MediaSegmentsResponse =
            serde_json::from_str("{}").expect("valid empty envelope");
        assert!(response.items.is_empty());
        assert_eq!(response.total_record_count, None);
    }

    #[test]
    fn segment_type_api_values_match_the_openapi_enum() {
        for (segment_type, expected) in [
            (MediaSegmentType::Unknown, "Unknown"),
            (MediaSegmentType::Commercial, "Commercial"),
            (MediaSegmentType::Preview, "Preview"),
            (MediaSegmentType::Recap, "Recap"),
            (MediaSegmentType::Outro, "Outro"),
            (MediaSegmentType::Intro, "Intro"),
        ] {
            assert_eq!(segment_type.as_api_value(), expected);
            let encoded = serde_json::to_string(&segment_type).expect("serialize");
            assert_eq!(encoded, format!("\"{expected}\""));
        }
    }
}
