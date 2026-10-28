//! Live TV recordings and recording timers.
//!
//! Recordings are ordinary items (`GET /LiveTv/Recordings`) and play like any other item.
//! A timer schedules one program: the client asks the server for its default timer for that
//! program (`GET /LiveTv/Timers/Defaults`) and posts it back unchanged (`POST /LiveTv/Timers`),
//! so padding and keep-up-to rules stay the server's.

use reqwest::{Method, StatusCode};
use serde::Deserialize;

use crate::{ApiError, BaseItemDto, ItemsResponse, JellyfinClient, Session, home};

/// Most recordings or timers one request returns.
pub const MAX_RECORDINGS: u32 = 100;

/// A scheduled recording.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct LiveTvTimer {
    pub id: String,
    pub name: Option<String>,
    pub channel_name: Option<String>,
    pub program_id: Option<String>,
    /// UTC timestamp text from the server.
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// `New`, `InProgress`, `Completed`, `Cancelled`, ...
    pub status: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct TimerPage {
    items: Vec<LiveTvTimer>,
}

impl JellyfinClient {
    /// Finished and in-progress recordings, newest first.
    pub async fn list_live_tv_recordings(
        &self,
        session: &Session,
    ) -> Result<Vec<BaseItemDto>, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["LiveTv", "Recordings"]))
            .query(&[
                ("userId", session.user.id.clone()),
                ("limit", MAX_RECORDINGS.to_string()),
                ("enableUserData", "true".to_owned()),
                ("fields", "Overview".to_owned()),
            ]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let page: ItemsResponse = crate::decode_response(response).await?;
        Ok(page.items)
    }

    /// Scheduled recordings that have not finished.
    pub async fn list_live_tv_timers(
        &self,
        session: &Session,
    ) -> Result<Vec<LiveTvTimer>, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["LiveTv", "Timers"]))
            .query(&[("isActive", "false"), ("isScheduled", "true")]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let mut page: TimerPage = crate::decode_response(response).await?;
        page.items.truncate(MAX_RECORDINGS as usize);
        Ok(page.items)
    }

    /// Schedule a recording of `program_id` with the server's default timer settings.
    pub async fn record_live_tv_program(
        &self,
        session: &Session,
        program_id: &str,
    ) -> Result<(), ApiError> {
        home::validate_id(program_id)?;
        let request = self
            .http
            .request(
                Method::GET,
                self.endpoint(&["LiveTv", "Timers", "Defaults"]),
            )
            .query(&[("programId", program_id)]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let defaults: serde_json::Value = crate::decode_response(response).await?;
        if !defaults.is_object() {
            return Err(ApiError::InvalidResponse);
        }
        let request = self
            .http
            .request(Method::POST, self.endpoint(&["LiveTv", "Timers"]))
            .json(&defaults);
        self.send_no_content(session, request).await
    }

    /// Cancel a scheduled recording.
    pub async fn cancel_live_tv_timer(
        &self,
        session: &Session,
        timer_id: &str,
    ) -> Result<(), ApiError> {
        home::validate_id(timer_id)?;
        let request = self.http.request(
            Method::DELETE,
            self.endpoint(&["LiveTv", "Timers", timer_id]),
        );
        self.send_no_content(session, request).await
    }

    async fn send_no_content(
        &self,
        session: &Session,
        request: reqwest::RequestBuilder,
    ) -> Result<(), ApiError> {
        let request = self.add_metadata(request, Some(&session.access_token));
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

    use crate::{ClientConfig, JellyfinClient, Session, UserDto};

    /// Answers each request in turn with `(status, body)` and returns what it received.
    async fn serve(
        replies: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let task = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let read = stream.read(&mut buffer).await.expect("read");
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    let Some(header_end) = text.find("\r\n\r\n") else {
                        if read == 0 {
                            break;
                        }
                        continue;
                    };
                    let length = text[..header_end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + length || read == 0 {
                        break;
                    }
                }
                seen.push(String::from_utf8_lossy(&request).into_owned());
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
            seen
        });
        (format!("http://{address}"), task)
    }

    fn client(base: &str) -> JellyfinClient {
        JellyfinClient::new(ClientConfig::new(base, "Kattri", "device", "id", "1").expect("config"))
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
    async fn recording_a_program_posts_the_servers_defaults_back() {
        let defaults = r#"{"ProgramId":"p1","ChannelId":"c1","PrePaddingSeconds":60}"#;
        let (base, task) = serve(vec![(200, defaults), (204, "")]).await;
        client(&base)
            .record_live_tv_program(&session(), "p1")
            .await
            .expect("scheduled");
        let requests = task.await.expect("server");
        assert!(requests[0].starts_with("GET /LiveTv/Timers/Defaults?programId=p1 "));
        assert!(requests[1].starts_with("POST /LiveTv/Timers "));
        assert!(requests[1].contains(r#""PrePaddingSeconds":60"#));
    }

    #[tokio::test]
    async fn timers_and_recordings_are_listed_and_a_timer_cancels() {
        let timers = r#"{"Items":[{"Id":"t1","Name":"News","ChannelName":"One","ProgramId":"p1","StartDate":"2026-10-28T18:00:00Z","Status":"New"}]}"#;
        let recordings = r#"{"Items":[{"Id":"r1","Name":"Match","Type":"Recording"}]}"#;
        let (base, task) = serve(vec![(200, timers), (200, recordings), (204, "")]).await;
        let client = client(&base);
        let listed = client
            .list_live_tv_timers(&session())
            .await
            .expect("timers");
        assert_eq!(listed[0].id, "t1");
        assert_eq!(listed[0].channel_name.as_deref(), Some("One"));
        let recorded = client
            .list_live_tv_recordings(&session())
            .await
            .expect("recordings");
        assert_eq!(recorded[0].id, "r1");
        client
            .cancel_live_tv_timer(&session(), "t1")
            .await
            .expect("cancelled");
        let requests = task.await.expect("server");
        assert!(requests[0].starts_with("GET /LiveTv/Timers?"));
        assert!(requests[1].starts_with("GET /LiveTv/Recordings?"));
        assert!(requests[2].starts_with("DELETE /LiveTv/Timers/t1 "));
    }
}
