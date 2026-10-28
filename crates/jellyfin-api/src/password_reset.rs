//! The server's forgot-password flow (`POST /Users/ForgotPassword`). The server decides what
//! happens: it writes a PIN file an administrator reads, refuses outside the local network,
//! or says to contact the administrator. No credentials are sent.

use reqwest::Method;
use serde::Deserialize;

use crate::{ApiError, JellyfinClient};

/// What the server asks the user to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgotPasswordAction {
    /// Ask the server administrator.
    ContactAdmin,
    /// A PIN was written to a file on the server; the administrator gives it to the user.
    PinCode,
    /// Password reset works only from the server's local network.
    InNetworkRequired,
    /// An action this client does not know.
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgotPasswordResult {
    pub action: ForgotPasswordAction,
    /// Server path of the PIN file, for the administrator.
    pub pin_file: Option<String>,
    /// When the PIN stops working, as the server's timestamp text.
    pub pin_expiration_date: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ForgotPasswordResponse {
    action: String,
    #[serde(default)]
    pin_file: Option<String>,
    #[serde(default)]
    pin_expiration_date: Option<String>,
}

impl JellyfinClient {
    /// Start a password reset for `entered_username`.
    pub async fn forgot_password(
        &self,
        entered_username: &str,
    ) -> Result<ForgotPasswordResult, ApiError> {
        let username = entered_username.trim();
        if username.is_empty() || username.len() > 256 || username.chars().any(char::is_control) {
            return Err(ApiError::InvalidConfiguration);
        }
        let url = self.endpoint(&["Users", "ForgotPassword"]);
        let body = serde_json::json!({ "EnteredUsername": username });
        let response: ForgotPasswordResponse =
            self.send_json(Method::POST, url, None, Some(body)).await?;
        let action = match response.action.as_str() {
            "ContactAdmin" => ForgotPasswordAction::ContactAdmin,
            "PinCode" => ForgotPasswordAction::PinCode,
            "InNetworkRequired" => ForgotPasswordAction::InNetworkRequired,
            other => ForgotPasswordAction::Other(other.to_owned()),
        };
        Ok(ForgotPasswordResult {
            action,
            pin_file: response.pin_file,
            pin_expiration_date: response.pin_expiration_date,
        })
    }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::ForgotPasswordAction;
    use crate::{ApiError, ClientConfig, JellyfinClient};

    #[tokio::test]
    async fn forgot_password_posts_the_username_and_reads_the_action() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !String::from_utf8_lossy(&request).contains("EnteredUsername") {
                let read = stream.read(&mut buffer).await.expect("read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let body = r#"{"Action":"PinCode","PinFile":"/config/passwordreset1.json","PinExpirationDate":"2026-10-28T12:00:00Z"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.expect("write");
            String::from_utf8_lossy(&request).into_owned()
        });
        let client = JellyfinClient::new(
            ClientConfig::new(&format!("http://{address}"), "Kattri", "device", "id", "1")
                .expect("config"),
        )
        .expect("client");
        let result = client
            .forgot_password(" sam ")
            .await
            .expect("reset started");
        assert_eq!(result.action, ForgotPasswordAction::PinCode);
        assert_eq!(
            result.pin_file.as_deref(),
            Some("/config/passwordreset1.json")
        );
        let request = server.await.expect("server");
        assert!(request.starts_with("POST /Users/ForgotPassword "));
        assert!(request.contains(r#""EnteredUsername":"sam""#));
        assert!(matches!(
            client.forgot_password("  ").await,
            Err(ApiError::InvalidConfiguration)
        ));
    }
}
