# Changelog

All notable changes to **SlipStream** are documented in this file following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/).

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
