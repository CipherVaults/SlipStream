import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview, type DragDropEvent } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type {
  AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings, BrowserKind,
  BrowserSetupResult, BrowserStatus, MediaProbeResult, RefreshDownloadUrlRequest, TelemetryBatch,
  ToolchainStatus, TorrentFileEntry, UpdateProgress, UpdateStatus,
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
  probeMedia: (url: string, playlist?: boolean) =>
    command<MediaProbeResult>("probe_media", playlist === undefined ? { url } : { url, playlist }, () => demo.probe(url, playlist)),
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
  pausePlaylist: (playlist_id: string) =>
    command<void>("pause_playlist", { playlist_id }, () => demo.pausePlaylist(playlist_id)),
  resumePlaylist: (playlist_id: string) =>
    command<void>("resume_playlist", { playlist_id }, () => demo.resumePlaylist(playlist_id)),
  revealPlaylistFolder: (playlist_id: string) =>
    command<void>("reveal_playlist_folder", { playlist_id }, () => undefined),
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
  updateYtdlp: () => command<ToolchainStatus>("update_ytdlp", undefined, demo.updateYtdlp),
  listBrowsers: () => command<BrowserStatus[]>("list_browsers", undefined, demo.listBrowsers),
  openBrowserExtensionSetup: (browserId: BrowserKind, customExtensionId?: string | null) =>
    command<BrowserSetupResult>("open_browser_extension_setup", { browserId, customExtensionId: customExtensionId ?? null },
      () => demo.openBrowserExtensionSetup(browserId, customExtensionId)),
  installBrowserHosts: (chrome_id: string | null, edge_id: string | null, firefox_id: string | null) =>
    command<string[]>("install_browser_hosts", { chrome_id, edge_id, firefox_id },
      () => demo.installHosts([chrome_id, edge_id, firefox_id])),
  hibernateToTray: () => command<void>("hibernate_to_tray", undefined, () => undefined),
  checkForUpdates: () => command<UpdateStatus>("check_for_updates", undefined, demo.checkForUpdates),
  checkForStartupUpdates: (): Promise<UpdateStatus | null> => isTauri()
    ? invoke<UpdateStatus | null>("check_for_updates", { startup: true })
    : desktop.checkForUpdates(),
  installUpdate: () => command<boolean>("install_update", undefined, demo.installUpdate),

  async onUpdateProgress(handler: (progress: UpdateProgress) => void) {
    if (isTauri()) {
      try {
        return await listen<UpdateProgress>("slipstream://update-progress", event => handler(event.payload));
      } catch {
        // Updates remain usable when the event plugin is unavailable.
      }
    }
    return () => {};
  },
  async onTelemetry(handler: (batch: TelemetryBatch) => void) {
    return isTauri() ? listen<TelemetryBatch>("slipstream://telemetry", event => handler(event.payload))
      : demo.listenTelemetry(handler);
  },
  async markUiReady(): Promise<void> {
    if (!isTauri()) return;
    try {
      await invoke("mark_ui_ready");
    } catch {
      // Older backends or tests without mark_ui_ready remain usable.
    }
  },
  async onOpenPicker(handler: (url: string | null) => void) {
    return isTauri() ? listen<{ url: string | null }>("slipstream://open-picker", ({ payload }) => {
      handler(typeof payload?.url === "string" ? payload.url : null);
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
