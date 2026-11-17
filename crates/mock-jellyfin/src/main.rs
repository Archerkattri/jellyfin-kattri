//! Run the mock Jellyfin server on 127.0.0.1.
//!
//! Usage: mock-jellyfin [--port <PORT>] [--audio-items <COUNT>]
//!        [--synthetic-audio-wav] [--shutdown-file <PATH>]

use std::{path::PathBuf, time::Duration};

use mock_jellyfin::{MockConfig, spawn};

const DEFAULT_PORT: u16 = 8096;
const MAX_AUDIO_ITEMS: usize = 1_000;

struct Options {
    port: u16,
    audio_item_count: Option<usize>,
    synthetic_audio_wav: bool,
    shutdown_file: Option<PathBuf>,
    syncplay_live: bool,
    media_file: Option<PathBuf>,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        port: DEFAULT_PORT,
        audio_item_count: None,
        synthetic_audio_wav: false,
        shutdown_file: None,
        syncplay_live: false,
        media_file: None,
    };
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" | "-p" => {
                let value = iter.next().ok_or("--port requires a value")?;
                options.port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port: {value}"))?;
            }
            "--audio-items" => {
                let value = iter.next().ok_or("--audio-items requires a value")?;
                let count = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid audio item count: {value}"))?;
                if !(1..=MAX_AUDIO_ITEMS).contains(&count) {
                    return Err(format!(
                        "audio item count must be between 1 and {MAX_AUDIO_ITEMS}"
                    ));
                }
                options.audio_item_count = Some(count);
            }
            "--synthetic-audio-wav" => options.synthetic_audio_wav = true,
            "--syncplay-live" => options.syncplay_live = true,
            "--media-file" => {
                let value = iter.next().ok_or("--media-file requires a path")?;
                if value.is_empty() {
                    return Err("--media-file requires a non-empty path".into());
                }
                options.media_file = Some(PathBuf::from(value));
            }
            "--shutdown-file" => {
                let value = iter.next().ok_or("--shutdown-file requires a path")?;
                if value.is_empty() {
                    return Err("--shutdown-file requires a non-empty path".into());
                }
                options.shutdown_file = Some(PathBuf::from(value));
            }
            "--help" | "-h" => {
                println!(
                    "mock-jellyfin [--port <PORT>] [--audio-items <1..={MAX_AUDIO_ITEMS}>] [--synthetic-audio-wav] [--syncplay-live] [--shutdown-file <PATH>]\n\
                     Serves a deterministic mock Jellyfin API on 127.0.0.1. Not a real server.\n\
                     Synthetic WAV data is opt-in; without it audio streams remain placeholder bytes.\n\
                     --syncplay-live enables the stateful SyncPlay group simulator (WebSocket + anchors).\n\
                     --media-file <PATH> serves a real local media file (with ranges) for stream requests.\n\
                     A shutdown file requests graceful process shutdown when it appears."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(options)
}

async fn wait_for_shutdown_file(path: PathBuf) {
    loop {
        if path.is_file() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let options = match parse_options(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(2);
        }
    };
    let server = match spawn(MockConfig {
        port: options.port,
        audio_item_count: options.audio_item_count,
        synthetic_audio_wav: options.synthetic_audio_wav,
        syncplay_live: options.syncplay_live,
        media_file: options.media_file,
        ..MockConfig::default()
    })
    .await
    {
        Ok(server) => server,
        Err(error) => {
            eprintln!("error: could not bind 127.0.0.1:{}: {error}", options.port);
            std::process::exit(1);
        }
    };
    println!(
        "mock-jellyfin listening on {} (not a real server; synthetic WAV: {})",
        server.base_url(),
        options.synthetic_audio_wav
    );

    if let Some(path) = options.shutdown_file {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    eprintln!("error: failed to listen for Ctrl+C: {error}");
                }
            }
            () = wait_for_shutdown_file(path) => {
                println!("mock-jellyfin received graceful shutdown request");
            }
        }
    } else if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("error: failed to listen for Ctrl+C: {error}");
    }
    server.shutdown().await;
}
