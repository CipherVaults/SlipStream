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
export type MediaAction = "4k" | "1080p" | "mp3_320" | "flac" | "picker" | "mute" | "links";
export type PopupAction =
  | { type: "pause_transfer" | "resume_transfer"; transfer_id: string }
  | { type: "open_desktop_window" };
export type BrowserMessage =
  | { kind: "alt"; held: boolean }
  | { kind: "stream" }
  | { kind: "wake" }
  | { kind: "media"; action: MediaAction; url: string; links?: string[] }
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

export function formatSpeed(bytes: number): string {
  const units = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"];
  const speed = Number.isFinite(bytes) ? Math.max(0, bytes) : 0;
  let scaled = speed;
  let unit = 0;
  while (scaled >= 1024 && unit < units.length - 1) {
    scaled /= 1024;
    unit++;
  }
  return scaled.toFixed(unit === 0 ? 0 : 1) + " " + units[unit];
}

export function ratio(value: number): number {
  return Number.isFinite(value) ? Math.min(1, Math.max(0, value)) : 0;
}
