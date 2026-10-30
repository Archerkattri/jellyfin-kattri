# Jellyfin Kattri

[![Rust client CI](https://github.com/Archerkattri/jellyfin-kattri/actions/workflows/rust-client-ci.yml/badge.svg)](https://github.com/Archerkattri/jellyfin-kattri/actions/workflows/rust-client-ci.yml)
[![Latest release](https://img.shields.io/github/v/release/Archerkattri/jellyfin-kattri?include_prereleases&label=release)](https://github.com/Archerkattri/jellyfin-kattri/releases)
[![License: GPL-3.0-only](https://img.shields.io/badge/license-GPL--3.0--only-blue)](LICENSE)

A native desktop client for Jellyfin. Rust, egui and libmpv, with no web view, no
JavaScript UI layer and no server-side changes. Your Jellyfin server stays where it
is; Kattri connects to it like any other client and plays your media in a real
desktop window.

![Jellyfin Kattri home screen](assets/screenshots/home.jpg)

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
  driven by keyboard or gamepad as well as the mouse, with Standard, Large and
  10-foot interface sizes.

## Screenshots

| Movies library | Item details |
| --- | --- |
| ![Movies library](assets/screenshots/library.jpg) | ![Item details](assets/screenshots/detail.jpg) |

All screenshots are framebuffer captures of the running application against a real
Jellyfin 12.4.0 server (Windows x64, 2026-10-29). Server artwork is shown; no mock
data is used.

## Features

| Area | Included |
| --- | --- |
| Sign-in | Password sign-in, Quick Connect, saved servers, multiple profiles, stay signed in, forgot-password flow |
| Home | Continue Watching, Next Up, Latest, My Media and Collections shelves, hero banner, per-section visibility settings |
| Library | Paging, sorting, play-state, favorites, genre and studio filters, alphabet picker, 1,000-item photo and poster grids, music All/Albums/Songs views |
| Details | Metadata, overview, cast, similar titles, seasons and episodes with overview and progress, series Next Up, media info, version picker, trailers and extras, lyrics, play and download actions |
| Search | Debounced cross-type search grouped by movies, series, episodes, music, playlists, photos, people and channels, with Show all and server subtitle search |
| Playback | Direct play, direct stream and transcode through the relay, resume, playback reporting, audio and subtitle tracks, subtitle delay, chapters, speed control, segment skip, bounded mid-stream recovery |
| Player | Icon control bar (transport, 10-second seek, frame step, chapter step and chapter menu, volume and mute, speed, A-B loop, picture-in-picture, fullscreen) with title and elapsed/total time; previous/next for tracks and episodes, an Up Next card and auto-play of the next episode; audio and subtitle menu, auto-hiding overlay, media info panel (`I`), equalizer and loudness filters, HDR to SDR tone mapping, refresh-rate matching on Windows, open a stream address |
| Music | Album and playlist browsing, queue with shuffle and repeat, queue restore after restart |
| Live TV | Channel list, 24-hour guide, tune through the relay, record the current program, recordings and scheduled recordings |
| Offline | Download manager with pause, resume, range fallback, checksum verification and external subtitles |
| Extras | Right-click item menu (play, mark watched, favorite, download, add to playlist or queue), playlists, collections, favorites, photos with slideshow, admin-lite dashboard, parental PIN |
| Administration | Edit metadata, Identify, and Edit images for administrators, plus Add to collection, Refresh metadata and confirmed Delete |
| Settings | General, playback, audio, subtitles, video, input, server, About; versioned TOML per profile |
| Input | Keyboard map, gamepad support in menus and during playback, full focus navigation, rebindable actions |
| Interface | Ambient movie/show backdrops with poster fallback and reduced-motion-aware crossfades; optional local custom backdrop; Standard, Large and TV (10-foot) sizes; narrow-window More menu |

## Download

Grab a build from the [releases page](https://github.com/Archerkattri/jellyfin-kattri/releases).
The current prerelease is **v0.3.1**.

| Platform | Artifacts | libmpv |
| --- | --- | --- |
| Windows x64 | `JellyfinKattri-<version>-x64.msi`, `-x64-portable.zip` | Bundled next to the executable |
| Windows ARM64 | `JellyfinKattri-<version>-arm64.msi`, `-arm64-portable.zip` | Bundled next to the executable |
| macOS universal | `kattri-<version>-macos-universal.dmg` | Bundled inside the app |
| Linux x64 | `kattri_<version>-1_amd64.deb`, `kattri-<version>-1.x86_64.rpm` | Uses the distribution package (`libmpv2` or `libmpv1`) |
| Linux x64 | `kattri-<version>-linux-x64.AppImage`, `kattri-<version>-linux-x64.flatpak` | Bundled |
| Linux ARM64 | Built and tested in CI | No package yet |

Every package and bundle ships with a `.sha256` file. Install examples:

```sh
sudo apt install ./kattri_0.3.1-1_amd64.deb        # Debian/Ubuntu; uses a system libmpv
sudo dnf install ./kattri-0.3.1-1.x86_64.rpm       # Fedora; uses a system libmpv
chmod +x kattri-0.3.1-linux-x64.AppImage && ./kattri-0.3.1-linux-x64.AppImage
flatpak install --user --bundle ./kattri-0.3.1-linux-x64.flatpak
```

The packages are **unsigned**: Windows SmartScreen and macOS Gatekeeper will warn.
The macOS app is ad-hoc signed, not notarized. This is a development prerelease, not
a 1.0 release.

## Build from source

Install Rust through rustup; the toolchain is pinned in `rust-toolchain.toml` and
dependencies in `Cargo.lock`.

```sh
cargo build --workspace --locked
cargo run --locked -p egui-gl-host-proof --bin jellyfin-kattri
```

The executable is `jellyfin-kattri`. libmpv is loaded at run time, so it is not needed
to build. The optional eframe demo is a UI preview only; it does not host libmpv.

Platform prerequisites:

```sh
# Linux: gamepad input headers, X11 keyboard library, and a libmpv to load
# (Ubuntu 24.04 and Debian 12 ship libmpv2; Ubuntu 22.04 ships libmpv1)
sudo apt-get install pkg-config libudev-dev libxkbcommon-x11-0 libmpv2

# macOS: a libmpv build; the packaged app carries its own
brew install mpv

# Windows: place libmpv-2.dll beside jellyfin-kattri.exe or on PATH
```

## First run

1. Start the app and enter your server address. Plain HTTP is accepted for loopback
   and private-network addresses; public servers must use HTTPS unless you explicitly
   allow otherwise.
2. Sign in with a password or Quick Connect. Enable "Stay signed in" to keep the
   access token in the OS credential store.
3. Pick a profile with `--profile <name>` to keep separate settings and queues.

Settings, profiles and logs live in the platform config directory; the About screen
shows the exact log folder. `KATTRI_LOG` overrides the log level for one run. Logs
redact credentials and server addresses, and nothing is uploaded anywhere.

## Keyboard and gamepad

The default keymap covers navigation and playback. Every action can be rebound in
Settings → Input, and a gamepad can drive menus and the player.

| Action | Keys |
| --- | --- |
| Navigate | Arrow keys or `W` `A` `S` `D` |
| Confirm / back | `Enter` · `Esc`, `Backspace` or `B` |
| Home / search / settings | `H` · `Ctrl+F` · `M` |
| Play, pause, stop | `Space` or `P` · `Ctrl+P` · `X` |
| Seek and step | `PgUp` / `PgDn` · `Home` / `End` |
| Audio, subtitles, mute | `E` / `Ctrl+A` · `L` / `Ctrl+L`, `C` · `N` |
| Volume and speed | `=` / `-` · `Ctrl+Up` / `Ctrl+Down` |
| Subtitle delay | `Alt+S` / `Alt+Shift+S`, reset with `Alt+R` |
| Fullscreen, screenshot, close | `F11` · `Ctrl+S` · `Ctrl+Q` |

## Platform support

| Platform | Packages | Automated checks | Runtime evidence |
| --- | --- | --- | --- |
| Windows x64 | MSI, portable zip | Build, tests; the clean-runner MSI install → window → close → uninstall check is written but its last run failed | Browsing, search, PlaybackInfo, native video presentation, and an HDR10 → SDR tone-mapped run on an RTX 4060 |
| Windows ARM64 | MSI, portable zip | Build, tests; same MSI check, not yet green | Install and launch only |
| macOS universal | DMG | Build, mount, signature verification, launch and quit | Launch only |
| Linux x64 | deb, rpm, AppImage, flatpak | Build plus fresh-runner install, launch and normal close for all four formats (2026-10-29) | Launch only |
| Linux ARM64 | CI build | Build and tests | Not exercised |

Playback needs a graphical session and a libmpv the platform loader can find. The
clean-runner checks that pass (macOS and Linux) started the application and closed it
normally; none of these checks played media.

## Project status

Kattri is an early development build, not a production release. The build and test
workflows are green on Windows x64/ARM64, macOS and Linux x64/ARM64. More than 2,800
automated tests run in the default workspace suite against local fixtures and an
in-process mock Jellyfin server.

What has actually been exercised:

- **Real-server browsing** against Jellyfin 12.x: sign-in, Home rows, libraries, item
  details, search, people and similar items, and PlaybackInfo negotiation.
- **Stream-only playback** through the relay on Windows x64: a 10-minute DirectPlay
  run and a 30-second Transcode run with software/null outputs. Visible video and
  audible audio were not part of that run.
- **A Windows RTX 4060 pass** that used NVDEC and rendered an HDR10 source tone-mapped
  to SDR.
- **Packaging**: release-built artifacts for every platform. Clean-runner install,
  startup and normal-close checks pass for the macOS DMG and all four Linux formats;
  the Windows MSI check is implemented but red on its last run.

These checks do not establish complete playback, release readiness, or feature parity
with Jellyfin Web or Jellyfin Desktop. Unvalidated areas include visible/audible
playback on macOS and Linux, GPU decode across hardware, HDR passthrough, long HLS
sessions, SyncPlay with two clients, physical controllers, and accessibility walks on
each platform.

The screenshots above come from a 2026-10-29 Windows x64 pass against Jellyfin 12.4.0
that exercised Home, the Movies library, movie details, search and settings. Playback
was not part of that pass.

### Where Kattri still differs from Jellyfin Web

A source-level audit against jellyfin-web (2026-10-27) found real gaps. Kattri is not
yet a replacement for the official clients. The main ones:

- **Item menu:** Play, Play from beginning, Open details, Go to series, watched and
  favorite toggles, Download, playlists and queue, plus administrator actions (Edit
  metadata, Identify, Edit images, Add to collection, Refresh, Delete). Missing:
  shuffle from the menu and multi-select.
- **Library views:** one rich grid per type with filters and an alphabet picker.
  Missing Suggestions, Collections, Upcoming and Episodes tabs, alternate view modes,
  and an artist view for music.
- **Player:** no quality/source picker inside the player; SyncPlay is not connected to
  playback yet.
- **Details:** theme media and lyrics synced to playback are missing.
- **Live TV:** no series recordings or library management; the dashboard is admin-lite.
- **Distribution:** unsigned packages, no automatic updates in 1.0, and no Discord
  presence until 1.1.

### Work still required for a production release

**Playback and reliability**

- Validate the complete graphical path: visible video, audible audio, pause, seek,
  speed, chapters, tracks, subtitles, stop and clean shutdown on each platform.
- Exercise mid-stream recovery against a real failure; the bounded reopen logic is
  unit-tested only.
- Run DirectPlay, DirectStream and Transcode end to end, plus a long live HLS session.
- Validate resume, start/progress/stop reporting and watched-state behavior.
- Run disconnect, reconnect, slow-server, stalled-stream and cancellation scenarios.

**Integration and acceptance**

- Finish SyncPlay shared-queue playback, ready/buffering reports and two-client
  synchronization acceptance.
- Connect the sandboxed plugin runtime to the app end to end.
- Validate music queue advancement, tuner-backed Live TV, the download lifecycle,
  cast and remote control, and the admin dashboards with a real administrator.
- Walk keyboard, gamepad and 10-foot navigation across every screen and record
  screen-reader, focus and contrast checks per platform.

**Media and hardware**

- Validate HDR output per platform; HDR passthrough, Dolby Vision and HDR10+ remain
  open.
- Expand hardware decode across codecs, drivers and GPUs; validate refresh-rate
  switching and restoration.
- Validate audio devices, equalizer, filters, passthrough and fallback on real
  equipment, and ASS/SSA styling visually.
- Complete or explicitly scope ISO/BDMV/DVD playback.
- Measure startup, frame time and memory against the budgets, and finish the
  eight-hour stability soak.

**Distribution**

- Test every artifact on clean physical machines, including the bundled libmpv.
- Complete macOS signing/notarization; Windows packages stay unsigned in 1.0 by owner
  decision.
- Verify the embedded SBOM, licence notices and reproducible-build result.
- Review the ignored Linux accessibility dependency advisories and record the
  release decision.

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/jellyfin-api` | Typed Jellyfin client: connect, login, libraries, home, PlaybackInfo, playback sources, reporting |
| `crates/client-core` | Pure logic: playback phase machine, source selection, report scheduling, downloads, subtitle sync |
| `crates/playback-relay` | Loopback relay with capability URLs, origin binding and HLS rewrite |
| `crates/kattri-*` | Settings, input, platform services, downloads, parental control, plugins, updater, i18n, logging |
| `proofs/egui-client` | The egui application: screens, rows, hero, player controls, API worker |
| `proofs/mpv-render` | Dynamically loaded libmpv FFI, render context, tracks, subtitle delay |
| `proofs/egui-gl-host` | winit + glutin host; the binary that runs libmpv and the UI together |
| `packaging/` | Windows MSI/zip, macOS app/DMG, deb/rpm/AppImage/Flatpak, pinned libmpv builds |

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

or on bash with `bash scripts/check.sh`. It also runs the credential-helper
regressions, the no-network live-runner checks, and the release-target and packaging
contract checks.

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
