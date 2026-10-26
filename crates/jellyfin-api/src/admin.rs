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
//! `VirtualFolderInfo` has no per-library item count. The count comes from
//! `GET /Items` (operation `GetItems`, schema `BaseItemQueryResult`) with
//! `parentId`, `recursive=true` and `limit=1`; only `TotalRecordCount` is read
//! and no item is decoded. That operation needs no elevated rights, so a
//! non-admin session can read counts but not the library list.
//!
//! - `GET /Users` (operation `GetUsers`, schema `UserDto[]`) answers with
//!   `FirstTimeSetupOrElevated`. Only a total and a disabled count leave this
//!   module: names, ids and policies are dropped while decoding.
//! - `GET /ScheduledTasks` (operation `GetTasks`, schema `TaskInfo[]`) answers
//!   with `FirstTimeSetupOrElevated`. Only the name, the state and the last
//!   execution status are kept, capped by [`MAX_SCHEDULED_TASKS`].
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

/// Upper bound on the scheduled tasks kept from one read. A server with more
/// tasks is truncated rather than letting the dashboard grow without limit.
pub const MAX_SCHEDULED_TASKS: usize = 100;

/// Upper bound on one library GUID accepted by [`JellyfinClient::library_item_count`].
const MAX_LIBRARY_ID_LEN: usize = 64;

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

/// User totals from `GET /Users`. Names, ids and per-user policies stay in the
/// response body; only these counts are returned.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct UserCounts {
    pub total: usize,
    pub enabled: usize,
    pub disabled: usize,
}

/// One entry from `GET /Users`. The policy flag is the only field kept.
#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct UserEntry {
    policy: Option<UserPolicyFlag>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct UserPolicyFlag {
    is_disabled: Option<bool>,
}

/// One scheduled task from `GET /ScheduledTasks`, reduced to what the
/// read-only dashboard shows.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct ScheduledTask {
    pub name: Option<String>,
    pub state: Option<String>,
    pub last_status: Option<String>,
}

/// One entry from `GET /ScheduledTasks`.
#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ScheduledTaskEntry {
    name: Option<String>,
    state: Option<String>,
    last_execution_result: Option<TaskExecutionResult>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct TaskExecutionResult {
    status: Option<String>,
}

/// The `TotalRecordCount` of a `GET /Items` page. The items themselves are not
/// decoded, so a malformed item cannot break a count.
#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ItemCountResponse {
    total_record_count: Option<u32>,
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

    /// Count the server's users and how many are disabled (`GET /Users`).
    /// Names and ids are dropped while decoding; see [`UserCounts`].
    pub async fn user_counts(&self, session: &Session) -> Result<UserCounts, ApiError> {
        let entries: Vec<UserEntry> = self.get_admin_json(session, &["Users"]).await?;
        let disabled = entries
            .iter()
            .filter(|entry| {
                entry
                    .policy
                    .as_ref()
                    .is_some_and(|policy| policy.is_disabled == Some(true))
            })
            .count();
        Ok(UserCounts {
            total: entries.len(),
            enabled: entries.len().saturating_sub(disabled),
            disabled,
        })
    }

    /// Read the scheduled tasks and their last execution status
    /// (`GET /ScheduledTasks`), capped at [`MAX_SCHEDULED_TASKS`].
    pub async fn scheduled_tasks(&self, session: &Session) -> Result<Vec<ScheduledTask>, ApiError> {
        let entries: Vec<ScheduledTaskEntry> =
            self.get_admin_json(session, &["ScheduledTasks"]).await?;
        Ok(entries
            .into_iter()
            .take(MAX_SCHEDULED_TASKS)
            .map(|entry| ScheduledTask {
                name: entry.name,
                state: entry.state,
                last_status: entry.last_execution_result.and_then(|result| result.status),
            })
            .collect())
    }

    /// Count the items under one library (`GET /Items` with `parentId`,
    /// `recursive=true` and `limit=1`). `Ok(None)` means the server answered
    /// without `TotalRecordCount`. A 403 means the session is not elevated.
    pub async fn library_item_count(
        &self,
        session: &Session,
        library_id: &str,
    ) -> Result<Option<u32>, ApiError> {
        if !is_library_id(library_id) {
            return Err(ApiError::InvalidConfiguration);
        }
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Items"]))
            .query(&[
                ("parentId", library_id),
                ("recursive", "true"),
                ("limit", "1"),
                ("enableImages", "false"),
                ("enableUserData", "false"),
            ]);
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
        let response = request.send().await.map_err(crate::transport_error)?;
        let page: ItemCountResponse = decode_response(response).await?;
        Ok(page.total_record_count)
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

/// A library GUID as the server prints it. Rejecting anything else keeps the
/// query parameter to a bounded, identifier-shaped value.
fn is_library_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LIBRARY_ID_LEN
        && value
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-')
}
