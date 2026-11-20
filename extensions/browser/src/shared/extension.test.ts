// @vitest-environment jsdom
// @vitest-environment-options { "url": "https://media.example/watch?v=1" }
import type { NativeBridgeRequest, NativeBridgeResponse, TransferSnapshot } from "@slipstream/ipc-types";
import { afterEach, describe, expect, it, vi } from "vitest";
import popupHtml from "../../popup.html?raw";
import { downloadLinks, magnetUri, readSettings, safeFilename } from "./downloads";
import { applyAppearance, emptyState, readState, STATE_KEY } from "./state";

function makeTransfer(overrides: Partial<TransferSnapshot> = {}): TransferSnapshot {
  return {
    id: "tr-1",
    kind: "http",
    status: "active",
    filename: "ubuntu-26.04.iso",
    source_url: "https://cdn.example/ubuntu-26.04.iso",
    source_domain: "cdn.example",
    save_dir: "C:\\Downloads",
    total_bytes: 1000,
    downloaded_bytes: 500,
    uploaded_bytes: 0,
    download_speed_bps: 2048,
    upload_speed_bps: 0,
    eta_seconds: 10,
    active_connections: 8,
    active_peers: 0,
    protocol_label: "HTTP/2",
    format_tags: ["16-seg"],
    ui_segments: Array.from({ length: 16 }, (_, i) => (i < 8 ? 1 : 0.25)),
    segments: [],
    sha256_verified: null,
    resumable: true,
    queue_id: "default",
    error_message: null,
    created_at: "2026-11-20T00:00:00Z",
    updated_at: "2026-11-20T00:00:00Z",
    ...overrides,
  };
}

function makeFakePort() {
  const messageListeners: Array<(msg: NativeBridgeResponse) => void> = [];
  const disconnectListeners: Array<() => void> = [];
  const posted: NativeBridgeRequest[] = [];
  let disconnected = false;
  const port = {
    postMessage: vi.fn((msg: NativeBridgeRequest) => {
      posted.push(msg);
    }),
    disconnect: vi.fn(() => {
      disconnected = true;
    }),
    onMessage: {
      addListener: vi.fn((fn: (msg: NativeBridgeResponse) => void) => {
        messageListeners.push(fn);
      }),
    },
    onDisconnect: {
      addListener: vi.fn((fn: () => void) => {
        disconnectListeners.push(fn);
      }),
    },
    emitMessage(msg: NativeBridgeResponse) {
      for (const fn of [...messageListeners]) fn(msg);
    },
    emitDisconnect() {
      disconnected = true;
      for (const fn of [...disconnectListeners]) fn();
    },
    get isDisconnected() {
      return disconnected;
    },
    posted,
  };
  return port;
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
  document.body.replaceChildren();
  delete (globalThis as Record<string, unknown>).chrome;
});

describe("URL, filename, and preference guards", () => {
  it("validates magnet URIs for user-initiated torrent actions", () => {
    const hex = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Fedora";
    const b32 = "magnet:?xt=urn:btih:ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    const v2 = `magnet:?xt=urn:btmh:1220${"a".repeat(64)}`;
    expect(magnetUri(hex)).toBe(hex);
    expect(magnetUri(b32)).toBe(b32);
    expect(magnetUri(v2)).toBe(v2);
    expect(magnetUri("magnet:?xt=urn:btih:invalid")).toBeUndefined();
    expect(magnetUri("https://example.com/file.torrent")).toBeUndefined();
    expect(magnetUri("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567\n")).toBeUndefined();
  });

  it("rejects Windows reserved names, ADS colons, BiDi controls, and oversized filenames", () => {
    expect(safeFilename("C:\\Downloads\\Ubuntu-26.04.iso")).toBe("Ubuntu-26.04.iso");
    for (const bad of [
      "CON",
      "con.mp4",
      "NUL.txt",
      "LPT9.exe",
      "file.mp4:Zone.Identifier",
      "\u202Ecod.exe",
      " trailing.",
      "x".repeat(201),
    ]) {
      expect(safeFilename(bad)).toBeUndefined();
    }
  });

  it("deduplicates and strips fragments from downloadable page links", () => {
    expect(
      downloadLinks([
        "https://cdn.example/a.zip#part1",
        "https://cdn.example/a.zip#part2",
        "javascript:alert(1)",
        "file:///C:/Windows/win.ini",
        "https://user:pass@cdn.example/secret.zip",
        "http://cdn.example/b.mp4",
      ]),
    ).toEqual(["https://cdn.example/a.zip", "http://cdn.example/b.mp4"]);
  });

  it("sanitizes persisted settings and state", () => {
    const settings = readSettings({ enabled: false, minBytes: -5, extensions: ["iso", 123] });
    expect(settings.enabled).toBe(false);
    expect(settings.minBytes).toBe(0);
    expect(settings.extensions).toEqual(["iso"]);
    expect(readState({ connected: true, batch: null })).toEqual(emptyState);
    const root = document.createElement("html");
    applyAppearance(root, { theme: "system", accent: "mono" }, true);
    expect(root.dataset.theme).toBe("dark");
    expect(root.dataset.accent).toBe("mono");
  });
});

describe("NativeClient reconnect and polling", () => {
  it("syncs status, forwards commands, and reconnects after disconnect", async () => {
    vi.resetModules();
    vi.useFakeTimers();
    const ports: Array<ReturnType<typeof makeFakePort>> = [];
    (globalThis as Record<string, unknown>).chrome = {
      runtime: {
        lastError: undefined,
        connectNative: vi.fn(() => {
          const port = makeFakePort();
          ports.push(port);
          return port;
        }),
      },
    };
    const { NativeClient } = await import("./native");
    const changes: boolean[] = [];
    const client = new NativeClient((state) => changes.push(state.connected));
    client.start();
    expect(ports).toHaveLength(1);
    expect(ports[0].posted[0]).toEqual({ type: "get_status" });

    ports[0].emitMessage({
      type: "status",
      theme: "light",
      accent: "mono",
      batch: {
        aggregate_down_bps: 2048,
        aggregate_up_bps: 0,
        active_count: 1,
        active_threads: 8,
        speed_limit_bps: null,
        transfers: [makeTransfer()],
        timestamp: "2026-11-20T00:00:00Z",
      },
    });
    await Promise.resolve();
    expect(client.state.connected).toBe(true);
    expect(client.state.theme).toBe("light");
    expect(client.state.accent).toBe("mono");
    expect(changes).toEqual([true]);

    const pausePromise = client.send({ type: "pause_transfer", transfer_id: "tr-1" });
    expect(ports[0].posted[1]).toEqual({ type: "pause_transfer", transfer_id: "tr-1" });
    ports[0].emitMessage({ type: "ack", ok: true, message: null });
    expect(await pausePromise).toEqual({ type: "ack", ok: true, message: null });

    ports[0].emitDisconnect();
    expect(client.state.connected).toBe(false);
    expect(changes).toEqual([true, false]);

    vi.advanceTimersByTime(500);
    expect(ports).toHaveLength(2);
    expect(ports[1].posted[0]).toEqual({ type: "get_status" });

    // wake() cancels remaining backoff when disconnected and refreshes immediately when connected.
    ports[1].emitDisconnect();
    expect(ports).toHaveLength(2);
    client.wake();
    expect(ports).toHaveLength(3);
    expect(ports[2].posted[0]).toEqual({ type: "get_status" });

    ports[2].emitMessage({
      type: "status",
      theme: "dark",
      accent: "blue",
      batch: {
        aggregate_down_bps: 0,
        aggregate_up_bps: 0,
        active_count: 0,
        active_threads: 0,
        speed_limit_bps: null,
        transfers: [],
        timestamp: "2026-11-20T00:00:01Z",
      },
    });
    await Promise.resolve();
    client.wake();
    expect(ports[2].posted.filter((req) => req.type === "get_status")).toHaveLength(2);
  });
});

describe("toolbar popup", () => {
  it("syncs theme from Status, renders 16-segment bars, and dispatches controls", async () => {
    vi.resetModules();
    const bodyMatch = /<body>([\s\S]*)<\/body>/i.exec(popupHtml);
    document.body.innerHTML = bodyMatch ? bodyMatch[1] : "";
    const sent: unknown[] = [];
    const sessionState = {
      connected: true,
      theme: "light",
      accent: "mono",
      batch: {
        aggregate_down_bps: 1024 * 1024,
        aggregate_up_bps: 512,
        active_count: 1,
        active_threads: 8,
        speed_limit_bps: null,
        transfers: [makeTransfer()],
        timestamp: "2026-11-20T00:00:00Z",
      },
    };
    (globalThis as Record<string, unknown>).chrome = {
      runtime: {
        sendMessage: vi.fn(async (msg: unknown) => {
          sent.push(msg);
          return { ok: true, message: "Dispatched" };
        }),
      },
      storage: {
        onChanged: { addListener: vi.fn() },
        session: { get: vi.fn(async () => ({ [STATE_KEY]: sessionState })) },
        local: { get: vi.fn(async () => ({})) },
      },
    };
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: vi.fn(() => ({ matches: true, addEventListener: vi.fn(), removeEventListener: vi.fn() })),
    });

    await import("../popup/index");
    await Promise.resolve();
    await Promise.resolve();

    expect(document.documentElement.dataset.theme).toBe("light");
    expect(document.documentElement.dataset.accent).toBe("mono");
    expect(document.getElementById("connection")?.textContent).toBe("Connected");
    expect(document.getElementById("down")?.textContent).toBe("1.0 MB/s");
    expect(document.getElementById("up")?.textContent).toBe("512 B/s");
    expect(document.querySelectorAll("#transfers .segments span")).toHaveLength(16);

    const pauseBtn = document.querySelector<HTMLButtonElement>("#transfers button")!;
    expect(pauseBtn.textContent).toBe("Pause");
    pauseBtn.click();
    await Promise.resolve();
    await Promise.resolve();
    expect(sent).toContainEqual({
      kind: "desktop",
      request: { type: "pause_transfer", transfer_id: "tr-1" },
    });

    const openBtn = document.getElementById("open-desktop") as HTMLButtonElement;
    openBtn.click();
    await Promise.resolve();
    await Promise.resolve();
    expect(sent[0]).toEqual({ kind: "wake" });
    expect(sent).toContainEqual({
      kind: "desktop",
      request: { type: "open_desktop_window" },
    });
  });
});

describe("SPA navigation stream cache invalidation", () => {
  it("clears stale streamUrl and refreshes on SPA navigation before dispatching", async () => {
    vi.resetModules();
    const sent: unknown[] = [];
    let currentStream: string | undefined = "https://cdn.example/video-1.m3u8";
    const storageListeners: Array<(...args: unknown[]) => void> = [];
    (globalThis as Record<string, unknown>).ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    };
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: vi.fn(() => ({ matches: true, addEventListener: vi.fn(), removeEventListener: vi.fn() })),
    });
    window.history.replaceState({}, "", "/watch?v=1");
    (globalThis as Record<string, unknown>).chrome = {
      runtime: {
        id: "ext-test-id",
        sendMessage: vi.fn(async (msg: { kind?: string }) => {
          sent.push(msg);
          if (msg.kind === "stream") return { url: currentStream };
          return { ok: true };
        }),
        onMessage: { addListener: vi.fn() },
      },
      storage: {
        local: { get: vi.fn(async () => ({})) },
        onChanged: { addListener: vi.fn((fn: (...args: unknown[]) => void) => storageListeners.push(fn)) },
      },
    };
    const attachShadow = vi.spyOn(Element.prototype, "attachShadow");
    const video = document.createElement("video");
    Object.defineProperty(video, "getBoundingClientRect", {
      value: () => ({ left: 0, top: 0, right: 640, bottom: 360, width: 640, height: 360 }),
    });
    document.body.append(video);

    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      cb(0);
      return 1;
    });
    await import("../content");
    await Promise.resolve();
    await Promise.resolve();

    expect(attachShadow).toHaveBeenCalledWith({ mode: "closed" });
    currentStream = undefined;
    window.history.pushState({}, "", "/watch?v=2");
    window.dispatchEvent(new PopStateEvent("popstate"));
    await Promise.resolve();
    await Promise.resolve();

    expect(sent.filter((msg) => typeof msg === "object" && msg !== null && (msg as { kind?: string }).kind === "stream")).toHaveLength(2);
    delete (globalThis as Record<string, unknown>).ResizeObserver;
    vi.unstubAllGlobals();
  });
});
