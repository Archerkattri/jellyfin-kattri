# Jellyfin Kattri

[![Rust client CI](https://github.com/Archerkattri/jellyfin-kattri/actions/workflows/rust-client-ci.yml/badge.svg)](https://github.com/Archerkattri/jellyfin-kattri/actions/workflows/rust-client-ci.yml)
[![Latest release](https://img.shields.io/github/v/release/Archerkattri/jellyfin-kattri?include_prereleases&label=release)](https://github.com/Archerkattri/jellyfin-kattri/releases)
[![License: GPL-3.0-only](https://img.shields.io/badge/license-GPL--3.0--only-blue)](LICENSE)

A fast, native desktop client for Jellyfin. Built in Rust with egui and libmpv:
no web view, no JavaScript UI, no server changes. Kattri connects to your Jellyfin
server like any other client and plays your library in a real desktop window.

![Jellyfin Kattri home screen](assets/screenshots/home.jpg)

## Why Kattri

- **Native all the way down.** The interface is rendered by egui on an OpenGL
  surface and video is decoded by libmpv into the same window. No bundled browser,
  no second UI stack, no web-app memory footprint.
- **Your credentials stay yours.** libmpv never receives a token or your server URL.
  It plays a single-use loopback relay URL, bound to one origin. Remembered sign-ins
  live in the operating-system credential store, not a settings file.
- **Plays what the server has.** Kattri asks for DirectPlay, DirectStream or Transcode
  only for formats its local libmpv can actually present, then falls back through the
  server's transcode path when needed.
- **Made for a couch and a desk.** Hero artwork, poster rows and focus animation,
  driven by keyboard, gamepad or mouse, with Standard, Large and 10-foot interface
  sizes.

## Screenshots

| Movies library | Item details |
| --- | --- |
| ![Movies library](assets/screenshots/library.jpg) | ![Item details](assets/screenshots/detail.jpg) |

## Features

| | |
| --- | --- |
| **Sign-in** | Password sign-in, Quick Connect, saved servers, multiple profiles, stay signed in, forgot-password flow |
| **Home** | Continue Watching, Next Up, Latest, My Media and Collections shelves, hero banner, per-section visibility |
| **Library** | Paging, sorting, play state, favorites, genre and studio filters, alphabet picker, 1,000-item poster and photo grids, music All/Albums/Songs views |
| **Details** | Metadata, overview, cast, similar titles, seasons and episodes, series Next Up, media info, version picker, trailers and extras, lyrics, play and download |
| **Search** | Debounced cross-type search grouped by movies, series, episodes, music, playlists, photos, people and channels; subtitle search |
| **Playback** | Direct play, direct stream and transcode through the relay; resume; playback reporting; audio and subtitle tracks; subtitle delay; chapters; speed; segment skip; bounded mid-stream recovery |
| **Player** | Transport bar, 10-second seek, frame step, chapter step and menu, volume and mute, speed, A-B loop, picture-in-picture, fullscreen; previous/next; Up Next card and auto-play; track menus; media info (`I`); equalizer and loudness filters; HDR-to-SDR tone mapping; refresh-rate matching on Windows; open a stream address |
| **Music** | Album and playlist browsing, queue with shuffle and repeat, queue restored after restart |
| **Live TV** | Channel list, 24-hour guide, tune through the relay, record the current program, recordings and scheduled recordings |
| **Offline** | Download manager with pause, resume, range fallback, checksum verification and external subtitles |
| **Extras** | Item menu (play, mark watched, favorite, download, add to playlist or queue), playlists, collections, favorites, photo slideshow, admin-lite dashboard, parental PIN |
| **Administration** | Edit metadata, Identify, Edit images, Add to collection, Refresh metadata and confirmed Delete for administrators |
| **Settings** | General, playback, audio, subtitles (including bold), video, input, server, About; versioned TOML per profile |
| **Input** | Rebindable keyboard map, gamepad menus and player control, full focus navigation |
| **Interface** | Ambient backdrops with poster fallback, optional local backdrop, reduced-motion support, Standard/Large/TV sizes, narrow-window More menu |

## Install

Download the installer for your platform from the
[releases page](https://github.com/Archerkattri/jellyfin-kattri/releases). The current
prerelease is **v0.4.0**.

| Platform | Download | Notes |
| --- | --- | --- |
| Windows x64 | `JellyfinKattri-0.4.0-x64.msi` or `-x64-portable.zip` | libmpv is bundled; the portable zip runs `jellyfin-kattri.exe` |
| Windows ARM64 | `JellyfinKattri-0.4.0-arm64.msi` or `-arm64-portable.zip` | libmpv is bundled |
| macOS (universal) | `kattri-0.4.0-macos-universal.dmg` | libmpv is bundled; ad-hoc signed |
| Linux x64 | `kattri_0.4.0-1_amd64.deb`, `kattri-0.4.0-1.x86_64.rpm`, `kattri-0.4.0-linux-x64.AppImage`, `kattri-0.4.0-linux-x64.flatpak` | deb/rpm use the distro libmpv; AppImage and Flatpak bundle it |

Every download has a matching `.sha256` file.

```sh
# Debian / Ubuntu (needs libmpv2 or libmpv1 installed)
sudo apt install ./kattri_0.4.0-1_amd64.deb

# Fedora (needs libmpv)
sudo dnf install ./kattri-0.4.0-1.x86_64.rpm

# AppImage, no install needed
chmod +x kattri-0.4.0-linux-x64.AppImage
./kattri-0.4.0-linux-x64.AppImage

# Flatpak
flatpak install --user --bundle kattri-0.4.0-linux-x64.flatpak
```

Windows SmartScreen and macOS Gatekeeper will warn because the packages are not
code-signed. The macOS app is ad-hoc signed and not notarized. This is a development
prerelease, not a 1.0 release.

### First run

1. Enter your server address. Plain HTTP is accepted for loopback and private-network
   addresses; public servers must use HTTPS unless you explicitly allow otherwise.
2. Sign in with a password or Quick Connect. "Stay signed in" keeps the access token
   in the OS credential store.
3. Use `--profile <name>` to keep separate settings and queues for another profile.

Settings, profiles and logs live in the platform config directory; the About screen
shows the exact log folder. `KATTRI_LOG` overrides the log level for one run. Logs
redact credentials and server addresses and are never uploaded.

## Keyboard and gamepad

Every action can be rebound in Settings → Input. A gamepad can drive menus and the
player.

| Action | Keys |
| --- | --- |
| Navigate | Arrow keys or `W` `A` `S` `D` |
| Confirm / back | `Enter` · `Esc`, `Backspace` |
| Home / search / settings | `H` · `Ctrl+F` · `M` |
| Play, pause, stop | `Space` or `P` · `Ctrl+P` · `X` |
| Seek and step | `PgUp` / `PgDn` · `Home` / `End` |
| Audio, subtitles, mute | `E` / `Ctrl+A` · `L` / `Ctrl+L`, `C` · `N` |
| Volume and speed | `=` / `-` · `Ctrl+Up` / `Ctrl+Down` |
| Subtitle delay | `Alt+S` / `Alt+Shift+S`, reset with `Alt+R` |
| Fullscreen, screenshot, close | `F11` · `Ctrl+S` · `Ctrl+Q` |

## Security and privacy

- Access tokens are zeroized in memory and stored only in the OS credential store,
  bound to the profile, server and exact base address.
- The playback relay listens on loopback only, uses a random capability, enforces the
  upstream origin, refuses redirects and copies only allow-listed response headers.
- JSON, image, manifest and stream inputs are bounded; logs and error messages are
  redacted.
- Remote plain HTTP requires an explicit opt-in. No telemetry is collected.

## Status and limitations

Kattri is an early development build, not a production release. The build and test
workflows are green on Windows x64/ARM64, macOS and Linux x64/ARM64, and the default
workspace suite runs more than 2,800 tests against local fixtures and an in-process
mock Jellyfin server.

Validated so far:

- Real-server browsing against Jellyfin 12.x: sign-in, Home rows, libraries, item
  details, search, people, similar items and PlaybackInfo negotiation.
- Stream-only playback through the relay on Windows x64: a 10-minute DirectPlay run
  and a 30-second Transcode run with software/null outputs.
- A Windows RTX 4060 run using NVDEC with an HDR10 source tone-mapped to SDR.
- Packaging: release artifacts for every platform. Clean-runner install, launch and
  normal close pass for the macOS DMG and all four Linux formats.

Not yet validated: visible and audible playback on macOS and Linux, GPU decode across
hardware, HDR passthrough, long HLS sessions, SyncPlay with two clients, physical
controllers, and screen-reader walks on each platform. The Windows MSI clean-runner
check also fails on GitHub's GPU-less runners (the window never gains a title there);
the installer and uninstaller themselves complete.

Known functional gaps compared with the official clients: no custom CA or client
certificates yet, no automatic updates in this release, no shuffle-from-menu or
multi-select, no theme media or lyrics synced to playback, and no SyncPlay inside the
player. See `docs/BLOCKERS.md` in a source checkout for the tracked list.

## Build from source

Install Rust through rustup; the toolchain is pinned in `rust-toolchain.toml` and
dependencies in `Cargo.lock`.

```sh
cargo build --workspace --locked
cargo run --locked -p egui-gl-host-proof --bin jellyfin-kattri
```

The executable is `jellyfin-kattri`. libmpv is loaded at run time, so it is not needed
to build.

```sh
# Linux: gamepad input headers, X11 keyboard library and libmpv
sudo apt-get install pkg-config libudev-dev libxkbcommon-x11-0 libmpv2

# macOS: a libmpv build; the packaged app carries its own
brew install mpv

# Windows: place libmpv-2.dll beside jellyfin-kattri.exe or on PATH
```

## Development checks

The default suite needs no server, libmpv, display or stored credentials.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The full gate also runs credential-helper, live-runner and release-target checks:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check.ps1
```

Live-server tests are opt-in and ignored by default; point them at a test server with
`JELLYFIN_TEST_URL`, `JELLYFIN_TEST_USER` and `JELLYFIN_TEST_PASSWORD`. Never commit
credentials, tokens or live output.

## License

GPL-3.0-only. See `LICENSE` and `NOTICE`. Bundled native libraries and codecs keep
their own licenses; the libmpv packaging notice is under
`packaging/libmpv/NOTICE.txt`. Jellyfin Kattri is an independent client and is not
affiliated with the Jellyfin project.
