# SlipStream Architecture & Security Boundaries

## 1. System Topology

```text
[Untrusted Web Page]
        │  DOM / Network Requests
        ▼
[MV3 Browser Extension] (extensions/browser)
        │  browser.runtime.connectNative("com.ciphervaults.slipstream")
        ▼
[Native Messaging Host] (crates/native-host --stdio)
        │  Windows Named Pipe \\.\pipe\SlipStream.IPC.<sid-hash>
        │  SDDL: D:(A;;GA;;;<USER_SID>) + 256-bit per-session bearer token
        ▼
[SlipStream Core Daemon / Tauri v2] (apps/desktop/src-tauri)
   ├── slipstream-protocol   (Schema + Path/ADS/Device-Name/BiDi + URL/SSRF Guards)
   ├── slipstream-engine     (1–32 Dynamic HTTP Range Segments, SQLite WAL, MotW)
   ├── slipstream-media      (Hardened yt-dlp + FFmpeg runner, Format/Audio/Subtitles)
   └── slipstream-torrent    (BitTorrent / Magnet, Selective Files, Piece Map)
        │  Batched 20 Hz IPC Snapshot
        ▼
[React 19 + Tailwind v4 Desktop UI] (apps/desktop/src)
```

## 2. Trust Boundaries & Mandatory Invariants

1. **Web Page ↔ Browser Extension:**
   - The floating media pill renders inside a closed Shadow DOM root; page scripts cannot spoof extension commands or inspect IPC tokens.
   - URLs are restricted strictly to `http://` and `https://` (plus `magnet:` when user-initiated).
2. **Browser Extension ↔ Native Host ↔ Core Daemon:**
   - **Zero unauthenticated localhost HTTP server.** Browser Native Messaging manifests pin `allowed_origins` strictly to SlipStream's extension IDs under `HKCU\Software\...\NativeMessagingHosts\com.ciphervaults.slipstream`.
   - Native Messaging stdin/stdout uses 4-byte little-endian length framing capped at **1 MiB** (`1_048_576` bytes).
   - Named Pipe creation applies an explicit Windows SDDL DACL allowing **only the current user's SID** (`D:(A;;GA;;;<USER_SID>)`) and requires a 256-bit per-session token written to `%LOCALAPPDATA%\SlipStream\session.token` before executing privileged operations.
3. **Network Redirects & SSRF Defense (`crates/engine`):**
   - `Cookie` and `Authorization` headers supplied by the browser are scoped strictly to the initial origin and **stripped on cross-origin 30x redirects**.
   - Public URLs redirecting to loopback (`127.0.0.0/8`, `::1`) or private/link-local/CGNAT ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, `fc00::/7`, `fe80::/10`) are blocked by default unless explicitly permitted in controlled local-test configurations.
4. **Filesystem & Mark-of-the-Web (`crates/protocol` + `crates/engine` + `crates/torrent`):**
   - Every filename from `Content-Disposition`, URLs, media titles, or `.torrent` manifests passes `slipstream_protocol::sanitize_filename` and `safe_join_under`.
   - Rejects `..`, `/` or `\`, NUL bytes, Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, even with extensions like `CON.mp4`), NTFS Alternate Data Stream colons (`:`), and Unicode BiDi override characters (`U+202A`–`U+202E`, `U+2066`–`U+2069`).
   - Completed HTTP/media downloads write Windows **Mark-of-the-Web** (`:Zone.Identifier` ADS with `ZoneId=3`, `HostUrl`, and `ReferrerUrl`) atomically alongside final commit.
5. **External Sidecar Execution (`crates/media`):**
   - Invokes `yt-dlp.exe` and `ffmpeg.exe` solely via `std::process::Command::new(pinned_binary)` / `tokio::process::Command` with discrete argument arrays and `--` option termination — **never** `cmd.exe`, PowerShell, or string concatenation.
   - Allowlists exact safe options; rejects dangerous flags (`--exec`, `--batch-file`, `--config-locations`, raw untrusted FFmpeg `-filter_complex` injection).

## 3. Windows-Specific Performance Engineering

- **Tray-Hibernate Mode:** Closing the main window hides/destroys the WebView2 surface while the Rust core (`engine` + `torrent` + Named Pipe server) stays alive in the Windows tray at ~10–15 MB RAM.
- **Batched 20 Hz Telemetry:** Segment progress and thread speeds aggregate in Rust and emit snapshots at 50ms intervals (`20 Hz`) instead of per-packet IPC calls.
- **Native Windows 11 Frame:** Custom frameless 52px toolbar paired with Win32 `DwmSetWindowAttribute` rounded corners and `WM_NCHITTEST` border/Snap Support.
