//! Request/response contracts for library endpoints against a disconnected HTTP fixture.
use jellyfin_api::{ClientConfig, ItemQuery, ItemsEndpoint, JellyfinClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn client(base: &str) -> JellyfinClient {
    JellyfinClient::new(ClientConfig::new(base, "test", "test", "test", "test").expect("config"))
        .expect("client")
}

async fn fixture(body: String) -> (JellyfinClient, tokio::task::JoinHandle<url::Url>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 16 * 1024, "bounded request headers");
            bytes.push(socket.read_u8().await.expect("header byte"));
        }
        let request = std::str::from_utf8(&bytes).expect("HTTP header");
        assert!(request.to_ascii_lowercase().contains("authorization:"));
        let path = request
            .lines()
            .next()
            .expect("request line")
            .split_whitespace()
            .nth(1)
            .expect("path");
        let url = url::Url::parse(&format!("http://fixture{path}")).expect("request URL");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("response");
        url
    });
    (client(&base), task)
}

#[tokio::test]
async fn library_endpoint_queries_keep_user_paging_and_guid_filters() {
    let server = mock_jellyfin::spawn(Default::default())
        .await
        .expect("mock");
    let session = client(&server.base_url())
        .authenticate("demo", "demo")
        .await
        .expect("login");
    for (endpoint, path) in [
        (ItemsEndpoint::Items, "/Items"),
        (ItemsEndpoint::Genres, "/Genres"),
        (ItemsEndpoint::Studios, "/Studios"),
        (ItemsEndpoint::Artists, "/Artists"),
        (ItemsEndpoint::AlbumArtists, "/Artists/AlbumArtists"),
        (ItemsEndpoint::UpcomingEpisodes, "/Shows/Upcoming"),
    ] {
        let (api, request) =
            fixture(r#"{"Items":[{"Id":"result","Name":"Fixture"}],"TotalRecordCount":1}"#.into())
                .await;
        let result = api
            .list_items_at(
                &session,
                endpoint,
                &ItemQuery {
                    start_index: 20,
                    limit: 10,
                    genre_ids: Some(vec!["genre-1".into(), "genre-2".into()]),
                    studio_ids: Some(vec!["studio-1".into(), "studio-2".into()]),
                    artist_ids: Some(vec!["artist-1".into(), "artist-2".into()]),
                    ..Default::default()
                },
            )
            .await
            .expect("paged result");
        assert_eq!(result.items[0].id, "result");
        assert_eq!(result.total_record_count, Some(1));
        let url = request.await.expect("fixture");
        assert_eq!(url.path(), path);
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            query.get("userId").map(|v| v.as_ref()),
            Some(session.user.id.as_str())
        );
        for (key, value) in [
            ("startIndex", "20"),
            ("limit", "10"),
            ("genreIds", "genre-1,genre-2"),
            ("studioIds", "studio-1,studio-2"),
            ("artistIds", "artist-1,artist-2"),
        ] {
            assert_eq!(query.get(key).map(|v| v.as_ref()), Some(value));
        }
    }
}

#[tokio::test]
async fn recommendation_requests_bound_counts_and_preserve_unknown_categories() {
    let server = mock_jellyfin::spawn(Default::default())
        .await
        .expect("mock");
    let session = client(&server.base_url())
        .authenticate("demo", "demo")
        .await
        .expect("login");
    let (api, request) =
        fixture(r#"[{"Items":[],"RecommendationType":"FutureCategory"}]"#.into()).await;
    let result = api
        .movie_recommendations(&session, Some("library"), u32::MAX, u32::MAX)
        .await
        .expect("recommendations");
    assert_eq!(
        result[0].recommendation_type,
        jellyfin_api::RecommendationKind::Other
    );
    let url = request.await.expect("fixture");
    assert_eq!(url.path(), "/Movies/Recommendations");
    let query = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(query.get("categoryLimit").map(|v| v.as_ref()), Some("10"));
    assert_eq!(query.get("itemLimit").map(|v| v.as_ref()), Some("30"));
    assert_eq!(query.get("parentId").map(|v| v.as_ref()), Some("library"));
}
