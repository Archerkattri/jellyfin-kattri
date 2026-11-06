# Changelog

All notable changes to Jellyfin Kattri are recorded here. The project follows
[Semantic Versioning](https://semver.org/). Dates are release dates on GitHub.
Everything before 0.7.0 was published as a prerelease.

## 0.7.0 - 2026-11-05

### Added

- Library tabs for Movies and Shows: Suggestions, and for Shows also Upcoming and
  Episodes, matching Jellyfin Web's library navigation. Suggestions and Upcoming are
  one bounded page; Episodes pages newest first.
- Download all on album pages, through the same bounded batch queue as playlists and
  multi-select.
- Per-title subtitle style overrides, so one movie or show can keep its own font,
  size, colors, outline, background and placement.
- Subtitle burn-in for server transcodes (Settings > Subtitles). Direct play and
  direct stream are not affected.
- Idle screensaver, off by default, with a configurable delay.
- Theme media: an item's server theme video or theme song can be played from its
  detail page.
- Settings keys for the features above, all additive and backward compatible.
- `CHANGELOG.md` and a source security review for 0.7.0.

### Changed

- Subtitle style changes now apply to the running player instead of only the next
  file.
- Mutual-TLS identity buffers are zeroized on drop.
- Download requests bypass system proxies, like the credentialed API client.
- Wasmtime trap diagnostics are redacted and bounded like guest error strings.
- Fullscreen playback hides the mouse cursor after a short idle period.

### Fixed

- Plugin guest diagnostics could quote unredacted text.

## 0.6.0 - 2026-11-02

### Added

- LAN server discovery and `jellyfin://` links on Windows, Linux and macOS.
- Administrator user listing, creation, rename and access-policy editing in the app.
- SyncPlay shared queues and native player status.
- Origin-bound custom request headers for reverse proxies.
- Optional combined Continue Watching/Next Up shelf, per-series hiding from those
  shelves, and custom accent, background and surface colors.
- Signed update manifest fetching with bounded transport.
- Batched download worker that follows the server's download permission policy.
- Playlist Download all and optional Windows-wide playback shortcuts.
- Multi-select with bulk watched, favorite, queue and download actions; `Ctrl+1`
  1:1 window sizing and `Ctrl+2` direct 2x speed.

### Changed

- Wasmtime security advisories patched.
- Plain HTTP to `.local` names refused; only `.localhost` and literal loopback or
  private addresses are trusted.
- Playback redraw, controls and group-speed hardening.

## 0.5.1 - 2026-10-30

### Added

- SF Pro Display typeface, with Inter as the open fallback.
- Automated click-through QA (`scripts/qa_clickthrough.ps1`): every top-level page,
  every settings category, item details, the item menu, search, playback and player
  controls, with a screenshot and report per run.

## 0.5.0 - 2026-10-30

### Added

- Private CA bundles and mutual-TLS client certificates in Settings > Server.
- Window opens maximized instead of fullscreen.
- Redesigned player bar with a loop icon for A-B.

### Changed

- Bundled Inter typeface as the open fallback for the Apple TV look.

## 0.4.1 - 2026-10-30

### Added

- Red Kattri mark and wordmark in the top bar.
- Optional resume rewind before the saved playback position.

## 0.4.0 - 2026-10-30

### Added

- Sleep and display inhibition during playback (Windows first).
- Bold subtitles setting.
- Product README with screenshots, install steps and an honest status section.

## 0.3.1 - 2026-10-30

### Fixed

- Parental controls fail closed for unrated media and cover every media list.
- Expired sessions, paging, card-menu toggles, artwork, episode stills and
  neighbours recover after transient failures.
- Download progress is persisted per job after every synced chunk, so an interrupted
  transfer resumes instead of restarting, and a legal short range response continues
  with the next range.
- Provider thumbnails require public HTTPS by default; the credentialed API client
  bypasses system proxies; HLS rewriting is classified by URL and content type.
- Resize reuses the video texture instead of leaking one per frame.

## 0.2.0 - 2026-10-28

### Added

- Ambient movie and show artwork on Home and library screens with a 300 ms,
  reduced-motion-aware crossfade.
- Server artwork, a local custom JPEG/PNG backdrop, or no backdrop.
- Real-server Home, Movies and item-detail screenshots in the README.
