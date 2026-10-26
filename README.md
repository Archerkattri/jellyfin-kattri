# Jellyfin Kattri

A native desktop client for Jellyfin. Rust, egui and libmpv, with no web view, no
JavaScript UI layer and no server-side changes. Your Jellyfin server stays where it
is; Kattri connects to it like any other client and plays your media in a real
desktop window.

![Home screen](assets/screenshots/home.jpg)

## Why Kattri

- **Native, not a wrapped web app.** The interface is drawn with egui on an OpenGL
  surface, and video is rendered by libmpv into the same window. There is no
  QtWebEngine, no bundled browser and no second UI stack.
- **Your credentials stay in Rust.** libmpv never sees a token or a server URL. It
  plays a loopback relay URL issued per playback, bound to one origin and one
  capability. Remembered sign-ins go to the operating-system credential store, not
  to a settings file.
- **Evidence-based playback.** Kattri probes the local libmpv build and asks the
  server for DirectPlay, DirectStream or Transcode only for formats it can actually
  present. Unsupported sources fall back through the server's transcode path.
- **Built for a TV and a desk.** Hero banners, artwork rows and focus animation,
  driven by keyboard or gamepad as well as the mouse.

## What it does

| Area | Included |
| --- | --- |
| Sign-in | Password sign-in, Quick Connect, saved servers, multiple profiles, stay signed in |
| Home | Continue Watching, Next Up, Latest, My Media and Collections shelves, hero banner |
| Library | Paging, sorting, play-state and favorites filters, 1,000-item photo and poster grids |
| Details | Metadata, overview, cast, similar titles, seasons and episodes, play and download actions |
| Search | Debounced cross-type search grouped by movies, series, episodes, music, playlists, photos, people and channels |
| Playback | Direct play, direct stream and transcode through the relay, resume, playback reporting, audio and subtitle tracks, subtitle delay, chapters, speed control, segment skip |
| Music | Album and playlist browsing, queue with shuffle and repeat, queue restore after restart |
| Live TV | Channel list, 24-hour guide, tune through the relay |
| Offline | Download manager with pause, resume, range fallback, checksum verification and external subtitles |
| Extras | Playlists, collections, favorites, watched state, photos, admin-lite dashboard, parental PIN |
| Settings | General, playback, audio, subtitles, video, input, server, About; versioned TOML per profile |
| Input | Keyboard map, gamepad support, full focus navigation, rebindable actions |

### Library browsing

![Movies library](assets/screenshots/library.jpg)

### Item details

![Item details](assets/screenshots/detail.jpg)

## Platform support

| Platform | Build | Runtime |
| --- | --- | --- |
| Windows x64 | Supported | Exercised: browsing, negotiation, native video presentation |
| Windows ARM64 | Release build and installer scripts | Not exercised on hardware |
| macOS | Universal build scripts and CI build | Not exercised |
| Linux x64 / ARM64 | Packages and CI build | Not exercised |

Playback needs libmpv available to the platform's library loader and a graphical
session. Signed installers and clean-machine installs are not yet part of a release.

## Build from source

Install Rust through rustup; the toolchain is pinned in `rust-toolchain.toml` and
dependencies in `Cargo.lock`.

```sh
cargo build --workspace --locked
cargo run --locked -p egui-gl-host-proof --bin jellyfin-kattri
```

The executable is `jellyfin-kattri`. The optional eframe demo is a UI preview only;
it does not host libmpv.

Linux builds also need pkg-config and libudev development headers for gamepad input:

```sh
sudo apt-get install pkg-config libudev-dev
```

## First run

1. Start the app and enter your server address. Plain HTTP is accepted for loopback
   and private-network addresses; public servers must use HTTPS unless you explicitly
   allow otherwise.
2. Sign in with a password or Quick Connect. Enable "Stay signed in" to keep the
   access token in the OS credential store.
3. Pick a profile with `--profile <name>` to keep separate settings and queues.

Settings and profiles live in the platform config directory. `KATTRI_LOG` overrides
the log level for one run, and the About screen shows the log location. Logs redact
credentials and server addresses; nothing is uploaded anywhere.

## Project status

Kattri is an early development build, not a production release. On Windows x64 the
native client has been exercised against a live Jellyfin 12.0.0 server for browsing,
search, playback negotiation, native video presentation and stream-only relay
playback. Audio output, the complete control surface, hardware decoding, HDR,
macOS/Linux runtime and signed packaging are still being validated, and no full
feature parity with Jellyfin Web or Jellyfin Desktop is claimed.

The project tracks every feature with an explicit acceptance status rather than a
completion percentage; the internal development tree documents the architecture and
the evidence behind each claim. Nothing here should be read as a guarantee that a
given feature works on your hardware.

## Development checks

The default test suite uses local fixtures and an in-process mock Jellyfin server. It
needs no server, no libmpv, no display and no stored credentials.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Run the full gate on Windows with:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check.ps1
```

or on bash with `bash scripts/check.sh`.

Live-server tests are opt-in and ignored by default. Point them at a test server with
`JELLYFIN_TEST_URL`, `JELLYFIN_TEST_USER` and `JELLYFIN_TEST_PASSWORD`, and read each
test before running it: some perform playback or mutate a disposable playlist. Never
commit credentials, tokens or live output.

## Security and privacy

- Access tokens are zeroized in memory and stored only in the OS credential store,
  bound to the profile, server and exact base address.
- The playback relay listens on loopback only, uses a random capability, enforces the
  upstream origin, refuses redirects and copies only allow-listed response headers.
- JSON, image, manifest and stream inputs are bounded; logs and error messages are
  redacted.
- Remote plain HTTP requires an explicit opt-in. No telemetry is collected.

## License

GPL-3.0-only. See `LICENSE` and `NOTICE`. Bundled native libraries and codecs keep
their own licenses; the libmpv packaging notice is under `packaging/libmpv/NOTICE.txt`.
Jellyfin Kattri is an independent client and is not affiliated with the Jellyfin
project.
