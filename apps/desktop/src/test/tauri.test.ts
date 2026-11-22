import { emit } from "@tauri-apps/api/event";
import { clearMocks, mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_SETTINGS, demo } from "../lib/demo";
import { desktop, isTauri } from "../lib/tauri";
import { DEFAULT_CHROMIUM_EXTENSION_ID, DEFAULT_FIREFOX_EXTENSION_ID } from "../types/ipc";

const invoke = vi.fn();

beforeEach(() => {
  invoke.mockReset();
  invoke.mockResolvedValue(undefined);
});
afterEach(() => {
  clearMocks();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
  Reflect.deleteProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__");
});

function useTauri() {
  mockWindows("main");
  mockIPC(invoke, { shouldMockEvents: true });
}

describe("Tauri v2 adapter", () => {
  it("sends the exact commands and snake_case argument keys", async () => {
    useTauri();
    expect(isTauri()).toBe(true);
    const url = "https://example.org/video.mp4";
    const id = "transfer-1";
    const http = { url, headers: { Accept: "*/*" }, max_connections: 16, expected_sha256: "a".repeat(64) };
    const media = { url, format_id: "401", embed_subtitles_langs: ["en"], lossless_remux: true, normalize_audio: false };
    const torrent = { source: "magnet:?xt=urn:btih:example", sequential: true, selected_file_indices: [1, 3] };
    const refresh = { transfer_id: id, new_url: url, headers: {}, referrer: null };
    const digest = "a".repeat(64);

    await desktop.getSnapshot();
    await desktop.addHttpDownload(http);
    await desktop.expandWildcardUrls("https://example.org/[1-3].zip");
    await desktop.probeMedia(url);
    await desktop.addMediaDownload(media);
    await desktop.addTorrent(torrent);
    await desktop.listTorrentFiles(id);
    await desktop.setTorrentFiles(id, [1, 3]);
    await desktop.pauseTransfer(id);
    await desktop.resumeTransfer(id);
    await desktop.cancelTransfer(id, true);
    await desktop.refreshDownloadUrl(refresh);
    await desktop.setSpeedLimit(null);
    await desktop.verifyTransferChecksum(id, digest);
    await desktop.revealInExplorer(id);
    await desktop.getSettings();
    await desktop.saveSettings(DEFAULT_SETTINGS);
    await desktop.getToolchainStatus();
    await desktop.installBrowserHosts("chrome", null, "firefox@example.org");
    await desktop.hibernateToTray();

    const update = { available: true, current_version: "1.1.0", version: "1.1.1" };
    invoke.mockResolvedValueOnce(update).mockResolvedValueOnce(true);
    await expect(desktop.checkForUpdates()).resolves.toEqual(update);
    await expect(desktop.installUpdate()).resolves.toBe(true);

    expect(invoke.mock.calls).toEqual([
      ["get_snapshot", {}], ["add_http_download", { request: http }],
      ["expand_wildcard_urls", { pattern: "https://example.org/[1-3].zip" }],
      ["probe_media", { url }], ["add_media_download", { request: media }], ["add_torrent", { request: torrent }],
      ["list_torrent_files", { transfer_id: id }], ["set_torrent_files", { transfer_id: id, selected_indices: [1, 3] }],
      ["pause_transfer", { transfer_id: id }], ["resume_transfer", { transfer_id: id }],
      ["cancel_transfer", { transfer_id: id, delete_files: true }], ["refresh_download_url", { request: refresh }],
      ["set_speed_limit", { limit_bps: null }], ["verify_transfer_checksum", { transfer_id: id, expected_sha256: digest }],
      ["reveal_in_explorer", { transfer_id: id }], ["get_settings", {}], ["save_settings", { settings: DEFAULT_SETTINGS }],
      ["get_toolchain_status", {}], ["install_browser_hosts", { chrome_id: "chrome", edge_id: null, firefox_id: "firefox@example.org" }],
      ["hibernate_to_tray", {}], ["check_for_updates", {}], ["install_update", {}],
    ]);
  });

  it("uses the native toolchain and browser commands with Tauri's camelCase setup keys", async () => {
    useTauri();
    const tools = demo.toolchain();
    const browsers = demo.listBrowsers();
    const setup = {
      browser_id: "edge", extension_dir: "C:\\SlipStream\\extensions\\chromium",
      extension_page_url: "edge://extensions/", extension_id: DEFAULT_CHROMIUM_EXTENSION_ID,
      launched_browser: true, revealed_extension_dir: true, native_host_registered: true,
      instructions: "Enable Developer mode and choose Load unpacked.",
    };
    invoke.mockResolvedValueOnce(tools).mockResolvedValueOnce(browsers).mockResolvedValueOnce(setup);
    await expect(desktop.updateYtdlp()).resolves.toEqual(tools);
    await expect(desktop.listBrowsers()).resolves.toEqual(browsers);
    await expect(desktop.openBrowserExtensionSetup("edge")).resolves.toEqual(setup);
    await desktop.openBrowserExtensionSetup("firefox", "custom@example.org");
    expect(invoke.mock.calls).toEqual([
      ["update_ytdlp", {}], ["list_browsers", {}],
      ["open_browser_extension_setup", { browserId: "edge", customExtensionId: null }],
      ["open_browser_extension_setup", { browserId: "firefox", customExtensionId: "custom@example.org" }],
    ]);
  });

  it("subscribes to native update progress and removes the handler on cleanup", async () => {
    useTauri();
    const handler = vi.fn();
    const unsubscribe = await desktop.onUpdateProgress(handler);
    const progress = { stage: "downloading", downloaded_bytes: 42, total_bytes: 100 };
    await emit("slipstream://update-progress", progress);
    expect(handler).toHaveBeenCalledExactlyOnceWith(progress);
    await unsubscribe();
    await emit("slipstream://update-progress", { stage: "verifying", downloaded_bytes: 100 });
    expect(handler).toHaveBeenCalledOnce();
  });

  it("keeps update progress optional outside Tauri and when native listening fails", async () => {
    const handler = vi.fn();
    expect(isTauri()).toBe(false);
    const stopDemo = await desktop.onUpdateProgress(handler);
    expect(() => stopDemo()).not.toThrow();
    expect(invoke).not.toHaveBeenCalled();
    mockIPC(invoke);
    expect(isTauri()).toBe(true);
    invoke.mockRejectedValueOnce(new Error("Event plugin unavailable"));
    const stopNative = await desktop.onUpdateProgress(handler);
    expect(() => stopNative()).not.toThrow();
    expect(invoke).toHaveBeenCalledWith("plugin:event|listen", expect.objectContaining({
      event: "slipstream://update-progress",
    }));
    expect(handler).not.toHaveBeenCalled();
  });

  it("unwraps telemetry and { url: string | null } picker payloads and returns event cleanup", async () => {
    useTauri();
    const telemetry = vi.fn();
    const picker = vi.fn();
    const offTelemetry = await desktop.onTelemetry(telemetry);
    const offPicker = await desktop.onOpenPicker(picker);
    const batch = demo.snapshot();
    await emit("slipstream://telemetry", batch);
    await emit("slipstream://open-picker", { url: "magnet:?xt=example" });
    await emit("slipstream://open-picker", { url: null });
    await emit("slipstream://open-picker", { url: 123 });
    expect(telemetry).toHaveBeenCalledWith(batch);
    expect(picker.mock.calls).toEqual([["magnet:?xt=example"], [null], [null]]);
    await offTelemetry();
    await offPicker();
    await emit("slipstream://telemetry", batch);
    await emit("slipstream://open-picker", { url: "https://unused.example.org" });
    expect(telemetry).toHaveBeenCalledOnce();
    expect(picker).toHaveBeenCalledTimes(3);
  });

  it("guards native window controls and passes native drop paths through", async () => {
    await desktop.minimize();
    await desktop.toggleMaximize();
    await desktop.close();
    expect(invoke).not.toHaveBeenCalled();
    useTauri();
    await desktop.minimize();
    await desktop.toggleMaximize();
    await desktop.close();
    expect(invoke.mock.calls).toEqual([
      ["plugin:window|minimize", { label: "main" }],
      ["plugin:window|toggle_maximize", { label: "main" }],
      ["plugin:window|close", { label: "main" }],
    ]);
    const handler = vi.fn();
    const off = await desktop.onFileDrop(handler);
    const paths = ["D:\\Downloads\\sample.torrent"];
    await emit("tauri://drag-drop", { paths, position: { x: 2, y: 3 } });
    expect(handler).toHaveBeenCalledWith(expect.objectContaining({ type: "drop", paths }));
    await off();
    await emit("tauri://drag-drop", { paths, position: { x: 2, y: 3 } });
    expect(handler).toHaveBeenCalledOnce();
  });

  it("preserves native errors instead of falling back to demo data", async () => {
    useTauri();
    invoke.mockRejectedValueOnce(new Error("Backend offline"));
    await expect(desktop.getSnapshot()).rejects.toThrow("Backend offline");
  });
});

describe("browser demo", () => {
  it("reports the current version and has no updates to install", async () => {
    await expect(desktop.checkForUpdates()).resolves.toEqual({
      available: false, current_version: "1.1.0", version: null, notes: null, pub_date: null,
    });
    await expect(desktop.installUpdate()).resolves.toBe(false);
    expect(invoke).not.toHaveBeenCalled();
  });

  it("tracks demo tool updates and per-browser native host registration", async () => {
    const previous = demo.getSettings();
    expect(await desktop.getToolchainStatus()).toMatchObject({
      ytdlp_version: "2026.11.17", ytdlp_source: "bundled", ffmpeg_source: "bundled",
      ffprobe_path: "C:\\SlipStream\\tools\\ffprobe.exe", ffprobe_source: "bundled", native_hosts_installed: false,
    });
    expect(await desktop.updateYtdlp()).toMatchObject({ ytdlp_version: "2026.11.22", ytdlp_source: "managed" });
    try {
      await desktop.saveSettings({ ...previous, ytdlp_path: "C:\\Custom\\yt-dlp.exe", ffmpeg_path: "C:\\Custom\\ffmpeg.exe" });
      expect(await desktop.getToolchainStatus()).toMatchObject({
        ytdlp_path: "C:\\Custom\\yt-dlp.exe", ytdlp_source: "custom", ffmpeg_source: "custom",
      });
    } finally {
      await desktop.saveSettings(previous);
    }
    expect(await desktop.listBrowsers()).toMatchObject([
      { id: "chrome", extension_id: DEFAULT_CHROMIUM_EXTENSION_ID, native_host_registered: false },
      { id: "edge", extension_id: DEFAULT_CHROMIUM_EXTENSION_ID, native_host_registered: false },
      { id: "brave", extension_id: DEFAULT_CHROMIUM_EXTENSION_ID, installed: false, native_host_registered: false },
      { id: "firefox", extension_id: DEFAULT_FIREFOX_EXTENSION_ID, native_host_registered: false },
    ]);
    expect(await desktop.openBrowserExtensionSetup("brave", "  custom-id  ")).toMatchObject({
      browser_id: "brave", extension_id: "custom-id", extension_page_url: "brave://extensions/",
      native_host_registered: true, launched_browser: true, revealed_extension_dir: true,
    });
    const browsers = await desktop.listBrowsers();
    expect(browsers.find(browser => browser.id === "brave")).toMatchObject({
      installed: true, native_host_registered: true, extension_id: "custom-id",
    });
    expect(browsers.filter(browser => browser.native_host_registered)).toHaveLength(1);
    expect(await desktop.getToolchainStatus()).toMatchObject({ native_hosts_installed: true });
    expect(invoke).not.toHaveBeenCalled();
  });

  it("publishes stateful telemetry, expands patterns and supports picker subscriptions", async () => {
    const updates = vi.fn();
    const open = vi.fn();
    const offTelemetry = await desktop.onTelemetry(updates);
    const offPicker = await desktop.onOpenPicker(open);
    try {
      const initial = await desktop.getSnapshot();
      expect(initial.transfers).toHaveLength(7);
      expect(initial.aggregate_down_bps).toBe(98_400_000);
      expect(initial.transfers[0].ui_segments).toHaveLength(16);
      const id = initial.transfers[0].id;
      await desktop.pauseTransfer(id);
      expect((await desktop.getSnapshot()).transfers.find(transfer => transfer.id === id)?.status).toBe("paused");
      await desktop.resumeTransfer(id);
      await desktop.setSpeedLimit(50_000_000);
      expect(updates.mock.lastCall?.[0].speed_limit_bps).toBe(50_000_000);
      expect(await desktop.expandWildcardUrls("https://example.org/[a-b]/[001-003:2].zip")).toEqual([
        "https://example.org/a/001.zip", "https://example.org/a/003.zip",
        "https://example.org/b/001.zip", "https://example.org/b/003.zip",
      ]);
      demo.openPicker("https://example.org/demo.zip");
      expect(open).toHaveBeenCalledWith("https://example.org/demo.zip");
      expect(invoke).not.toHaveBeenCalled();
    } finally {
      await desktop.setSpeedLimit(null);
      offTelemetry();
      offPicker();
    }
    demo.openPicker(null);
    expect(open).toHaveBeenCalledTimes(1);
  });
});
