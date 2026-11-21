# SlipStream

**A fast, quiet, security-hardened Windows 10/11 download manager built with Rust, Tauri v2, React 19, and a Manifest V3 browser extension.**

SlipStream combines a multi-segment HTTP download engine, a managed `yt-dlp` + `FFmpeg` media format/audio pipeline, native BitTorrent/Magnet support, and an isolated browser extension inside a dual-theme (**Soft Graphite Dark** ↔ **Porcelain Light**) native desktop workspace.

---

## About SlipStream

Traditional Windows download managers often expose an unauthenticated localhost HTTP server to intercept browser downloads—allowing arbitrary websites or local scripts to hit control APIs. SlipStream replaces that pattern with **Browser Native Messaging → current-user SID-ACL Windows Named Pipe (`PIPE_REJECT_REMOTE_CLIENTS` + 256-bit `D:P` `session.token`)**, narrowed browser RPC privileges, explicit path/junction confinement, and clean visual restraint.

- **Target OS:** Windows 10 / Windows 11 (`x86_64`)
- **Organization:** [CipherVaults](https://github.com/CipherVaults/SlipStream)
- **Current Release:** `v1.0.0` (MIT License)

---

## What We Offer (v1.0.0 Features)

### 1. Segmented HTTP/1.1 & HTTP/2 Engine (`crates/engine`)
- **Dynamic 1–32 Connections & Mid-Flight Splitting:** Probes Range support (`HEAD` / `GET bytes=0-0`) and splits active workers automatically whenever remaining chunks exceed `512 KiB`.
- **Crash-Safe SQLite WAL Resume:** Persists segment cursors, `If-Range`, strong `ETag`, and `Last-Modified` validators in SQLite WAL, with atomic `.slipstream-part` → final file commit via `MoveFileExW`.
- **Expired URL Recovery (`Refresh Download Address`):** Swap an expired signed URL (`403` / `410`) in-place via context menu or Transfer Details while keeping already-downloaded bytes intact.
- **Wildcard Batch Expansion & Queues:** Expand padded/unpadded/stepped numeric or alphabetic wildcard patterns up to `10,000` URLs with queue concurrency caps and token-bucket global/per-transfer speed limits.
- **Built-In Integrity & SSRF Hardening:** SHA-256 / MD5 verification, `CheckedDns` IP filtering across up to `10` redirect hops, cross-origin `Cookie` / `Authorization` stripping on redirects, credential scrubbing on HTTP cancel, and Windows NTFS `:Zone.Identifier` (`ZoneId=3`) Mark-of-the-Web preservation.

### 2. Hardened Media & Format Pipeline (`crates/media`)
- **Zero Shell Interpolation:** Invokes `yt-dlp.exe`, `ffmpeg.exe`, and `ffprobe.exe` strictly with discrete argument arrays, allowlisted flags, `--`, forced local FFmpeg demuxers, `YTDLP_NO_PLUGINS=1`, and junction-free canonical `.exe` checks.
- **Format, Container & Audio Selection:** Structured `4K 2160p60` / codec (`AV1`, `VP9`, `H.264`) / container (`MP4`, `MKV`, `WebM`) selection, lossless stream remuxing, audio extraction (`MP3 320k`, `M4A`, `Opus`, `FLAC`, `WAV`) with optional EBU R128 loudness normalization, subtitle language embedding (`.srt`, `.vtt`, `.ass`, `.ssa`), and playlist item selection.
- **Isolated Staging & MotW:** Downloads to scoped UUID staging directories, tags outputs with `:Zone.Identifier`, and publishes atomically via non-overwriting `fs::hard_link`.

### 3. Native BitTorrent & Magnet (`crates/torrent`)
- **Embedded Rust Engine (`librqbit`):** Supports `.torrent` files, raw torrent bytes, and `magnet:` URIs without external torrent daemons.
- **Manifest & Confinement Guards:** Depth-64 bencode decoder with duplicate-key and symlink (`attr 'l'` / `symlink path`) rejection, Windows reserved DOS device and BiDi control filtering, `ConfinedTorrentStorage`, selective file priorities, 16-segment piece-bitmap projection, and NTFS `:Zone.Identifier` tagging on completed files.

### 4. Browser Integration (`crates/native-host` + `extensions/browser`)
- **Cross-Browser MV3:** Supports **Chrome**, **Edge**, **Brave**, and **Firefox** (`slipstream@ciphervaults.com`) using `4`-byte little-endian `1 MiB` Native Messaging framing and per-user `HKCU` registration.
- **Floating Shadow-DOM Media Pill:** Closed Shadow DOM page overlay for 1-click `4K`, `1080p`, `MP3 320k`, `FLAC`, Format Picker, Mute Site, or Grab All Links—guarded by `event.isTrusted`, top-frame origin verification, and SPA navigation invalidation.
- **Privacy & Context Isolation:** `downloads.onCreated` honors `Alt`-key bypass, file extensions, size thresholds, muted/excluded sites, and **Incognito / Firefox `cookieStoreId` isolation**. Browser `GetStatus` responses redact desktop query tokens and filesystem paths before leaving the daemon.

### 5. Dual-Theme Windows Desktop App & Auto-Updater (`apps/desktop`)
- **Soft Graphite Dark ↔ Porcelain Light:** Live instant theme switching (plus `Follow System` and `Blue` / `Monochrome` accents), 16-segment progress visualizer, `T01–T16` active thread map, throughput sparkline, and `Ctrl+K` / `Ctrl+N` command shortcuts.
- **Tray-Hibernate Mode & OS Security:** Closes the WebView2 surface cleanly to the Windows system tray while active downloads continue at minimal RAM, reveals completed files via `explorer.exe /select`, and optionally queues Windows Defender (`MpCmdRun.exe`) scans.
- **Signed NSIS Auto-Updates:** Distributed as a per-user **NSIS installer** (`currentUser`, bundling `slipstream-native-host.exe`) with **Tauri v2 `tauri-plugin-updater`** and **Ed25519 (`minisign`)** verified updates accessible in **Settings → About & Updates**.

---

## Security & Verification

- **6-Phase `/security-audit`:** Completed 3 hunting waves across 19 coverage units, 3 independent coverage critics, cross-model candidate validation, and fresh record verification (`0` confirmed vulnerabilities; proactive hardening landed for browser `GetStatus` redaction, cancel credential scrubbing, torrent `:Zone.Identifier` tagging, and trailing-dot `localhost.` SSRF normalization).
- **Part II Safety & Resilience Evaluation (`tests/safety-eval`):** Preregistered (`slipstream-safety-v1`) deterministic oracle suite passing **1,200 / 1,200** adversarial + benign single-input boundary cases and **108 / 108** orthogonal 5-factor stateful HTTP chaos chains (`0` broken items, `0` escapes, `0` benign false rejections, strict `bias-audit` Cramér's `V = 0.0`).

---

## Download, Installation & Browser Setup

> Note: SlipStream `v1.0.0` installer and updater assets are published under **GitHub Releases** on `CipherVaults/SlipStream`. While the repository is **Private**, automated in-app updater checks against `github.com/.../releases/latest/download/latest.json` require either public release hosting or authenticated release access.

1. **Windows Desktop App (Recommended — Auto-Updating NSIS Installer):**
   - Download `SlipStream_1.0.0_x64-setup.exe` from the [v1.0.0 Release](https://github.com/CipherVaults/SlipStream/releases/tag/v1.0.0) and run the per-user installer (no UAC elevation required; bundles `slipstream-native-host.exe` in `bin/`).
   - Or download `SlipStream-1.0.0-windows-x64-portable.zip` for portable use (keep `slipstream-native-host.exe` beside `slipstream-desktop.exe`).
2. **Optional Media Sidecars:**
   - Install [`yt-dlp.exe`](https://github.com/yt-dlp/yt-dlp) and [`ffmpeg.exe` / `ffprobe.exe`](https://www.gyan.dev/ffmpeg/builds/) on `PATH`, or configure their exact `.exe` paths under **Settings → Toolchain**.
3. **Browser Extension:**
   - **Chrome / Edge / Brave:** Extract `slipstream-extension-1.0.0-chrome-edge-brave.zip`, load unpacked in Developer Mode, copy the generated Extension ID, open SlipStream **Settings → Browser Integration**, paste the ID, and click **Install Browser Native Hosts**.
   - **Firefox:** Load `slipstream-extension-1.0.0-firefox.zip` (`manifest.json` pinned to `slipstream@ciphervaults.com`) and click **Install Browser Native Hosts** in SlipStream Settings.

### Building from Source (Windows 10/11)

Prerequisites: **Rust (stable)**, **Node.js 22**, **pnpm 9+**, **VS Build Tools (C++ x64)**, **WebView2 Runtime**.

```powershell
pnpm.cmd install
pnpm.cmd -r typecheck
pnpm.cmd -r test
pnpm.cmd -r build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p slipstream-native-host
Push-Location apps/desktop
npx.cmd --yes @tauri-apps/cli@2 build --config src-tauri/tauri.release.conf.json
Pop-Location
```

---

## Roadmap / Planned for v1.1+

Items intentionally visible in configuration or planned beyond `v1.0.0`:
- **HTTP/3 (QUIC) Transport:** Upgrade HTTP engine beyond HTTP/1.1 & HTTP/2 with `reqwest` HTTP/3 support when stabilized.
- **Active Clipboard Watcher & Power Actions:** Connect `monitor_clipboard` listener and OS `PostQueueAction::{Sleep, Shutdown}` execution in the daemon.
- **Sidecar SHA-256 Pinning & Authenticode:** Managed binary hash verification UI/manifest for `yt-dlp.exe`/`ffmpeg.exe` and EV/trusted code-signing for installers.
- **Published Store Extension Listings:** Fixed Web Store / Edge Add-ons / AMO signed extension IDs so Chromium Native Host registration is zero-copy out of the box.

---

## Repository Layout

```text
SlipStream/
├── .github/workflows/     # Windows CI + tag-driven signed NSIS/ZIP Release workflows
├── apps/desktop/          # Tauri v2 shell (NSIS + updater) + React 19 / Tailwind v4 UI
├── crates/
│   ├── protocol/          # Shared IPC schemas + strict path/URL/SSRF security primitives
│   ├── engine/            # Dynamic segmented HTTP downloader, SQLite WAL, scheduler, MotW
│   ├── media/             # Hardened yt-dlp + FFmpeg resolver, format picker, muxer
│   ├── torrent/           # Native Rust BitTorrent / Magnet engine (librqbit)
│   └── native-host/       # Browser Native Messaging host + SID-ACL Named Pipe bridge
├── extensions/browser/    # MV3 Chrome / Edge / Brave / Firefox extension
├── packages/ipc-types/    # Shared TypeScript protocol mirrors
├── docs/                  # Architecture & Design System specs
└── tests/safety-eval/     # Part II safety & resilience evaluation harness
```

## License

Released under the [MIT License](Cargo.toml).
