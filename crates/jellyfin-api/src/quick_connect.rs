//! Quick Connect sign-in.
//!
//! Flow: check [`JellyfinClient::quick_connect_enabled`], start with
//! [`JellyfinClient::initiate_quick_connect`] and show its
//! [`QuickConnectRequest::code`] to the user, poll
//! [`JellyfinClient::quick_connect_status`] until it returns
//! [`QuickConnectStatus::Authorized`], then call
//! [`JellyfinClient::authenticate_with_quick_connect`]. The poll interval, the
//! overall time limit, and cancellation belong to the caller. A status poll
//! that returns [`ApiError::HttpStatus`] with 404 means the request is unknown
//! or expired and the user must start again.
//!
//! Routes (all from the official Jellyfin OpenAPI, which declares no security
//! requirement for them, so they are sent without a token):
//! - `GET /QuickConnect/Enabled` returns a bool.
//! - `POST /QuickConnect/Initiate` returns `Secret` and `Code`; 401 means
//!   Quick Connect is not active on this server.
//! - `GET /QuickConnect/Connect?secret=...` returns `Authenticated`; 404 means
//!   the secret is unknown or expired.
//! - `POST /Users/AuthenticateWithQuickConnect` takes `{"Secret": ...}` and
//!   returns the usual authentication result.
//!
//! Secret handling: [`QuickConnectSecret`] is zeroized on drop, redacted in
//! `Debug`, and never logged or formatted. The OpenAPI requires `secret` as a
//! query parameter on `GET /QuickConnect/Connect`, so that one request places
//! the secret in the URL. Reverse proxies and server access logs may record
//! it. This module never logs that URL, and errors carry no URL text. The
//! JSON body of the authenticate request is copied into transport buffers that
//! this crate cannot zeroize.

use std::fmt;

use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::{ApiError, AuthenticationResult, JellyfinClient, Session, decode_response};

/// Longest accepted Quick Connect secret. Real secrets are far shorter.
const MAX_SECRET_LEN: usize = 256;
/// Longest accepted user-facing Quick Connect code.
const MAX_CODE_LEN: usize = 64;

/// A Quick Connect secret. The value is zeroized on drop and is never shown
/// by `Debug`. It is intentionally not `Clone`, `Display`, or `Serialize`.
pub struct QuickConnectSecret(String);

impl QuickConnectSecret {
    fn new(value: String) -> Result<Self, ApiError> {
        if value.is_empty() || value.len() > MAX_SECRET_LEN || value.chars().any(char::is_control) {
            return Err(ApiError::InvalidResponse);
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl Zeroize for QuickConnectSecret {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for QuickConnectSecret {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl fmt::Debug for QuickConnectSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QuickConnectSecret([REDACTED])")
    }
}

/// A started Quick Connect request. The `code` is shown to the user; the
/// `secret` stays inside the client and is used only for status and
/// authentication calls.
pub struct QuickConnectRequest {
    code: String,
    secret: QuickConnectSecret,
}

impl QuickConnectRequest {
    /// The short code the user approves on another signed-in device.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The secret that identifies this request for status and authentication.
    pub fn secret(&self) -> &QuickConnectSecret {
        &self.secret
    }
}

impl fmt::Debug for QuickConnectRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QuickConnectRequest")
            .field("code", &self.code)
            .field("secret", &self.secret)
            .finish()
    }
}

/// Result of one [`JellyfinClient::quick_connect_status`] poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuickConnectStatus {
    /// The request has not been approved yet; keep polling.
    Pending,
    /// The request is approved; call [`JellyfinClient::authenticate_with_quick_connect`].
    Authorized,
}

/// Wire shape of `QuickConnectResult`. Field names are PascalCase in JSON.
/// The secret is zeroized if this value is dropped unconsumed.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QuickConnectResultDto {
    #[serde(default)]
    authenticated: bool,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

impl Drop for QuickConnectResultDto {
    fn drop(&mut self) {
        if let Some(secret) = self.secret.as_mut() {
            secret.zeroize();
        }
    }
}

/// Wire shape of `QuickConnectDto`, the body of the authenticate request.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct QuickConnectBody<'a> {
    secret: &'a str,
}

fn request_from_result(mut result: QuickConnectResultDto) -> Result<QuickConnectRequest, ApiError> {
    let secret = QuickConnectSecret::new(result.secret.take().ok_or(ApiError::InvalidResponse)?)?;
    let code = result.code.take().ok_or(ApiError::InvalidResponse)?;
    if code.is_empty() || code.len() > MAX_CODE_LEN || code.chars().any(char::is_control) {
        return Err(ApiError::InvalidResponse);
    }
    Ok(QuickConnectRequest { code, secret })
}

impl JellyfinClient {
    /// Ask the server whether Quick Connect is enabled.
    ///
    /// A 401 or 404 response means the server does not offer Quick Connect and
    /// is reported as `Ok(false)`. Other failures are returned as errors.
    pub async fn quick_connect_enabled(&self) -> Result<bool, ApiError> {
        let url = self.endpoint(&["QuickConnect", "Enabled"]);
        let request = self.add_metadata(self.http.request(Method::GET, url), None);
        let response = request.send().await.map_err(crate::transport_error)?;
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND
        ) {
            return Ok(false);
        }
        decode_response(response).await
    }

    /// Start a Quick Connect request. The returned code is for display; the
    /// secret is kept private.
    ///
    /// A 401 response means Quick Connect is not active on this server. It is
    /// returned as [`ApiError::HttpStatus`] with 401 rather than
    /// [`ApiError::ReauthenticationRequired`], because no session exists yet.
    pub async fn initiate_quick_connect(&self) -> Result<QuickConnectRequest, ApiError> {
        let url = self.endpoint(&["QuickConnect", "Initiate"]);
        let request = self.add_metadata(self.http.request(Method::POST, url), None);
        let response = request.send().await.map_err(crate::transport_error)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(ApiError::HttpStatus(StatusCode::UNAUTHORIZED));
        }
        let result: QuickConnectResultDto = decode_response(response).await?;
        request_from_result(result)
    }

    /// Poll the state of a Quick Connect request.
    ///
    /// A 404 response means the secret is unknown or has expired and is
    /// returned as [`ApiError::HttpStatus`] with 404. The caller should show
    /// "Quick Connect code expired. Start again."
    pub async fn quick_connect_status(
        &self,
        secret: &QuickConnectSecret,
    ) -> Result<QuickConnectStatus, ApiError> {
        let url = self.endpoint(&["QuickConnect", "Connect"]);
        // The OpenAPI requires `secret` as a query parameter. See the module docs.
        let request = self
            .http
            .request(Method::GET, url)
            .query(&[("secret", secret.expose())]);
        let request = self.add_metadata(request, None);
        let response = request.send().await.map_err(crate::transport_error)?;
        let result: QuickConnectResultDto = decode_response(response).await?;
        Ok(if result.authenticated {
            QuickConnectStatus::Authorized
        } else {
            QuickConnectStatus::Pending
        })
    }

    /// Exchange an authorized Quick Connect secret for a session. Secrets are
    /// single use: after success, the same secret returns 404 when polled, and
    /// authenticating again returns 401.
    pub async fn authenticate_with_quick_connect(
        &self,
        secret: &QuickConnectSecret,
    ) -> Result<Session, ApiError> {
        let url = self.endpoint(&["Users", "AuthenticateWithQuickConnect"]);
        let body = QuickConnectBody {
            secret: secret.expose(),
        };
        let request = self.add_metadata(self.http.request(Method::POST, url).json(&body), None);
        let response = request.send().await.map_err(crate::transport_error)?;
        let result: AuthenticationResult = decode_response(response).await?;
        if result.access_token.is_empty() || result.access_token.chars().any(char::is_control) {
            return Err(ApiError::InvalidResponse);
        }
        Ok(Session {
            access_token: result.access_token,
            user: result.user,
            server_id: result.server_id,
            session_info: result.session_info,
        })
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroize;

    use super::{QuickConnectBody, QuickConnectResultDto, QuickConnectSecret, request_from_result};
    use crate::ApiError;

    fn parse(json: &str) -> QuickConnectResultDto {
        serde_json::from_str(json).expect("valid fixture")
    }

    #[test]
    fn initiate_response_yields_code_and_secret() {
        let request = request_from_result(parse(
            r#"{"Authenticated": false, "Secret": "s3cret-value", "Code": "123456",
                "DeviceId": "d", "DeviceName": "n", "AppName": "a", "AppVersion": "1",
                "DateAdded": "2026-01-01T00:00:00Z"}"#,
        ))
        .expect("valid initiate response");
        assert_eq!(request.code(), "123456");
        assert_eq!(request.secret().expose(), "s3cret-value");
    }

    #[test]
    fn initiate_response_without_secret_or_code_is_invalid() {
        let missing_secret = request_from_result(parse(r#"{"Code": "123456"}"#));
        assert!(matches!(missing_secret, Err(ApiError::InvalidResponse)));
        let missing_code = request_from_result(parse(r#"{"Secret": "abc"}"#));
        assert!(matches!(missing_code, Err(ApiError::InvalidResponse)));
    }

    #[test]
    fn empty_or_control_character_secret_is_rejected() {
        let empty = request_from_result(parse(r#"{"Secret": "", "Code": "1"}"#));
        assert!(matches!(empty, Err(ApiError::InvalidResponse)));
        let control = request_from_result(parse(r#"{"Secret": "a\nb", "Code": "1"}"#));
        assert!(matches!(control, Err(ApiError::InvalidResponse)));
    }

    #[test]
    fn status_response_reads_authenticated_flag() {
        assert!(parse(r#"{"Authenticated": true, "Code": "1"}"#).authenticated);
        assert!(!parse(r#"{"Authenticated": false}"#).authenticated);
        assert!(!parse(r#"{}"#).authenticated);
    }

    #[test]
    fn secret_debug_output_is_redacted() {
        let secret = QuickConnectSecret::new("visible-secret-text".to_owned()).expect("valid");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("visible-secret-text"));
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn request_debug_output_redacts_secret_but_shows_code() {
        let request = request_from_result(parse(
            r#"{"Secret": "visible-secret-text", "Code": "424242"}"#,
        ))
        .expect("valid");
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("visible-secret-text"));
        assert!(rendered.contains("424242"));
    }

    #[test]
    fn zeroize_clears_secret_bytes() {
        let mut secret = QuickConnectSecret::new("zeroize-me".to_owned()).expect("valid");
        secret.zeroize();
        assert!(secret.expose().is_empty());
    }

    #[test]
    fn authenticate_body_uses_pascal_case_secret_field() {
        let value = serde_json::to_value(QuickConnectBody { secret: "abc" }).expect("serialize");
        assert_eq!(value, serde_json::json!({ "Secret": "abc" }));
    }
}
