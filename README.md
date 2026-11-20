# SlipStream

High-speed Windows 10/11 download manager built with **Tauri v2 (Rust + React 19 / TypeScript / Tailwind CSS v4)** and a **Manifest V3 Browser Extension** (Chrome, Edge, Brave, Firefox).

## Core Capabilities

- **Dynamic Segmented HTTP Engine (`crates/engine`)**: Up to 32 concurrent HTTP Range (`206`) connections with mid-flight segment splitting, SQLite WAL crash-safe resume, `If-Range`/`ETag` validation, token-bucket speed limiters, wildcard batch expansion, queues/scheduler, SHA-256/MD5 verification, **Refresh Download Address**, and Windows Mark-of-the-Web (`Zone.Identifier`) preservation.
- **Rich Media & Format Pipeline (`crates/media`)**: Managed `yt-dlp` + `FFmpeg` sidecars with structured resolution/FPS/codec/container selection (`4K 2160p60`, `AV1`, `VP9`, `H.264`, `MP4`, `MKV`, `WebM`), lossless remuxing, audio extraction (`MP3 320k`, `M4A`, `Opus`, `FLAC`, `WAV`), subtitle embedding, and playlist support — zero shell interpolation.
- **Native BitTorrent & Magnet (`crates/torrent`)**: Selective file downloads, DHT/PEX, piece-to-16-segment visualization, ratio & speed controls, and strict path-traversal rejection.
- **Secure Browser Hook (`crates/native-host` + `extensions/browser`)**: Native Messaging host communicating over a Windows Named Pipe with a current-user SID DACL and per-session token (no exposed localhost HTTP port), paired with a Shadow-DOM floating media pill and live toolbar popup.
- **Mac-Craft Windows 11 UI (`apps/desktop`)**: Instant switching between **Soft Graphite Dark** and **Porcelain Light** themes (plus system follow and optional Graphite monochrome accent), Tray-Hibernate mode (~10–15 MB idle RAM), 20 Hz batched telemetry, and live 16-segment thread visualizer.

## Repository Layout

```text
SlipStream/
├── apps/desktop/          # Tauri v2 shell + React 19 / Tailwind v4 UI
├── crates/
│   ├── protocol/          # Shared IPC schemas + strict path/URL/SSRF security primitives
│   ├── engine/            # Dynamic segmented HTTP downloader, SQLite WAL, scheduler, MotW
│   ├── media/             # Hardened yt-dlp + FFmpeg resolver, format picker, muxer
│   ├── torrent/           # Native Rust BitTorrent / Magnet engine
│   └── native-host/       # Browser Native Messaging host + SID-ACL Named Pipe bridge
├── extensions/browser/    # MV3 Chromium + Firefox extension
├── packages/ipc-types/    # Shared TypeScript protocol mirrors
├── docs/                  # Architecture & Design System specs
└── tests/safety-eval/     # Part II safety & resilience evaluation harness
```
