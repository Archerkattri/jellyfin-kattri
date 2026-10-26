//! Opt-in M3-15 playlist write check against an existing Jellyfin server.
//!
//! Owner limits (docs/DECISIONS.md, 2026-10-21): create one playlist whose name starts
//! with `kattri-test-m3-15-` and a timestamp, add three movies, move the last one to the
//! front, remove one entry, and delete the playlist. No other object, playback, session
//! report, watched state, setting, or playlist is changed.
//!
//! Guards, checked before any write:
//! - the generated name must carry the prefix; any other name is refused;
//! - the listing of existing playlists must be complete, and none may carry the prefix.
//!   A playlist that already has the prefix is left alone and the run stops.
//!
//! Cleanup runs after the steps even when one fails, so the test playlist is not
//! left behind after a bad response. The delete is confirmed with a 404 read.
//!
//! The default run (`cargo test`) covers the guards and the cleanup path against
//! `mock-jellyfin`. The mock is not a real server, so those passes say nothing about
//! the owner server.
//!
//! Environment: `JELLYFIN_TEST_URL` (defaults to the owner server), `JELLYFIN_TEST_USER`,
//! `JELLYFIN_TEST_PASSWORD`. Use `scripts/run_live_m3_15.ps1`, which loads the encrypted
//! local credentials for this process only.
//!
//! Output has status lines and error classes only. It never prints playlist names, IDs,
//! item titles, URLs, or tokens.

use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use jellyfin_api::{ApiError, ClientConfig, ItemQuery, JellyfinClient, MAX_HOME_LIMIT, Session};
use reqwest::StatusCode;
use zeroize::Zeroizing;

const DEFAULT_SERVER: &str = "http://127.0.0.1:8096/";
const TEST_PREFIX: &str = "kattri-test-m3-15-";
const ITEMS_TO_ADD: usize = 3;
/// Server-side playlist name limit, in characters after trimming.
const MAX_NAME_CHARS: usize = 100;
const MAX_LISTING_PAGES: u32 = 50;
/// An ID that cannot name a real entry. Used only by the failure-path test.
const MISSING_ENTRY_ID: &str = "00000000000000000000000000000fff";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuardError {
    UnexpectedName,
    ExistingTestPlaylist,
    IncompleteListing,
}

#[derive(Debug)]
enum CheckError {
    Guard(GuardError),
    Api {
        step: &'static str,
        error: ApiError,
    },
    Mismatch(&'static str),
    /// `error` is `None` when the playlist is still readable after the delete.
    Cleanup {
        step: &'static str,
        error: Option<ApiError>,
    },
}

impl CheckError {
    fn describe(&self) -> String {
        match self {
            Self::Guard(guard) => format!("guard refused: {guard:?}"),
            Self::Api { step, error } => format!("{step}: class={:?}", error.classify()),
            Self::Mismatch(step) => format!("{step}: unexpected result"),
            Self::Cleanup {
                step,
                error: Some(error),
            } => format!("{step}: class={:?}", error.classify()),
            Self::Cleanup { step, error: None } => {
                format!("{step}: playlist still readable after delete")
            }
        }
    }
}

/// Failure to inject into the steps. `None` is the owner run; the other value
/// exists so the mock test can prove cleanup after a failed step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    MoveUnknownEntry,
}

/// Refuse any name that does not start with the owner-approved prefix. Matching is
/// case-sensitive because generated names are always lowercase.
fn check_new_name(name: &str) -> Result<(), GuardError> {
    match name.strip_prefix(TEST_PREFIX) {
        Some(suffix) if !suffix.is_empty() && name.chars().count() <= MAX_NAME_CHARS => Ok(()),
        _ => Err(GuardError::UnexpectedName),
    }
}

/// Refuse when any existing playlist looks like a test playlist. The comparison
/// ignores case and surrounding spaces, so a near match also stops the run.
fn check_existing(names: &[String]) -> Result<(), GuardError> {
    let clashes = names
        .iter()
        .any(|name| name.trim().to_ascii_lowercase().starts_with(TEST_PREFIX));
    if clashes {
        Err(GuardError::ExistingTestPlaylist)
    } else {
        Ok(())
    }
}

fn new_playlist_name(now_millis: u128) -> String {
    format!("{TEST_PREFIX}{now_millis}")
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default()
}

/// Read every existing playlist name. A short or unknown total fails closed.
async fn existing_playlist_names(
    client: &JellyfinClient,
    session: &Session,
) -> Result<Vec<String>, CheckError> {
    let mut names = Vec::new();
    let mut start_index = 0_u32;
    for _ in 0..MAX_LISTING_PAGES {
        let page = client
            .list_items(
                session,
                &ItemQuery {
                    recursive: true,
                    start_index,
                    limit: MAX_HOME_LIMIT,
                    include_item_types: Some(vec!["Playlist".to_owned()]),
                    enable_total_record_count: Some(true),
                    ..ItemQuery::default()
                },
            )
            .await
            .map_err(|error| CheckError::Api {
                step: "list playlists",
                error,
            })?;
        let fetched = page.items.len();
        names.extend(page.items.into_iter().map(|item| item.name));
        let complete = match page.total_record_count {
            Some(total) => names.len() >= total as usize,
            None => fetched < MAX_HOME_LIMIT as usize,
        };
        if complete {
            println!("M3-15 list existing playlists: PASS");
            return Ok(names);
        }
        if fetched == 0 {
            break;
        }
        start_index += fetched as u32;
    }
    Err(CheckError::Guard(GuardError::IncompleteListing))
}

/// Pick three distinct movies to add. Read-only, so it runs before the create.
async fn pick_items(client: &JellyfinClient, session: &Session) -> Result<Vec<String>, CheckError> {
    let page = client
        .list_items(
            session,
            &ItemQuery {
                recursive: true,
                limit: ITEMS_TO_ADD as u32,
                include_item_types: Some(vec!["Movie".to_owned()]),
                ..ItemQuery::default()
            },
        )
        .await
        .map_err(|error| CheckError::Api {
            step: "select movies",
            error,
        })?;
    let ids: Vec<String> = page.items.into_iter().map(|item| item.id).collect();
    let mut distinct = ids.clone();
    distinct.sort();
    distinct.dedup();
    if ids.len() != ITEMS_TO_ADD || distinct.len() != ITEMS_TO_ADD {
        return Err(CheckError::Mismatch("need three distinct movies to add"));
    }
    Ok(ids)
}

/// Playlist entries as (item ID, entry ID) pairs in playlist order.
async fn read_entries(
    client: &JellyfinClient,
    session: &Session,
    playlist_id: &str,
    step: &'static str,
) -> Result<Vec<(String, String)>, CheckError> {
    let page = client
        .list_playlist_items(session, playlist_id, 0, MAX_HOME_LIMIT)
        .await
        .map_err(|error| CheckError::Api { step, error })?;
    let count = page.items.len();
    if page
        .total_record_count
        .is_some_and(|total| total as usize != count)
    {
        return Err(CheckError::Mismatch(step));
    }
    page.items
        .into_iter()
        .map(|entry| match entry.playlist_item_id {
            Some(entry_id) => Ok((entry.item.id, entry_id)),
            None => Err(CheckError::Mismatch(step)),
        })
        .collect()
}

fn check_order(
    entries: &[(String, String)],
    expected_item_ids: &[&str],
    step: &'static str,
) -> Result<(), CheckError> {
    let actual: Vec<&str> = entries.iter().map(|(item, _)| item.as_str()).collect();
    if actual == expected_item_ids {
        Ok(())
    } else {
        Err(CheckError::Mismatch(step))
    }
}

async fn run_steps(
    client: &JellyfinClient,
    session: &Session,
    playlist_id: &str,
    items: &[String],
    fault: Fault,
) -> Result<(), CheckError> {
    let (a, b, c) = (items[0].as_str(), items[1].as_str(), items[2].as_str());

    client
        .add_playlist_items(session, playlist_id, items)
        .await
        .map_err(|error| CheckError::Api {
            step: "add three items",
            error,
        })?;
    let entries = read_entries(client, session, playlist_id, "read after add").await?;
    check_order(&entries, &[a, b, c], "add: order")?;
    println!("M3-15 add three items: PASS");

    if fault == Fault::MoveUnknownEntry {
        return match client
            .move_playlist_entry(session, playlist_id, MISSING_ENTRY_ID, 0)
            .await
        {
            Err(error) => Err(CheckError::Api {
                step: "move with unknown entry",
                error,
            }),
            Ok(()) => Err(CheckError::Mismatch("move with unknown entry accepted")),
        };
    }

    // Move the last entry to the front; the other two keep their relative order.
    let last_entry = entries[2].1.clone();
    client
        .move_playlist_entry(session, playlist_id, &last_entry, 0)
        .await
        .map_err(|error| CheckError::Api {
            step: "move last entry to front",
            error,
        })?;
    let entries = read_entries(client, session, playlist_id, "read after move").await?;
    check_order(&entries, &[c, a, b], "move: order")?;
    println!("M3-15 move last to front: PASS");

    // Remove the entry for the first item, now at index 1.
    let removed_entry = entries[1].1.clone();
    client
        .remove_playlist_entries(session, playlist_id, std::slice::from_ref(&removed_entry))
        .await
        .map_err(|error| CheckError::Api {
            step: "remove one entry",
            error,
        })?;
    let entries = read_entries(client, session, playlist_id, "read after remove").await?;
    check_order(&entries, &[c, b], "remove: order")?;
    println!("M3-15 remove one entry: PASS");
    Ok(())
}

/// Delete the playlist and confirm it is gone. Prints the result either way.
async fn delete_and_confirm(
    client: &JellyfinClient,
    session: &Session,
    playlist_id: &str,
) -> Result<(), CheckError> {
    if let Err(error) = client.delete_playlist(session, playlist_id).await {
        println!(
            "M3-15 delete test playlist: FAIL class={:?}",
            error.classify()
        );
        return Err(CheckError::Cleanup {
            step: "delete test playlist",
            error: Some(error),
        });
    }
    match client.get_playlist(session, playlist_id).await {
        Err(ApiError::HttpStatus(status)) if status == StatusCode::NOT_FOUND => {
            println!("M3-15 delete test playlist: PASS (confirmed 404)");
            Ok(())
        }
        Err(error) => {
            println!(
                "M3-15 delete test playlist: FAIL confirm class={:?}",
                error.classify()
            );
            Err(CheckError::Cleanup {
                step: "confirm deletion",
                error: Some(error),
            })
        }
        Ok(_) => {
            println!("M3-15 delete test playlist: FAIL still readable");
            Err(CheckError::Cleanup {
                step: "confirm deletion",
                error: None,
            })
        }
    }
}

/// Run the guarded write sequence. Cleanup runs even when a step fails, and the
/// step's error is returned when both fail.
async fn playlist_write_check(
    client: &JellyfinClient,
    session: &Session,
    name: &str,
    fault: Fault,
) -> Result<(), CheckError> {
    check_new_name(name).map_err(CheckError::Guard)?;
    let existing = existing_playlist_names(client, session).await?;
    check_existing(&existing).map_err(CheckError::Guard)?;
    let items = pick_items(client, session).await?;

    let playlist_id = client
        .create_playlist(session, name, &[])
        .await
        .map_err(|error| CheckError::Api {
            step: "create test playlist",
            error,
        })?;
    println!("M3-15 create test playlist: PASS");

    let steps = run_steps(client, session, &playlist_id, &items, fault).await;
    let cleanup = delete_and_confirm(client, session, &playlist_id).await;
    match (steps, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            println!(
                "M3-15 cleanup after failed step: FAIL {}",
                cleanup_error.describe()
            );
            Err(error)
        }
    }
}

#[tokio::test]
#[ignore = "writes a temporary playlist to an existing Jellyfin server; use scripts/run_live_m3_15.ps1"]
async fn live_playlist_write_check() {
    let base = env::var("JELLYFIN_TEST_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_owned());
    let config = ClientConfig::new(
        &base,
        "Kattri validator",
        "validation",
        "kattri-validation-m3-15",
        "1",
    )
    .expect("valid server configuration");
    let client = JellyfinClient::new(config).expect("HTTP client");
    let info = client
        .public_system_info()
        .await
        .expect("public info request failed");
    println!(
        "M3-15 server_version: {}",
        info.version.as_deref().unwrap_or("unknown")
    );

    let username = env::var("JELLYFIN_TEST_USER")
        .ok()
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    let password = env::var("JELLYFIN_TEST_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    let (Some(username), Some(password)) = (username, password) else {
        panic!("M3-15 credentials unavailable; no playlist was created");
    };
    let session = client
        .authenticate(&username, &password)
        .await
        .expect("authentication failed; response details are suppressed");
    println!("M3-15 authenticate: PASS");

    let name = new_playlist_name(now_millis());
    match playlist_write_check(&client, &session, &name, Fault::None).await {
        Ok(()) => println!("M3-15 playlist write check: PASS"),
        Err(error) => {
            println!("M3-15 playlist write check: FAIL {}", error.describe());
            panic!("M3-15 playlist write check failed");
        }
    }
}

#[cfg(test)]
mod guard_tests {
    use super::{GuardError, check_existing, check_new_name, new_playlist_name};

    #[test]
    fn refuses_names_without_the_owner_prefix() {
        assert_eq!(check_new_name("Road trip"), Err(GuardError::UnexpectedName));
        assert_eq!(
            check_new_name("KATTRI-TEST-M3-15-1"),
            Err(GuardError::UnexpectedName)
        );
        assert_eq!(
            check_new_name("kattri-test-m3-15-"),
            Err(GuardError::UnexpectedName)
        );
        let too_long = format!("kattri-test-m3-15-{}", "x".repeat(100));
        assert_eq!(check_new_name(&too_long), Err(GuardError::UnexpectedName));
    }

    #[test]
    fn accepts_generated_names() {
        let name = new_playlist_name(1_700_000_000_000);
        assert!(name.starts_with("kattri-test-m3-15-"));
        assert_eq!(check_new_name(&name), Ok(()));
    }

    #[test]
    fn refuses_when_an_existing_playlist_has_the_prefix() {
        let names = vec![
            "Favorites".to_owned(),
            "  Kattri-Test-M3-15-17  ".to_owned(),
        ];
        assert_eq!(
            check_existing(&names),
            Err(GuardError::ExistingTestPlaylist)
        );
        assert_eq!(check_existing(&["Favorites".to_owned()]), Ok(()));
        assert_eq!(check_existing(&[]), Ok(()));
    }
}

#[cfg(test)]
mod mock_tests {
    use super::{
        CheckError, Fault, GuardError, existing_playlist_names, new_playlist_name,
        playlist_write_check,
    };
    use jellyfin_api::{ApiError, ClientConfig, JellyfinClient, Session};
    use mock_jellyfin::{MockConfig, RunningServer, spawn};
    use reqwest::StatusCode;
    use serde_json::Value;

    fn client(base_url: &str) -> JellyfinClient {
        let config = ClientConfig::new(
            base_url,
            "Kattri test",
            "test device",
            "device-id",
            "test-version",
        )
        .expect("valid client configuration");
        JellyfinClient::new(config).expect("configured Jellyfin client")
    }

    async fn signed_in(server: &RunningServer) -> (JellyfinClient, Session) {
        let client = client(&server.base_url());
        let session = client.authenticate("demo", "demo").await.expect("sign in");
        (client, session)
    }

    async fn request_log(server: &RunningServer) -> Vec<Value> {
        reqwest::Client::new()
            .get(format!("{}/__mock/requests", server.base_url()))
            .send()
            .await
            .expect("read request log")
            .error_for_status()
            .expect("request log status")
            .json::<Value>()
            .await
            .expect("request log JSON")
            .as_array()
            .expect("request entries")
            .clone()
    }

    /// Requests logged as `"{method} {path}"`, including the query string.
    fn logged(log: &[Value]) -> Vec<String> {
        log.iter()
            .map(|entry| {
                format!(
                    "{} {}",
                    entry["Method"].as_str().unwrap_or_default(),
                    entry["Path"].as_str().unwrap_or_default()
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn write_sequence_passes_against_the_mock_and_leaves_no_playlist() {
        let server = spawn(MockConfig::default()).await.expect("mock starts");
        let (client, session) = signed_in(&server).await;

        let name = new_playlist_name(1_700_000_000_000);
        playlist_write_check(&client, &session, &name, Fault::None)
            .await
            .expect("sequence passes on the mock");

        let calls = logged(&request_log(&server).await);
        assert!(
            calls.iter().any(|call| call == "POST /Playlists"),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("POST /Playlists/") && call.contains("/Items?")),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|call| call.contains("/Move/0")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("DELETE /Playlists/") && call.contains("/Items?")),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|call| call.starts_with("DELETE /Items/")),
            "{calls:?}"
        );
        let remaining = existing_playlist_names(&client, &session)
            .await
            .expect("list after run");
        assert!(remaining.is_empty(), "{remaining:?}");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn cleanup_deletes_the_playlist_after_a_failed_step() {
        let server = spawn(MockConfig::default()).await.expect("mock starts");
        let (client, session) = signed_in(&server).await;

        let name = new_playlist_name(1_700_000_000_001);
        let result = playlist_write_check(&client, &session, &name, Fault::MoveUnknownEntry).await;
        assert!(
            matches!(
                result,
                Err(CheckError::Api {
                    error: ApiError::HttpStatus(status),
                    ..
                }) if status == StatusCode::NOT_FOUND
            ),
            "{result:?}"
        );

        let calls = logged(&request_log(&server).await);
        assert!(
            calls.iter().any(|call| call.starts_with("DELETE /Items/")),
            "{calls:?}"
        );
        let remaining = existing_playlist_names(&client, &session)
            .await
            .expect("list after failed run");
        assert!(remaining.is_empty(), "{remaining:?}");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn guard_refuses_before_any_write_and_leaves_other_playlists() {
        let server = spawn(MockConfig::default()).await.expect("mock starts");
        let (client, session) = signed_in(&server).await;

        // A playlist with the prefix already exists. The run must stop and must not delete it.
        let existing = client
            .create_playlist(&session, "kattri-test-m3-15-old", &[])
            .await
            .expect("seed existing playlist");
        let before = logged(&request_log(&server).await).len();

        let clash = playlist_write_check(
            &client,
            &session,
            &new_playlist_name(1_700_000_000_002),
            Fault::None,
        )
        .await;
        assert!(
            matches!(
                clash,
                Err(CheckError::Guard(GuardError::ExistingTestPlaylist))
            ),
            "{clash:?}"
        );

        // A name without the prefix is refused before the existing-playlist listing.
        let unexpected = playlist_write_check(&client, &session, "Road trip", Fault::None).await;
        assert!(
            matches!(
                unexpected,
                Err(CheckError::Guard(GuardError::UnexpectedName))
            ),
            "{unexpected:?}"
        );

        let calls = logged(&request_log(&server).await);
        let new_writes = calls[before..]
            .iter()
            .filter(|call| call.starts_with("POST ") || call.starts_with("DELETE "))
            .count();
        assert_eq!(new_writes, 0, "{calls:?}");
        client
            .get_playlist(&session, &existing)
            .await
            .expect("existing playlist was left in place");
        server.shutdown().await;
    }
}
