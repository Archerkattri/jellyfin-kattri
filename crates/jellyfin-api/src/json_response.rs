//! Bound successful API JSON before deserialization, including chunked bodies.

use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use crate::{ApiError, transport_error};

// This is a client resource budget, not a Jellyfin protocol limit. Browsing
// already pages requests; unusually large metadata fails with a typed error.
const MAX_JSON_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub(super) async fn decode<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ApiError> {
    decode_with_limit(response, MAX_JSON_RESPONSE_BYTES).await
}

async fn decode_with_limit<T: DeserializeOwned>(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<T, ApiError> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(ApiError::JsonResponseTooLarge);
    }

    // Authentication responses can contain tokens. Do not leave our additional
    // raw-response buffer behind after parsing or an early error.
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(ApiError::JsonResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| ApiError::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{MAX_JSON_RESPONSE_BYTES, decode_with_limit};
    use crate::{ApiError, ClientConfig, ErrorClass, JellyfinClient};

    async fn response(bytes: Vec<u8>) -> reqwest::Response {
        let (url, task) = serve_once(bytes).await;
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test client")
            .get(url)
            .send()
            .await
            .expect("local response headers");
        task.await.expect("fixture finishes");
        response
    }

    async fn serve_once(bytes: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local fixture");
        let address = listener.local_addr().expect("fixture address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("fixture request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.expect("request headers");
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() <= 16 * 1024);
            }
            socket.write_all(&bytes).await.expect("fixture response");
        });
        (format!("http://{address}"), task)
    }

    fn fixed(body: &[u8]) -> Vec<u8> {
        let mut bytes = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ).into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    #[tokio::test]
    async fn accepts_a_valid_response_at_the_exact_limit() {
        let body = br#"{"Name":"fixture"}"#;
        let parsed: HashMap<String, String> =
            decode_with_limit(response(fixed(body)).await, body.len())
                .await
                .expect("exact limit is allowed");
        assert_eq!(parsed.get("Name").map(String::as_str), Some("fixture"));
    }

    #[tokio::test]
    async fn rejects_large_declared_length_before_reading_a_body() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Length: 65\r\nConnection: close\r\n\r\n";
        let result =
            decode_with_limit::<serde_json::Value>(response(headers.to_vec()).await, 64).await;
        assert!(matches!(result, Err(ApiError::JsonResponseTooLarge)));
    }

    #[tokio::test]
    async fn rejects_chunked_overflow_without_a_content_length() {
        let bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n[123\r\n4\r\n456]\r\n0\r\n\r\n";
        let result =
            decode_with_limit::<serde_json::Value>(response(bytes.to_vec()).await, 7).await;
        assert!(matches!(result, Err(ApiError::JsonResponseTooLarge)));
    }

    #[tokio::test]
    async fn accepts_chunked_json_at_the_exact_limit() {
        let bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n[123\r\n4\r\n456]\r\n0\r\n\r\n";
        let parsed: Vec<u32> = decode_with_limit(response(bytes.to_vec()).await, 8)
            .await
            .expect("bounded chunks");
        assert_eq!(parsed, vec![123456]);
    }

    #[tokio::test]
    async fn rejects_close_delimited_overflow() {
        let bytes = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n[123456]";
        let result =
            decode_with_limit::<serde_json::Value>(response(bytes.to_vec()).await, 7).await;
        assert!(matches!(result, Err(ApiError::JsonResponseTooLarge)));
    }

    #[tokio::test]
    async fn invalid_json_does_not_expose_response_text() {
        let sentinel = "not-json-secret-sentinel";
        let result =
            decode_with_limit::<serde_json::Value>(response(fixed(sentinel.as_bytes())).await, 64)
                .await;
        let error = result.expect_err("malformed response");
        assert!(matches!(error, ApiError::InvalidResponse));
        assert!(!format!("{error:?} {error}").contains(sentinel));
    }

    #[tokio::test]
    async fn public_client_path_enforces_the_production_limit() {
        let bytes = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_JSON_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let (url, server) = serve_once(bytes).await;
        let client = JellyfinClient::new(
            ClientConfig::new(&url, "test", "test", "test", "test").expect("local config"),
        )
        .expect("client");
        let error = client.public_system_info().await.expect_err("body limit");
        assert!(matches!(error, ApiError::JsonResponseTooLarge));
        assert_eq!(error.classify(), ErrorClass::Unexpected);
        assert!(!error.classify().is_retryable());
        assert!(!error.classify().counts_toward_offline());
        server.await.expect("fixture finishes");
    }

    #[tokio::test]
    async fn status_classification_precedes_body_parsing() {
        let bytes = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_JSON_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let (url, server) = serve_once(bytes).await;
        let client = JellyfinClient::new(
            ClientConfig::new(&url, "test", "test", "test", "test").expect("local config"),
        )
        .expect("client");
        assert!(matches!(
            client.public_system_info().await,
            Err(ApiError::ReauthenticationRequired)
        ));
        server.await.expect("fixture finishes");
    }
}
