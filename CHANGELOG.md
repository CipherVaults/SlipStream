# Changelog

All notable changes to **SlipStream** are documented in this file following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/).

## [1.3.0] - 2026-11-26

### Added

- **Standalone Floating Download Windows (`apps/desktop`):** Added IDM-style lightweight windows outside the main SlipStream window (working even while hibernated to tray), each toggleable in **Settings → Floating Download Windows** (`show_download_dialog`, `show_progress_dialog`, `show_complete_dialog`, all enabled by default):
  - **Duplicate Download Link dialog:** Detects matching URL/path downloads and lets you choose numbered naming, atomic **overwrite existing**, or resume / open completion for the existing transfer.
  - **Download File Info dialog:** Stages browser-intercepted downloads in volatile-scrubbed RAM (`pending_downloads`, bounded to `32` entries / `15 min` TTL) and shows URL, auto-matched category, **Save As** with native Win32 folder picker (`Browse`), **Remember this path for category**, **Description**, file size, **Download Later** (`start_paused`), and **Start Download**.
  - **Download Progress dialog (`{pct}% {filename}`):** Live telemetry bar, server line (`host · protocol · connect_rtt_ms`), expandable **Show details >>** per-connection chunk map & `N. | Downloaded | Info` table, live **Speed Limiter** (`KB/s` / `MB/s`), and **Options on completion** (`Show complete dialog`, `Auto close progress`, `Open containing folder`).
  - **Download Complete dialog:** Displays filename, verified SHA-256 badge, saved path, and **Open**, **Open folder**, and **Close** controls.
- **Download Categories & Custom Subfolders (`crates/protocol`, `apps/desktop`):** Added pre-seeded categories (`General`, `Programs`, `Compressed`, `Video`, `Music`, `Documents`) defaulting to subfolders under `default_download_dir` (`General` uses the root folder), plus a **+** modal in Download File Info to create custom categories with extension lists.

### Fixed

- **HLS `.m3u8` & `Add Download` `HTTP 403 Forbidden` compatibility (`crates/protocol`, `crates/media`, `apps/desktop`, `extensions/browser`):**
  - Forwarded validated `Referer`, `Origin`, `User-Agent`, and `Accept-Language` headers from browser `open_desktop_window` (`picker`/`playlist`) and `add_media_download` actions into `OpenPickerEvent` -> `AddDialog` (`probeMedia`, `addHttpDownload`, and `addMediaDownload`), clearing seeded headers automatically if the user changes the URL to a different origin.
  - Added a default Chrome `User-Agent` header when missing in `yt-dlp` probes and downloads, allowlisted `--force-ipv4` in `BuildYtDlpArgs`, and added a single automatic `--force-ipv4` retry on recognized HTTP 403 failures during `yt-dlp` probe and clean-stage single-video downloads.

### Security & Hardening

- **Audit hardening (`run-3`, Codex CLI cross-model audit on `v1.3.0`):**
  - Enforced per-window label/ID command authorization across all 39 Tauri commands (`commands.rs`), replaced `"core:default"` in `capabilities/dialogs.json` with exact minimal permissions (no `core:event:allow-emit`), and restricted WebView navigation & popups cleanly in `shell.rs`.
  - Cleared `overwrite_existing` at native bridge ingress, added durable overwrite record ownership invalidation in `crates/engine` (`output_superseded` clearing old SHA-256 verification badges and file actions), added a COM `Drop` unwind guard in Win32 `pick_download_folder`, made `confirm_pending_download` single-flight with volatile credential scrubbing on failure, preserved Windows drive-root paths (`C:\`) in `splitPath`, and added live `slipstream://settings-updated` synchronization so main window saves never revert remembered category paths.

## [1.2.2] - 2026-11-26

### Fixed

- **Direct download reliability across mirrors and CDNs (`crates/engine`):**
  - Enabled `.http1_title_case_headers()` and default `Accept-Language` so WAF/Cloudflare endpoints no longer reject HTTP/1.1 requests for lowercase headers.
  - Added an **IPv4-only fallback** and independent **403 header fallback ladder** (`full` → `strip Cookies` → `strip Referer/Origin`), retaining the original `403` cleanly when IPv6-only hosts have no IPv4 route.
  - Added **adaptive concurrency backdown** on `HTTP 503` / `429` / `403` (staggered worker startup, halving active connections down to `1`, respecting bounded `Retry-After` delays including on `bytes=0-0` probes, retiring excess workers back to `Pending`, and falling back to full `GET` cleanly without erasing partial bytes before replacement succeeds).
  - Reused the already-open plain `GET` response from `probe()` when ranges are unsupported so single-use signed URLs are never double-spent.
  - Added transparent single-stream decompression (`gzip`, `deflate`, `brotli`) for non-ranged full `GET` responses with strict underlying HTTP stream EOF verification.
  - Upgraded same-origin redirect `Set-Cookie` handling to enforce RFC 6265 `Path`, `Domain`, `Secure`, `Max-Age`, and `Expires` attributes (removing deleted/expired cookies instead of forwarding them).
- **Universal browser download interception (`extensions/browser`):**
  - Added Chromium `chrome.downloads.onDeterminingFilename` interception (with a 3500 ms `suggest()` deadline, late-reply cancellation guard, and verified `cancel` → `erase`), while preserving `downloads.onCreated` fallback on Firefox.
  - Expanded default extensions (`.exe`, `.msi`, `.msix`, `.dmg`, `.pkg`, `.deb`, `.rpm`, `.apk`, archives, disk images, media, `.pdf`, `.torrent`) and added extensionless MIME inference that still honors explicit filename extension allowlists.
  - Added `"cookies"` manifest permission with non-private, non-downgrade, SameSite-aware target cookie retrieval (never reading incognito or `firefox-private-*` stores) and automatic `NativeClient.send()` service-worker wake reconnect.
- **Floating video pill compatibility across sites (`extensions/browser`, `crates/protocol`, `apps/desktop/src-tauri`):**
  - Stopped dropping `HTTP 206 Partial Content` direct video streams; parsed `Content-Range` totals (`>= 300 KB`, keeping `/*` totals unknown instead of mistaking slice `Content-Length` for total size) and surfaced instant **Download Video** direct HTTP actions.
  - Allowed public cross-origin `<video src>` / `<source src>` targets from trusted frames, stopped wrong-player stream substitution when explicit player URLs differ, and preserved direct video actions alongside probed formats.
  - Forwarded `headers` (`Referer`, `Origin`, `User-Agent`, `Accept-Language`) in `ProbeMedia` (with strict cross-origin origin-only `Referer` scoping) and added pre-launch DNS SSRF verification (`ensure_public_media_host`) before spawning `yt-dlp`.
  - Fixed `selectPillFormatId` to choose the best format `<= expectedHeight` (never exceeding the requested height cap) and only append `+bestaudio` when an audio-only format exists.

## [1.2.1] - 2026-11-26

### Security & Hardening

- **Credential scrubbing & redirect isolation (`crates/engine`):** Scrubbed `headers`, `referrer`, and URL credentials/query/fragment on `TransferStatus::Completed` (as well as `Canceled`), added startup migration scrubbing for legacy terminal SQLite rows and dismissal cleanup, and restricted cross-origin redirects to an explicit safe header allowlist with origin-only `Referer` preservation.
- **Media pipeline & process-tree confinement (`crates/media`, `apps/desktop/src-tauri`):** Rejected credential-bearing URLs in `validate_media_url`, cleared proxy environment variables (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`) on spawned sidecars, added `-protocol_whitelist file` to `ffprobe` stream counting, bounded concurrent media probes (`Semaphore(4)`) and single-video jobs, enforced Win32 Job Objects (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) to terminate `ffmpeg` grandchildren on cancellation, and moved staging cleanup retries off async workers.
- **BitTorrent & MotW hardening (`crates/torrent`, `daemon.rs`):** Filtered `magnet:` `tr` and `.torrent` `announce`/`announce-list` entries to public `http`/`https`/`udp` endpoints, and updated `scan_torrent_completions` so Mark-of-the-Web only marks paths after `Zone.Identifier` succeeds while stopping infinite retries on non-NTFS volumes.
- **IPC, WebView & CI/Release supply-chain hardening:** Added a 512-URL cap and upfront validation in `GrabBatchUrls`, hardened `tools.rs` runtime `yt-dlp` client (`.use_rustls_tls().no_proxy()`), tightened Tauri CSP (`object-src 'none'; base-uri 'none'; frame-ancestors 'none'`), added `referrerPolicy="no-referrer"` + private-host checks to remote thumbnails, pinned all GitHub Actions `uses:` entries in CI/Release workflows to commit SHAs, switched `release.yml` from `npx --yes` to lockfile-pinned `@tauri-apps/cli@2.12.4`, and enabled `--frozen-lockfile` + least-privilege `permissions` in `ci.yml`.
- **Browser extension hardening (`extensions/browser`):** Unified `pillOpen` and `mediaAction` guards (`<= 8192` URL cap, incognito no-probe isolation, max-3 concurrent probe limit), allowed public cross-origin embedded players and `isKnownVideoPage` targets in `pillOpen`, rejected 6to4 (`2002::/16`) and Teredo (`2001:0:`/`2001::`) + non-public HLS variants, validated `NativeClient.probe()` responses before caching, capped `grabLinks` at 512 URLs, and fixed Chrome spanning-mode incognito refresh tab fallback.

### Fixed

- **Floating pill live format rendering & units (`extensions/browser`):** Fixed `updateFormats()` deferral in `pill.ts` so probed video formats and **Download Playlist…** render immediately while the pill menu is open, added `formatBytes()` so format sizes display `KB`/`MB` instead of `MB/s`, guarded fullscreen `showPopover()` calls, and removed the dead `video-seen` message wakeup.
- **Playlist & file collision reliability (`crates/engine`, `apps/desktop/src-tauri`):** Updated `ensure_playlist_subfolder` to deduplicate existing directory names (`Playlist (2)`, …), auto-numbered colliding HTTP target filenames (`file (1).ext`) on publish instead of failing at 100%, rejected duplicate `selected_entries` indices, rolled back `enqueue_playlist` state on start errors, truncated >256-byte `GetStatus` error/notice fields instead of dropping them, skipped corrupt SQLite transfer rows on startup, added an explicit `mark_ui_ready` Tauri command, and made `reveal_playlist_folder` async.
- **Desktop UI playlist state & dialogs (`apps/desktop/src`):** Fixed failed/canceled playlist classification so they no longer appear under **Completed** or skew sidebar badge counts, corrected whole-playlist completion summaries, fixed `AddDialog` re-probe selection, filtered Select All/Clear/Invert, `Playlist Items` sync, sanitized `playlist_folder_name` submission, `SettingsDialog` browser-button busy state, and `TransferDialog` header control-character validation.

## [1.2.0] - 2026-11-25

### Added

- **Faster media probing.** Repeat probes can return immediately from `ProbeCache`, which holds up to 128 results for five minutes and coalesces concurrent requests by normalized URL, playlist mode, and canonical headers. The probe path uses cached toolchain discovery and needs only `yt-dlp`, skipping unpinned executable hashing and `ffmpeg` and `ffprobe` version checks. Full toolchain discovery runs concurrently, and downloads can reuse a matching probe via `pre_probe`. The isolated `<data_dir>/yt-dlp-cache` rejects expandable paths and descendant reparse points.
- **Selective playlist downloads.** Structured `PlaylistEntry` results (`playlist_items`) power a searchable checklist in **Add Download**, with **Select All**, **Clear**, **Invert**, an item range field, and a live destination folder preview. The `PlaylistCoordinator` downloads selected videos one at a time into a sanitized `<save_dir>/<Playlist Title>/` folder, continues after individual failures, and preserves explicit selections beyond the 512-item preview. Folder creation guards against reparse points and Windows reserved device names, and cross-origin child URLs drop the parent `Referer` and `Origin`. Collapsible playlist groups in **Transfers** show live counts and offer **Pause All**, **Resume All**, and **Open Folder**. The daemon reports `playlist_paused` across tray hibernation.
- **Upgraded browser media pill (Chrome and Firefox v1.2.0).** A capture-phase guard in `early.js` runs at `document_start` with `all_frames: true` and `match_about_blank: true` to protect pill clicks from page handlers. Integer `left` and `top` coordinates, a 2 px movement threshold, debounced hiding, and interaction locking reduce flicker. Drag the pill to snap it to a player edge, dismiss it in one click, or enable it in fullscreen from the popup. Per-site positions are saved outside incognito windows.
- **More player and stream detection.** The extension detects videos in open shadow roots, covered iframe players, and social feed video post links via `resolveVideoPostUrl`. A trusted pill interaction can read an HLS master playlist (`.m3u8`) or request native probe results for the dynamic quality menu. Supported playlist pages offer **Download Playlist…**. Stream detection filters out `206` or `Content-Range` responses, media chunks under 300 KB, and `.ts` and `.m4s` segments.

### Fixed

- **HTTP 403 failures on direct CDN and video-host downloads.** Requests supply a browser-compatible `User-Agent` and `Accept: */*` when omitted and preserve caller overrides. A `Range: bytes=0-0` probe rejected with `400`, `403`, `405`, or `416` falls back once to a plain `GET`. The extension can forward `Referer`, `Origin`, and `User-Agent` for same-tab cross-origin CDN requests. Cookies stay same-origin, and HTTPS-to-HTTP downgrades strip `Referer` and `Origin`.
- **Native IPC and browser isolation.** `MediaProbed` and `Status` responses enforce frame budgets below 1 MiB, with progressive compaction of terminal transfer history in `Status`. Subframes use their own stream observations, and Chromium incognito popups cannot write extension settings.

## [1.1.1] - 2026-11-23

### Added

- Whole-playlist download support: auto-detects pure playlist links, adds a **Single Video / Whole Playlist** switcher for watch, `youtu.be`, Shorts, Live, and embed URLs containing a `list` parameter, provides playlist quality presets (`Best Available` through `360p`), optional playlist item ranges, flat-playlist subtitle language selection, live probed titles before completion, smooth video/audio playlist progress, and partial-completion notices in Inspector when unavailable items or subtitles are skipped.

### Fixed

- Format picker deduplication & ranking: collapses duplicate HLS (`m3u8`) video formats whenever equivalent direct HTTP/HTTPS streams exist (while preserving muxed HLS streams alongside video-only options and keeping HLS-only sources intact), removes redundant `-drc` and unknown-codec HLS audio duplicates, preserves distinct audio languages with original/default tracks ranked first, and sorts direct known-size streams ahead of HLS.
- Lossless Remux & `yt-dlp` postprocessing reliability: pairs video-only selections with container-compatible original audio (`${id}+${audio}/${id}+bestaudio/${id}`), converts validated canonical `\\?\` Windows drive paths before calling `yt-dlp` / `ffmpeg` / `ffprobe`, passes `--merge-output-format`, detects actual MPEG-TS demuxers disguised under `.mp4`/`.mkv`, preserves convertible embedded text subtitles across `.mkv`/`.mov`/`.m4v`/`.mp4`, skips redundant second FFmpeg passes on clean Matroska outputs, and retries failed implicit `MP4`/`WebM` remuxes as `MKV`.
- Sidebar version badge now reads the application package version (`v1.1.1`) instead of a hardcoded `v0.1` label.

## [1.1.0]

### Added

- Automatic startup and manual-only update checks, plus live progress for signed app updates.
- Bundled yt-dlp, FFmpeg 9.1 Essentials, and ffprobe in the Windows installer and portable ZIP. Settings displays each tool's version and source and offers a one-click, SHA-256-verified yt-dlp update.
- A browser picker for Chrome, Edge, Brave, and Firefox that opens extension setup and locates the bundled extension. Chromium browsers share the fixed extension ID `nikdbkenglojniombimjdkneanmponcp`.

### Changed

- Locked down the main WebView2 window at the engine and application levels. Browser shortcuts, context menus, status UI, and zoom are suppressed. Release builds also disable DevTools. Ctrl+F selects download search, and editable fields and diagnostic text remain selectable.
- Release builds stage verified media tools, the native messaging host, both browser extension trees, and third-party notices. The portable ZIP launches as `SlipStream.exe` and shares the installer's `bin/` and `extensions/` layout.
- Release packaging validates the tag and app version, generates the updater manifest with the matching installer signature, and publishes asset checksums. The README documents v1.1.0 setup from the public repository.

## [1.0.0] - 2026-11-21

### Added
- **Segmented HTTP/1.1 & HTTP/2 Download Engine (`crates/engine`):** Dynamic 1–32 connection HTTP Range (`206`) scheduler with mid-flight segment splitting above 512 KiB, SQLite WAL crash-safe cursor recovery, strong `ETag`/`If-Range`/`Last-Modified` validation, `CheckedDns` SSRF protection across 10 redirect hops with cross-origin `Cookie`/`Authorization` stripping, wildcard batch expansion (up to 10,000 URLs), token-bucket speed limiters, queue scheduling, SHA-256/MD5 checksum verification, **Refresh Download Address**, Windows NTFS `:Zone.Identifier` (`ZoneId=3`) Mark-of-the-Web, and atomic `MoveFileExW` commit.
- **Hardened Media & Format Pipeline (`crates/media`):** Structured `yt-dlp` JSON probing, discrete zero-shell-interpolation argv builders for `yt-dlp` and `FFmpeg`, codec/resolution/FPS/container pickers (`4K 2160p60`, `AV1`, `VP9`, `H.264`, `MP4`, `MKV`, `WebM`), lossless remuxing, audio extraction (`MP3 320k`, `M4A`, `Opus`, `FLAC`, `WAV`), subtitle muxing, staged hard-link publishing, and NTFS `:Zone.Identifier` tagging.
- **Native BitTorrent & Magnet Engine (`crates/torrent`):** Embedded `librqbit 8.1.1` engine with depth-64 bencode decoding, strict path/symlink/DOS-device rejection, `ConfinedTorrentStorage`, selective file downloads, 16-segment piece-bitmap projection, speed caps, and NTFS `:Zone.Identifier` tagging on completed files.
- **SID-Scoped Native Messaging & Cross-Browser MV3 Extension (`crates/native-host` + `extensions/browser`):** Chrome, Edge, Brave, and Firefox MV3 extension communicating over `slipstream-native-host` (`4`-byte LE `1 MiB` frames) -> Windows Named Pipe (`D:(A;;GA;;;<SID>)`, `PIPE_REJECT_REMOTE_CLIENTS`, `D:P` 256-bit `session.token`), closed Shadow-DOM floating media pill, same-origin/incognito/`cookieStoreId` isolated header forwarding, redacted `GetStatus` responses for the popup, and context-menu expired-link refresh.
- **Mac-Craft Windows 10/11 Desktop UI & Auto-Updater (`apps/desktop`):** Tauri v2 + React 19 + Tailwind CSS v4 interface with live **Soft Graphite Dark** ↔ **Porcelain Light** theme switching (`System` follow + Blue/Monochrome accents), 16-segment progress visualizer, `T01–T16` thread map, command palette (`Ctrl+K`), Tray-Hibernate mode, Windows Defender `MpCmdRun.exe` post-download scan queue, per-user NSIS installer bundling `slipstream-native-host.exe`, and Ed25519-signed `tauri-plugin-updater` auto-updates (`About & Updates` in Settings).
- **Security Audit & Safety Evaluation Gate (`tests/safety-eval`):** Preregistered (`slipstream-safety-v1`) deterministic suite covering 1,200 single-input boundary cases and 108 orthogonal stateful HTTP chaos chains, alongside a full 6-phase `/security-audit` (`0` confirmed vulnerabilities; proactive hardening applied for browser `GetStatus` redaction, HTTP cancel credential scrubbing, BitTorrent NTFS MotW, and trailing-dot `localhost.` SSRF rejection).
