//! An item's extras: local trailers (`GET /Items/{id}/LocalTrailers`) and special features
//! (`GET /Items/{id}/SpecialFeatures`), which answer a bare JSON array of items that play like
//! any other item, and a track's lyrics (`GET /Audio/{id}/Lyrics`).

use reqwest::{Method, StatusCode};
use serde::Deserialize;

use crate::{ApiError, BaseItemDto, JellyfinClient, Session, home};

/// Most extras of one kind kept for a details page.
pub const MAX_EXTRAS: usize = 50;

/// Most lyric lines kept for one track.
pub const MAX_LYRIC_LINES: usize = 2_000;

/// One line of a track's lyrics. `start_ticks` is set for synced lyrics.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct LyricLine {
    pub text: String,
    #[serde(rename = "Start")]
    pub start_ticks: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct LyricsResponse {
    lyrics: Vec<LyricLine>,
}

impl JellyfinClient {
    /// The lyrics of an audio track; empty when the track has none (the server answers 404).
    pub async fn track_lyrics(
        &self,
        session: &Session,
        item_id: &str,
    ) -> Result<Vec<LyricLine>, ApiError> {
        home::validate_id(item_id)?;
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Audio", item_id, "Lyrics"]));
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let mut lyrics: LyricsResponse = crate::decode_response(response).await?;
        lyrics.lyrics.truncate(MAX_LYRIC_LINES);
        Ok(lyrics.lyrics)
    }

    /// Local trailers, then special features, of `item_id`.
    pub async fn item_extras(
        &self,
        session: &Session,
        item_id: &str,
    ) -> Result<Vec<BaseItemDto>, ApiError> {
        home::validate_id(item_id)?;
        let mut extras = self.extras_of(session, item_id, "LocalTrailers").await?;
        extras.extend(self.extras_of(session, item_id, "SpecialFeatures").await?);
        Ok(extras)
    }

    async fn extras_of(
        &self,
        session: &Session,
        item_id: &str,
        kind: &str,
    ) -> Result<Vec<BaseItemDto>, ApiError> {
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Items", item_id, kind]))
            .query(&[("userId", session.user.id.as_str())]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let mut items: Vec<BaseItemDto> = crate::decode_response(response).await?;
        items.truncate(MAX_EXTRAS);
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use crate::{ClientConfig, JellyfinClient, Session, UserDto};

    #[tokio::test]
    async fn trailers_come_before_special_features() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for body in [
                r#"[{"Id":"t1","Name":"Trailer"}]"#,
                r#"[{"Id":"f1","Name":"Making of"}]"#,
            ] {
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
                seen.push(String::from_utf8_lossy(&request).into_owned());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
            seen
        });
        let client = JellyfinClient::new(
            ClientConfig::new(&format!("http://{address}"), "Kattri", "device", "id", "1")
                .expect("config"),
        )
        .expect("client");
        let session = Session {
            access_token: "token".into(),
            user: UserDto {
                policy: None,
                id: "user".into(),
                name: "User".into(),
                server_id: None,
            },
            server_id: None,
            session_info: None,
        };
        let extras = client.item_extras(&session, "movie").await.expect("extras");
        let ids: Vec<&str> = extras.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["t1", "f1"]);
        let requests = server.await.expect("server");
        assert!(requests[0].starts_with("GET /Items/movie/LocalTrailers?userId=user "));
        assert!(requests[1].starts_with("GET /Items/movie/SpecialFeatures?userId=user "));
    }

    #[test]
    fn lyric_lines_read_text_and_start() {
        let response: super::LyricsResponse = serde_json::from_str(
            r#"{"Metadata":{},"Lyrics":[{"Text":"Hello","Start":10000000},{"Text":"World"}]}"#,
        )
        .expect("lyrics");
        assert_eq!(response.lyrics[0].text, "Hello");
        assert_eq!(response.lyrics[0].start_ticks, Some(10_000_000));
        assert_eq!(response.lyrics[1].start_ticks, None);
    }
}
