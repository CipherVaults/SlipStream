// @vitest-environment jsdom
// @vitest-environment-options { "url": "https://media.example/watch?v=1" }
import type { NativeBridgeRequest, NativeBridgeResponse, TransferSnapshot } from "@slipstream/ipc-types";
import { afterEach, describe, expect, it, vi } from "vitest";
import popupHtml from "../../popup.html?raw";
import { downloadLinks, magnetUri, readSettings, safeFilename } from "./downloads";
import { applyAppearance, canUseSiteStorage, emptyState, PILL_SPOTS_KEY, readPillSpot, readState, rememberPillSpot, SETTINGS_KEY, STATE_KEY } from "./state";
import { resolveVideoPostUrl } from "./streams";

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

const listener = <T>() => ({ addListener: vi.fn<(callback: T) => void>() });

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
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

  it("deduplicates, strips fragments, and caps downloadable page links at 512", () => {
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
    const many = Array.from({ length: 600 }, (_, index) => `https://cdn.example/file-${index}.zip`);
    expect(downloadLinks(many)).toHaveLength(512);
  });

  it("sanitizes persisted settings and state", () => {
    const settings = readSettings({ enabled: false, minBytes: -5, extensions: ["iso", 123] });
    expect(settings.enabled).toBe(false);
    expect(settings.minBytes).toBe(0);
    expect(settings.extensions).toEqual(["iso"]);
    expect(settings.showInFullscreen).toBe(false);
    expect(readSettings({ showInFullscreen: true }).showInFullscreen).toBe(true);
    expect(readState({ connected: true, batch: null })).toEqual(emptyState);
    const root = document.createElement("html");
    applyAppearance(root, { theme: "system", accent: "mono" }, true);
    expect(root.dataset.theme).toBe("dark");
    expect(root.dataset.accent).toBe("mono");
  });

  it("keeps 200 valid per-site positions in usage order and rejects incognito site storage", () => {
    const spot = { x: 1, y: 0.4 };
    const entries = Object.fromEntries(Array.from({ length: 200 }, (_, index) => ["site" + index + ".example", spot]));
    const reused = rememberPillSpot(entries, "site0.example", spot);
    const added = rememberPillSpot(reused, "new.example", { x: 0, y: 0 });
    expect(Object.keys(added)).toHaveLength(200);
    expect(added["site0.example"]).toEqual(spot);
    expect(added["site1.example"]).toBeUndefined();
    expect(readPillSpot({ x: -1, y: 0 })).toBeUndefined();
    expect(readPillSpot({ x: NaN, y: 0 })).toBeUndefined();
    expect(canUseSiteStorage(false, true)).toBe(false);
    expect(canUseSiteStorage(true, false)).toBe(false);
    expect(canUseSiteStorage(false, false)).toBe(true);
  });
});

describe("feed video links", () => {
  it.each([
    ["https://www.youtube.com/watch?v=42&si=tracking#time", "https://www.youtube.com/watch?v=42"],
    ["https://www.tiktok.com/@name/video/42?utm_source=feed", "https://www.tiktok.com/@name/video/42"],
    ["https://www.instagram.com/reel/abc/?igsh=tracking", "https://www.instagram.com/reel/abc/"],
    ["https://www.facebook.com/permalink.php?story_fbid=42&id=7&fbclid=x", "https://www.facebook.com/permalink.php?story_fbid=42&id=7"],
    ["https://x.com/name/status/42?utm_campaign=feed#media", "https://x.com/name/status/42"],
    ["https://vimeo.com/42#t=6", "https://vimeo.com/42"],
    ["https://www.dailymotion.com/video/abc", "https://www.dailymotion.com/video/abc"],
    ["https://www.reddit.com/r/example/comments/abc/post/", "https://www.reddit.com/r/example/comments/abc/post/"],
    ["https://www.twitch.tv/name/clip/abc", "https://www.twitch.tv/name/clip/abc"],
  ])("binds the video to its own %s link across a shadow boundary", (href, expected) => {
    document.body.innerHTML = '<section><article><div><span></span></div><a></a></article><a href="https://x.com/else/status/999"></a></section>';
    const root = document.querySelector("span")!.attachShadow({ mode: "open" });
    root.innerHTML = "<section><div><video></video></div></section>";
    const anchor = document.querySelector("article a") as HTMLAnchorElement;
    anchor.href = href;
    expect(resolveVideoPostUrl(root.querySelector("video")!)).toBe(expected);
    anchor.href = "https://youtube.com.example/watch?v=42";
    expect(resolveVideoPostUrl(root.querySelector("video")!)).toBeUndefined();
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
      tabs: { query: vi.fn(async () => [{ incognito: false }]) },
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
    const fullscreen = document.getElementById("show-in-fullscreen") as HTMLInputElement;
    expect(fullscreen.checked).toBe(false);
    fullscreen.checked = true;
    document.getElementById("settings")!.dispatchEvent(new Event("submit", { cancelable: true }));
    await vi.waitFor(() => expect(sent).toContainEqual(expect.objectContaining({
      kind: "save-settings", settings: expect.objectContaining({ showInFullscreen: true }),
    })));
  });

  it("hides settings and rejects a submit from a focused Chromium incognito tab", async () => {
    vi.resetModules();
    document.body.innerHTML = /<body>([\s\S]*)<\/body>/i.exec(popupHtml)![1];
    const sendMessage = vi.fn(async (_message: unknown) => ({ ok: true }));
    const query = vi.fn(async () => [{ incognito: true }]);
    const readPreferences = vi.fn(async () => ({}));
    vi.stubGlobal("chrome", {
      extension: { inIncognitoContext: false }, tabs: { query }, runtime: { sendMessage },
      storage: {
        onChanged: { addListener: vi.fn() }, session: { get: vi.fn(async () => ({})) }, local: { get: readPreferences },
      },
    });
    await import("../popup/index");
    expect(query).toHaveBeenCalledWith({ active: true, currentWindow: true });
    expect(document.getElementById("preferences")?.hidden).toBe(true);
    document.getElementById("settings")!.dispatchEvent(new Event("submit", { cancelable: true }));
    await Promise.resolve();
    expect(readPreferences).not.toHaveBeenCalled();
    expect(sendMessage.mock.calls).toEqual([[{ kind: "wake" }]]);
  });

  it("never lets delayed session.get or wake hydration clobber fresher storage.onChanged updates", async () => {
    vi.resetModules();
    const bodyMatch = /<body>([\s\S]*)<\/body>/i.exec(popupHtml);
    document.body.innerHTML = bodyMatch ? bodyMatch[1] : "";
    let resolveSession!: (val: Record<string, unknown>) => void;
    let resolveWake!: (val: unknown) => void;
    const onChangedListeners: Array<(changes: Record<string, { newValue?: unknown }>, area: string) => void> = [];
    (globalThis as Record<string, unknown>).chrome = {
      runtime: {
        sendMessage: vi.fn(() => new Promise((resolve) => { resolveWake = resolve; })),
      },
      storage: {
        onChanged: {
          addListener: vi.fn((fn: (changes: Record<string, { newValue?: unknown }>, area: string) => void) => {
            onChangedListeners.push(fn);
          }),
        },
        session: { get: vi.fn(() => new Promise((resolve) => { resolveSession = resolve; })) },
        local: { get: vi.fn(async () => ({})) },
      },
    };
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: vi.fn(() => ({ matches: true, addEventListener: vi.fn(), removeEventListener: vi.fn() })),
    });

    await import("../popup/index");
    onChangedListeners[0]({
      [STATE_KEY]: {
        newValue: {
          connected: true,
          theme: "dark",
          accent: "blue",
          batch: {
            aggregate_down_bps: 4096,
            aggregate_up_bps: 0,
            active_count: 1,
            active_threads: 4,
            speed_limit_bps: null,
            transfers: [makeTransfer()],
            timestamp: "2026-11-20T00:00:00Z",
          },
        },
      },
    }, "session");
    expect(document.getElementById("connection")?.textContent).toBe("Connected");

    resolveSession({ [STATE_KEY]: { connected: false, theme: "light", accent: "mono", batch: null } });
    resolveWake({ ok: true, state: { connected: false, theme: "light", accent: "mono", batch: null } });
    await Promise.resolve();
    await Promise.resolve();

    expect(document.getElementById("connection")?.textContent).toBe("Connected");
    expect(document.documentElement.dataset.theme).toBe("dark");
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
    await vi.waitFor(() => expect(attachShadow).toHaveBeenCalledWith({ mode: "closed" }));
    currentStream = undefined;
    window.history.pushState({}, "", "/watch?v=2");
    window.dispatchEvent(new PopStateEvent("popstate"));
    await Promise.resolve();
    await Promise.resolve();

    expect(sent).toContainEqual({ kind: "frame-reset" });
    await vi.waitFor(() => expect(sent.filter((msg) => typeof msg === "object" && msg !== null && (msg as { kind?: string }).kind === "stream")).toHaveLength(2));
    delete (globalThis as Record<string, unknown>).ResizeObserver;
    vi.unstubAllGlobals();
  });
});

describe("direct HTTP handoff", () => {
  const page = "https://vidtube.example/watch?video=1";
  const start = "https://vidtube.example/download";
  const cdn = "https://s42.cdn.example/file.mp4?signature=test";
  const browserAgent = "Fixture Browser/1.0";
  const captured = [
    { name: "Referer", value: page },
    { name: "Origin", value: "https://vidtube.example" },
    { name: "User-Agent", value: "Captured Browser/2.0" },
    { name: "Cookie", value: "private=session" },
    { name: "Authorization", value: "Bearer private" },
  ];

  async function setup() {
    vi.resetModules();
    vi.useFakeTimers();
    vi.spyOn(navigator, "userAgent", "get").mockReturnValue(browserAgent);
    const onCreated = listener<(item: { id: number }) => void>();
    const onHeaders = listener<(details: object) => void>();
    const onMenu = listener<(info: object, tab: object) => void>();
    const onMessage = listener<(message: object, sender: object, reply: (value: unknown) => void) => true | undefined>();
    const port = makeFakePort();
    const tab = { id: 7, url: page, incognito: false, cookieStoreId: "firefox-default" };
    const item = {
      id: 13, url: start, finalUrl: cdn, filename: "file.mp4", fileSize: 500_000, referrer: page,
      state: "in_progress", paused: false, incognito: false, cookieStoreId: tab.cookieStoreId,
    };
    const cancel = vi.fn(async (_id: number) => {});
    const search = vi.fn(async () => [item]);
    const sessionGet = vi.fn(async (_key: string): Promise<Record<string, unknown>> => ({}));
    (globalThis as Record<string, unknown>).chrome = {
      runtime: {
        id: "ext-test-id", getURL: (file: string) => "chrome-extension://ext-test-id/" + file,
        getManifest: () => ({ background: { service_worker: "background.js" } }),
        onMessage, onConnect: listener(), onInstalled: listener(), connectNative: vi.fn(() => port),
      },
      downloads: { onCreated, search, cancel },
      contextMenus: { onClicked: onMenu },
      alarms: { onAlarm: listener(), create: vi.fn() },
      tabs: { get: vi.fn(async () => tab), query: vi.fn(async () => [tab]), onUpdated: listener(), onRemoved: listener() },
      webRequest: { onSendHeaders: onHeaders, onHeadersReceived: listener() },
      storage: {
        onChanged: listener(), local: { get: vi.fn(async () => ({})), set: vi.fn(async () => {}) },
        session: { get: sessionGet, set: vi.fn(async () => {}), remove: vi.fn(async () => {}) },
      },
    };
    await import("../background");
    port.emitMessage({ type: "status", theme: "light", accent: "blue", batch: {
      aggregate_down_bps: 0, aggregate_up_bps: 0, active_count: 1, active_threads: 8,
      speed_limit_bps: null, transfers: [makeTransfer()], timestamp: "2026-11-20T00:00:00Z",
    } });
    await Promise.resolve();

    async function observe(url: string, details: object = {}) {
      onHeaders.addListener.mock.calls[0][0]({
        tabId: tab.id, frameId: 0, requestId: url, url, initiator: page, documentUrl: page,
        requestHeaders: captured, ...details,
      });
      await Promise.resolve();
      await Promise.resolve();
    }

    async function intercept(overrides: Partial<typeof item> = {}) {
      search.mockResolvedValueOnce([{ ...item, ...overrides }]);
      const previousPosts = port.posted.length;
      onCreated.addListener.mock.calls[0][0](item);
      await vi.waitFor(() => expect(port.posted).toHaveLength(previousPosts + 1));
      const posted = port.posted.at(-1);
      expect(cancel).not.toHaveBeenCalled();
      port.emitMessage({ type: "transfer_created", transfer_id: "http-id" });
      await vi.waitFor(() => expect(cancel).toHaveBeenCalledWith(item.id));
      cancel.mockClear();
      return posted;
    }

    return { intercept, observe, onMenu, onMessage, port, sessionGet, tab };
  }

  it("uses the final URL's captured headers after a cross-origin redirect", async () => {
    const { observe, intercept } = await setup();
    await observe(start);
    await observe(cdn);
    expect(await intercept()).toEqual({ type: "add_http_download", request: {
      url: cdn, filename: "file.mp4", referrer: page,
      headers: { Referer: page, Origin: "https://vidtube.example", "User-Agent": "Captured Browser/2.0", "Accept-Language": "en-US,en;q=0.9" },
    } });
  });

  it("uses the browser's referrer and agent when the relocated URL has no exact capture", async () => {
    const { observe, intercept } = await setup();
    await observe(start);
    await observe(cdn + "-different");
    expect(await intercept({ referrer: page + "#private" })).toEqual({ type: "add_http_download", request: {
      url: cdn, filename: "file.mp4", referrer: page, headers: { "User-Agent": browserAgent, "Accept-Language": "en-US,en;q=0.9" },
    } });
  });

  it("drops captured and fallback page headers when a download downgrades to HTTP", async () => {
    const { observe, intercept } = await setup();
    const insecure = cdn.replace("https:", "http:");
    await observe(insecure);
    expect(await intercept({ finalUrl: insecure })).toEqual({ type: "add_http_download", request: {
      url: insecure, filename: "file.mp4", referrer: undefined, headers: { "User-Agent": "Captured Browser/2.0", "Accept-Language": "en-US,en;q=0.9" },
    } });
  });

  it("isolates incognito, container, and initiator mismatches from page headers", async () => {
    const { observe, intercept } = await setup();
    await observe(cdn);
    for (const overrides of [{ incognito: true }, { cookieStoreId: "firefox-container-1" }]) {
      expect(await intercept(overrides)).toEqual({ type: "add_http_download", request: {
        url: cdn, filename: "file.mp4", referrer: undefined, headers: { "User-Agent": browserAgent, "Accept-Language": "en-US,en;q=0.9" },
      } });
    }
    const unrelated = cdn + "-unrelated";
    await observe(unrelated, { initiator: "https://unrelated.example" });
    expect(await intercept({ finalUrl: unrelated, referrer: "" })).toEqual({ type: "add_http_download", request: {
      url: unrelated, filename: "file.mp4", referrer: undefined, headers: { "User-Agent": browserAgent, "Accept-Language": "en-US,en;q=0.9" },
    } });
  });

  it("keeps same-origin cookies limited to attributed downloads without cross-origin redirects", async () => {
    const { observe, intercept } = await setup();
    await observe(start);
    const publicHeaders = { Referer: page, Origin: "https://vidtube.example", "User-Agent": "Captured Browser/2.0", "Accept-Language": "en-US,en;q=0.9" };
    const requestHeaders = async (overrides: Parameters<typeof intercept>[0]) => {
      const response = await intercept(overrides);
      return response?.type === "add_http_download" ? response.request.headers : undefined;
    };
    expect(await requestHeaders({ finalUrl: start })).toEqual({ ...publicHeaders, Cookie: "private=session" });
    expect(await requestHeaders({ finalUrl: start, referrer: "" })).toEqual(publicHeaders);
    expect(await requestHeaders({ url: cdn, finalUrl: start })).toEqual(publicHeaders);
  });

  it("uses a scoped referrer and browser agent for context menu downloads and refreshes", async () => {
    const { onMenu, onMessage, port, sessionGet, tab } = await setup();
    onMenu.addListener.mock.calls[0][0]({ menuItemId: "download", linkUrl: cdn, pageUrl: page }, tab);
    const originReferrer = "https://vidtube.example/";
    await vi.waitFor(() => expect(port.posted.at(-1)).toEqual({ type: "add_http_download", request: {
      url: cdn, filename: undefined, referrer: originReferrer, headers: { "User-Agent": browserAgent, "Accept-Language": "en-US,en;q=0.9" },
    } }));
    port.emitMessage({ type: "transfer_created", transfer_id: "http-id" });

    sessionGet.mockResolvedValueOnce({ "refresh:menu": { url: cdn, tabId: tab.id, origin: new URL(page).origin, expires: Date.now() + 60_000 } });
    const result = new Promise((resolve) => onMessage.addListener.mock.calls[0][0](
      { kind: "refresh", id: "menu", transferId: "tr-1" },
      { id: "ext-test-id", url: "chrome-extension://ext-test-id/popup.html" }, resolve,
    ));
    await vi.waitFor(() => expect(port.posted.at(-1)).toEqual({ type: "refresh_download_url", request: {
      transfer_id: "tr-1", new_url: cdn, referrer: originReferrer, headers: { "User-Agent": browserAgent, "Accept-Language": "en-US,en;q=0.9" },
    } }));
    port.emitMessage({ type: "ack", ok: true, message: null });
    expect(await result).toEqual({ ok: true });

    sessionGet.mockResolvedValueOnce({
      "refresh:priv": { url: cdn, tabId: tab.id, origin: new URL(page).origin, incognito: true, expires: Date.now() + 60_000 },
    });
    const mismatch = new Promise((resolve) => onMessage.addListener.mock.calls[0][0](
      { kind: "refresh", id: "priv", transferId: "tr-1" },
      { id: "ext-test-id", url: "chrome-extension://ext-test-id/popup.html" }, resolve,
    ));
    expect(await mismatch).toEqual({ ok: false, message: "This refresh link belongs to a different browsing context." });
  });
});

describe("frame-scoped media handoff", () => {
  it("discovers formats only when the pill opens and keeps explicit actions responsive", async () => {
    vi.resetModules();
    vi.useFakeTimers();
    const onMessage = listener<(message: object, sender: chrome.runtime.MessageSender, reply: (value: unknown) => void) => true | undefined>();
    const onHeaders = listener<(details: object) => void>();
    const onResponse = listener<(details: object) => void>();
    const port = makeFakePort();
    const probePorts: ReturnType<typeof makeFakePort>[] = [];
    const tabUpdated = listener<(id: number, change: object) => void>();
    let focusedIncognito = false;
    const query = vi.fn(async () => [{ incognito: focusedIncognito }]);
    const localGet = vi.fn(async (_key: string) => ({}));
    const localSet = vi.fn(async (_values: object) => {});
    const push = vi.fn(async (_id: number, _message: object, _options: object) => {});
    const page = "https://media.example/watch";
    const top = "https://container.example/feed";
    const url = "https://media.example/video.mp4";
    const tab = { id: 7, url: top, incognito: false } as chrome.tabs.Tab;
    (globalThis as Record<string, unknown>).chrome = {
      runtime: {
        id: "ext-test-id", getURL: (file: string) => "chrome-extension://ext-test-id/" + file,
        getManifest: () => ({ background: { service_worker: "background.js" } }),
        onMessage, onConnect: listener(), onInstalled: listener(),
        connectNative: vi.fn().mockReturnValueOnce(port).mockImplementation(() => {
          const probe = makeFakePort();
          probePorts.push(probe);
          return probe;
        }),
      },
      downloads: { onCreated: listener() },
      contextMenus: { onClicked: listener() },
      alarms: { onAlarm: listener(), create: vi.fn() },
      tabs: { get: vi.fn(async () => tab), query, onUpdated: tabUpdated, onRemoved: listener(), sendMessage: push },
      webRequest: { onSendHeaders: onHeaders, onHeadersReceived: onResponse },
      storage: {
        onChanged: listener(),
        session: { get: vi.fn(async () => ({})), set: vi.fn(async () => {}) },
        local: { get: localGet, set: localSet },
      },
    };
    await import("../background");
    port.emitMessage({ type: "status", theme: "light", accent: "blue", batch: {
      aggregate_down_bps: 0, aggregate_up_bps: 0, active_count: 0, active_threads: 0,
      speed_limit_bps: null, transfers: [], timestamp: "2026-11-20T00:00:00Z",
    } });
    await Promise.resolve();

    const sender = { id: "ext-test-id", url: page, origin: new URL(page).origin, frameId: 3, tab };
    const receive = onMessage.addListener.mock.calls[0][0];
    const request = (msg: object, from: chrome.runtime.MessageSender = sender) => new Promise((resolve) => {
      if (receive(msg, from, resolve) !== true) resolve(undefined);
    });
    const privateSender = { ...sender, tab: { ...tab, incognito: true } };
    expect(await request({ kind: "pill-spot", spot: { x: 1, y: 0 } }, privateSender)).toBeUndefined();
    expect(await request({ kind: "pill-spot" }, privateSender)).toBeUndefined();
    expect(await request({ kind: "pill-open", url, requestId: 1 }, privateSender)).toBeUndefined();
    expect(await request({ kind: "pill-open", url, requestId: 1 }, { ...sender, origin: "null" })).toBeUndefined();
    expect(await request({ kind: "pill-open", url: "https://other.example/video", requestId: 1 })).toEqual({ ok: false });
    expect(localGet).not.toHaveBeenCalledWith(PILL_SPOTS_KEY);
    expect(localSet.mock.calls.every(([values]) => !Object.hasOwn(values, PILL_SPOTS_KEY))).toBe(true);

    const master = "https://media.example/hls/master.m3u8";
    const fetchManifest = vi.fn(async () => new Response(
      "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720\n720.m3u8",
    ));
    vi.stubGlobal("fetch", fetchManifest);
    const detect = onResponse.addListener.mock.calls[0][0];
    const hlsResponse = {
      tabId: 7, frameId: 3, initiator: page, url: master, statusCode: 200, requestId: "hls",
      responseHeaders: [{ name: "Content-Type", value: "application/x-mpegurl" }],
    };
    detect({ ...hlsResponse, statusCode: 206 });
    detect({ ...hlsResponse, responseHeaders: [{ name: "Content-Range", value: "bytes 0-10/50" }] });
    await Promise.resolve();
    expect(await request({ kind: "stream" })).toMatchObject({ url: undefined });
    detect(hlsResponse);
    await vi.waitFor(async () => expect(await request({ kind: "stream" })).toMatchObject({ url: master }));
    expect(fetchManifest).not.toHaveBeenCalled();
    expect(probePorts).toHaveLength(0);
    expect(await request({ kind: "pill-open", url: master })).toEqual({ ok: true });
    await vi.waitFor(async () => expect(await request({ kind: "stream" })).toMatchObject({
      url: master,
      formats: [{ id: "hls:https://media.example/hls/720.m3u8", label: "720p · HLS" }],
    }));
    expect(fetchManifest).toHaveBeenCalledWith(master, expect.objectContaining({ credentials: "omit", redirect: "error" }));
    expect(await request({ kind: "stream" }, privateSender)).toMatchObject({ url: undefined });
    expect(probePorts).toHaveLength(0);

    expect(await request({ kind: "pill-open", url, requestId: 1 })).toEqual({ ok: true });
    expect(await request({ kind: "pill-open", url, requestId: 2 })).toEqual({ ok: true });
    expect(probePorts).toHaveLength(1);
    expect(probePorts[0].posted).toEqual([{
      type: "probe_media",
      url,
      headers: expect.objectContaining({
        Referer: page,
        Origin: "https://media.example",
        "Accept-Language": "en-US,en;q=0.9",
      }),
    }]);
    const playlist = request({ kind: "media", action: "playlist", url: page });
    await Promise.resolve();
    expect(port.posted.at(-1)).toEqual({ type: "open_desktop_window", open_picker_url: page });
    port.emitMessage({ type: "ack", ok: true, message: null });
    expect(await playlist).toMatchObject({ ok: true });
    probePorts[0].emitMessage({ type: "media_probed", probe: {
      title: "Video", source_url: url, extractor: "generic", duration_seconds: 10, thumbnail_url: null,
      is_playlist: false, playlist_count: null, subtitles: [],
      formats: [{ format_id: "137", label: "1080p · MP4", container: "mp4", video_codec: "avc1", audio_codec: "aac",
        height: 1080, fps: 60, filesize_bytes: 1_000_000, is_audio_only: false, has_audio: true, has_video: true }],
    } });
    await vi.waitFor(() => expect(push).toHaveBeenCalledWith(7, expect.objectContaining({
      kind: "pill-formats",
      requestId: 2,
      formats: [
        expect.objectContaining({ id: `direct:${url}` }),
        expect.objectContaining({ id: "format:137" }),
      ],
    }), { frameId: 3 }));

    onHeaders.addListener.mock.calls[0][0]({
      tabId: 7, frameId: 3, initiator: page, url, requestId: "get",
      requestHeaders: [{ name: "Referer", value: page }, { name: "Cookie", value: "secret" }, { name: "Accept-Language", value: "en" }],
    });
    await Promise.resolve();
    await Promise.resolve();
    const action = request({ kind: "media", action: "format:137", url });
    await Promise.resolve();
    expect(port.posted.at(-1)).toEqual({
      type: "add_media_download",
      request: {
        url, format_id: "137", lossless_remux: true, normalize_audio: false,
        audio_only_preset: undefined, embed_subtitles_langs: [],
        headers: { Referer: page, "Accept-Language": "en" },
      },
    });
    port.emitMessage({ type: "ack", ok: true, message: null });
    expect(await action).toMatchObject({ ok: true });
    expect(probePorts).toHaveLength(1);
    expect(await request({ kind: "pill-open", url: page, requestId: 3 })).toEqual({ ok: true });
    tabUpdated.addListener.mock.calls[0][0](7, { status: "loading" });
    expect(probePorts[1].disconnect).toHaveBeenCalledOnce();
    expect(await request({ kind: "stream" })).toMatchObject({ url: undefined });

    focusedIncognito = true;
    const popupSender = { id: "ext-test-id", url: "chrome-extension://ext-test-id/popup.html" };
    expect(await request({ kind: "save-settings", settings: { enabled: false } }, popupSender)).toEqual({
      ok: false, message: "Settings cannot be changed from an incognito window.",
    });
    expect(query).toHaveBeenCalledWith({ active: true, currentWindow: true });
    expect(localSet.mock.calls.every(([values]) => !Object.hasOwn(values, SETTINGS_KEY))).toBe(true);

    // F-03: cross-origin known video page is allowed for pillOpen, private mediaAction probe is blocked, and oversized URLs are rejected.
    expect(await request({ kind: "pill-open", url: "https://youtu.be/dQw4w9WgXcQ", requestId: 4 })).toEqual({ ok: true });
    expect(await request({ kind: "media", action: "1080p", url }, privateSender)).toEqual({
      ok: false, message: "Automatic format probing is disabled in private windows.",
    });
    expect(await request({ kind: "media", action: "1080p", url: "https://media.example/" + "a".repeat(8200) })).toEqual({
      ok: false, message: "Media URL is too long.",
    });

    // F-08: malformed probe response settles cleanly and releases the probe slot.
    const badProbePort = probePorts.at(-1)!;
    badProbePort.emitMessage(null as unknown as NativeBridgeResponse);
    await Promise.resolve();
    await Promise.resolve();
    expect(badProbePort.disconnect).toHaveBeenCalledOnce();
  });

  it("auto-connects NativeClient.send() when service worker wakes disconnected", async () => {
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
    const client = new NativeClient(() => {});
    const sendPromise = client.send({ type: "open_desktop_window" });
    expect(ports).toHaveLength(1);
    expect(ports[0].posted[0]).toEqual({ type: "get_status" });

    ports[0].emitMessage({
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
        timestamp: "2026-11-26T00:00:00Z",
      },
    });
    await vi.waitFor(() => expect(ports[0].posted[1]).toEqual({ type: "open_desktop_window" }));
    ports[0].emitMessage({ type: "ack", ok: true, message: null });
    expect(await sendPromise).toEqual({ type: "ack", ok: true, message: null });
  });

  it("intercepts via onDeterminingFilename with cookies isolation, erase, and 3500ms timeout guard", async () => {
    vi.resetModules();
    vi.useFakeTimers();
    vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Fixture Browser/1.0");
    const onDeterminingFilename = listener<(item: object, suggest: () => void) => boolean | undefined>();
    const onCreated = listener<(item: object) => void>();
    const port = makeFakePort();
    const cancel = vi.fn(async (_id: number) => {});
    const erase = vi.fn(async (_query: object) => []);
    const getAllCookies = vi.fn(async ({ url, storeId }: { url: string; storeId?: string }) => {
      if (storeId === "firefox-container-1") return [{ name: "box", value: "container1" }];
      if (url.startsWith("https://releases.example/")) {
        return [
          { name: "auth", value: "tok123" },
          { name: "bad\r\n", value: "evil" },
        ];
      }
      return [];
    });
    const tab = { id: 9, url: "https://releases.example/project", incognito: false };
    (globalThis as Record<string, unknown>).chrome = {
      runtime: {
        id: "ext-test-id",
        getURL: (file: string) => "chrome-extension://ext-test-id/" + file,
        getManifest: () => ({ background: { service_worker: "background.js" } }),
        onMessage: listener(),
        onConnect: listener(),
        onInstalled: listener(),
        connectNative: vi.fn(() => port),
      },
      downloads: { onDeterminingFilename, onCreated, cancel, erase, search: vi.fn(async () => []) },
      cookies: { getAll: getAllCookies },
      contextMenus: { onClicked: listener() },
      alarms: { onAlarm: listener(), create: vi.fn() },
      tabs: { get: vi.fn(async () => tab), query: vi.fn(async () => [tab]), onUpdated: listener(), onRemoved: listener() },
      webRequest: { onSendHeaders: listener(), onHeadersReceived: listener() },
      storage: {
        onChanged: listener(),
        local: { get: vi.fn(async () => ({})), set: vi.fn(async () => {}) },
        session: { get: vi.fn(async () => ({})), set: vi.fn(async () => {}), remove: vi.fn(async () => {}) },
      },
    };
    await import("../background");
    port.emitMessage({
      type: "status", theme: "dark", accent: "blue",
      batch: { aggregate_down_bps: 0, aggregate_up_bps: 0, active_count: 0, active_threads: 0, speed_limit_bps: null, transfers: [], timestamp: "2026-11-26T00:00:00Z" },
    });
    await Promise.resolve();

    // 1. Extensionless URL with application/octet-stream intercepts with sanitized cookies, calls suggest(), cancel(), erase().
    const suggest1 = vi.fn();
    const ret = onDeterminingFilename.addListener.mock.calls[0][0]({
      id: 101,
      url: "https://releases.example/download?id=99",
      finalUrl: "https://releases.example/download?id=99",
      filename: "C:\\Downloads\\installer-v2.bin",
      mime: "application/octet-stream",
      fileSize: 4_000_000,
      referrer: "https://releases.example/project",
      incognito: false,
    }, suggest1);
    expect(ret).toBe(true);
    await vi.waitFor(() => expect(port.posted.at(-1)).toEqual({
      type: "add_http_download",
      request: {
        url: "https://releases.example/download?id=99",
        filename: "installer-v2.bin",
        referrer: "https://releases.example/project",
        headers: {
          "User-Agent": "Fixture Browser/1.0",
          "Accept-Language": "en-US,en;q=0.9",
          Cookie: "auth=tok123",
        },
      },
    }));
    port.emitMessage({ type: "transfer_created", transfer_id: "tr-101" });
    await vi.waitFor(() => {
      expect(suggest1).toHaveBeenCalledOnce();
      expect(cancel).toHaveBeenCalledWith(101);
      expect(erase).toHaveBeenCalledWith({ id: 101 });
    });

    // 2. Incognito download without storeId never queries default cookies.
    getAllCookies.mockClear();
    const suggest2 = vi.fn();
    onDeterminingFilename.addListener.mock.calls[0][0]({
      id: 102,
      url: "https://releases.example/download?id=100",
      filename: "secret.zip",
      mime: "application/zip",
      fileSize: 1_000_000,
      referrer: "https://releases.example/project",
      incognito: true,
    }, suggest2);
    await vi.waitFor(() => expect(port.posted.at(-1)).toMatchObject({
      type: "add_http_download",
      request: { url: "https://releases.example/download?id=100", filename: "secret.zip" },
    }));
    const incognitoReq = port.posted.at(-1);
    expect(incognitoReq?.type === "add_http_download" ? incognitoReq.request.headers : {}).not.toHaveProperty("Cookie");
    expect(getAllCookies).not.toHaveBeenCalled();
    port.emitMessage({ type: "transfer_created", transfer_id: "tr-102" });
    await vi.waitFor(() => expect(suggest2).toHaveBeenCalledOnce());

    // 3. Safety timeout calls suggest() once after 3500ms if desktop hangs, never hanging Chromium, and late reply does NOT cancel or erase.
    cancel.mockClear();
    erase.mockClear();
    const suggest3 = vi.fn();
    onDeterminingFilename.addListener.mock.calls[0][0]({
      id: 103,
      url: "https://releases.example/slow.zip",
      filename: "slow.zip",
      mime: "application/zip",
      fileSize: 1_000_000,
      referrer: "https://releases.example/project",
      incognito: false,
    }, suggest3);
    await vi.waitFor(() => expect(port.posted.at(-1)).toMatchObject({
      type: "add_http_download",
      request: { url: "https://releases.example/slow.zip" },
    }));
    expect(suggest3).not.toHaveBeenCalled();
    vi.advanceTimersByTime(3500);
    expect(suggest3).toHaveBeenCalledOnce();
    port.emitMessage({ type: "transfer_created", transfer_id: "late-103" });
    await Promise.resolve();
    await Promise.resolve();
    expect(suggest3).toHaveBeenCalledOnce();
    expect(cancel).not.toHaveBeenCalled();
    expect(erase).not.toHaveBeenCalled();
    if (port.posted.at(-1)?.type === "get_status") {
      port.emitMessage({
        type: "status", theme: "dark", accent: "blue",
        batch: { aggregate_down_bps: 0, aggregate_up_bps: 0, active_count: 0, active_threads: 0, speed_limit_bps: null, transfers: [], timestamp: "2026-11-26T00:00:00Z" },
      });
      await Promise.resolve();
    }

    // 4. Firefox private storeId never leaks private cookies into shared desktop SQLite.
    getAllCookies.mockClear();
    const suggest4 = vi.fn();
    onDeterminingFilename.addListener.mock.calls[0][0]({
      id: 104,
      url: "https://releases.example/private.zip",
      filename: "private.zip",
      mime: "application/zip",
      fileSize: 1_000_000,
      referrer: "https://releases.example/project",
      cookieStoreId: "firefox-private-1",
      incognito: false,
    }, suggest4);
    await vi.waitFor(() => expect(port.posted.at(-1)).toMatchObject({
      type: "add_http_download",
      request: { url: "https://releases.example/private.zip" },
    }));
    const privateStoreReq = port.posted.at(-1);
    expect(privateStoreReq?.type === "add_http_download" ? privateStoreReq.request.headers : {}).not.toHaveProperty("Cookie");
    expect(getAllCookies).not.toHaveBeenCalled();
    port.emitMessage({ type: "transfer_created", transfer_id: "tr-104" });
    await vi.waitFor(() => expect(suggest4).toHaveBeenCalledOnce());
  });

  it("sniffs 206 Partial Content direct MP4 via Content-Range (>= 300KB) and downloads cross-origin video with cookies and origin-scoped Referer", async () => {
    vi.resetModules();
    vi.useFakeTimers();
    vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Fixture Browser/1.0");
    const onMessage = listener<(message: object, sender: chrome.runtime.MessageSender, reply: (value: unknown) => void) => true | undefined>();
    const onResponse = listener<(details: object) => void>();
    const port = makeFakePort();
    const push = vi.fn(async (_id: number, _message: object, _options: object) => {});
    const page = "https://vidtube.example/watch/123";
    const cdnVideo = "https://cdn-video.xyz/streams/movie.mp4?token=abc";
    const smallVideo = "https://cdn-video.xyz/streams/preview.mp4";
    const wildcardRangeVideo = "https://cdn-video.xyz/streams/live.mp4";
    const tab = { id: 12, url: page, incognito: false } as chrome.tabs.Tab;
    const getAllCookies = vi.fn(async ({ url }: { url: string }) =>
      url.startsWith("https://cdn-video.xyz/")
        ? [
            { name: "cdn_sid", value: "xyz789", sameSite: "no_restriction", secure: true },
            { name: "strict_sid", value: "withheld", sameSite: "strict", secure: true },
          ]
        : [],
    );
    (globalThis as Record<string, unknown>).chrome = {
      runtime: {
        id: "ext-test-id",
        getURL: (file: string) => "chrome-extension://ext-test-id/" + file,
        getManifest: () => ({ background: { service_worker: "background.js" } }),
        onMessage,
        onConnect: listener(),
        onInstalled: listener(),
        connectNative: vi.fn(() => port),
      },
      downloads: { onCreated: listener() },
      cookies: { getAll: getAllCookies },
      contextMenus: { onClicked: listener() },
      alarms: { onAlarm: listener(), create: vi.fn() },
      tabs: { get: vi.fn(async () => tab), query: vi.fn(async () => [tab]), onUpdated: listener(), onRemoved: listener(), sendMessage: push },
      webRequest: { onSendHeaders: listener(), onHeadersReceived: onResponse },
      storage: {
        onChanged: listener(),
        local: { get: vi.fn(async () => ({})), set: vi.fn(async () => {}) },
        session: { get: vi.fn(async () => ({})), set: vi.fn(async () => {}) },
      },
    };
    await import("../background");
    port.emitMessage({
      type: "status", theme: "dark", accent: "blue",
      batch: { aggregate_down_bps: 0, aggregate_up_bps: 0, active_count: 0, active_threads: 0, speed_limit_bps: null, transfers: [], timestamp: "2026-11-26T00:00:00Z" },
    });
    await Promise.resolve();

    const sender = { id: "ext-test-id", url: page, origin: new URL(page).origin, frameId: 0, tab };
    const receive = onMessage.addListener.mock.calls[0][0];
    const request = (msg: object, from: chrome.runtime.MessageSender = sender) => new Promise((resolve) => {
      if (receive(msg, from, resolve) !== true) resolve(undefined);
    });

    const detect = onResponse.addListener.mock.calls[0][0];
    // 206 Partial Content < 300KB is rejected
    detect({
      tabId: 12, frameId: 0, initiator: page, url: smallVideo, statusCode: 206, requestId: "small-206",
      responseHeaders: [
        { name: "Content-Type", value: "video/mp4" },
        { name: "Content-Range", value: "bytes 0-65535/250000" },
      ],
    });
    await Promise.resolve();
    await Promise.resolve();
    expect(await request({ kind: "stream" })).toMatchObject({ url: undefined });

    // 206 with Content-Range /* keeps total size unknown instead of reporting chunk length as file size.
    detect({
      tabId: 12, frameId: 0, initiator: page, url: wildcardRangeVideo, statusCode: 206, requestId: "wild-206",
      responseHeaders: [
        { name: "Content-Type", value: "video/mp4" },
        { name: "Content-Length", value: "500000" },
        { name: "Content-Range", value: "bytes 0-499999/*" },
      ],
    });
    await vi.waitFor(async () => expect(await request({ kind: "stream" })).toMatchObject({
      url: wildcardRangeVideo,
      formats: [{ id: `direct:${wildcardRangeVideo}`, label: "Download Video", sub: "MP4" }],
    }));

    // 206 Partial Content >= 300KB is accepted and exposes direct Download Video choice
    detect({
      tabId: 12, frameId: 0, initiator: page, url: cdnVideo, statusCode: 206, requestId: "big-206",
      responseHeaders: [
        { name: "Content-Type", value: "video/mp4" },
        { name: "Content-Range", value: "bytes 0-65535/1500000000" },
      ],
    });
    await vi.waitFor(async () => expect(await request({ kind: "stream" })).toMatchObject({
      url: cdnVideo,
      formats: [{ id: `direct:${cdnVideo}`, label: "Download Video", sub: "MP4 · 1.4 GB" }],
    }));

    // Clicking direct: on cross-origin <video src> sends add_http_download with origin-only Referer, Origin, UA, Accept-Language, and cross-site Cookie (filtering out SameSite=Strict).
    const downloadPromise = request({ kind: "media", action: `direct:${cdnVideo}`, url: cdnVideo, embedded: true });
    await vi.waitFor(() => expect(port.posted.at(-1)).toEqual({
      type: "add_http_download",
      request: {
        url: cdnVideo,
        filename: "movie.mp4",
        referrer: "https://vidtube.example/",
        headers: {
          Referer: "https://vidtube.example/",
          Origin: "https://vidtube.example",
          "User-Agent": "Fixture Browser/1.0",
          "Accept-Language": "en-US,en;q=0.9",
          Cookie: "cdn_sid=xyz789",
        },
      },
    }));
    port.emitMessage({ type: "transfer_created", transfer_id: "vid-1" });
    expect(await downloadPromise).toEqual({ ok: true });
  });
});
