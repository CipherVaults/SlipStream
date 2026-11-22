# SlipStream

**A fast, quiet, security-hardened Windows 10/11 download manager built with Rust, Tauri v2, React 19, and a Manifest V3 browser extension.**

SlipStream combines a multi-segment HTTP download engine, a managed `yt-dlp` + `FFmpeg` media format/audio pipeline, native BitTorrent/Magnet support, and an isolated browser extension inside a dual-theme (**Soft Graphite Dark** ↔ **Porcelain Light**) native desktop workspace.

---

## About SlipStream

Traditional Windows download managers often expose an unauthenticated localhost HTTP server to intercept browser downloads—allowing arbitrary websites or local scripts to hit control APIs. SlipStream replaces that pattern with **Browser Native Messaging → current-user SID-ACL Windows Named Pipe (`PIPE_REJECT_REMOTE_CLIENTS` + 256-bit `D:P` `session.token`)**, narrowed browser RPC privileges, explicit path/junction confinement, and clean visual restraint.

- **Target OS:** Windows 10 / Windows 11 (`x86_64`)
- **Public repository** [CipherVaults/SlipStream](https://github.com/CipherVaults/SlipStream)
- **Release** [v1.1.0](https://github.com/CipherVaults/SlipStream/releases/tag/v1.1.0)

---

## Features

### 1. Segmented HTTP/1.1 & HTTP/2 Engine (`crates/engine`)
- **Dynamic 1–32 Connections & Mid-Flight Splitting:** Probes Range support (`HEAD` / `GET bytes=0-0`) and splits active workers automatically whenever remaining chunks exceed `512 KiB`.
- **Crash-Safe SQLite WAL Resume:** Persists segment cursors, `If-Range`, strong `ETag`, and `Last-Modified` validators in SQLite WAL, with atomic `.slipstream-part` → final file commit via `MoveFileExW`.
- **Expired URL Recovery (`Refresh Download Address`):** Swap an expired signed URL (`403` / `410`) in-place via context menu or Transfer Details while keeping already-downloaded bytes intact.
- **Wildcard Batch Expansion & Queues:** Expand padded/unpadded/stepped numeric or alphabetic wildcard patterns up to `10,000` URLs with queue concurrency caps and token-bucket global/per-transfer speed limits.
- **Built-In Integrity & SSRF Hardening:** SHA-256 / MD5 verification, `CheckedDns` IP filtering across up to `10` redirect hops, cross-origin `Cookie` / `Authorization` stripping on redirects, credential scrubbing on HTTP cancel, and Windows NTFS `:Zone.Identifier` (`ZoneId=3`) Mark-of-the-Web preservation.

### 2. Hardened Media & Format Pipeline (`crates/media`)
- **Bundled tools** The installer and portable ZIP include yt-dlp 2026.10.26 and the FFmpeg 9.1 Essentials builds of `ffmpeg.exe` and `ffprobe.exe`, with license files in `bin/`. **Settings → Toolchain → Update yt-dlp** resolves a single upstream release, checks the executable against that release’s SHA-256 checksum, and installs it in your per-user tool directory. Settings shows the active versions and sources of all three tools and supports custom paths.
- **Zero Shell Interpolation:** Invokes `yt-dlp.exe`, `ffmpeg.exe`, and `ffprobe.exe` strictly with discrete argument arrays, allowlisted flags, `--`, forced local FFmpeg demuxers, `YTDLP_NO_PLUGINS=1`, and junction-free canonical `.exe` checks.
- **Format, Container & Audio Selection:** Structured `4K 2160p60` / codec (`AV1`, `VP9`, `H.264`) / container (`MP4`, `MKV`, `WebM`) selection, lossless stream remuxing, audio extraction (`MP3 320k`, `M4A`, `Opus`, `FLAC`, `WAV`) with optional EBU R128 loudness normalization, subtitle language embedding (`.srt`, `.vtt`, `.ass`, `.ssa`), and playlist item selection.
- **Isolated Staging & MotW:** Downloads to scoped UUID staging directories, tags outputs with `:Zone.Identifier`, and publishes atomically via non-overwriting `fs::hard_link`.

### 3. Native BitTorrent & Magnet (`crates/torrent`)
- **Embedded Rust Engine (`librqbit`):** Supports `.torrent` files, raw torrent bytes, and `magnet:` URIs without external torrent daemons.
- **Manifest & Confinement Guards:** Depth-64 bencode decoder with duplicate-key and symlink (`attr 'l'` / `symlink path`) rejection, Windows reserved DOS device and BiDi control filtering, `ConfinedTorrentStorage`, selective file priorities, 16-segment piece-bitmap projection, and NTFS `:Zone.Identifier` tagging on completed files.

### 4. Browser Integration (`crates/native-host` + `extensions/browser`)
- **Cross-Browser MV3:** Supports **Chrome**, **Edge**, **Brave**, and **Firefox** (`slipstream@ciphervaults.com`) using `4`-byte little-endian `1 MiB` Native Messaging framing and per-user `HKCU` registration.
- **One-click Browser Picker** Choose **Add Extension** or **Open Setup** for your browser in **Settings → Browser Integration**. SlipStream opens the browser’s extension setup page and reveals the bundled extension folder. Chrome, Edge, and Brave share the fixed ID `nikdbkenglojniombimjdkneanmponcp`.
- **Floating Shadow-DOM Media Pill:** Closed Shadow DOM page overlay for 1-click `4K`, `1080p`, `MP3 320k`, `FLAC`, Format Picker, Mute Site, or Grab All Links—guarded by `event.isTrusted`, top-frame origin verification, and SPA navigation invalidation.
- **Privacy & Context Isolation:** `downloads.onCreated` honors `Alt`-key bypass, file extensions, size thresholds, muted/excluded sites, and **Incognito / Firefox `cookieStoreId` isolation**. Browser `GetStatus` responses redact desktop query tokens and filesystem paths before leaving the daemon.

### 5. Dual-Theme Windows Desktop App & Auto-Updater (`apps/desktop`)
- **Soft Graphite Dark ↔ Porcelain Light:** Live instant theme switching (plus `Follow System` and `Blue` / `Monochrome` accents), 16-segment progress visualizer, `T01–T16` active thread map, throughput sparkline, and `Ctrl+K` / `Ctrl+N` command shortcuts.
- **Tray-Hibernate Mode & OS Security:** Closes the WebView2 surface cleanly to the Windows system tray while active downloads continue at minimal RAM, reveals completed files via `explorer.exe /select`, and optionally queues Windows Defender (`MpCmdRun.exe`) scans.
- **Signed NSIS updates** In **Settings → About & Updates**, choose **Automatic on startup** or **Manual only**. An automatic check runs once per app process, including across tray restores. **Check for Updates** is available in either mode. **Install Update & Restart** shows live progress for the signed update. If installation is interrupted after downloads are stopped, quit and reopen SlipStream to resume them.
- **Native WebView2 lockdown** SlipStream disables default browser context menus, status UI, browser accelerators, and zoom controls before showing the main window, including after restoring it from the tray. Release builds also disable DevTools. **Ctrl+F** selects the download search field, and text editing shortcuts and selection remain available in editable fields. You can also select paths, IDs, and error text where displayed.

---

## Security & Verification

- **6-Phase `/security-audit`:** Completed 3 hunting waves across 19 coverage units, 3 independent coverage critics, cross-model candidate validation, and fresh record verification (`0` confirmed vulnerabilities; proactive hardening landed for browser `GetStatus` redaction, cancel credential scrubbing, torrent `:Zone.Identifier` tagging, and trailing-dot `localhost.` SSRF normalization).
- **Part II Safety & Resilience Evaluation (`tests/safety-eval`):** Preregistered (`slipstream-safety-v1`) deterministic oracle suite passing **1,200 / 1,200** adversarial + benign single-input boundary cases and **108 / 108** orthogonal 5-factor stateful HTTP chaos chains (`0` broken items, `0` escapes, `0` benign false rejections, strict `bias-audit` Cramér's `V = 0.0`).

---

## Download, Installation & Browser Setup

1. **Installer** Download `SlipStream_1.1.0_x64-setup.exe` and run the per-user installer. It includes the native messaging host, media tools, and both browser extension builds. [v1.1.0 release assets](https://github.com/CipherVaults/SlipStream/releases/tag/v1.1.0)
2. **Portable ZIP** Extract `SlipStream-1.1.0-windows-x64-portable.zip` and run `SlipStream.exe`. Keep the extracted `bin/` and `extensions/` directories beside it.
3. **Browser extension** In **Settings → Browser Integration**, select your browser. For Chrome, Edge, or Brave, enable **Developer mode**, select **Load unpacked**, and choose the revealed folder. In Firefox, choose **Load Temporary Add-on** and select the revealed `manifest.json`. Separate browser extension ZIPs are also available on the release page.

**Upgrading from v1.0.0** SlipStream keeps the valid native messaging IDs already registered for each browser and adds the bundled extension ID. If an older unpacked copy’s ID is missing from the registration, copy the ID from the browser’s extensions page and enter it in **Settings → Browser Integration** for that browser.

### Building from Source (Windows 10/11)

Prerequisites: **Rust (stable)**, **Node.js 22**, **pnpm 9+**, **VS Build Tools (C++ x64)**, **WebView2 Runtime**.

```powershell
pnpm.cmd install
if ($LASTEXITCODE -ne 0) { throw 'Workspace dependency installation failed.' }
pnpm.cmd -r typecheck
if ($LASTEXITCODE -ne 0) { throw 'Type checking failed.' }
pnpm.cmd -r test
if ($LASTEXITCODE -ne 0) { throw 'Frontend tests failed.' }
pnpm.cmd -r build
if ($LASTEXITCODE -ne 0) { throw 'Frontend build failed.' }
cargo fetch --locked --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw 'Rust dependency fetch failed.' }
$env:CARGO_NET_OFFLINE = 'true'
cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { throw 'Rust formatting check failed.' }
cargo clippy --locked --workspace --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { throw 'Clippy failed.' }
cargo test --locked --workspace
if ($LASTEXITCODE -ne 0) { throw 'Rust tests failed.' }
cargo build --locked --release -p slipstream-desktop --features tauri/custom-protocol
```

The release workflow smoke-tests pinned media tools in a job with no secrets. The signing job verifies fresh downloads against the same pinned SHA-256 digests and packages the tools, notices, and extensions. The release includes the yt-dlp source tarball alongside the installer and ZIPs. [Release workflow](.github/workflows/release.yml), [third-party notices](THIRD_PARTY_NOTICES.md)

---

## Roadmap

- **HTTP/3 (QUIC) Transport:** Upgrade HTTP engine beyond HTTP/1.1 & HTTP/2 with `reqwest` HTTP/3 support when stabilized.
- **Active Clipboard Watcher & Power Actions:** Connect `monitor_clipboard` listener and OS `PostQueueAction::{Sleep, Shutdown}` execution in the daemon.
- **Toolchain integrity and Authenticode** Show tool hashes in Settings and add trusted Windows code-signing for installers.
- **Browser store listings** Publish the extension to Chrome Web Store, Edge Add-ons, and Firefox Add-ons.

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

SlipStream is released under the MIT License. Bundled components have separate licenses. [SlipStream license](Cargo.toml), [third-party notices](THIRD_PARTY_NOTICES.md)
