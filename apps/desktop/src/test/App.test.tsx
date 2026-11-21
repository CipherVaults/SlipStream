import { StrictMode } from "react";
import { clearMocks, mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "../App";
import { demo } from "../lib/demo";
import { desktop } from "../lib/tauri";
import type { TelemetryBatch, UpdateStatus } from "../types/ipc";

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
    const listSpy = vi.spyOn(desktop, "listTorrentFiles");
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

    await user.click(within(row(blender)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    expect(await inspector().findByRole("checkbox", { name: /Studio_Textures/ })).toBeVisible();
    expect(listSpy).toHaveBeenCalledTimes(1);

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
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    expect(await inspector().findByRole("checkbox", { name: /Studio_Textures/ })).toBeVisible();
    expect(listSpy).toHaveBeenCalledTimes(2);
  });

  it("retries unresolved magnet errors on return to Torrent Files", async () => {
    const listSpy = vi.spyOn(desktop, "listTorrentFiles")
      .mockRejectedValueOnce(new Error("Metadata pending"))
      .mockResolvedValueOnce([{ index: 0, relative_path: "Resolved_File.bin", size_bytes: 10_000_000, downloaded_bytes: 0, selected: true }]);
    const user = await start();
    await user.click(within(row(blender)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    expect(await inspector().findByRole("alert")).toHaveTextContent("Metadata pending");

    await user.click(inspector().getByRole("tab", { name: "Overview" }));
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    expect(await inspector().findByRole("checkbox", { name: /Resolved_File\.bin/ })).toBeChecked();
    expect(listSpy).toHaveBeenCalledTimes(2);
  });

  it("auto-retries magnet files when total_bytes resolves mid-load", async () => {
    const baseSnap = demo.snapshot();
    const unresolved = {
      ...baseSnap.transfers.find(item => item.id === "blender")!,
      total_bytes: null,
      downloaded_bytes: 0,
    };
    const resolvedSnap = {
      ...baseSnap,
      timestamp: new Date(Date.parse(baseSnap.timestamp) + 1000).toISOString(),
    };
    vi.spyOn(desktop, "getSnapshot").mockResolvedValue({
      ...baseSnap,
      transfers: baseSnap.transfers.map(item => (item.id === "blender" ? unresolved : item)),
    });
    let rejectFirst!: (error: Error) => void;
    const listSpy = vi.spyOn(desktop, "listTorrentFiles")
      .mockImplementationOnce(() => new Promise((_, reject) => { rejectFirst = reject; }))
      .mockResolvedValueOnce([{ index: 0, relative_path: "Resolved_MidLoad.bin", size_bytes: 5_000_000, downloaded_bytes: 0, selected: true }]);

    const user = await start();
    await user.click(within(row(blender)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    await waitFor(() => expect(listSpy).toHaveBeenCalledTimes(1));

    act(() => pushTelemetry(resolvedSnap));
    act(() => rejectFirst(new Error("Metadata pending")));

    expect(await inspector().findByRole("checkbox", { name: /Resolved_MidLoad\.bin/ })).toBeChecked();
    expect(listSpy).toHaveBeenCalledTimes(2);
  });

  it("discards stale listTorrentFiles across re-add and purges vanished setTorrentFiles entries on settle", async () => {
    const baseSnap = demo.snapshot();
    const t0 = Date.parse(baseSnap.timestamp);
    const snapWithoutBlender = {
      ...baseSnap,
      timestamp: new Date(t0 + 1000).toISOString(),
      transfers: baseSnap.transfers.filter(item => item.id !== "blender"),
    };
    const snapReadded = {
      ...baseSnap,
      timestamp: new Date(t0 + 2000).toISOString(),
    };
    const snapDroppedAgain = {
      ...baseSnap,
      timestamp: new Date(t0 + 3000).toISOString(),
      transfers: baseSnap.transfers.filter(item => item.id !== "blender"),
    };
    const snapRestored = {
      ...baseSnap,
      timestamp: new Date(t0 + 4000).toISOString(),
    };

    const getSnapSpy = vi.spyOn(desktop, "getSnapshot").mockResolvedValue(baseSnap);
    vi.spyOn(desktop, "addTorrent").mockResolvedValue("blender");

    let resolveStaleList!: (files: Awaited<ReturnType<typeof desktop.listTorrentFiles>>) => void;
    let resolveSet!: () => void;
    vi.spyOn(desktop, "setTorrentFiles").mockImplementation(() => new Promise<void>(resolve => { resolveSet = resolve; }));

    const listSpy = vi.spyOn(desktop, "listTorrentFiles")
      .mockImplementationOnce(() => new Promise(resolve => { resolveStaleList = resolve; }))
      .mockResolvedValueOnce([{ index: 0, relative_path: "Fresh_Readded.bin", size_bytes: 8_000_000, downloaded_bytes: 0, selected: true }])
      .mockResolvedValueOnce([{ index: 0, relative_path: "After_Vanish.bin", size_bytes: 9_000_000, downloaded_bytes: 0, selected: true }]);

    const user = await start();
    await user.click(within(row(blender)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    await waitFor(() => expect(listSpy).toHaveBeenCalledTimes(1));

    act(() => pushTelemetry(snapWithoutBlender));
    expect(screen.queryByRole("article", { name: blender })).not.toBeInTheDocument();

    getSnapSpy.mockResolvedValue(snapReadded);
    await user.click(screen.getByRole("button", { name: "Add" }));
    const sheet = dialog("Add Download");
    await user.type(sheet.getByRole("textbox", { name: "Download source" }), "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567");
    await user.click(sheet.getByRole("button", { name: "Download" }));
    expect(row(blender)).toBeVisible();

    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    const freshCheckbox = await inspector().findByRole("checkbox", { name: /Fresh_Readded\.bin/ });
    expect(freshCheckbox).toBeChecked();

    act(() => resolveStaleList([{ index: 0, relative_path: "Stale_Old.bin", size_bytes: 1_000, downloaded_bytes: 0, selected: false }]));
    expect(inspector().queryByRole("checkbox", { name: /Stale_Old\.bin/ })).not.toBeInTheDocument();
    expect(inspector().getByRole("checkbox", { name: /Fresh_Readded\.bin/ })).toBeChecked();

    await user.click(freshCheckbox);
    act(() => pushTelemetry(snapDroppedAgain));
    expect(screen.queryByRole("article", { name: blender })).not.toBeInTheDocument();
    await act(async () => { resolveSet(); });

    act(() => pushTelemetry(snapRestored));
    await user.click(within(row(blender)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    expect(await inspector().findByRole("checkbox", { name: /After_Vanish\.bin/ })).toBeChecked();
    expect(listSpy).toHaveBeenCalledTimes(3);
  });

  it("cancels queued setTorrentFiles and unblocks fresh file loading across same-ID Remove and re-add", async () => {
    const baseSnap = demo.snapshot();
    const blenderCompleted = {
      ...baseSnap.transfers.find(item => item.id === "blender")!,
      status: "completed" as const,
      downloaded_bytes: 2_320_000_000,
      total_bytes: 2_320_000_000,
    };
    const initialSnap = {
      ...baseSnap,
      transfers: baseSnap.transfers.map(item => (item.id === "blender" ? blenderCompleted : item)),
    };
    const readdedSnap = {
      ...baseSnap,
      timestamp: new Date(Date.parse(baseSnap.timestamp) + 1000).toISOString(),
    };
    const getSnapSpy = vi.spyOn(desktop, "getSnapshot").mockResolvedValue(initialSnap);
    vi.spyOn(desktop, "cancelTransfer").mockResolvedValue();
    vi.spyOn(desktop, "addTorrent").mockResolvedValue("blender");

    let resolveInFlightSet!: () => void;
    const setSpy = vi.spyOn(desktop, "setTorrentFiles")
      .mockImplementationOnce(() => new Promise<void>(resolve => { resolveInFlightSet = resolve; }))
      .mockResolvedValue();
    const listSpy = vi.spyOn(desktop, "listTorrentFiles")
      .mockResolvedValueOnce([
        { index: 0, relative_path: "First.bin", size_bytes: 1_000_000, downloaded_bytes: 1_000_000, selected: true },
        { index: 1, relative_path: "Second.bin", size_bytes: 2_000_000, downloaded_bytes: 2_000_000, selected: true },
      ])
      .mockResolvedValueOnce([
        { index: 0, relative_path: "Readded_File.bin", size_bytes: 5_000_000, downloaded_bytes: 0, selected: true },
      ]);

    const user = await start();
    await user.click(within(row(blender)).getByRole("button", { name: /Inspect/ }));
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    const firstBox = await inspector().findByRole("checkbox", { name: /First\.bin/ });
    const secondBox = inspector().getByRole("checkbox", { name: /Second\.bin/ });

    // Queue two writes while first stays in-flight
    await user.click(firstBox);
    await user.click(secondBox);
    expect(setSpy).toHaveBeenCalledTimes(1);

    // Remove while writes are pending/queued, then immediately re-add with the same transfer ID
    await user.click(within(row(blender)).getByRole("button", { name: /More actions/ }));
    await user.click(within(row(blender)).getByRole("button", { name: "Remove" }));
    expect(screen.queryByRole("article", { name: blender })).not.toBeInTheDocument();

    getSnapSpy.mockResolvedValue(readdedSnap);
    await user.click(screen.getByRole("button", { name: "Add" }));
    const sheet = dialog("Add Download");
    await user.type(sheet.getByRole("textbox", { name: "Download source" }), "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567");
    await user.click(sheet.getByRole("button", { name: "Download" }));

    // Fresh load succeeds immediately without getting blocked by old pending count
    await user.click(inspector().getByRole("tab", { name: "Torrent Files" }));
    expect(await inspector().findByRole("checkbox", { name: /Readded_File\.bin/ })).toBeChecked();
    expect(listSpy).toHaveBeenCalledTimes(2);

    // Resolving the old in-flight write neither fires the queued stale write nor wipes the new torrent cache
    await act(async () => { resolveInFlightSet(); });
    expect(setSpy).toHaveBeenCalledTimes(1);
    expect(inspector().getByRole("checkbox", { name: /Readded_File\.bin/ })).toBeChecked();
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

  it("shows About & Updates and blocks other settings actions while checking", async () => {
    let resolveCheck!: (status: UpdateStatus) => void;
    const check = vi.spyOn(desktop, "checkForUpdates").mockImplementation(
      () => new Promise<UpdateStatus>(resolve => { resolveCheck = resolve; }),
    );
    const user = await start();
    await user.click(screen.getByRole("button", { name: "Settings" }));
    const settings = dialog("Settings & Browser Integration");
    const about = within(settings.getByRole("region", { name: "About & Updates" }));

    expect(about.getByText("SlipStream v1.0.0")).toBeVisible();
    expect(about.getByText("CipherVaults · Windows 10/11 · MIT License")).toBeVisible();
    expect(about.getByText("Signed NSIS Auto-Update (GitHub Releases)")).toBeVisible();
    await user.click(about.getByRole("button", { name: "Check for Updates" }));
    expect(check).toHaveBeenCalledOnce();
    expect(about.getByRole("button", { name: "Checking…" })).toBeDisabled();
    expect(settings.getByRole("button", { name: "Save Settings" })).toBeDisabled();
    expect(settings.getByRole("button", { name: "Cancel" })).toBeDisabled();

    await act(async () => resolveCheck({ available: false, current_version: "1.0.0" }));
    expect(about.getByRole("status")).toHaveTextContent("Up to date (v1.0.0).");
    expect(about.queryByRole("button", { name: "Install & Restart" })).not.toBeInTheDocument();
    expect(settings.getByRole("button", { name: "Save Settings" })).toBeEnabled();
  });

  it("offers an available update with release details and invokes Install & Restart", async () => {
    vi.spyOn(desktop, "checkForUpdates").mockResolvedValue({
      available: true, current_version: "1.0.0", version: "1.1.0",
      notes: "Improved download recovery.", pub_date: "2026-11-21 0:00:00.0 +00:00:00",
    });
    const install = vi.spyOn(desktop, "installUpdate")
      .mockRejectedValueOnce(new Error("Update signature verification failed"))
      .mockResolvedValueOnce(true);
    const save = vi.spyOn(desktop, "saveSettings");
    const user = await start();
    await user.click(screen.getByRole("button", { name: "Settings" }));
    const settings = dialog("Settings & Browser Integration");
    const about = within(settings.getByRole("region", { name: "About & Updates" }));

    await user.click(about.getByRole("button", { name: "Check for Updates" }));
    expect(about.getByRole("status")).toHaveTextContent("Update v1.1.0 available.");
    expect(about.getByText("Improved download recovery.")).toBeVisible();
    expect(about.getByText("Released 2026-11-21")).toBeVisible();
    await user.click(about.getByRole("button", { name: "Install & Restart" }));
    expect(await settings.findByRole("alert")).toHaveTextContent("Update signature verification failed");
    await user.click(about.getByRole("button", { name: "Install & Restart" }));
    expect(install).toHaveBeenCalledTimes(2);
    expect(save).not.toHaveBeenCalled();
    expect(settings.queryByRole("alert")).not.toBeInTheDocument();
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

  it("prevents settings overwrite on startup failure, rejects fractional-zero limits & >32 concurrent, clears earlier toast on retry, and preserves queued limit edits", async () => {
    const getSettingsSpy = vi.spyOn(desktop, "getSettings")
      .mockRejectedValueOnce(new Error("Startup error"))
      .mockRejectedValueOnce(new Error("Click error"))
      .mockRejectedValueOnce(new Error("Secondary error"));
    const saveSpy = vi.spyOn(desktop, "saveSettings").mockResolvedValue();
    const user = await start();

    expect(await screen.findByRole("alert")).toHaveTextContent("Startup error");
    await user.click(screen.getByRole("button", { name: "Graphite" }));
    expect(saveSpy).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Settings" }));
    const settingsDlg = dialog("Settings & Browser Integration");
    expect(await settingsDlg.findByText("Secondary error")).toBeVisible();
    expect(settingsDlg.getByRole("button", { name: "Save Settings" })).toBeDisabled();

    getSettingsSpy.mockResolvedValue({ ...demo.getSettings(), theme: "dark", accent: "blue" });
    await user.click(settingsDlg.getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(settingsDlg.getByRole("button", { name: "Save Settings" })).toBeEnabled());
    expect(screen.queryByText("Startup error")).not.toBeInTheDocument();
    expect(screen.queryByText("Click error")).not.toBeInTheDocument();
    expect(screen.queryByText("Secondary error")).not.toBeInTheDocument();

    fireEvent.change(settingsDlg.getByLabelText("Global speed limit in MB/s"), { target: { value: "0.0000004" } });
    fireEvent.submit(settingsDlg.getByRole("button", { name: "Save Settings" }).closest("form")!);
    expect(settingsDlg.getByRole("alert")).toHaveTextContent("positive finite speed limit");
    expect(saveSpy).not.toHaveBeenCalled();

    fireEvent.change(settingsDlg.getByLabelText("Global speed limit in MB/s"), { target: { value: "" } });
    fireEvent.change(settingsDlg.getByLabelText("Max concurrent downloads"), { target: { value: "64" } });
    fireEvent.submit(settingsDlg.getByRole("button", { name: "Save Settings" }).closest("form")!);
    expect(settingsDlg.getByRole("alert")).toHaveTextContent("between 1 and 32");
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

  it("handles StrictMode auto-probe without duplicate IPC, skips autoProbe on https .torrent, handles pre-commit picker bursts, and protects open modals on Ctrl+K/N", async () => {
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
    expect(probeSpy).toHaveBeenCalledTimes(1);
    await user.click(probedSheet.getByRole("button", { name: "Cancel" }));

    act(() => {
      openPicker("https://example.org/ubuntu.torrent");
      openPicker("https://example.org/burst-2.mp4");
    });
    const burstSheet = dialog("Add Download");
    expect(burstSheet.getByRole("textbox", { name: "Download source" })).toHaveValue("https://example.org/ubuntu.torrent");
    expect(probeSpy).toHaveBeenCalledTimes(1);
    await user.click(burstSheet.getByRole("button", { name: "Cancel" }));
    const burst2Sheet = await screen.findByRole("dialog", { name: "Add Download" });
    expect(within(burst2Sheet).getByRole("textbox", { name: "Download source" })).toHaveValue("https://example.org/burst-2.mp4");
    await user.click(within(burst2Sheet).getByRole("button", { name: "Cancel" }));

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

  it("refreshes canonical backend settings from get_settings after save_settings in Tauri mode", async () => {
    let backendSettings = { ...demo.getSettings(), default_download_dir: "D:\\Downloads" };
    mockWindows("main");
    mockIPC((cmd, args) => {
      if (cmd === "get_snapshot") return demo.snapshot();
      if (cmd === "get_settings") return backendSettings;
      if (cmd === "get_toolchain_status") return demo.toolchain();
      if (cmd === "save_settings") {
        const submitted = (args as { settings: typeof backendSettings }).settings;
        backendSettings = { ...submitted, default_download_dir: "D:\\Canonical\\Downloads" };
        return undefined;
      }
      return undefined;
    }, { shouldMockEvents: true });
    try {
      const user = await start();
      await user.click(screen.getByRole("button", { name: "Settings" }));
      const settingsDlg = dialog("Settings & Browser Integration");
      await user.clear(settingsDlg.getByLabelText("Default download folder"));
      await user.type(settingsDlg.getByLabelText("Default download folder"), "D:\\Downloads\\..\\Canonical\\Downloads");
      await user.click(settingsDlg.getByRole("button", { name: "Save Settings" }));
      expect(await screen.findByText("Settings saved.")).toBeVisible();
      expect(screen.getByTitle("D:\\Canonical\\Downloads")).toBeVisible();
    } finally {
      clearMocks();
      Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
      Reflect.deleteProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__");
    }
  });

  it("guards pre-commit shortcuts via synchronous overlayRef/busyRef and restricts remote torrent detection to URL pathname", async () => {
    let rejectAdd!: (error: Error) => void;
    vi.spyOn(desktop, "addHttpDownload").mockImplementation(
      () => new Promise<string>((_resolve, reject) => { rejectAdd = reject; }),
    );
    const probeSpy = vi.spyOn(desktop, "probeMedia");
    const user = await start();

    const torrentQueryUrl = "https://example.org/watch?v=1&caption=sample.torrent#sample.torrent";
    act(() => {
      openPicker(torrentQueryUrl);
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "k", ctrlKey: true, bubbles: true }));
    });
    expect(screen.queryByRole("dialog", { name: "Command Palette" })).not.toBeInTheDocument();
    const probedDialog = dialog("Add Download");
    expect(probedDialog.getByRole("textbox", { name: "Download source" })).toHaveValue(torrentQueryUrl);
    expect(await probedDialog.findByRole("button", { name: /2160p 4K/ })).toBeVisible();
    expect(probeSpy).toHaveBeenCalledWith(torrentQueryUrl);
    await user.click(probedDialog.getByRole("button", { name: "Cancel" }));

    await user.click(screen.getByRole("button", { name: "Add" }));
    const sheet = dialog("Add Download");
    await user.click(sheet.getByRole("tab", { name: "Audio Only" }));
    expect(sheet.getByRole("tab", { name: "Audio Only" })).toHaveAttribute("aria-selected", "true");
    await user.type(sheet.getByRole("textbox", { name: "Download source" }), "https://example.org/ubuntu.torrent");
    expect(sheet.getByRole("tab", { name: "Video" })).toHaveAttribute("aria-selected", "true");
    expect(sheet.getByText(/Direct HTTP download/)).toBeVisible();

    const downloadBtn = sheet.getByRole("button", { name: "Download" });
    act(() => {
      fireEvent.click(downloadBtn);
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    expect(dialog("Add Download")).toBeTruthy();

    await act(async () => { rejectAdd(new Error("Timed out")); });
    expect(await sheet.findByRole("alert")).toHaveTextContent("Timed out");
    await user.click(sheet.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();

    const expandSpy = vi.spyOn(desktop, "expandWildcardUrls").mockResolvedValue([
      "https://example.org/file_001.torrent",
      "https://example.org/file_002.torrent",
    ]);
    act(() => openPicker("https://example.org/file_[001-002].torrent"));
    const batchDialog = dialog("Add Download");
    expect(batchDialog.getByRole("tab", { name: "Torrent / Magnet" })).toHaveAttribute("aria-selected", "true");
    expect(await batchDialog.findByText("Wildcard Batch Preview")).toBeVisible();
    expect(expandSpy).toHaveBeenCalledWith("https://example.org/file_[001-002].torrent");
  });

  it("reports post-save get_settings failure without claiming success, clears dirtyFields, and avoids duplicate alerts on Retry in Tauri mode", async () => {
    let getSettingsCalls = 0;
    const initialSettings = { ...demo.getSettings(), default_download_dir: "D:\\Downloads" };
    const recoveredSettings = { ...initialSettings, default_download_dir: "D:\\Recovered\\Downloads" };
    mockWindows("main");
    mockIPC(cmd => {
      if (cmd === "get_snapshot") return demo.snapshot();
      if (cmd === "get_toolchain_status") return demo.toolchain();
      if (cmd === "save_settings") return undefined;
      if (cmd === "get_settings") {
        getSettingsCalls++;
        if (getSettingsCalls === 1) return initialSettings;
        if (getSettingsCalls === 2) throw new Error("Disk unreadable");
        if (getSettingsCalls === 3) throw new Error("Retry still offline");
        return recoveredSettings;
      }
      return undefined;
    }, { shouldMockEvents: true });
    try {
      const user = await start();
      await user.click(screen.getByRole("button", { name: "Settings" }));
      const settingsDlg = dialog("Settings & Browser Integration");
      await user.clear(settingsDlg.getByLabelText("Default download folder"));
      await user.type(settingsDlg.getByLabelText("Default download folder"), "D:\\Downloads\\..\\Edited\\Downloads");
      await user.click(settingsDlg.getByRole("button", { name: "Save Settings" }));

      expect(await settingsDlg.findByRole("alert")).toHaveTextContent(
        "Settings saved, but failed to refresh canonical settings: Disk unreadable",
      );
      expect(screen.queryByText("Settings saved.")).not.toBeInTheDocument();
      expect(settingsDlg.getByRole("button", { name: "Save Settings" })).toBeDisabled();

      // Failing Retry should surface only the inline alert inside SettingsDialog (no duplicate global toast)
      await user.click(settingsDlg.getByRole("button", { name: "Retry" }));
      expect(await settingsDlg.findByRole("alert")).toHaveTextContent("Retry still offline");
      expect(screen.getAllByRole("alert")).toHaveLength(1);

      // Successful Retry clears stale dirtyFields so the canonical folder populates the input
      await user.click(settingsDlg.getByRole("button", { name: "Retry" }));
      await waitFor(() => expect(settingsDlg.getByRole("button", { name: "Save Settings" })).toBeEnabled());
      expect(settingsDlg.queryByRole("alert")).not.toBeInTheDocument();
      expect(getSettingsCalls).toBe(4);
      expect(settingsDlg.getByLabelText("Default download folder")).toHaveValue("D:\\Recovered\\Downloads");
      expect(screen.getByTitle("D:\\Recovered\\Downloads")).toBeVisible();
    } finally {
      clearMocks();
      Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
      Reflect.deleteProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__");
    }
  });
});
