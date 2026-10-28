//! Administrator actions on one library item: refresh its metadata, or delete it.
//!
//! Both need an administrator account (or, for delete, a user allowed to delete content);
//! the server answers 403 otherwise, returned as [`ApiError::Forbidden`].

use reqwest::{Method, StatusCode};

use crate::{ApiError, JellyfinClient, Session, home};

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

    async fn send_admin(
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
}
