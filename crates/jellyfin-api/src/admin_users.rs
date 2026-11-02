//! Typed user-management requests (server administrator operations).
//!
//! Routes and wire DTOs were checked against Jellyfin Server `v12.0`, commit
//! `6c073e19ddf604b2369c638716164fdab4c952dc`:
//!
//! - `GET /Users` (`UserController.GetUsers`, response `UserDto[]`)
//! - `GET /Users/{userId}` (`GetUserById`, response `UserDto`)
//! - `POST /Users/New` (`CreateUserByName`, body `Name` and optional `Password`)
//! - `POST /Users?userId={userId}` (`UpdateUser`, body `UserDto`)
//! - `POST /Users/{userId}/Policy` (`UpdateUserPolicy`, body `UserPolicy`)
//! - `DELETE /Users/{userId}` (`DeleteUser`)
//!
//! Source: <https://github.com/jellyfin/jellyfin/blob/6c073e19ddf604b2369c638716164fdab4c952dc/Jellyfin.Api/Controllers/UserController.cs>
//! DTOs: <https://github.com/jellyfin/jellyfin/blob/6c073e19ddf604b2369c638716164fdab4c952dc/MediaBrowser.Model/Users/UserPolicy.cs>
//! and <https://github.com/jellyfin/jellyfin/blob/6c073e19ddf604b2369c638716164fdab4c952dc/MediaBrowser.Model/Dto/UserDto.cs>.
//!
//! Policy writes first read the current `UserDto` and preserve every unmodeled
//! policy field before changing the requested flags. This avoids sending a
//! partial policy that would reset server-owned permissions or guessed enum
//! values. The server remains authoritative for authorization and invariants
//! such as retaining an enabled administrator.

use std::fmt;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::{ApiError, JellyfinClient, Session, decode_response};

/// Client resource cap for a single administrator user listing.
pub const MAX_ADMIN_USERS: usize = 10_000;

// Client-side request bounds; these are not claims about Jellyfin's own limits.
const MAX_USER_ID_BYTES: usize = 36;
const MAX_USER_NAME_BYTES: usize = 256;
const MAX_USER_PASSWORD_BYTES: usize = 4_096;

/// A server user with only the identity and policy fields used by management UI.
///
/// Unknown top-level DTO properties are ignored; this response type therefore
/// cannot retain an unexpected password or token field from a server response.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct AdminUser {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub policy: Option<AdminUserPolicy>,
}

/// User-policy flags surfaced directly to callers.
///
/// The remaining server policy is retained opaquely for safe read-modify-write
/// behavior. This includes string-backed server enums such as `SyncPlayAccess`;
/// the client deliberately does not guess their variant set. Additional
/// fields are omitted from `Debug` output.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct AdminUserPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_administrator: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_media_playback: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_remote_access: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    authentication_provider_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    password_reset_provider_id: String,
    #[serde(flatten)]
    additional_fields: Map<String, Value>,
}

impl fmt::Debug for AdminUserPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdminUserPolicy")
            .field("is_administrator", &self.is_administrator)
            .field("is_hidden", &self.is_hidden)
            .field("is_disabled", &self.is_disabled)
            .field("enable_media_playback", &self.enable_media_playback)
            .field("enable_remote_access", &self.enable_remote_access)
            .field(
                "authentication_provider_id",
                &self.authentication_provider_id,
            )
            .field(
                "password_reset_provider_id",
                &self.password_reset_provider_id,
            )
            .field("additional_fields", &"[REDACTED]")
            .finish()
    }
}

/// Changes to the boolean user-policy settings exposed by this API.
///
/// Unset values are left as the server currently has them. Apply this patch
/// through [`JellyfinClient::update_user_policy`] so the full existing policy
/// is retained in the request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdminUserPolicyChanges {
    pub is_administrator: Option<bool>,
    pub is_hidden: Option<bool>,
    pub is_disabled: Option<bool>,
    pub enable_media_playback: Option<bool>,
    pub enable_remote_access: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct CreateUserBody<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<&'a str>,
}

impl JellyfinClient {
    /// List users visible to the authenticated session (`GET /Users`).
    ///
    /// The response uses the shared bounded JSON decoder and is additionally
    /// capped at [`MAX_ADMIN_USERS`]. Jellyfin authorizes the listing request.
    pub async fn list_admin_users(&self, session: &Session) -> Result<Vec<AdminUser>, ApiError> {
        let request = self.http.request(Method::GET, self.endpoint(&["Users"]));
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        let users: Vec<AdminUser> = decode_response(response).await?;
        if users.len() > MAX_ADMIN_USERS {
            return Err(ApiError::InvalidResponse);
        }
        Ok(users)
    }

    /// Create a user (`POST /Users/New`). The server requires an elevated session.
    ///
    /// The optional password is consumed as a zeroizing string and is sent only
    /// in the request body. It is not retained in the returned DTO or errors.
    pub async fn create_user(
        &self,
        session: &Session,
        name: &str,
        password: Option<Zeroizing<String>>,
    ) -> Result<AdminUser, ApiError> {
        if !valid_user_name(name)
            || password
                .as_ref()
                .is_some_and(|value| value.len() > MAX_USER_PASSWORD_BYTES)
        {
            return Err(ApiError::InvalidConfiguration);
        }

        let password = password.as_deref().map(String::as_str);
        let body = CreateUserBody { name, password };
        let request = self
            .http
            .request(Method::POST, self.endpoint(&["Users", "New"]))
            .json(&body);
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }

    /// Change a user's name (`POST /Users?userId={userId}`).
    ///
    /// Jellyfin's update DTO also writes user configuration, so this method
    /// reads the current DTO first and preserves its `Configuration` and other
    /// server fields while replacing `Name`.
    pub async fn update_user_name(
        &self,
        session: &Session,
        user_id: &str,
        name: &str,
    ) -> Result<(), ApiError> {
        if !valid_user_id(user_id) || !valid_user_name(name) {
            return Err(ApiError::InvalidConfiguration);
        }

        let mut document = self.get_user_document(session, user_id).await?;
        let Some(fields) = document.as_object_mut() else {
            return Err(ApiError::InvalidResponse);
        };
        if !fields.get("Configuration").is_some_and(Value::is_object) {
            return Err(ApiError::InvalidResponse);
        }
        fields.insert("Name".to_owned(), Value::String(name.to_owned()));

        let request = self
            .http
            .request(Method::POST, self.endpoint(&["Users"]))
            .query(&[("userId", user_id)])
            .json(&document);
        self.send_admin(session, request).await
    }

    /// Update selected policy flags (`POST /Users/{userId}/Policy`).
    ///
    /// The target ID is explicit. This method fetches the complete existing
    /// policy and changes only `Some` fields in `changes`; the server requires
    /// an elevated session and remains responsible for permission invariants.
    pub async fn update_user_policy(
        &self,
        session: &Session,
        user_id: &str,
        changes: &AdminUserPolicyChanges,
    ) -> Result<(), ApiError> {
        if !valid_user_id(user_id) {
            return Err(ApiError::InvalidConfiguration);
        }
        if changes.is_empty() {
            return Ok(());
        }

        let mut user = self.get_admin_user(session, user_id).await?;
        let Some(mut policy) = user.policy.take() else {
            return Err(ApiError::InvalidResponse);
        };
        policy.apply(changes);
        if policy.authentication_provider_id.is_empty()
            || policy.password_reset_provider_id.is_empty()
        {
            return Err(ApiError::InvalidResponse);
        }

        let request = self
            .http
            .request(Method::POST, self.endpoint(&["Users", user_id, "Policy"]))
            .json(&policy);
        self.send_admin(session, request).await
    }

    /// Delete the explicitly identified user (`DELETE /Users/{userId}`).
    ///
    /// This is destructive and requires an elevated session. The server may
    /// reject deletion when it would remove the last user or administrator.
    pub async fn delete_user(&self, session: &Session, user_id: &str) -> Result<(), ApiError> {
        if !valid_user_id(user_id) {
            return Err(ApiError::InvalidConfiguration);
        }
        let request = self
            .http
            .request(Method::DELETE, self.endpoint(&["Users", user_id]));
        self.send_admin(session, request).await
    }

    async fn get_admin_user(
        &self,
        session: &Session,
        user_id: &str,
    ) -> Result<AdminUser, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Users", user_id]));
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        let user: AdminUser = decode_response(response).await?;
        if !same_user_id(&user.id, user_id) {
            return Err(ApiError::InvalidResponse);
        }
        Ok(user)
    }

    async fn get_user_document(&self, session: &Session, user_id: &str) -> Result<Value, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Users", user_id]));
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        let document: Value = decode_response(response).await?;
        let Some(response_id) = document.get("Id").and_then(Value::as_str) else {
            return Err(ApiError::InvalidResponse);
        };
        if !same_user_id(response_id, user_id) {
            return Err(ApiError::InvalidResponse);
        }
        Ok(document)
    }
}

impl AdminUserPolicy {
    fn apply(&mut self, changes: &AdminUserPolicyChanges) {
        if let Some(value) = changes.is_administrator {
            self.is_administrator = Some(value);
        }
        if let Some(value) = changes.is_hidden {
            self.is_hidden = Some(value);
        }
        if let Some(value) = changes.is_disabled {
            self.is_disabled = Some(value);
        }
        if let Some(value) = changes.enable_media_playback {
            self.enable_media_playback = Some(value);
        }
        if let Some(value) = changes.enable_remote_access {
            self.enable_remote_access = Some(value);
        }
    }
}

impl AdminUserPolicyChanges {
    fn is_empty(&self) -> bool {
        self.is_administrator.is_none()
            && self.is_hidden.is_none()
            && self.is_disabled.is_none()
            && self.enable_media_playback.is_none()
            && self.enable_remote_access.is_none()
    }
}

fn valid_user_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= MAX_USER_NAME_BYTES
        && !name.chars().any(char::is_control)
}

fn valid_user_id(user_id: &str) -> bool {
    if user_id.len() == 32 {
        return user_id
            .chars()
            .all(|character| character.is_ascii_hexdigit());
    }
    user_id.len() == MAX_USER_ID_BYTES
        && user_id
            .chars()
            .enumerate()
            .all(|(index, character)| match index {
                8 | 13 | 18 | 23 => character == '-',
                _ => character.is_ascii_hexdigit(),
            })
}

fn same_user_id(left: &str, right: &str) -> bool {
    if !valid_user_id(left) || !valid_user_id(right) {
        return false;
    }
    left.replace('-', "")
        .eq_ignore_ascii_case(&right.replace('-', ""))
}
