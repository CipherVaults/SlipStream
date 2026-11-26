import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_CATEGORIES, DEFAULT_SETTINGS } from "../lib/demo";
import { desktop } from "../lib/tauri";
import type {
  PendingDownloadPreview, TelemetryBatch, TransferCompletionOptions, TransferSnapshot,
} from "../types/ipc";
import { DownloadCompleteWindow } from "../windows/DownloadCompleteWindow";
import { DownloadConfirmWindow } from "../windows/DownloadConfirmWindow";
import { DownloadProgressWindow } from "../windows/DownloadProgressWindow";

function makeTransfer(overrides: Partial<TransferSnapshot> = {}): TransferSnapshot {
  return {
    id: "tr-100",
    kind: "http",
    status: "active",
    filename: "SlipStream-Setup.exe",
    source_url: "https://releases.example.com/SlipStream-Setup.exe",
    source_domain: "releases.example.com",
    save_dir: "D:\\Downloads\\Programs",
    total_bytes: 100_000_000,
    downloaded_bytes: 50_000_000,
    uploaded_bytes: 0,
    download_speed_bps: 12_500_000,
    upload_speed_bps: 0,
    eta_seconds: 4,
    active_connections: 4,
    active_peers: 0,
    protocol_label: "HTTP/2",
    format_tags: [],
    ui_segments: [1, 1, 0.5, 0],
    segments: [
      { index: 0, start_byte: 0, end_byte: 25_000_000, downloaded_bytes: 25_000_000, speed_bps: 0, state: "completed" },
      { index: 1, start_byte: 25_000_000, end_byte: 50_000_000, downloaded_bytes: 15_000_000, speed_bps: 6_000_000, state: "active" },
      { index: 2, start_byte: 50_000_000, end_byte: 75_000_000, downloaded_bytes: 10_000_000, speed_bps: 6_500_000, state: "active" },
      { index: 3, start_byte: 75_000_000, end_byte: 100_000_000, downloaded_bytes: 0, speed_bps: 0, state: "pending" },
    ],
    sha256_verified: null,
    resumable: true,
    queue_id: "default",
    error_message: null,
    connect_rtt_ms: 24,
    speed_limit_bps: null,
    description: null,
    created_at: "2026-11-26T10:00:00Z",
    updated_at: "2026-11-26T10:00:05Z",
    ...overrides,
  };
}

function makeBatch(transfers: TransferSnapshot[]): TelemetryBatch {
  return {
    aggregate_down_bps: transfers.reduce((sum, item) => sum + item.download_speed_bps, 0),
    aggregate_up_bps: 0,
    active_count: transfers.filter(item => item.status === "active").length,
    active_threads: transfers.reduce((sum, item) => sum + item.active_connections, 0),
    speed_limit_bps: null,
    transfers,
    timestamp: new Date().toISOString(),
  };
}

function makePending(overrides: Partial<PendingDownloadPreview> = {}): PendingDownloadPreview {
  return {
    pending_id: "pend-1",
    url: "https://releases.example.com/SlipStream-Setup.exe",
    source_domain: "releases.example.com",
    filename: "SlipStream-Setup.exe",
    suggested_category_id: "programs",
    suggested_save_dir: "D:\\Downloads\\Programs",
    expected_size_bytes: 85_960_000,
    duplicate: null,
    ...overrides,
  };
}

beforeEach(() => {
  vi.restoreAllMocks();
  vi.spyOn(desktop, "getSettings").mockResolvedValue({
    ...DEFAULT_SETTINGS,
    default_download_dir: "D:\\Downloads",
    categories: DEFAULT_CATEGORIES.map(cat => ({ ...cat, extensions: [...cat.extensions] })),
  });
  vi.spyOn(desktop, "close").mockResolvedValue();
  vi.spyOn(desktop, "minimize").mockResolvedValue();
  vi.spyOn(desktop, "setWindowTitle").mockResolvedValue();
  vi.spyOn(desktop, "setWindowSize").mockResolvedValue();
});

describe("DownloadConfirmWindow", () => {
  it("handles all 3 duplicate download radio choices cleanly", async () => {
    const user = userEvent.setup();
    const preview = makePending({
      duplicate: {
        transfer_id: "dup-existing",
        filename: "SlipStream-Setup.exe",
        status: "paused",
        save_dir: "D:\\Downloads\\Programs",
        total_bytes: 85_960_000,
        downloaded_bytes: 12_000_000,
      },
    });
    vi.spyOn(desktop, "getPendingDownload").mockResolvedValue(preview);
    const confirmSpy = vi.spyOn(desktop, "confirmPendingDownload").mockResolvedValue("new-id");
    const resolveDupSpy = vi.spyOn(desktop, "resolveDuplicateExisting").mockResolvedValue("dup-existing");

    // Choice 1: numbered file name -> overwrite_existing: false
    const { unmount } = render(<DownloadConfirmWindow pendingId="pend-1" />);
    await screen.findByText(/This address is already in the list of downloads/i);
    await user.click(screen.getByRole("button", { name: "OK" }));
    await screen.findByLabelText("Save As");
    await user.click(screen.getByRole("button", { name: /Start Download/i }));
    expect(confirmSpy).toHaveBeenCalledWith(expect.objectContaining({
      pending_id: "pend-1",
      overwrite_existing: false,
      start_immediately: true,
    }));
    unmount();

    // Choice 2: overwrite existing -> overwrite_existing: true
    confirmSpy.mockClear();
    const view2 = render(<DownloadConfirmWindow pendingId="pend-1" />);
    await screen.findByText(/This address is already in the list of downloads/i);
    await user.click(screen.getByLabelText(/overwrite the existing file/i));
    await user.click(screen.getByRole("button", { name: "OK" }));
    await screen.findByLabelText("Save As");
    await user.click(screen.getByRole("button", { name: /Start Download/i }));
    expect(confirmSpy).toHaveBeenCalledWith(expect.objectContaining({
      pending_id: "pend-1",
      overwrite_existing: true,
    }));
    view2.unmount();

    // Choice 3: resolve existing (show complete or resume)
    const view3 = render(<DownloadConfirmWindow pendingId="pend-1" />);
    await screen.findByText(/This address is already in the list of downloads/i);
    await user.click(screen.getByLabelText(/If existing file is complete/i));
    await user.click(screen.getByRole("button", { name: "OK" }));
    expect(resolveDupSpy).toHaveBeenCalledWith("pend-1");
    expect(desktop.close).toHaveBeenCalled();
    view3.unmount();
  });

  it("switches category subfolders, picks folder, supports Download Later and Cancel", async () => {
    const user = userEvent.setup();
    vi.spyOn(desktop, "getPendingDownload").mockResolvedValue(makePending());
    const pickSpy = vi.spyOn(desktop, "pickDownloadFolder").mockResolvedValue("E:\\CustomDir");
    const confirmSpy = vi.spyOn(desktop, "confirmPendingDownload").mockResolvedValue("later-id");
    const cancelSpy = vi.spyOn(desktop, "cancelPendingDownload").mockResolvedValue();

    const { unmount } = render(<DownloadConfirmWindow pendingId="pend-1" />);
    const saveAs = await screen.findByLabelText<HTMLInputElement>("Save As");
    expect(saveAs.value).toBe("D:\\Downloads\\Programs\\SlipStream-Setup.exe");
    expect(screen.getByText("85.96 MB")).toBeInTheDocument();

    // Category change updates folder part of Save As
    await user.selectOptions(screen.getByLabelText("Category"), "compressed");
    expect(saveAs.value).toBe("D:\\Downloads\\Compressed\\SlipStream-Setup.exe");

    // General category uses base download folder
    await user.selectOptions(screen.getByLabelText("Category"), "general");
    expect(saveAs.value).toBe("D:\\Downloads\\SlipStream-Setup.exe");

    // Browse button calls pickDownloadFolder
    await user.click(screen.getByRole("button", { name: "Browse folder" }));
    expect(pickSpy).toHaveBeenCalledWith("D:\\Downloads");
    expect(saveAs.value).toBe("E:\\CustomDir\\SlipStream-Setup.exe");

    // Remember path + description + Download Later
    await user.click(screen.getByLabelText(/Remember this path/i));
    await user.type(screen.getByLabelText("Description"), "Nightly installer");
    await user.click(screen.getByRole("button", { name: /Download Later/i }));
    expect(confirmSpy).toHaveBeenCalledWith({
      pending_id: "pend-1",
      filename: "SlipStream-Setup.exe",
      save_dir: "E:\\CustomDir",
      category_id: "general",
      remember_category_path: true,
      start_immediately: false,
      overwrite_existing: false,
      description: "Nightly installer",
    });
    unmount();

    // Cancel via Escape calls cancelPendingDownload + close
    render(<DownloadConfirmWindow pendingId="pend-1" />);
    await screen.findByLabelText("Save As");
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(cancelSpy).toHaveBeenCalledWith("pend-1"));
    expect(desktop.close).toHaveBeenCalled();
  });
});

describe("DownloadProgressWindow", () => {
  it("renders live telemetry, toggles connection details table, and saves speed limit & completion options", async () => {
    const user = userEvent.setup();
    const transfer = makeTransfer();
    let emitTelemetry: ((batch: TelemetryBatch) => void) | undefined;
    vi.spyOn(desktop, "onTelemetry").mockImplementation(async handler => {
      emitTelemetry = handler;
      return () => {};
    });
    vi.spyOn(desktop, "getSnapshot").mockResolvedValue(makeBatch([transfer]));
    const completionOpts: TransferCompletionOptions = {
      show_complete_dialog: true,
      auto_close_progress: false,
      open_folder_on_complete: false,
    };
    vi.spyOn(desktop, "getTransferCompletionOptions").mockResolvedValue(completionOpts);
    const setCompletionSpy = vi.spyOn(desktop, "setTransferCompletionOptions").mockResolvedValue();
    const setLimitSpy = vi.spyOn(desktop, "setTransferSpeedLimit").mockResolvedValue();
    const pauseSpy = vi.spyOn(desktop, "pauseTransfer").mockResolvedValue();

    render(<DownloadProgressWindow transferId="tr-100" />);
    await screen.findByText("Receiving data...");
    expect(screen.getByText(/50\.000 MB \(50\.00 %\)/)).toBeInTheDocument();
    expect(screen.getByText("12.50 MB/s")).toBeInTheDocument();
    expect(screen.getByText("releases.example.com · HTTP/2 · 24 ms")).toBeInTheDocument();
    expect(desktop.setWindowTitle).toHaveBeenCalledWith("50% SlipStream-Setup.exe");

    // Toggle Show details >> and verify resize + live connections table
    await user.click(screen.getByRole("button", { name: /Show details/i }));
    expect(desktop.setWindowSize).toHaveBeenCalledWith(540, 515);
    const table = screen.getByRole("table");
    expect(within(table).getByText("N.")).toBeInTheDocument();
    expect(within(table).getByText("Downloaded")).toBeInTheDocument();
    expect(within(table).getByText("Info")).toBeInTheDocument();
    expect(within(table).getByText("Completed")).toBeInTheDocument();
    expect(within(table).getByText(/6\.00 MB\/s/)).toBeInTheDocument();

    // Hide details resizes back down
    await user.click(screen.getByRole("button", { name: /Hide details/i }));
    expect(desktop.setWindowSize).toHaveBeenCalledWith(540, 335);

    // Pause button invokes pauseTransfer
    await user.click(screen.getByRole("button", { name: /Pause/i }));
    expect(pauseSpy).toHaveBeenCalledWith("tr-100");

    // Live telemetry update reflects new progress
    act(() => {
      emitTelemetry?.(makeBatch([makeTransfer({ downloaded_bytes: 75_000_000 })]));
    });
    expect(desktop.setWindowTitle).toHaveBeenCalledWith("75% SlipStream-Setup.exe");

    // Speed Limiter tab
    await user.click(screen.getByRole("tab", { name: "Speed Limiter" }));
    await user.click(screen.getByLabelText("Use speed limiter"));
    const speedInput = screen.getByLabelText("Speed limit value");
    await user.clear(speedInput);
    await user.type(speedInput, "2.5");
    await user.selectOptions(screen.getByLabelText("Speed limit unit"), "MB/s");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    expect(setLimitSpy).toHaveBeenCalledWith("tr-100", 2_500_000);

    // Options on completion tab
    await user.click(screen.getByRole("tab", { name: "Options on completion" }));
    await user.click(screen.getByLabelText(/Close this progress window when finished/i));
    expect(setCompletionSpy).toHaveBeenCalledWith("tr-100", {
      show_complete_dialog: true,
      auto_close_progress: true,
      open_folder_on_complete: false,
    });
  });
});

describe("DownloadCompleteWindow", () => {
  it("renders completed file details, SHA-256 badge, and dispatches Open, Open folder, and Close", async () => {
    const user = userEvent.setup();
    const completed = makeTransfer({
      id: "tr-done",
      status: "completed",
      downloaded_bytes: 100_000_000,
      download_speed_bps: 0,
      sha256_verified: true,
    });
    vi.spyOn(desktop, "onTelemetry").mockResolvedValue(() => {});
    vi.spyOn(desktop, "getSnapshot").mockResolvedValue(makeBatch([completed]));
    const openSpy = vi.spyOn(desktop, "openCompletedFile").mockResolvedValue();
    const revealSpy = vi.spyOn(desktop, "revealInExplorer").mockResolvedValue();

    render(<DownloadCompleteWindow transferId="tr-done" />);
    await screen.findByText("SlipStream-Setup.exe");
    expect(screen.getByText("SHA-256 Verified")).toBeInTheDocument();
    expect(screen.getByText("D:\\Downloads\\Programs\\SlipStream-Setup.exe")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Open" }));
    expect(openSpy).toHaveBeenCalledWith("tr-done");
    expect(desktop.close).toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Open folder" }));
    expect(revealSpy).toHaveBeenCalledWith("tr-done");

    await user.click(screen.getByRole("button", { name: "Close" }));
    expect(desktop.close).toHaveBeenCalledTimes(3);
  });
});
