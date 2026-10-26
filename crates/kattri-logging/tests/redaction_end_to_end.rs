//! End to end: the real subscriber writes a real file, and nothing secret is in it.
//!
//! One test function, because the global subscriber can be installed once per process. The
//! session comes from the in-process mock over loopback; no Jellyfin server is involved. This
//! proves the file contents for these inputs, not that every possible log call is safe.

use std::fs;
use std::panic;

use jellyfin_api::{ClientConfig, JellyfinClient};
use kattri_logging::{LOG_FILE_NAME, LogConfig, LogLevel, install_panic_hook};
use mock_jellyfin::{MockConfig, spawn};

/// The mock's fixed access token (crate-private in mock-jellyfin).
const MOCK_TOKEN: &str = "mock-access-token-0001";

#[tokio::test]
async fn logging_a_session_a_url_a_header_and_a_panic_leaves_no_secret() {
    let dir = std::env::temp_dir().join(format!("kattri-logging-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);

    let guard = kattri_logging::init(&LogConfig::new(dir.clone()).with_level(LogLevel::Debug))
        .expect("init logging");
    assert!(
        kattri_logging::init(&LogConfig::new(dir.clone())).is_err(),
        "a second init must be refused"
    );
    install_panic_hook();

    let server = spawn(MockConfig::default()).await.expect("spawn mock");
    let config = ClientConfig::new(
        &server.base_url(),
        "Kattri test",
        "device",
        "dev-1",
        "0.0.0",
    )
    .expect("config");
    let client = JellyfinClient::new(config).expect("client");
    let session = client.authenticate("demo", "demo").await.expect("sign in");
    let authorization = client.session_authorization(&session);

    tracing::info!("signed in: {session:?}");
    tracing::debug!(
        url = %format!("{}/Videos/1/stream?static=true&api_key={MOCK_TOKEN}", server.base_url()),
        "requesting stream"
    );
    tracing::debug!("GET /Items?ApiKey={MOCK_TOKEN} and api_key={MOCK_TOKEN}");
    tracing::debug!("header Authorization: {}", authorization.header_value());
    tracing::debug!("X-Emby-Authorization: {}", authorization.header_value());
    tracing::debug!("sign-in body {{\"Username\":\"demo\",\"Pw\":\"demo-password-1\"}}");
    tracing::info!("kept: playback started in 42 ms");

    // The level can change while the app runs (the Settings page does this).
    assert_eq!(kattri_logging::current_level(), Some(LogLevel::Debug));
    assert!(kattri_logging::set_level(LogLevel::Warn));
    assert_eq!(kattri_logging::current_level(), Some(LogLevel::Warn));
    tracing::info!("dropped: info below the warn level");
    tracing::warn!("kept: a warning");
    assert!(kattri_logging::set_level(LogLevel::Debug));
    tracing::debug!("kept: debug again");

    let caught = panic::catch_unwind(|| {
        panic!("boom while handling Token=\"{MOCK_TOKEN}\"");
    });
    assert!(caught.is_err());
    let caught = panic::catch_unwind(|| {
        panic!("plain failure");
    });
    assert!(caught.is_err());

    drop(guard);
    server.shutdown().await;

    let text = fs::read_to_string(dir.join(LOG_FILE_NAME)).expect("log file");
    for secret in [MOCK_TOKEN, "demo-password-1", "127.0.0.1:"] {
        assert!(
            !text.contains(secret),
            "{secret:?} leaked into the log:\n{text}"
        );
    }
    assert!(text.contains("kept: playback started in 42 ms"), "{text}");
    assert!(!text.contains("dropped:"), "{text}");
    for kept in ["kept: a warning", "kept: debug again"] {
        assert!(text.contains(kept), "{kept} missing:\n{text}");
    }
    // The redactor is fail-closed: the Session Debug line has an `access_token:` field, so the
    // rest of that line is dropped even though the token itself is already masked.
    assert!(text.contains("signed in: Session {"), "{text}");
    assert!(text.contains("ERROR panic: boom while handling"), "{text}");
    assert!(
        text.contains("ERROR panic: plain failure location=")
            && text.contains("redaction_end_to_end.rs"),
        "panic location:\n{text}"
    );

    let _ = fs::remove_dir_all(&dir);
}
