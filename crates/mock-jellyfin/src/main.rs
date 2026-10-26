//! Run the mock Jellyfin server on 127.0.0.1.
//!
//! Usage: mock-jellyfin [--port <PORT>]   (default 8096; use 0 for ephemeral)

use mock_jellyfin::{MockConfig, spawn};

const DEFAULT_PORT: u16 = 8096;

fn parse_port(args: &[String]) -> Result<u16, String> {
    let mut port = DEFAULT_PORT;
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" | "-p" => {
                let value = iter.next().ok_or("--port requires a value")?;
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port: {value}"))?;
            }
            "--help" | "-h" => {
                println!(
                    "mock-jellyfin [--port <PORT>]\nServes a deterministic mock Jellyfin API on 127.0.0.1. Not a real server."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(port)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port = match parse_port(&args) {
        Ok(port) => port,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(2);
        }
    };
    let server = match spawn(MockConfig {
        port,
        ..MockConfig::default()
    })
    .await
    {
        Ok(server) => server,
        Err(error) => {
            eprintln!("error: could not bind 127.0.0.1:{port}: {error}");
            std::process::exit(1);
        }
    };
    println!(
        "mock-jellyfin listening on {} (not a real Jellyfin server)",
        server.base_url()
    );
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("error: failed to listen for Ctrl+C: {error}");
    }
    server.shutdown().await;
}
