import { emit } from "@tauri-apps/api/event";
import { clearMocks, mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_SETTINGS, demo } from "../lib/demo";
import { desktop, isTauri } from "../lib/tauri";

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
      ["hibernate_to_tray", {}],
    ]);
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
    expect(telemetry).toHaveBeenCalledWith(batch);
    expect(picker.mock.calls).toEqual([["magnet:?xt=example"], [null]]);
    await offTelemetry();
    await offPicker();
    await emit("slipstream://telemetry", batch);
    await emit("slipstream://open-picker", { url: "https://unused.example.org" });
    expect(telemetry).toHaveBeenCalledOnce();
    expect(picker).toHaveBeenCalledTimes(2);
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
