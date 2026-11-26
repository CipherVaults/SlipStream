import { invoke } from "@tauri-apps/api/core";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview, type DragDropEvent } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type {
  AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings, BrowserKind,
  BrowserSetupResult, BrowserStatus, ConfirmPendingDownloadRequest, DownloadCategory, MediaProbeResult,
  PendingDownloadPreview, RefreshDownloadUrlRequest, TelemetryBatch, ToolchainStatus, TorrentFileEntry,
  TransferCompletionOptions, UpdateProgress, UpdateStatus,
} from "../types/ipc";
import { demo } from "./demo";

export function isTauri() {
  return typeof window !== "undefined" && Boolean((window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__);
}

async function command<T>(name: string, args: Record<string, unknown> | undefined, fallback: () => T): Promise<T> {
  if (!isTauri()) return fallback();
  return args ? invoke<T>(name, args) : invoke<T>(name);
}

const ALLOWED_PICKER_HEADERS = new Map<string, string>([
  ["referer", "Referer"],
  ["origin", "Origin"],
  ["user-agent", "User-Agent"],
  ["accept-language", "Accept-Language"],
]);

export const desktop = {
  getSnapshot: () => command<TelemetryBatch>("get_snapshot", undefined, demo.snapshot),
  addHttpDownload: (request: AddHttpDownloadRequest) =>
    command<string>("add_http_download", { request }, () => demo.addHttp(request)),
  expandWildcardUrls: (pattern: string) =>
    command<string[]>("expand_wildcard_urls", { pattern }, () => demo.expandWildcard(pattern)),
  probeMedia: (url: string, playlist?: boolean, headers?: Record<string, string>) => {
    const args: Record<string, unknown> = { url };
    if (playlist !== undefined) args.playlist = playlist;
    if (headers && Object.keys(headers).length > 0) args.headers = headers;
    return command<MediaProbeResult>("probe_media", args, () => demo.probe(url, playlist));
  },
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
  getPendingDownload: (pendingId: string) =>
    command<PendingDownloadPreview>("get_pending_download", { pending_id: pendingId }, () => demo.getPendingDownload(pendingId)),
  confirmPendingDownload: (request: ConfirmPendingDownloadRequest) =>
    command<string>("confirm_pending_download", { request }, () => demo.confirmPendingDownload(request)),
  cancelPendingDownload: (pendingId: string) =>
    command<void>("cancel_pending_download", { pending_id: pendingId }, () => demo.cancelPendingDownload(pendingId)),
  resolveDuplicateExisting: (pendingId: string) =>
    command<string>("resolve_duplicate_existing", { pending_id: pendingId }, () => demo.resolveDuplicateExisting(pendingId)),
  addDownloadCategory: (name: string, extensions: string[], saveDir?: string | null) =>
    command<DownloadCategory>("add_download_category", { name, extensions, save_dir: saveDir ?? null },
      () => demo.addDownloadCategory(name, extensions, saveDir ?? null)),
  setTransferSpeedLimit: (transferId: string, limitBps: number | null) =>
    command<void>("set_transfer_speed_limit", { transfer_id: transferId, limit_bps: limitBps },
      () => demo.setTransferSpeedLimit(transferId, limitBps)),
  getTransferCompletionOptions: (transferId: string) =>
    command<TransferCompletionOptions>("get_transfer_completion_options", { transfer_id: transferId },
      () => demo.getTransferCompletionOptions(transferId)),
  setTransferCompletionOptions: (transferId: string, options: TransferCompletionOptions) =>
    command<void>("set_transfer_completion_options", { transfer_id: transferId, options },
      () => demo.setTransferCompletionOptions(transferId, options)),
  openProgressDialog: (transferId: string) =>
    command<void>("open_progress_dialog", { transfer_id: transferId }, () => demo.openProgressDialog(transferId)),
  openCompletedFile: (transferId: string) =>
    command<void>("open_completed_file", { transfer_id: transferId }, () => demo.openCompletedFile(transferId)),
  pickDownloadFolder: (initialDir?: string | null) =>
    command<string | null>("pick_download_folder", { initial_dir: initialDir ?? null },
      () => demo.pickDownloadFolder(initialDir ?? null)),
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
  async onSettingsUpdated(handler: () => void) {
    return isTauri() ? listen("slipstream://settings-updated", () => handler()) : () => {};
  },
  async onOpenPicker(handler: (url: string | null, headers?: Record<string, string>) => void) {
    return isTauri() ? listen<{ url: string | null; headers?: Record<string, string> }>("slipstream://open-picker", ({ payload }) => {
      const nextUrl = typeof payload?.url === "string" ? payload.url : null;
      const rawHeaders = payload?.headers;
      const filtered: Record<string, string> = {};
      if (rawHeaders && typeof rawHeaders === "object" && !Array.isArray(rawHeaders)) {
        for (const [name, value] of Object.entries(rawHeaders)) {
          const canonical = ALLOWED_PICKER_HEADERS.get(name.toLowerCase());
          if (canonical && typeof value === "string" && value.length > 0 && !(canonical in filtered)) {
            filtered[canonical] = value;
          }
        }
      }
      if (Object.keys(filtered).length > 0) handler(nextUrl, filtered);
      else handler(nextUrl);
    }) : demo.listenPicker(handler);
  },
  async onFileDrop(handler: (event: DragDropEvent) => void) {
    if (isTauri()) return getCurrentWebview().onDragDropEvent(({ payload }) => handler(payload));
    return () => {};
  },
  async minimize() { if (isTauri()) await getCurrentWindow().minimize(); },
  async toggleMaximize() { if (isTauri()) await getCurrentWindow().toggleMaximize(); },
  async close() { if (isTauri()) await getCurrentWindow().close(); },
  async setWindowSize(width: number, height: number) {
    if (!isTauri()) return;
    try {
      await getCurrentWindow().setSize(new LogicalSize(width, height));
    } catch {
      // Ignore in non-resizable or test environments.
    }
  },
  async setWindowTitle(title: string) {
    if (typeof document !== "undefined") document.title = title;
    if (!isTauri()) return;
    try {
      await getCurrentWindow().setTitle(title);
    } catch {
      // Ignore in test environments where native title updates are unavailable.
    }
  },
};
