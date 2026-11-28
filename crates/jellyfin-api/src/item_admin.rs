//! Administrator actions on one library item: refresh its metadata, or delete it.
//!
//! Both need an administrator account (or, for delete, a user allowed to delete content);
//! the server answers 403 otherwise, returned as [`ApiError::Forbidden`].

use reqwest::{Method, StatusCode};
use serde::Serialize;

use crate::{ApiError, JellyfinClient, Session, home};

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct SessionMessageBody<'a> {
    header: &'a str,
    text: &'a str,
    timeout_ms: u32,
}

impl JellyfinClient {
    /// Queue a metadata refresh for `item_id` (`POST /Items/{itemId}/Refresh`) that fills
    /// missing metadata and images and keeps what is already there.
    pub async fn refresh_item_metadata(
        &self,
        session: &Session,
        item_id: &str,
    ) -> Result<(), ApiError> {
        home::validate_id(item_id)?;
        let request = self
            .http
            .request(Method::POST, self.endpoint(&["Items", item_id, "Refresh"]))
            .query(&[
                ("metadataRefreshMode", "Default"),
                ("imageRefreshMode", "Default"),
                ("replaceAllMetadata", "false"),
                ("replaceAllImages", "false"),
            ]);
        self.send_admin(session, request).await
    }

    /// Delete `item_id` from the library and from disk (`DELETE /Items/{itemId}`).
    pub async fn delete_item(&self, session: &Session, item_id: &str) -> Result<(), ApiError> {
        home::validate_id(item_id)?;
        let request = self
            .http
            .request(Method::DELETE, self.endpoint(&["Items", item_id]));
        self.send_admin(session, request).await
    }

    /// Add `item_id` to the box set `collection_id` (`POST /Collections/{id}/Items`).
    pub async fn add_to_collection(
        &self,
        session: &Session,
        collection_id: &str,
        item_id: &str,
    ) -> Result<(), ApiError> {
        home::validate_id(collection_id)?;
        home::validate_id(item_id)?;
        let request = self
            .http
            .request(
                Method::POST,
                self.endpoint(&["Collections", collection_id, "Items"]),
            )
            .query(&[("ids", item_id)]);
        self.send_admin(session, request).await
    }

    /// Trigger a scan of all libraries (`POST /Library/Refresh`).
    pub async fn refresh_all_libraries(&self, session: &Session) -> Result<(), ApiError> {
        let request = self
            .http
            .request(Method::POST, self.endpoint(&["Library", "Refresh"]));
        self.send_admin(session, request).await
    }

    /// Start a scheduled task (`POST /ScheduledTasks/Running/{taskId}`).
    pub async fn start_scheduled_task(
        &self,
        session: &Session,
        task_id: &str,
    ) -> Result<(), ApiError> {
        home::validate_id(task_id)?;
        let request = self.http.request(
            Method::POST,
            self.endpoint(&["ScheduledTasks", "Running", task_id]),
        );
        self.send_admin(session, request).await
    }

    /// Request a server restart (`POST /System/Restart`).
    pub async fn restart_server(&self, session: &Session) -> Result<(), ApiError> {
        let request = self
            .http
            .request(Method::POST, self.endpoint(&["System", "Restart"]));
        self.send_admin(session, request).await
    }

    /// Request a server shutdown (`POST /System/Shutdown`).
    pub async fn shutdown_server(&self, session: &Session) -> Result<(), ApiError> {
        let request = self
            .http
            .request(Method::POST, self.endpoint(&["System", "Shutdown"]));
        self.send_admin(session, request).await
    }

    /// Send a toast message to a remote session (`POST /Sessions/{sessionId}/Message`).
    pub async fn send_session_message(
        &self,
        session: &Session,
        session_id: &str,
        header: &str,
        text: &str,
    ) -> Result<(), ApiError> {
        home::validate_id(session_id)?;
        let body = SessionMessageBody {
            header,
            text,
            timeout_ms: 5000,
        };
        let request = self
            .http
            .request(
                Method::POST,
                self.endpoint(&["Sessions", session_id, "Message"]),
            )
            .json(&body);
        self.send_admin(session, request).await
    }

    /// Revoke and delete a registered device (`DELETE /Devices?id=...`).
    pub async fn revoke_device(&self, session: &Session, device_id: &str) -> Result<(), ApiError> {
        if device_id.trim().is_empty()
            || device_id.len() > 256
            || device_id.chars().any(char::is_control)
        {
            return Err(ApiError::InvalidConfiguration);
        }
        let request = self
            .http
            .request(Method::DELETE, self.endpoint(&["Devices"]))
            .query(&[("id", device_id)]);
        self.send_admin(session, request).await
    }

    pub(crate) async fn send_admin(
        &self,
        session: &Session,
        request: reqwest::RequestBuilder,
    ) -> Result<(), ApiError> {
        let request = self.add_metadata(request, Some(session.access_token.as_str()));
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
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use crate::{ApiError, ClientConfig, JellyfinClient, Session, UserDto};

    async fn serve(status: u16) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0; 2048];
            loop {
                let read = stream.read(&mut buffer).await.expect("read");
                request.extend_from_slice(&buffer[..read]);
                if read == 0 || request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            let response =
                format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            stream.write_all(response.as_bytes()).await.expect("write");
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{address}"), task)
    }

    fn client(base: &str) -> JellyfinClient {
        JellyfinClient::new(
            ClientConfig::new(base, "Kattri test", "device", "device-id", "1").expect("config"),
        )
        .expect("client")
    }

    fn session() -> Session {
        Session {
            access_token: "token".into(),
            user: UserDto {
                policy: None,
                id: "user".into(),
                name: "User".into(),
                server_id: None,
            },
            server_id: None,
            session_info: None,
        }
    }

    #[tokio::test]
    async fn refresh_posts_a_fill_missing_refresh() {
        let (base, task) = serve(204).await;
        client(&base)
            .refresh_item_metadata(&session(), "item-1")
            .await
            .expect("refresh queued");
        let request = task.await.expect("server");
        let line = request.lines().next().expect("request line");
        assert!(line.starts_with("POST /Items/item-1/Refresh?"), "{line}");
        assert!(line.contains("metadataRefreshMode=Default"));
        assert!(line.contains("replaceAllMetadata=false"));
    }

    #[tokio::test]
    async fn delete_sends_delete_and_reports_a_refusal() {
        let (base, task) = serve(204).await;
        client(&base)
            .delete_item(&session(), "item-1")
            .await
            .expect("deleted");
        let request = task.await.expect("server");
        assert!(request.starts_with("DELETE /Items/item-1 "), "{request}");

        let (base, _task) = serve(403).await;
        assert!(matches!(
            client(&base).delete_item(&session(), "item-1").await,
            Err(ApiError::Forbidden)
        ));
    }

    #[tokio::test]
    async fn add_to_collection_posts_the_item_id() {
        let (base, task) = serve(204).await;
        client(&base)
            .add_to_collection(&session(), "box-1", "item-1")
            .await
            .expect("added");
        let request = task.await.expect("server");
        assert!(
            request.starts_with("POST /Collections/box-1/Items?ids=item-1 "),
            "{request}"
        );
    }

    #[tokio::test]
    async fn refresh_all_libraries_posts_library_refresh() {
        let (base, task) = serve(204).await;
        client(&base)
            .refresh_all_libraries(&session())
            .await
            .expect("refreshed");
        let request = task.await.expect("server");
        assert!(request.starts_with("POST /Library/Refresh "), "{request}");
    }

    #[tokio::test]
    async fn start_scheduled_task_posts_task_id() {
        let (base, task) = serve(204).await;
        client(&base)
            .start_scheduled_task(&session(), "task-1")
            .await
            .expect("started");
        let request = task.await.expect("server");
        assert!(
            request.starts_with("POST /ScheduledTasks/Running/task-1 "),
            "{request}"
        );
    }

    #[test]
    fn administrator_flag_comes_from_the_policy() {
        let admin: UserDto = serde_json::from_value(serde_json::json!({
            "Id": "u", "Name": "Admin", "Policy": {"IsAdministrator": true}
        }))
        .expect("admin user");
        assert!(admin.is_administrator());
        let plain: UserDto =
            serde_json::from_value(serde_json::json!({"Id": "u", "Name": "User"})).expect("user");
        assert!(!plain.is_administrator());
    }

    #[tokio::test]
    async fn restart_and_shutdown_post_to_system_endpoints() {
        let (base, task) = serve(204).await;
        client(&base)
            .restart_server(&session())
            .await
            .expect("restart");
        let request = task.await.expect("server");
        assert!(request.starts_with("POST /System/Restart "), "{request}");

        let (base, task) = serve(204).await;
        client(&base)
            .shutdown_server(&session())
            .await
            .expect("shutdown");
        let request = task.await.expect("server");
        assert!(request.starts_with("POST /System/Shutdown "), "{request}");
    }

    #[tokio::test]
    async fn send_session_message_posts_json_body() {
        let (base, task) = serve(204).await;
        client(&base)
            .send_session_message(&session(), "sess-1", "Notice", "Server restarting")
            .await
            .expect("message sent");
        let request = task.await.expect("server");
        assert!(
            request.starts_with("POST /Sessions/sess-1/Message "),
            "{request}"
        );
    }

    #[tokio::test]
    async fn revoke_device_sends_delete_and_rejects_empty_id() {
        assert!(matches!(
            client("http://127.0.0.1:8096")
                .revoke_device(&session(), "  ")
                .await,
            Err(ApiError::InvalidConfiguration)
        ));

        let (base, task) = serve(204).await;
        client(&base)
            .revoke_device(&session(), "dev-1")
            .await
            .expect("revoked");
        let request = task.await.expect("server");
        assert!(
            request.starts_with("DELETE /Devices?id=dev-1 "),
            "{request}"
        );
    }
}
