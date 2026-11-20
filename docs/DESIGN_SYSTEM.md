# SlipStream Design System (`Soft Graphite` & `Porcelain`)

> **Aesthetic Mandate:** Genuine Apple/Mac desktop craft tailored cleanly for Windows 11. Zero AI slop, zero pitch-black `#000000` voids, zero neon glows, zero decorative gradients.

## 1. Same-App Theme & Accent Tokens

All UI components in `apps/desktop` and `extensions/browser` consume CSS custom properties controlled by `html[data-theme="dark" | "light"]` and `html[data-accent="blue" | "mono"]`, supporting `Follow System`, `Soft Graphite`, and `Porcelain`.

### Soft Graphite Dark (`data-theme="dark"`)
- `--bg-canvas`: `#1A1C22` *(warm velvety charcoal — never pure black)*
- `--bg-sidebar`: `#21242C`
- `--bg-inspector`: `#1F222A`
- `--bg-elevated`: `#262A34`
- `--bg-hover`: `#2C313D`
- `--bg-input`: `#15171C`
- `--track`: `#2E3340`
- `--border`: `rgba(255, 255, 255, 0.08)`
- `--border-strong`: `rgba(255, 255, 255, 0.13)`
- `--specular`: `inset 0 1px 0 rgba(255, 255, 255, 0.06)`
- `--text-1`: `#F0F2F6`
- `--text-2`: `#9DA3B4`
- `--text-3`: `#6E7587`
- `--accent` (`blue`): `#3B8CFF` (hover `#549BFF`, soft `rgba(59, 140, 255, 0.14)`, ink `#FFFFFF`)
- `--accent` (`mono`): `#E4E7EE` (hover `#F5F7FA`, soft `rgba(228, 231, 238, 0.12)`, ink `#14161C`)
- `--green`: `#34C759` | `--amber`: `#FFB340`

### Porcelain Light (`data-theme="light"`)
- `--bg-canvas`: `#FFFFFF`
- `--bg-sidebar`: `#F4F5F8`
- `--bg-inspector`: `#F8F9FB`
- `--bg-elevated`: `#F7F8FB`
- `--bg-hover`: `#EFF1F6`
- `--bg-input`: `#ECEEF3`
- `--track`: `#E6E8EF`
- `--border`: `rgba(15, 23, 42, 0.08)`
- `--border-strong`: `rgba(15, 23, 42, 0.14)`
- `--specular`: `inset 0 1px 0 rgba(255, 255, 255, 0.85)`
- `--text-1`: `#14171F`
- `--text-2`: `#5C6374`
- `--text-3`: `#8B92A4`
- `--accent` (`blue`): `#0071E3` (hover `#0077ED`, soft `rgba(0, 113, 227, 0.10)`, ink `#FFFFFF`)
- `--accent` (`mono`): `#181B22` (hover `#2C313D`, soft `rgba(24, 27, 34, 0.08)`, ink `#FFFFFF`)
- `--green`: `#1E9E49` | `--amber`: `#D9820B`

## 2. Typography & Metrics

- **UI Sans:** `"Segoe UI Variable Text", "Segoe UI Variable", -apple-system, BlinkMacSystemFont, "Inter", system-ui, sans-serif`
- **Display:** `"Segoe UI Variable Display", "Segoe UI Variable", system-ui, sans-serif`
- **Mono:** `"SF Mono", "JetBrains Mono", Consolas, ui-monospace, monospace`
- **Numeric Stability:** Every speed, byte offset, percentage, count, and ETA uses `font-variant-numeric: tabular-nums`.

## 3. Structural Rules

- **Three-Pane Shell:** `228px` sidebar (`Brand + Nav + Throughput Sparkline & Disk Footer`) + fluid center workspace (`52px` unified toolbar with minimal Windows 11 `— □ ×` controls at far right, transfer cards, completed today list) + `316px` docked Inspector (`Overview` / `Segments` 4×4 grid / `Media` or `Files`).
- **16-Segment Visualizer:** Every active download projects its byte ranges (or torrent piece bitmap) onto 16 distinct 5px-tall rounded micro-pills (`2.5px` gap).
- **Modal & Extension Cohesion:** Add/Format/Torrent sheet uses a clean 4-tab switcher (`Video` / `Audio Only` / `Subtitles` / `Torrent / Magnet`) and authentic pill toggles (ON = knob right). Browser floating media pill uses frosted dark/light surface matching the app tokens.
