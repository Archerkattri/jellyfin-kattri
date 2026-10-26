# Jellyfin Kattri

A Rust-native Jellyfin desktop client built with egui, OpenGL and libmpv.
Connect to your Jellyfin server, browse your libraries and play media in a native
desktop window. Server credentials remain in Rust; libmpv receives a local relay
URL. Remembered sign-ins use the operating system credential store.

## Status

This is an early development build, not a production release. Browsing, search,
playback negotiation and native Windows video have been exercised. Audio,
playback recovery, all controls, performance budgets and non-Windows runtime
behavior still need acceptance testing. Group watch, downloads, plugins and
updates are at different stages of integration. No full Jellyfin Web or Desktop
feature parity is claimed.

The intended platforms are Windows x64/ARM64, macOS and Linux. Packaging scripts
are included; their presence does not mean signed installers or every platform
have been validated.

## Build and run

Install Rust through rustup and the native compiler/linker for your platform.
The workspace toolchain is pinned in `rust-toolchain.toml`; dependency versions
are pinned in `Cargo.lock`.

Linux builds need pkg-config and libudev development headers for gamepad input.
On Debian or Ubuntu, install them with `sudo apt-get install pkg-config libudev-dev`.

```sh
cargo build --workspace --locked
cargo run --locked -p egui-gl-host-proof --bin jellyfin-kattri
```

Playback requires libmpv to be available to the platform's library loader and a
working graphical session. libmpv is loaded at runtime. The native executable
is `jellyfin-kattri`; the optional eframe demo is not the playback host.

## Development checks

The default test suite uses local fixtures and an in-process mock server. It
does not require a Jellyfin server, libmpv, a display or stored credentials.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Run the combined checks with `bash scripts/check.sh`, or on Windows:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check.ps1
```

Live tests are ignored by default. Use an authorized test server, set
`JELLYFIN_TEST_URL`, `JELLYFIN_TEST_USER` and `JELLYFIN_TEST_PASSWORD` only in the
test process environment, and inspect each test before opting in: some tests
perform playback or modify a disposable playlist. Never commit credentials,
tokens, private server details or live-test output.

## Source layout

- `crates/`: Jellyfin API, playback policy, credential-isolating relay, settings,
  input, platform services and supporting libraries.
- `proofs/egui-client/`: native egui application and bounded API worker.
- `proofs/egui-gl-host/`: desktop executable, OpenGL composition and player controls.
- `proofs/mpv-render/`: libmpv loading, rendering and event integration.
- `packaging/`: platform packaging and pinned libmpv fetch configuration.
- `scripts/`: development checks and opt-in validation tools.

## Diagnostics and security

Application logs are rotated and redact credentials and addresses. The About
screen shows their location. `KATTRI_LOG` can override the log level for one run.
Sign out to remove a remembered access token from the OS credential store.
Remote plain HTTP requires an explicit opt-in; loopback and private-network
addresses follow the client's local-server policy.

## License

GPL-3.0-only. See `LICENSE` and `NOTICE`. Native library and codec distributions
must also comply with their licenses; the libmpv packaging notice is included
under `packaging/libmpv/NOTICE.txt`. Jellyfin Kattri is an independent client.
