//! Read-only admin-lite calls (M4-22): server information and the library list.
//!
//! This module performs no server changes. Only `GET` requests are made here,
//! and a test in `tests/search_admin.rs` checks this file's source for any
//! mutating method.
//!
//! Endpoints, confirmed against the official stable OpenAPI document at
//! `https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json` (checked
//! 2026-10-15):
//!
//! - `GET /System/Info` (operation `GetSystemInfo`, schema `SystemInfo`). The
//!   security requirement is `DefaultAuthorization`.
//! - `GET /Library/VirtualFolders` (operation `GetVirtualFolders`, schema
//!   `VirtualFolderInfo[]`). The security requirement is
//!   `FirstTimeSetupOrElevated`, so a session without elevated rights receives
//!   403. The client maps that to [`ApiError::Forbidden`](crate::ApiError::Forbidden),
//!   and the caller should hide the admin section.
//!
//! `VirtualFolderInfo` has no per-library item count. This module does not
//! fetch counts. [UNCONFIRMED] Whether a per-library count is available from
//! another documented operation was not checked.
//!
//! `GET /Sessions` is [UNCONFIRMED]: the route, its `IsActive` field, and its
//! admin requirement were not checked against the OpenAPI document, because
//! this change allows no network access beyond loopback. The 403 mapping is
//! the same as for the library list.
//!
//! Admin status is decided by the server. The client only reads the outcome of
//! these calls.

use reqwest::Method;
use serde::Deserialize;

use crate::{ApiError, JellyfinClient, Session, decode_response};

/// Server identity and version from `GET /System/Info`. Every field is
/// optional so that a server omitting a field still decodes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct ServerInfo {
    pub id: Option<String>,
    pub server_name: Option<String>,
    pub version: Option<String>,
    pub product_name: Option<String>,
    pub operating_system_display_name: Option<String>,
    pub startup_wizard_completed: Option<bool>,
    pub local_address: Option<String>,
}

/// One library from `GET /Library/VirtualFolders`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct VirtualFolder {
    pub name: Option<String>,
    pub item_id: Option<String>,
    /// Server-extensible collection type such as `movies`, `tvshows`, or `music`.
    pub collection_type: Option<String>,
    pub locations: Vec<String>,
}

/// One entry from `GET /Sessions`. Only the activity flag is modeled. The
/// session record also carries user and device details and access tokens, and
/// none of them are kept here, so they cannot leak through `Debug` or logs.
#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct SessionEntry {
    is_active: Option<bool>,
}

impl JellyfinClient {
    /// Read server information (`GET /System/Info`).
    pub async fn server_info(&self, session: &Session) -> Result<ServerInfo, ApiError> {
        self.get_admin_json(session, &["System", "Info"]).await
    }

    /// List libraries with their storage locations and collection types
    /// (`GET /Library/VirtualFolders`). A 403 means the session is not
    /// elevated; see [`ApiError::Forbidden`].
    pub async fn virtual_folders(&self, session: &Session) -> Result<Vec<VirtualFolder>, ApiError> {
        self.get_admin_json(session, &["Library", "VirtualFolders"])
            .await
    }

    /// Count the sessions the server marks active (`GET /Sessions`, `IsActive`).
    /// Only the count leaves this module, so no session detail is exposed.
    /// [UNCONFIRMED] Route and field names; see the module documentation.
    pub async fn active_session_count(&self, session: &Session) -> Result<usize, ApiError> {
        let entries: Vec<SessionEntry> = self.get_admin_json(session, &["Sessions"]).await?;
        Ok(entries
            .iter()
            .filter(|entry| entry.is_active == Some(true))
            .count())
    }

    /// The only request path in this module. It always uses `GET`.
    async fn get_admin_json<T: for<'de> Deserialize<'de>>(
        &self,
        session: &Session,
        segments: &[&str],
    ) -> Result<T, ApiError> {
        let request = self.http.request(Method::GET, self.endpoint(segments));
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        decode_response(response).await
    }
}
