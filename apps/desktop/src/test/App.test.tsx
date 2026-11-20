import { StrictMode } from "react";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "../App";
import { demo } from "../lib/demo";
import { desktop } from "../lib/tauri";
import type { TelemetryBatch } from "../types/ipc";

const alpine = "Alpine_Cinema_4K_60fps.mp4";
const ubuntu = "Ubuntu-26.04-desktop-amd64.iso";
const blender = "Blender_5.2_Studio_Assets.torrent";
const symphony = "Symphony_Live_Master.flac";
const pdf = "Design_System_Specs_2026.pdf";
const mediaUrl = "https://example.org/watch?v=tokyo";

let pushTelemetry: (batch: TelemetryBatch) => void;
let openPicker: (url: string | null) => void;

beforeEach(() => {
  vi.spyOn(desktop, "onTelemetry").mockImplementation(async handler => {
    pushTelemetry = handler;
    return () => {};
  });
  vi.spyOn(desktop, "onOpenPicker").mockImplementation(async handler => {
    openPicker = handler;
    return () => {};
  });
});

async function start() {
  const user = userEvent.setup();
  render(<App />);
  await screen.findByRole("progressbar", { name: alpine + " progress" });
  return user;
}

const row = (filename: string) => screen.getByRole("article", { name: filename });
const inspector = () => within(screen.getByRole("complementary", { name: "Transfer inspector" }));
const dialog = (name: string | RegExp) => within(screen.getByRole("dialog", { name }));

describe("workspace and inspector", () => {
  it("filters by sidebar, segmented status, search, queue and night schedule", async () => {
    const user = await start();
    const nav = within(screen.getByRole("navigation", { name: "Download views" }));
    const toolbar = within(screen.getByRole("group", { name: "Transfer status" }));
    expect(row(pdf)).toBeVisible();
    expect(nav.getByRole("button", { name: /All Downloads/ })).toHaveTextContent("7");
    await user.click(nav.getByRole("button", { name: /Torrents/ }));
    expect(row(blender)).toBeVisible();
    expect(screen.queryByRole("article", { name: alpine })).not.toBeInTheDocument();
    await user.click(nav.getByRole("button", { name: /All Downloads/ }));
    await user.click(toolbar.getByRole("button", { name: "Active" }));
    expect(row(alpine)).toBeVisible();
    expect(screen.queryByRole("article", { name: symphony })).not.toBeInTheDocument();
    await user.click(toolbar.getByRole("button", { name: "Paused" }));
    expect(row(symphony)).toBeVisible();
    expect(screen.queryByRole("article", { name: blender })).not.toBeInTheDocument();
    await user.click(toolbar.getByRole("button", { name: "Done" }));
    expect(row(pdf)).toBeVisible();
    expect(screen.queryByRole("article", { name: alpine })).not.toBeInTheDocument();
    await user.click(toolbar.getByRole("button", { name: "All" }));
    const search = screen.getByRole("searchbox", { name: "Search downloads" });
    await user.type(search, "ubuntu");
    expect(row(ubuntu)).toBeVisible();
    expect(screen.queryByRole("article", { name: alpine })).not.toBeInTheDocument();
    await user.clear(search);
    await user.click(nav.getByRole("button", { name: /Queues/ }));
    expect(screen.getByRole("heading", { name: "Queues" })).toBeVisible();
    expect(screen.getByRole("heading", { name: "Default queue" })).toBeVisible();
    await user.click(nav.getByRole("button", { name: /Night Schedule/ }));
    expect(screen.getByRole("heading", { name: "Night Schedule" })).toBeVisible();
    expect(row(symphony)).toBeVisible();
    expect(screen.queryByRole("article", { name: blender })).not.toBeInTheDocument();
  });

  it("projects live ui_segments into 16 pills with active, torrent and paused treatments", async () => {
    await start();
    for (const [filename, mode, fill] of [[alpine, "active", "62%"], [blender, "torrent", "100%"], [symphony, "paused", "0%"]]) {
      const bar = within(row(filename)).getByRole("progressbar", { name: filename + " progress" });
      const pills = bar.querySelectorAll(".seg-cell." + mode);
      expect(pills).toHaveLength(16);
      expect(pills[11].firstElementChild).toHaveStyle({ width: fill });
      expect(bar).toHaveClass("tabular");
      expect(row(filename).querySelector(".transfer-stats")).toHaveClass("tabular");
    }
    const latest = demo.snapshot();
    const transfers = latest.transfers.map(transfer => transfer.id === "alpine"
      ? { ...transfer, ui_segments: Array<number>(16).fill(.25), download_speed_bps: 32_000_000 } : transfer);
    act(() => pushTelemetry({ ...latest, transfers }));
    expect(within(row(alpine)).getByText("32.0 MB/s")).toBeVisible();
    const bar = within(row(alpine)).getByRole("progressbar");
    expect(bar.querySelector(".seg-fill")).toHaveStyle({ width: "25%" });
  });

  it("toggles the inspector, shows the thread map and lists selectable torrent files", async () => {
    const list = vi.spyOn(desktop, "listTorrentFiles");
    const select = vi.spyOn(desktop, "setTorrentFiles").mockResolvedValue();
    const user = await start();
    expect(inspector().getByRole("tab", { name: "Overview" })).toHaveAttribute("aria-selected", "true");
    await user.keyboard("{Control>}i{/Control}");
    expect(screen.queryByRole("complementary", { name: "Transfer inspector" })).not.toBeInTheDocument();
    await user.keyboard("{Control>}i{/Control}");
    await user.click(inspector().getByRole("tab", { name: "Segments" }));
    expect(inspector().getByText("16-Thread Dynamic Split Map")).toBeVisible();
    expect(inspector().getAllByRole("progressbar")).toHaveLength(16);
    expect(inspector().getByText("T01")).toBeVisible();
    expect(inspector().getByText("T16")).toBeVisible();
    expect(inspector().getAllByText("Split")).toHaveLength(2);
    await user.click(within(row(blender)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    const textures = await inspector().findByRole("checkbox", { name: /Studio_Textures/ });
    expect(list).toHaveBeenCalledWith("blender");
    expect(textures).toBeChecked();
    await user.click(textures);
    await waitFor(() => expect(textures).not.toBeChecked());
    expect(select).toHaveBeenCalledWith("blender", [0, 2, 3]);
  });

  it("probes inspector media and opens the chosen format in Add Download", async () => {
    const spy = vi.spyOn(desktop, "probeMedia");
    const user = await start();
    await user.click(inspector().getByRole("tab", { name: "Media" }));
    const format = await inspector().findByRole("button", { name: /1440p QHD/ });
    expect(spy).toHaveBeenCalledWith("https://youtube.com/watch?v=alpine-cinema");
    await user.click(format);
    expect(dialog("Add Download").getByRole("button", { name: /1440p QHD/ })).toHaveAttribute("aria-pressed", "true");
  });

  it("uses the sidebar speed cap and quick transfer controls and restores re-added removed torrents", async () => {
    const pause = vi.spyOn(desktop, "pauseTransfer").mockResolvedValue();
    const cancel = vi.spyOn(desktop, "cancelTransfer").mockResolvedValue();
    const reveal = vi.spyOn(desktop, "revealInExplorer").mockResolvedValue();
    const cap = vi.spyOn(desktop, "setSpeedLimit").mockResolvedValue();
    const addTorrentSpy = vi.spyOn(desktop, "addTorrent").mockResolvedValue("blender");
    const user = await start();
    await user.click(screen.getByRole("button", { name: "Unlimited" }));
    expect(cap).toHaveBeenCalledWith(50_000_000);
    await user.click(within(row(alpine)).getByRole("button", { name: "Pause" }));
    expect(pause).toHaveBeenCalledWith("alpine");
    await user.click(within(row(pdf)).getByRole("button", { name: "Show in Explorer" }));
    expect(reveal).toHaveBeenCalledWith("design-pdf");
    await user.click(within(row(pdf)).getByRole("button", { name: /More actions/ }));
    await user.click(within(row(pdf)).getByRole("button", { name: "Remove" }));
    expect(cancel).toHaveBeenCalledWith("design-pdf", false);
    expect(screen.queryByRole("article", { name: pdf })).not.toBeInTheDocument();

    await user.click(within(row(blender)).getByRole("button", { name: /More actions/ }));
    await user.click(within(row(blender)).getByRole("button", { name: "Remove" }));
    expect(cancel).toHaveBeenCalledWith("blender", false);
    expect(screen.queryByRole("article", { name: blender })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Add" }));
    const sheet = dialog("Add Download");
    await user.type(sheet.getByRole("textbox", { name: "Download source" }), "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567");
    await user.click(sheet.getByRole("button", { name: "Download" }));
    expect(addTorrentSpy).toHaveBeenCalled();
    expect(row(blender)).toBeVisible();
  });

  it("uses exclusive end_byte, preserves pending torrent file updates across Inspector remounts, and clears stale Media state on URL refresh", async () => {
    const latest = demo.snapshot();
    const singleSegHttp = {
      ...latest.transfers[1],
      ui_segments: [1, 1, 1, 1, 1, 1, 1, 1, .5, 0, 0, 0, 0, 0, 0, 0],
      segments: [
        {
          index: 0,
          start_byte: 0,
          end_byte: 10,
          downloaded_bytes: 10,
          speed_bps: 0,
          state: "completed" as const,
        },
        {
          index: 1,
          start_byte: 10,
          end_byte: 20,
          downloaded_bytes: 5,
          speed_bps: 12_000_000,
          state: "active" as const,
        },
      ],
    };
    const getSnap = vi.spyOn(desktop, "getSnapshot").mockResolvedValue({
      ...latest,
      transfers: [singleSegHttp, ...latest.transfers.filter(item => item.id !== singleSegHttp.id)],
    });
    const probeSpy = vi.spyOn(desktop, "probeMedia");
    const listSpy = vi.spyOn(desktop, "listTorrentFiles");
    let resolveSelect!: () => void;
    const selectSpy = vi.spyOn(desktop, "setTorrentFiles").mockImplementation(
      () => new Promise<void>(resolve => { resolveSelect = resolve; }),
    );
    const user = await start();

    await user.click(within(row(ubuntu)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("tab", { name: "Segments" }));
    expect(inspector().getAllByText("Idle")).toHaveLength(14);
    expect(inspector().getAllByText("Done")).toHaveLength(1);
    expect(inspector().getByRole("progressbar", { name: "T01 progress" })).toHaveAttribute("aria-valuenow", "100");

    await user.click(inspector().getByRole("tab", { name: "Media" }));
    expect(probeSpy).not.toHaveBeenCalled();
    await user.click(inspector().getByRole("button", { name: "Probe Media Streams" }));
    expect(await inspector().findByRole("button", { name: /2160p 4K/ })).toBeVisible();

    getSnap.mockResolvedValue({
      ...latest,
      transfers: [{ ...singleSegHttp, source_url: "https://releases.ubuntu.com/26.04/refreshed.iso" },
        ...latest.transfers.filter(item => item.id !== singleSegHttp.id)],
    });
    vi.spyOn(desktop, "refreshDownloadUrl").mockResolvedValue();
    await user.click(inspector().getByRole("button", { name: "Refresh URL" }));
    const refDlg = dialog("Refresh Expired Link");
    await user.type(refDlg.getByLabelText("New URL"), "https://releases.ubuntu.com/26.04/refreshed.iso");
    await user.click(refDlg.getByRole("button", { name: "Refresh URL" }));
    expect(await inspector().findByRole("button", { name: "Probe Media Streams" })).toBeVisible();

    await user.click(within(row(blender)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    const textures = await inspector().findByRole("checkbox", { name: /Studio_Textures/ });
    expect(listSpy).toHaveBeenCalledTimes(1);
    await user.click(textures);
    expect(selectSpy).toHaveBeenCalledWith("blender", [0, 2, 3]);

    await user.keyboard("{Control>}i{/Control}");
    await user.keyboard("{Control>}i{/Control}");
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    expect(listSpy).toHaveBeenCalledTimes(1);
    expect(inspector().getByRole("checkbox", { name: /Studio_Textures/ })).not.toBeChecked();
    expect(inspector().getByRole("checkbox", { name: /Studio_Textures/ })).toBeDisabled();
    act(() => resolveSelect());
    await waitFor(() => expect(inspector().getByRole("checkbox", { name: /Studio_Textures/ })).toBeEnabled());
  });
});

describe("add and command palette", () => {
  it("handles Ctrl+K, command filtering, Ctrl+N, Escape and picker events", async () => {
    const user = await start();
    await user.keyboard("{Control>}k{/Control}");
    const palette = dialog("Command Palette");
    expect(palette.getByRole("button", { name: /Toggle Inspector/ })).toBeVisible();
    await user.type(palette.getByRole("textbox"), "Ubuntu");
    await user.keyboard("{Enter}");
    expect(screen.getByRole("searchbox")).toHaveValue("Ubuntu");
    expect(row(ubuntu)).toBeVisible();
    expect(screen.queryByRole("article", { name: alpine })).not.toBeInTheDocument();
    await user.keyboard("{Control>}n{/Control}");
    expect(dialog("Add Download").getByRole("textbox", { name: "Download source" })).toHaveFocus();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    act(() => openPicker(mediaUrl));
    const add = dialog("Add Download");
    expect(add.getByRole("textbox", { name: "Download source" })).toHaveValue(mediaUrl);
    expect(await add.findByRole("button", { name: /2160p 4K/ })).toBeVisible();
  });

  it("probes video, selects audio and subtitle options, and submits the chosen media format", async () => {
    const probe = vi.spyOn(desktop, "probeMedia");
    const add = vi.spyOn(desktop, "addMediaDownload").mockResolvedValue("media-id");
    const user = await start();
    await user.click(screen.getByRole("button", { name: "Add" }));
    const sheet = dialog("Add Download");
    await user.type(sheet.getByRole("textbox", { name: "Download source" }), mediaUrl);
    await user.click(sheet.getByRole("button", { name: "Probe" }));
    const video = await sheet.findByRole("button", { name: /2160p 4K/ });
    expect(probe).toHaveBeenCalledWith(mediaUrl);
    expect(video).toHaveAttribute("aria-pressed", "true");
    await user.click(sheet.getByRole("tab", { name: "Audio Only" }));
    for (const name of [/MP3 · High Quality/, /M4A · AAC/, /^Opus/, /FLAC · Lossless/, /WAV · Uncompressed/, /Audio · Lossless Studio/]) {
      expect(sheet.getByRole("button", { name })).toBeVisible();
    }
    await user.click(sheet.getByRole("tab", { name: "Subtitles" }));
    await user.click(sheet.getByRole("checkbox", { name: /Arabic/ }));
    await user.click(sheet.getByRole("tab", { name: "Video" }));
    const remux = sheet.getByRole("switch", { name: "Lossless Remux" });
    expect(remux.querySelector(".mac-switch")).toHaveClass("on");
    await user.click(remux);
    await user.click(sheet.getByRole("switch", { name: "Audio Track Normalization" }));
    expect(remux).toHaveAttribute("aria-checked", "false");
    await user.click(sheet.getByRole("button", { name: "Download" }));
    expect(add).toHaveBeenCalledWith(expect.objectContaining({
      url: mediaUrl, format_id: "401", embed_subtitles_langs: ["en", "ar"], lossless_remux: false, normalize_audio: true,
    }));
    expect(await screen.findByText("Download added.")).toBeVisible();
  });

  it("pairs adaptive video-only streams with +bestaudio and strips subtitles on audio-only downloads", async () => {
    vi.spyOn(desktop, "probeMedia").mockResolvedValue({
      title: "Adaptive Test",
      source_url: mediaUrl,
      extractor: "youtube",
      duration_seconds: 120,
      thumbnail_url: null,
      is_playlist: false,
      playlist_count: null,
      formats: [
        {
          format_id: "137",
          label: "1080p HD · 60fps",
          container: "mp4",
          video_codec: "H.264",
          audio_codec: null,
          height: 1080,
          fps: 60,
          filesize_bytes: 250_000_000,
          is_audio_only: false,
          has_video: true,
          has_audio: false,
        },
        {
          format_id: "140",
          label: "Audio · AAC",
          container: "m4a",
          video_codec: null,
          audio_codec: "AAC",
          height: null,
          fps: null,
          filesize_bytes: 12_000_000,
          is_audio_only: true,
          has_video: false,
          has_audio: true,
        },
      ],
      subtitles: [{ lang: "en", name: "English", is_auto: false }],
    });
    const add = vi.spyOn(desktop, "addMediaDownload").mockResolvedValue("media-id");
    const user = await start();

    await user.click(screen.getByRole("button", { name: "Add" }));
    let sheet = dialog("Add Download");
    await user.type(sheet.getByRole("textbox", { name: "Download source" }), mediaUrl);
    await user.click(sheet.getByRole("button", { name: "Probe" }));
    expect(await sheet.findByRole("button", { name: /1080p HD/ })).toBeVisible();
    await user.click(sheet.getByRole("button", { name: "Download" }));
    expect(add).toHaveBeenLastCalledWith(expect.objectContaining({
      format_id: "137+bestaudio",
      embed_subtitles_langs: ["en"],
    }));

    await user.click(screen.getByRole("button", { name: "Add" }));
    sheet = dialog("Add Download");
    await user.type(sheet.getByRole("textbox", { name: "Download source" }), mediaUrl);
    await user.click(sheet.getByRole("button", { name: "Probe" }));
    expect(await sheet.findByRole("button", { name: /1080p HD/ })).toBeVisible();
    await user.click(sheet.getByRole("tab", { name: "Audio Only" }));
    expect(sheet.queryByRole("switch", { name: "Embed Subtitles" })).not.toBeInTheDocument();
    await user.click(sheet.getByRole("button", { name: "Download" }));
    expect(add).toHaveBeenLastCalledWith(expect.objectContaining({
      audio_only_preset: "mp3_320",
      embed_subtitles_langs: [],
    }));
  });

  it("submits direct HTTP, torrent file and wildcard batch requests", async () => {
    const http = vi.spyOn(desktop, "addHttpDownload").mockResolvedValue("http-id");
    const torrent = vi.spyOn(desktop, "addTorrent").mockResolvedValue("torrent-id");
    const expand = vi.spyOn(desktop, "expandWildcardUrls");
    const user = await start();

    await user.click(screen.getByRole("button", { name: "Add" }));
    let sheet = dialog("Add Download");
    await user.type(sheet.getByRole("textbox", { name: "Download source" }), "https://example.org/archive.zip");
    await user.selectOptions(sheet.getByLabelText("Threads"), "32");
    await user.click(sheet.getByText("Filename / Expected SHA-256"));
    await user.type(sheet.getByRole("textbox", { name: "Filename" }), "studio.zip");
    await user.click(sheet.getByRole("button", { name: "Download" }));
    expect(http).toHaveBeenCalledWith(expect.objectContaining({
      url: "https://example.org/archive.zip", filename: "studio.zip", max_connections: 32, headers: {},
    }));

    await user.keyboard("{Control>}n{/Control}");
    sheet = dialog("Add Download");
    await user.click(sheet.getByRole("tab", { name: "Torrent / Magnet" }));
    await user.upload(sheet.getByLabelText("Choose a .torrent file"), new File(["demo"], "sample.torrent", { type: "application/x-bittorrent" }));
    await user.click(sheet.getByRole("switch", { name: "Sequential" }));
    await user.click(sheet.getByRole("button", { name: "Download" }));
    expect(torrent).toHaveBeenCalledWith({ source: "sample.torrent", save_dir: "D:\\Downloads", sequential: true });

    await user.click(screen.getByRole("button", { name: "Add" }));
    sheet = dialog("Add Download");
    const pattern = "https://example.org/file_[001-003].zip";
    fireEvent.change(sheet.getByRole("textbox", { name: "Download source" }), { target: { value: pattern } });
    expect(sheet.getByText("Wildcard Batch Preview")).toBeVisible();
    await user.click(sheet.getByRole("button", { name: "Probe" }));
    expect(expand).toHaveBeenCalledWith(pattern);
    expect(await sheet.findByText("https://example.org/file_002.zip")).toBeVisible();
    await user.click(sheet.getByRole("button", { name: "Download" }));
    expect(await screen.findByText("3 downloads added.")).toBeVisible();
    expect(http.mock.calls.slice(1).map(([request]) => request.url)).toEqual([
      "https://example.org/file_001.zip", "https://example.org/file_002.zip", "https://example.org/file_003.zip",
    ]);
  });
});


describe("appearance, settings and transfer dialogs", () => {
  it("switches dark, light, system, blue and mono using root data attributes", async () => {
    const listeners = new Set<() => void>();
    const system = {
      matches: false,
      addEventListener: (_event: string, listener: () => void) => { listeners.add(listener); },
      removeEventListener: (_event: string, listener: () => void) => { listeners.delete(listener); },
    };
    vi.stubGlobal("matchMedia", vi.fn(() => system));
    const save = vi.spyOn(desktop, "saveSettings").mockResolvedValue();
    const user = await start();
    const html = document.documentElement;
    expect(html).toHaveAttribute("data-theme", "dark");
    expect(html).toHaveAttribute("data-accent", "blue");
    await user.click(screen.getByRole("button", { name: "Graphite" }));
    expect(html).toHaveAttribute("data-theme", "light");
    await user.click(screen.getByRole("button", { name: "Switch to Monochrome accent" }));
    expect(html).toHaveAttribute("data-accent", "mono");
    await user.click(screen.getByRole("button", { name: "Switch to Blue accent" }));
    expect(html).toHaveAttribute("data-accent", "blue");
    await user.click(screen.getByRole("button", { name: "Settings" }));
    const settings = dialog(/Settings & Browser Integration/);
    await user.selectOptions(settings.getByLabelText("Theme"), "system");
    await user.selectOptions(settings.getByLabelText("Accent"), "mono");
    await user.click(settings.getByRole("button", { name: "Save Settings" }));
    await waitFor(() => expect(save).toHaveBeenLastCalledWith(expect.objectContaining({ theme: "system", accent: "mono" })));
    expect(html).toHaveAttribute("data-theme", "light");
    expect(html).toHaveAttribute("data-accent", "mono");
    act(() => {
      system.matches = true;
      listeners.forEach(listener => listener());
    });
    expect(html).toHaveAttribute("data-theme", "dark");
    expect(window.matchMedia).toHaveBeenCalledWith("(prefers-color-scheme: dark)");
  });

  it("shows the toolchain and installs and saves browser integration settings", async () => {
    const getTools = vi.spyOn(desktop, "getToolchainStatus");
    const install = vi.spyOn(desktop, "installBrowserHosts").mockResolvedValue(["Chrome", "Firefox"]);
    const save = vi.spyOn(desktop, "saveSettings").mockResolvedValue();
    const hibernate = vi.spyOn(desktop, "hibernateToTray").mockResolvedValue();
    const user = await start();
    await user.click(screen.getByRole("button", { name: "Settings" }));
    const settings = dialog("Settings & Browser Integration");
    expect(await settings.findByText(/yt-dlp .* FFmpeg/)).toBeVisible();
    expect(getTools).toHaveBeenCalled();
    await user.type(settings.getByLabelText("Chrome extension ID"), "chrome-demo");
    await user.type(settings.getByLabelText("Firefox extension ID"), "firefox@demo.test");
    await user.click(settings.getByRole("button", { name: "Install Browser Native Hosts" }));
    expect(install).toHaveBeenCalledWith("chrome-demo", null, "firefox@demo.test");
    expect(await settings.findByText("Browser hosts installed for Chrome, Firefox.")).toBeVisible();
    expect(getTools).toHaveBeenCalledTimes(2);
    await user.click(settings.getByRole("button", { name: "Hibernate Now" }));
    expect(hibernate).toHaveBeenCalledOnce();
    await user.selectOptions(settings.getByLabelText("Max connections"), "24");
    await user.type(settings.getByLabelText("Global speed limit in MB/s"), "25.5");
    await user.click(settings.getByRole("switch", { name: "Monitor clipboard" }));
    await user.click(settings.getByRole("button", { name: "Save Settings" }));
    expect(save).toHaveBeenCalledWith(expect.objectContaining({
      default_connections: 24, global_speed_limit_bps: 25_500_000, monitor_clipboard: true,
    }));
    expect(await screen.findByText("Settings saved.")).toBeVisible();
  });

  it("refreshes an HTTP link with optional request fields, excludes Refresh URL on media, and reports checksum results", async () => {
    const refresh = vi.spyOn(desktop, "refreshDownloadUrl").mockResolvedValue();
    const verify = vi.spyOn(desktop, "verifyTransferChecksum").mockResolvedValueOnce(false).mockResolvedValueOnce(true);
    const user = await start();
    expect(inspector().queryByRole("button", { name: "Refresh URL" })).not.toBeInTheDocument();
    await user.click(within(row(ubuntu)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("button", { name: "Refresh URL" }));
    const refreshDialog = dialog("Refresh Expired Link");
    await user.type(refreshDialog.getByLabelText("New URL"), "https://example.org/new.iso");
    await user.click(refreshDialog.getByText("Request options"));
    await user.type(refreshDialog.getByLabelText("Referrer"), "https://example.org/");
    await user.type(refreshDialog.getByLabelText("HTTP headers"), "Accept: application/octet-stream");
    await user.click(refreshDialog.getByRole("button", { name: "Refresh URL" }));
    expect(refresh).toHaveBeenCalledWith({ transfer_id: "ubuntu", new_url: "https://example.org/new.iso",
      headers: { Accept: "application/octet-stream" }, referrer: "https://example.org/" });
    expect(await screen.findByText("Download link refreshed.")).toBeVisible();

    await user.click(within(row(pdf)).getByRole("button", { name: "SHA-256" }));
    const verifyDialog = dialog("Verify SHA-256 Checksum");
    const input = verifyDialog.getByLabelText("Expected SHA-256");
    await user.type(input, "bad");
    await user.click(verifyDialog.getByRole("button", { name: "Verify SHA-256" }));
    expect(verifyDialog.getByRole("alert")).toHaveTextContent("64 hexadecimal characters");
    expect(verify).not.toHaveBeenCalled();
    await user.clear(input);
    const digest = "a".repeat(64);
    await user.type(input, digest);
    await user.click(verifyDialog.getByRole("button", { name: "Verify SHA-256" }));
    expect(verify).toHaveBeenCalledWith("design-pdf", digest);
    expect(verifyDialog.getByRole("status")).toHaveTextContent("SHA-256 checksum does not match.");
    await user.click(verifyDialog.getByRole("button", { name: "Verify SHA-256" }));
    expect(verifyDialog.getByRole("status")).toHaveTextContent("SHA-256 checksum matches.");
    await user.click(verifyDialog.getByRole("button", { name: "Done" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("prevents settings overwrite on startup failure, rejects fractional-zero limits, clears toast on retry, and preserves queued limit edits", async () => {
    const getSettingsSpy = vi.spyOn(desktop, "getSettings").mockRejectedValue(new Error("Settings unreadable"));
    const saveSpy = vi.spyOn(desktop, "saveSettings").mockResolvedValue();
    const user = await start();

    expect(await screen.findByRole("alert")).toHaveTextContent("Settings unreadable");
    await user.click(screen.getByRole("button", { name: "Graphite" }));
    expect(saveSpy).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Settings" }));
    const settingsDlg = dialog("Settings & Browser Integration");
    expect(settingsDlg.getByRole("button", { name: "Save Settings" })).toBeDisabled();

    getSettingsSpy.mockResolvedValue({ ...demo.getSettings(), theme: "dark", accent: "blue" });
    await user.click(settingsDlg.getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(settingsDlg.getByRole("button", { name: "Save Settings" })).toBeEnabled());
    expect(screen.queryByText("Settings unreadable")).not.toBeInTheDocument();

    fireEvent.change(settingsDlg.getByLabelText("Global speed limit in MB/s"), { target: { value: "0.0000004" } });
    fireEvent.submit(settingsDlg.getByRole("button", { name: "Save Settings" }).closest("form")!);
    expect(settingsDlg.getByRole("alert")).toHaveTextContent("positive finite speed limit");
    expect(saveSpy).not.toHaveBeenCalled();
    await user.click(settingsDlg.getByRole("button", { name: "Cancel" }));

    let resolveCap!: () => void;
    vi.spyOn(desktop, "setSpeedLimit").mockImplementation(() => new Promise<void>(resolve => { resolveCap = resolve; }));
    await user.click(screen.getByRole("button", { name: "Unlimited" }));
    await user.click(screen.getByRole("button", { name: "Settings" }));
    const openSettingsDlg = dialog("Settings & Browser Integration");
    await user.click(openSettingsDlg.getByRole("switch", { name: "Monitor clipboard" }));
    act(() => resolveCap());
    await user.click(openSettingsDlg.getByRole("button", { name: "Save Settings" }));
    await waitFor(() => expect(saveSpy).toHaveBeenCalledWith(expect.objectContaining({
      monitor_clipboard: true,
      global_speed_limit_bps: 50_000_000,
    })));
  });

  it("handles StrictMode auto-probe, routes local wildcard torrents vs https .torrent URLs, preserves FIFO pickers, and protects open modals on Ctrl+K/N", async () => {
    let rejectAdd!: (err: Error) => void;
    const httpSpy = vi.spyOn(desktop, "addHttpDownload").mockImplementation(
      () => new Promise<string>((_resolve, reject) => { rejectAdd = reject; }),
    );
    const torrentSpy = vi.spyOn(desktop, "addTorrent").mockResolvedValue("torrent-id");
    const expandSpy = vi.spyOn(desktop, "expandWildcardUrls");
    const probeSpy = vi.spyOn(desktop, "probeMedia");
    let dropHandler!: (event: { type: string; paths: string[] }) => void;
    vi.spyOn(desktop, "onFileDrop").mockImplementation(async handler => {
      dropHandler = handler as unknown as typeof dropHandler;
      return () => Promise.reject(new Error("teardown ignored"));
    });
    const user = userEvent.setup();
    render(<StrictMode><App /></StrictMode>);
    await screen.findByRole("progressbar", { name: alpine + " progress" });

    act(() => openPicker(mediaUrl));
    const probedSheet = dialog("Add Download");
    expect(await probedSheet.findByRole("button", { name: /2160p 4K/ })).toBeVisible();
    expect(probeSpy).toHaveBeenCalled();
    await user.click(probedSheet.getByRole("button", { name: "Cancel" }));

    await user.click(screen.getByRole("button", { name: "Add" }));
    const sheet = dialog("Add Download");
    await user.type(sheet.getByRole("textbox", { name: "Download source" }), "https://example.org/file.torrent");
    await user.clear(sheet.getByLabelText("Save folder"));
    await user.type(sheet.getByLabelText("Save folder"), "E:\\Custom");
    const downloadBtn = sheet.getByRole("button", { name: "Download" });
    fireEvent.click(downloadBtn);
    fireEvent.click(downloadBtn);
    expect(httpSpy).toHaveBeenCalledTimes(1);
    expect(torrentSpy).not.toHaveBeenCalled();

    await user.keyboard("{Control>}k{/Control}");
    expect(screen.queryByRole("dialog", { name: "Command Palette" })).not.toBeInTheDocument();
    await user.keyboard("{Control>}n{/Control}");
    expect(sheet.getByDisplayValue("E:\\Custom")).toBeVisible();

    act(() => openPicker("https://example.org/queued-1.mp4"));
    act(() => openPicker("https://example.org/queued-2.mp4"));
    act(() => rejectAdd(new Error("Network down")));
    expect(await sheet.findByRole("alert")).toHaveTextContent("Network down");

    act(() => dropHandler({ type: "drop", paths: ["C:\\Downloads\\Clip [1-2].torrent"] }));
    expect(sheet.getByRole("textbox", { name: "Download source" })).toHaveValue("C:\\Downloads\\Clip [1-2].torrent");
    expect(sheet.queryByText("Wildcard Batch Preview")).not.toBeInTheDocument();
    expect(expandSpy).not.toHaveBeenCalled();
    await user.click(sheet.getByRole("button", { name: "Download" }));
    expect(torrentSpy).toHaveBeenCalledWith(expect.objectContaining({ source: "C:\\Downloads\\Clip [1-2].torrent" }));

    const firstQueued = await screen.findByRole("dialog", { name: "Add Download" });
    expect(within(firstQueued).getByRole("textbox", { name: "Download source" })).toHaveValue("https://example.org/queued-1.mp4");
    await user.click(within(firstQueued).getByRole("button", { name: "Cancel" }));
    const secondQueued = await screen.findByRole("dialog", { name: "Add Download" });
    expect(within(secondQueued).getByRole("textbox", { name: "Download source" })).toHaveValue("https://example.org/queued-2.mp4");
  });
});
