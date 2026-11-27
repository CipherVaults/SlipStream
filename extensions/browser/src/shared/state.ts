import type { AccentMode, NativeBridgeResponse, TelemetryBatch, ThemeMode, TransferSnapshot } from "@slipstream/ipc-types";
import type { InterceptionSettings } from "./downloads";

export type Status = Extract<NativeBridgeResponse, { type: "status" }>;
export interface BridgeState {
  connected: boolean;
  theme: ThemeMode;
  accent: AccentMode;
  batch: TelemetryBatch | null;
}

export const emptyState: BridgeState = { connected: false, theme: "system", accent: "blue", batch: null };
export const STATE_KEY = "bridgeState";
export const SETTINGS_KEY = "interceptionSettings";
export const APPEARANCE_KEY = "appearance";
export const PILL_SPOTS_KEY = "pillSpots";
export const PLAYER_PORT = "slipstream-player";

export interface PillSpot { x: number; y: number }

export function readPillSpot(value: unknown): PillSpot | undefined {
  const spot = value as Partial<PillSpot> | null;
  const { x, y } = spot ?? {};
  if (typeof x === "number" && x >= 0 && x <= 1 && typeof y === "number" && y >= 0 && y <= 1) {
    return { x, y };
  }
}

export function rememberPillSpot(saved: unknown, site: string, spot: PillSpot): Record<string, PillSpot> {
  const entries = Object.entries(saved && typeof saved === "object" ? saved : {})
    .filter(([key, value]) => key !== site && readPillSpot(value));
  return Object.fromEntries([...entries.slice(-199), [site, spot]]);
}

export function canUseSiteStorage(inIncognitoContext?: boolean, tabIncognito?: boolean): boolean {
  return !inIncognitoContext && !tabIncognito;
}

export function readAppearance(value: unknown): Pick<BridgeState, "theme" | "accent"> {
  const saved = value as Partial<BridgeState> | undefined;
  return {
    theme: saved?.theme === "dark" || saved?.theme === "light" ? saved.theme : "system",
    accent: saved?.accent === "mono" ? "mono" : "blue",
  };
}

export function readState(value: unknown): BridgeState {
  const saved = value as Partial<BridgeState> | undefined;
  const batch = saved?.batch && Array.isArray(saved.batch.transfers) ? saved.batch : null;
  return { ...readAppearance(saved), connected: !!batch && saved?.connected === true, batch };
}
export type MediaAction = "4k" | "1080p" | "mp3_320" | "flac" | "picker" | "playlist" | "mute" | "links"
  | `format:${string}` | `hls:${string}` | `direct:${string}`;
export interface PillFormat {
  id: MediaAction;
  label: string;
  sub?: string;
}
export interface PillFormatsMessage {
  kind: "pill-formats";
  url: string;
  requestId?: number;
  formats: PillFormat[];
  isPlaylist: boolean;
}
export type PopupAction =
  | { type: "pause_transfer" | "resume_transfer"; transfer_id: string }
  | { type: "open_desktop_window" };
export type BrowserMessage =
  | { kind: "alt"; held: boolean }
  | { kind: "stream" }
  | { kind: "pill-spot"; spot?: PillSpot }
  | { kind: "pill-open"; url: string; isPlaylist: boolean; requestId?: number; embedded?: boolean }
  | { kind: "frame-reset" }
  | { kind: "wake" }
  | { kind: "magnet-click"; url: string }
  | { kind: "media"; action: MediaAction; url: string; links?: string[]; embedded?: boolean }
  | { kind: "desktop"; request: PopupAction }
  | { kind: "save-settings"; settings: InterceptionSettings }
  | { kind: "refresh"; id: string; transferId: string };

export function isRefreshableHttpTransfer(transfer: Pick<TransferSnapshot, "kind" | "status">): boolean {
  return transfer.kind === "http" && transfer.status !== "completed" && transfer.status !== "canceled";
}

export interface ActionResult {
  ok: boolean;
  message?: string;
}

export function applyAppearance(element: HTMLElement, state: Pick<BridgeState, "theme" | "accent">, prefersDark: boolean): void {
  element.dataset.theme = state.theme === "system" ? (prefersDark ? "dark" : "light") : state.theme;
  element.dataset.accent = state.accent;
}

export function formatBytes(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  const size = Number.isFinite(bytes) ? Math.max(0, bytes) : 0;
  let scaled = size;
  let unit = 0;
  while (scaled >= 1024 && unit < units.length - 1) {
    scaled /= 1024;
    unit++;
  }
  return scaled.toFixed(unit === 0 ? 0 : 1) + " " + units[unit];
}

export function formatSpeed(bytes: number): string {
  return formatBytes(bytes) + "/s";
}

export function ratio(value: number): number {
  return Number.isFinite(value) ? Math.min(1, Math.max(0, value)) : 0;
}
