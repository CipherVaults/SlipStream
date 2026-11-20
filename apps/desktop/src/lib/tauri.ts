import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview, type DragDropEvent } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type {
  AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings, MediaProbeResult,
  RefreshDownloadUrlRequest, TelemetryBatch, ToolchainStatus, TorrentFileEntry,
} from "../types/ipc";
import { demo } from "./demo";

export function isTauri() {
  return typeof window !== "undefined" && Boolean((window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__);
}

async function command<T>(name: string, args: Record<string, unknown> | undefined, fallback: () => T): Promise<T> {
  if (!isTauri()) return fallback();
  return args ? invoke<T>(name, args) : invoke<T>(name);
}

export const desktop = {
  getSnapshot: () => command<TelemetryBatch>("get_snapshot", undefined, demo.snapshot),
  addHttpDownload: (request: AddHttpDownloadRequest) =>
    command<string>("add_http_download", { request }, () => demo.addHttp(request)),
  expandWildcardUrls: (pattern: string) =>
    command<string[]>("expand_wildcard_urls", { pattern }, () => demo.expandWildcard(pattern)),
  probeMedia: (url: string) => command<MediaProbeResult>("probe_media", { url }, () => demo.probe(url)),
  addMediaDownload: (request: AddMediaDownloadRequest) =>
    command<string>("add_media_download", { request }, () => demo.addMedia(request)),
  addTorrent: (request: AddTorrentRequest) =>
    command<string>("add_torrent", { request }, () => demo.addTorrent(request)),
  listTorrentFiles: (transfer_id: string) =>
    command<TorrentFileEntry[]>("list_torrent_files", { transfer_id }, () => demo.listFiles(transfer_id)),
  setTorrentFiles: (transfer_id: string, selected_indices: number[]) =>
    command<void>("set_torrent_files", { transfer_id, selected_indices }, () => demo.selectFiles(transfer_id, selected_indices)),
  pauseTransfer: (transfer_id: string) =>
    command<void>("pause_transfer", { transfer_id }, () => demo.pause(transfer_id)),
  resumeTransfer: (transfer_id: string) =>
    command<void>("resume_transfer", { transfer_id }, () => demo.resume(transfer_id)),
  cancelTransfer: (transfer_id: string, delete_files = false) =>
    command<void>("cancel_transfer", { transfer_id, delete_files }, () => demo.cancel(transfer_id)),
  refreshDownloadUrl: (request: RefreshDownloadUrlRequest) =>
    command<void>("refresh_download_url", { request }, () => demo.refresh(request)),
  setSpeedLimit: (limit_bps: number | null) =>
    command<void>("set_speed_limit", { limit_bps }, () => demo.setLimit(limit_bps)),
  verifyTransferChecksum: (transfer_id: string, expected_sha256: string) =>
    command<boolean>("verify_transfer_checksum", { transfer_id, expected_sha256 }, () => demo.verify(transfer_id, expected_sha256)),
  revealInExplorer: (transfer_id: string) => command<void>("reveal_in_explorer", { transfer_id }, () => undefined),
  getSettings: () => command<AppSettings>("get_settings", undefined, demo.getSettings),
  saveSettings: (settings: AppSettings) => command<void>("save_settings", { settings }, () => demo.saveSettings(settings)),
  getToolchainStatus: () => command<ToolchainStatus>("get_toolchain_status", undefined, demo.toolchain),
  installBrowserHosts: (chrome_id: string | null, edge_id: string | null, firefox_id: string | null) =>
    command<string[]>("install_browser_hosts", { chrome_id, edge_id, firefox_id },
      () => demo.installHosts([chrome_id, edge_id, firefox_id])),
  hibernateToTray: () => command<void>("hibernate_to_tray", undefined, () => undefined),

  async onTelemetry(handler: (batch: TelemetryBatch) => void) {
    return isTauri() ? listen<TelemetryBatch>("slipstream://telemetry", event => handler(event.payload))
      : demo.listenTelemetry(handler);
  },
  async onOpenPicker(handler: (url: string | null) => void) {
    return isTauri() ? listen<{ url: string | null }>("slipstream://open-picker", ({ payload }) => {
      handler(payload?.url ?? null);
    }) : demo.listenPicker(handler);
  },
  async onFileDrop(handler: (event: DragDropEvent) => void) {
    if (isTauri()) return getCurrentWebview().onDragDropEvent(({ payload }) => handler(payload));
    return () => {};
  },
  async minimize() { if (isTauri()) await getCurrentWindow().minimize(); },
  async toggleMaximize() { if (isTauri()) await getCurrentWindow().toggleMaximize(); },
  async close() { if (isTauri()) await getCurrentWindow().close(); },
};
