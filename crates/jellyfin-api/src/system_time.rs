//! Server UTC time for SyncPlay clock alignment (M4-11).
//!
//! `GET /GetUtcTime` returns `RequestReceptionTime` and `ResponseTransmissionTime`, both in
//! Jellyfin's UTC form (for example `2026-10-15T20:00:00.0000000Z`). This module reads no
//! clock and parses no time. The caller stamps the local send and receive times around the
//! call and parses the server time with the SyncPlay timestamp parser.
//!
//! [UNCONFIRMED] The route and the field names come from memory of the Jellyfin server
//! source, not from its OpenAPI document. Confirm them on a live server before relying on
//! them. The mock server in `mock-jellyfin` does not serve this route.

use reqwest::Method;
use serde::Deserialize;

use crate::{ApiError, JellyfinClient, Session, decode_response};

/// Server times from `GET /GetUtcTime`. A missing field takes an empty string, which the
/// timestamp parser refuses, so a short response cannot produce a sample.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct UtcTimeResponse {
    /// When the server received the request.
    pub request_reception_time: String,
    /// When the server sent the response. The clock sampler uses this value because the
    /// estimator assumes the server stamped its reply about halfway through the round trip.
    pub response_transmission_time: String,
}

impl JellyfinClient {
    /// Read the server clock (`GET /GetUtcTime`), with the session token.
    pub async fn get_utc_time(&self, session: &Session) -> Result<UtcTimeResponse, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["GetUtcTime"]));
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }
}
