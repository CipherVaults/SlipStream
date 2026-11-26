import type { NativeBridgeResponse } from "@slipstream/ipc-types";
import { extension as api } from "./shared/browser";
import {
  blockedSite, canAttributeTab, defaults, downloadLinks, httpUrl, magnetUri, matchesSite,
  readSettings, safeFilename, sameDownloadOrigin, shouldIntercept, type InterceptionSettings,
} from "./shared/downloads";
import { NativeClient } from "./shared/native";
import { isPublicHttpUrl, ObservedNetwork } from "./shared/network";
import {
  APPEARANCE_KEY, canUseSiteStorage, formatBytes, isRefreshableHttpTransfer, PILL_SPOTS_KEY, PLAYER_PORT,
  readPillSpot, readState, rememberPillSpot, SETTINGS_KEY, STATE_KEY,
  type ActionResult, type BrowserMessage, type MediaAction, type PillFormat, type PillFormatsMessage,
} from "./shared/state";
import {
  isDirectMediaUrl, isHls, isKnownVideoPage, isManifest, isMediaStream, isPlaylistPage,
  mediaLength, mediaMime, parseContentRangeTotal, parseHlsMaster, probeFormats, selectPillFormatId, validFormatId,
} from "./shared/streams";

const popupUrl = api.runtime.getURL("popup.html");
const refreshKey = (id: string) => "refresh:" + id;
const network = new ObservedNetwork();
const heldAlt = new Set<number>();
let settings = defaults;
let settingsWrites: Promise<unknown> = Promise.resolve();
const settingsReady = api.storage.local.get(SETTINGS_KEY).then((values) => {
  settings = readSettings(values[SETTINGS_KEY]);
}).catch(report);
api.storage.onChanged.addListener((values, area) => {
  if (area === "local" && SETTINGS_KEY in values) {
    settings = readSettings(values[SETTINGS_KEY].newValue);
  }
});

let appearance = "";
let stateWrites: Promise<unknown> = Promise.resolve();
const native = new NativeClient((state) => {
  network.prune();
  stateWrites = stateWrites.then(async () => {
    await api.storage.session.set({ [STATE_KEY]: state });
    const { theme, accent } = state;
    if (appearance !== theme + accent) {
      await api.storage.local.set({ [APPEARANCE_KEY]: { theme, accent } });
      appearance = theme + accent;
    }
  }).catch(report);
});
stateWrites = api.storage.session.get(STATE_KEY).then((values) => {
  const previous = readState(values[STATE_KEY]);
  if (previous.connected && !native.state.connected) {
    return api.storage.session.set({ [STATE_KEY]: { ...previous, connected: false } });
  }
}).catch(report);

function report(error: unknown): void {
  console.warn("SlipStream extension:", error);
}

api.alarms.onAlarm.addListener(({ name }) => {
  if (name === "slipstream-connect") {
    network.prune();
    native.start();
  }
});
api.alarms.create("slipstream-connect", { periodInMinutes: 1 });
native.start();

type Frame = {
  page: URL;
  tabId: number;
  tabUrl: string;
  frameId: number;
  documentId?: string;
  cookieStoreId?: string;
  private: boolean;
};
type Probe = { frame: Frame; expires: number; controller: AbortController; response: Promise<NativeBridgeResponse>; settled: boolean };
const probes = new Map<string, Probe>();
const childPlayers = new Map<number, Set<chrome.runtime.Port>>();
const hlsRequests = new Map<string, AbortController>();
const tabLookups = new Map<number, Promise<chrome.tabs.Tab | undefined>>();

function getFrame(sender: chrome.runtime.MessageSender): Frame | undefined {
  const page = httpUrl(sender.url);
  const tab = sender.tab as (chrome.tabs.Tab & { cookieStoreId?: string }) | undefined;
  const top = httpUrl(tab?.url);
  const frameId = sender.frameId;
  if (sender.id !== api.runtime.id || !page || !top || tab?.id === undefined || frameId === undefined || frameId < 0
    || (sender.origin !== undefined && sender.origin !== page.origin) || (frameId === 0 && top.origin !== page.origin)
    || (sender.documentLifecycle !== undefined && sender.documentLifecycle !== "active")) return;
  return {
    page, tabId: tab.id, tabUrl: top.href, frameId, documentId: sender.documentId,
    cookieStoreId: tab.cookieStoreId,
    private: !canUseSiteStorage(api.extension?.inIncognitoContext, tab.incognito),
  };
}

function postToFrame(tabId: number, frameId: number, message: object, documentId?: string): void {
  void api.tabs.sendMessage(tabId, message, { frameId, ...(documentId && { documentId }) }).catch(() => {});
}

function childChanged(tabId: number): void {
  postToFrame(tabId, 0, { kind: "child-player-changed" });
}

api.runtime.onConnect.addListener((port) => {
  const frame = port.sender && getFrame(port.sender);
  if (port.name !== PLAYER_PORT || !frame || frame.frameId === 0) return;
  const ports = childPlayers.get(frame.tabId) ?? new Set();
  const first = !ports.size;
  ports.add(port);
  childPlayers.set(frame.tabId, ports);
  if (first) childChanged(frame.tabId);
  port.onDisconnect.addListener(() => {
    if (!ports.delete(port) || ports.size || childPlayers.get(frame.tabId) !== ports) return;
    childPlayers.delete(frame.tabId);
    childChanged(frame.tabId);
  });
});

function clearFrame(tabId: number, frameId?: number): void {
  if (frameId === undefined || frameId === 0) heldAlt.delete(tabId);
  network.clear(tabId, frameId);
  tabLookups.delete(tabId);
  for (const [key, probe] of probes) {
    if (probe.frame.tabId === tabId && (frameId === undefined || probe.frame.frameId === frameId)) {
      probes.delete(key);
      probe.controller.abort();
    }
  }
  const prefix = frameId === undefined ? tabId + ":" : tabId + ":" + frameId + ":";
  for (const [key, controller] of hlsRequests) {
    if (key.startsWith(prefix)) {
      hlsRequests.delete(key);
      controller.abort();
    }
  }
}
api.tabs.onRemoved.addListener((id) => {
  clearFrame(id);
  childPlayers.delete(id);
});
api.tabs.onUpdated.addListener((id, change) => {
  if (change.status === "loading" || change.url) clearFrame(id);
});

function observedTab(tabId: number): Promise<chrome.tabs.Tab | undefined> {
  const pending = tabLookups.get(tabId);
  if (pending) return pending;
  const lookup = api.tabs.get(tabId).then((tab): chrome.tabs.Tab | undefined => {
    if (tabLookups.get(tabId) !== lookup) return;
    tabLookups.delete(tabId);
    if (canUseSiteStorage(api.extension?.inIncognitoContext, tab.incognito)) return tab;
  }, (): undefined => {
    if (tabLookups.get(tabId) === lookup) tabLookups.delete(tabId);
    return undefined;
  });
  tabLookups.set(tabId, lookup);
  return lookup;
}

const webFilter = { urls: ["http://*/*", "https://*/*"] };
const firefox = Array.isArray((api.runtime.getManifest().background as { scripts?: string[] } | undefined)?.scripts);
type RequestOrigin = { initiator?: string; documentUrl?: string; originUrl?: string; incognito?: boolean };
function requestPage(details: RequestOrigin): URL | undefined {
  return httpUrl(details.documentUrl ?? details.initiator ?? details.originUrl);
}

api.webRequest.onSendHeaders.addListener((details) => {
  const origin = details as typeof details & RequestOrigin;
  if (details.tabId < 0 || origin.incognito || !details.requestHeaders?.length || !requestPage(origin)) return;
  void observedTab(details.tabId).then((tab) => {
    if (tab?.url) network.recordHeaders(origin, details.requestHeaders ?? [], tab.url);
  });
}, webFilter, firefox ? ["requestHeaders"] : ["requestHeaders", "extraHeaders"]);

function notifyStream(tabId: number, frameId: number): void {
  postToFrame(tabId, frameId, { kind: "stream-changed" });
  if (frameId !== 0) postToFrame(tabId, 0, { kind: "stream-changed" });
}

async function fetchManifest(url: string, signal: AbortSignal): Promise<string | undefined> {
  if (!isPublicHttpUrl(url)) return;
  const response = await fetch(url, {
    credentials: "omit", redirect: "error", referrerPolicy: "no-referrer", cache: "no-store",
    signal: AbortSignal.any([signal, AbortSignal.timeout(2500)]),
  });
  if (!response.ok || !response.body) return;
  if (response.status === 206 || response.headers.has("Content-Range")) {
    await response.body.cancel();
    return;
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let text = "";
  let bytes = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) return text + decoder.decode();
      bytes += value.byteLength;
      if (bytes > 512 * 1024) {
        await reader.cancel();
        return;
      }
      text += decoder.decode(value, { stream: true });
    }
  } finally {
    reader.releaseLock();
  }
}

type Stream = NonNullable<ReturnType<ObservedNetwork["stream"]>>;
function readHls(stream: Stream): void {
  const { tabId, frameId, url } = stream;
  const key = tabId + ":" + frameId + ":" + url;
  if (!isPublicHttpUrl(url) || hlsRequests.has(key) || hlsRequests.size >= 3) return;
  const controller = new AbortController();
  hlsRequests.set(key, controller);
  void fetchManifest(url, controller.signal).then((text) => {
    if (!text || hlsRequests.get(key) !== controller) return;
    const variants = parseHlsMaster(text, url).slice(0, 20);
    const separateAudio = text.split(/\r?\n/).some((line) =>
      /^\s*#EXT-X-MEDIA:/.test(line) && /\bTYPE=AUDIO\b/.test(line) && /\bURI\s*=/.test(line));
    if (variants.length && network.setHlsVariants(stream, variants, separateAudio)) notifyStream(tabId, frameId);
  }).catch(() => {}).finally(() => {
    if (hlsRequests.get(key) === controller) hlsRequests.delete(key);
  });
}

api.webRequest.onHeadersReceived.addListener((details): undefined => {
  const headers = details.responseHeaders ?? [];
  const mime = mediaMime(headers);
  const origin = details as typeof details & RequestOrigin;
  const page = requestPage(origin);
  const target = httpUrl(details.url);
  if (details.tabId < 0 || origin.incognito || !page || !target || details.statusCode < 200 || details.statusCode >= 300) return;
  const hasContentRange = headers.some(({ name }) => name.toLowerCase() === "content-range");
  if (isManifest(target, mime)) {
    if (details.statusCode === 206 || hasContentRange) return;
  } else {
    const totalLength = parseContentRangeTotal(headers) ?? (details.statusCode === 200 && !hasContentRange ? mediaLength(headers) : undefined);
    const minCheckLength = totalLength ?? mediaLength(headers);
    if (details.statusCode === 206 || hasContentRange) {
      if (minCheckLength === undefined || minCheckLength < 300_000) return;
    }
    if (!isMediaStream(details.url, mime, minCheckLength)) return;
  }
  void observedTab(details.tabId).then((tab) => {
    if (!tab?.url) return;
    if (network.recordStream(origin, tab.url, headers)) notifyStream(details.tabId, Math.max(0, details.frameId));
  }).catch(() => {});
  return undefined;
}, webFilter, ["responseHeaders"]);

function isSameSiteHostname(siteHost: string, targetHost: string): boolean {
  const a = siteHost.toLowerCase().replace(/\.$/, "");
  const b = targetHost.toLowerCase().replace(/\.$/, "");
  if (!a || !b) return false;
  if (a === b) return true;
  const partsA = a.split(".");
  const partsB = b.split(".");
  if (partsA.length < 2 || partsB.length < 2) return false;
  return partsA.slice(-2).join(".") === partsB.slice(-2).join(".");
}

async function collectCookiesForUrl(
  target: URL,
  storeId?: string,
  incognito?: boolean,
  siteHostname?: string,
  isSubresource = false,
): Promise<string | undefined> {
  if (!api.cookies?.getAll) return undefined;
  if (incognito || api.extension?.inIncognitoContext || storeId?.startsWith("firefox-private")) return undefined;
  try {
    const cookies = await api.cookies.getAll({
      url: target.href,
      ...(storeId ? { storeId } : {}),
    });
    if (!Array.isArray(cookies) || !cookies.length) return undefined;
    const crossSite = siteHostname ? !isSameSiteHostname(siteHostname, target.hostname) : false;
    const pairs: string[] = [];
    let totalBytes = 0;
    const encoder = new TextEncoder();
    for (const cookie of cookies) {
      if (!cookie || typeof cookie.name !== "string" || typeof cookie.value !== "string") continue;
      if (cookie.secure && target.protocol !== "https:") continue;
      if (crossSite) {
        if (cookie.sameSite === "strict") continue;
        if (isSubresource && (cookie.sameSite === "lax" || cookie.sameSite === "unspecified")) continue;
      }
      if (!cookie.name || /[\u0000-\u001f\u007f;=\r\n]/.test(cookie.name) || /[\u0000-\u001f\u007f\r\n]/.test(cookie.value)) {
        continue;
      }
      const pair = `${cookie.name}=${cookie.value}`;
      const added = encoder.encode(pairs.length ? `; ${pair}` : pair).length;
      if (totalBytes + added > 8192) break;
      pairs.push(pair);
      totalBytes += added;
    }
    return pairs.length ? pairs.join("; ") : undefined;
  } catch {
    return undefined;
  }
}

function httpHandoffContext(url: URL, tab: chrome.tabs.Tab | undefined, fallback?: URL, allowCookie = true) {
  const canUsePageHeaders = canUseSiteStorage(api.extension?.inIncognitoContext, tab?.incognito);
  const headers = canUsePageHeaders ? { ...network.headers(tab?.id, url.href, tab?.url) } : {};
  if (!allowCookie) delete headers.Cookie;
  const userAgent = typeof navigator === "undefined" ? undefined : navigator.userAgent;
  if (!headers["User-Agent"] && userAgent?.trim() && !/[\u0000-\u001f\u007f]/.test(userAgent)) {
    headers["User-Agent"] = userAgent;
  }
  if (!headers["Accept-Language"]) {
    headers["Accept-Language"] = "en-US,en;q=0.9";
  }
  let referrer = headers.Referer;
  if (!referrer && canUsePageHeaders && fallback && !(fallback.protocol === "https:" && url.protocol === "http:")) {
    const clean = new URL(fallback.href);
    clean.hash = "";
    referrer = clean.href;
  }
  return { headers, referrer };
}

function tabReferrer(url: URL, tab: chrome.tabs.Tab | undefined): URL | undefined {
  const page = httpUrl(tab?.url);
  // A tab URL can contain private paths and queries. Cross-origin fallback needs only the origin.
  return page && (page.origin === url.origin ? page : new URL(page.origin));
}

function handoffHttp(url: URL, context: ReturnType<typeof httpHandoffContext>, filename?: string): Promise<NativeBridgeResponse> {
  return native.send({
    type: "add_http_download",
    request: { url: url.href, filename, ...context },
  });
}

const handledDownloads = new Set<number>();

async function tryInterceptDownload(
  current: chrome.downloads.DownloadItem,
  isExpired?: () => boolean,
): Promise<boolean> {
  if (!native.state.connected && !(await native.ensureConnected())) return false;
  await settingsReady;
  if (isExpired?.()) return false;
  const [active] = await api.tabs.query({ active: true, lastFocusedWindow: true });
  if (isExpired?.()) return false;
  const target = httpUrl(current.finalUrl || current.url);
  if (!target) return false;
  const downloadContext = current as typeof current & { cookieStoreId?: string };
  const tabContext = active as (typeof active & { cookieStoreId?: string }) | undefined;
  const tab = canAttributeTab(downloadContext, tabContext) ? active : undefined;
  if (!shouldIntercept(current, settings, {
    connected: native.state.connected,
    altHeld: tab?.id !== undefined && heldAlt.has(tab.id),
    tabUrl: tab?.url,
  })) return false;
  const referrer = httpUrl(current.referrer);
  const page = httpUrl(tab?.url);
  const downgrade = Boolean((referrer?.protocol === "https:" || page?.protocol === "https:") && target.protocol === "http:");
  const sameRedirectOrigin = Boolean(referrer && sameDownloadOrigin(current));
  const allowCookie = sameRedirectOrigin && !downgrade && !current.incognito && !api.extension?.inIncognitoContext;
  const fallback = tab || (!active && !current.incognito)
    ? referrer ?? (page?.origin === target.origin ? page : undefined)
    : undefined;
  const context = httpHandoffContext(target, tab, fallback, allowCookie);
  if (allowCookie && !context.headers.Cookie) {
    const cookie = await collectCookiesForUrl(target, downloadContext.cookieStoreId, current.incognito, (referrer ?? page)?.hostname, false);
    if (cookie) context.headers.Cookie = cookie;
  }
  if (isExpired?.()) return false;
  const response = await handoffHttp(target, context, safeFilename(current.filename));
  return !isExpired?.() && response.type === "transfer_created";
}

if (api.downloads.onDeterminingFilename) {
  api.downloads.onDeterminingFilename.addListener((item, suggest) => {
    let suggested = false;
    let expired = false;
    const finishSuggest = () => {
      if (suggested) return;
      suggested = true;
      clearTimeout(safetyTimer);
      try {
        suggest();
      } catch {
        // Ignore if Chromium already finalized the download.
      }
    };
    const safetyTimer = setTimeout(() => {
      expired = true;
      finishSuggest();
    }, 3500);
    if (handledDownloads.has(item.id)) {
      finishSuggest();
      return true;
    }
    handledDownloads.add(item.id);
    void (async () => {
      try {
        const intercepted = await tryInterceptDownload(item, () => expired);
        if (expired) return;
        finishSuggest();
        if (intercepted) {
          const canceled = await api.downloads.cancel(item.id).then(() => true, () => false);
          if (canceled && api.downloads.erase) {
            await api.downloads.erase({ id: item.id }).catch(() => {});
          }
        }
      } catch (error) {
        report(error);
        finishSuggest();
      } finally {
        handledDownloads.delete(item.id);
      }
    })();
    return true;
  });
}

api.downloads.onCreated.addListener((item) => {
  if (api.downloads.onDeterminingFilename) return;
  if (handledDownloads.has(item.id)) return;
  handledDownloads.add(item.id);
  void (async () => {
    try {
      if (!native.state.connected && !(await native.ensureConnected())) return;
      await settingsReady;
      const [current] = await api.downloads.search({ id: item.id });
      if (!current || current.state !== "in_progress" || current.paused) return;
      const intercepted = await tryInterceptDownload(current);
      if (intercepted) {
        const canceled = await api.downloads.cancel(item.id).then(() => true, () => false);
        if (canceled && api.downloads.erase) {
          await api.downloads.erase({ id: item.id }).catch(() => {});
        }
      }
    } catch (error) {
      report(error);
    } finally {
      handledDownloads.delete(item.id);
    }
  })();
});

function result(
  response: NativeBridgeResponse,
  expected: ReadonlyArray<NativeBridgeResponse["type"]> = ["ack", "transfer_created"],
): ActionResult {
  if (response.type === "error") return { ok: false, message: response.message };
  if (!expected.includes(response.type)) {
    return { ok: false, message: "Unexpected response from SlipStream." };
  }
  if (response.type === "ack") return { ok: response.ok, message: response.message ?? undefined };
  return { ok: true };
}

async function grabLinks(links: unknown[]): Promise<ActionResult> {
  const urls = downloadLinks(links);
  if (!urls.length) return { ok: false, message: "No downloadable links found." };
  // At most 100 URLs of up to 8192 bytes each fit comfortably in a native frame.
  for (let offset = 0; offset < urls.length; offset += 100) {
    const response = result(await native.send({
      type: "grab_batch_urls", urls: urls.slice(offset, offset + 100),
    }));
    if (!response.ok) return response;
  }
  return { ok: true };
}

function updateSettings(update: (current: InterceptionSettings) => InterceptionSettings): Promise<unknown> {
  const written = settingsWrites.then(async () => {
    await settingsReady;
    settings = update(settings);
    await api.storage.local.set({ [SETTINGS_KEY]: settings });
  });
  settingsWrites = written.catch(report);
  return written;
}

let spotWrites: Promise<unknown> = Promise.resolve();
function siteSpot(frame: Frame, value: unknown): Promise<{ ok: true; spot?: ReturnType<typeof readPillSpot> }> {
  const site = frame.page.hostname;
  const operation = spotWrites.then(async () => {
    const saved = (await api.storage.local.get(PILL_SPOTS_KEY))[PILL_SPOTS_KEY] as Record<string, unknown> | undefined;
    const next = readPillSpot(value);
    const spot = next ?? readPillSpot(saved && Object.hasOwn(saved, site) ? saved[site] : undefined);
    if (spot && (next || Object.keys(saved ?? {}).at(-1) !== site)) {
      await api.storage.local.set({ [PILL_SPOTS_KEY]: rememberPillSpot(saved, site, spot) });
    }
    return { ok: true as const, spot };
  });
  spotWrites = operation.catch(report);
  return operation;
}

function observedStream(frame: Frame): Stream | undefined {
  if (!frame.private) return network.stream(frame.tabId, frame.tabUrl, frame.frameId, frame.page.href);
}

function probeKey(frame: Frame, url: string, playlist: boolean): string {
  return JSON.stringify([frame.tabId, frame.frameId, frame.documentId, frame.page.href, url, playlist]);
}

function cachedProbe(frame: Frame, url: string, playlist: boolean): Probe | undefined {
  if (frame.private) return;
  const key = probeKey(frame, url, playlist);
  const probe = probes.get(key);
  if (probe && (!probe.settled || probe.expires > Date.now())) return probe;
  probes.delete(key);
}

const MAX_URL_LENGTH = 8192;
const MAX_CONCURRENT_PROBES = 3;

function activeProbeCount(): number {
  let count = 0;
  for (const entry of probes.values()) {
    if (!entry.settled) count++;
  }
  return count;
}

function parseMediaTarget(frame: Frame, rawUrl: unknown, stream: Stream | undefined, embedded: boolean): URL | undefined {
  if (typeof rawUrl !== "string" || rawUrl.length > MAX_URL_LENGTH) return;
  const url = httpUrl(rawUrl);
  if (!url || url.href.length > MAX_URL_LENGTH || blockedSite(url, settings)) return;
  if (url.origin === frame.page.origin || stream?.url === url.href) return url;
  if (isPublicHttpUrl(url.href) && (isKnownVideoPage(url.href) || isDirectMediaUrl(url.href) || embedded)) return url;
}

function buildMediaHeaders(frame: Frame, target: URL): Record<string, string> {
  if (frame.private) return {};
  const headers = { ...network.mediaHeaders(frame.tabId, target.href, frame.tabUrl, frame.frameId, frame.page.href) };
  const downgrade = frame.page.protocol === "https:" && target.protocol === "http:";
  if (downgrade) {
    delete headers.Referer;
    delete headers.Origin;
  } else {
    if (!headers.Referer) {
      if (target.origin === frame.page.origin) {
        const clean = new URL(frame.page.href);
        clean.hash = "";
        headers.Referer = clean.href;
      } else {
        headers.Referer = `${frame.page.origin}/`;
      }
    }
    if (!headers.Origin) {
      headers.Origin = frame.page.origin;
    }
  }
  const userAgent = typeof navigator === "undefined" ? undefined : navigator.userAgent;
  if (!headers["User-Agent"] && userAgent?.trim() && !/[\u0000-\u001f\u007f]/.test(userAgent)) {
    headers["User-Agent"] = userAgent;
  }
  if (!headers["Accept-Language"]) {
    headers["Accept-Language"] = "en-US,en;q=0.9";
  }
  return headers;
}

function directStreamChoice(url: string, mime = "", sizeBytes?: number): PillFormat {
  const parsed = httpUrl(url);
  const extMatch = parsed ? /\.([a-z0-9]+)$/i.exec(parsed.pathname)?.[1]?.toUpperCase() : undefined;
  const mimeExt = mime.startsWith("video/") || mime.startsWith("audio/")
    ? mime.split("/")[1]?.replace(/^x-/, "").replace("matroska", "mkv").replace("quicktime", "mov").toUpperCase()
    : undefined;
  const container = extMatch || mimeExt || "VIDEO";
  const sub = sizeBytes && sizeBytes > 0 ? `${container} · ${formatBytes(sizeBytes)}` : container;
  return { id: `direct:${url}`, label: "Download Video", sub };
}

async function handoffDirectVideo(frame: Frame, target: URL): Promise<ActionResult> {
  if (blockedSite(target, settings)) {
    return { ok: false, message: "SlipStream is disabled for this site." };
  }
  const headers = buildMediaHeaders(frame, target);
  const downgrade = frame.page.protocol === "https:" && target.protocol === "http:";
  const captured = frame.private ? {} : network.headers(frame.tabId, target.href, frame.tabUrl);
  if (!frame.private && !downgrade && captured.Cookie) {
    headers.Cookie = captured.Cookie;
  } else if (!frame.private && !downgrade && !frame.cookieStoreId?.startsWith("firefox-private")) {
    const cookie = await collectCookiesForUrl(target, frame.cookieStoreId, frame.private, frame.page.hostname, true);
    if (cookie) headers.Cookie = cookie;
  }
  const referrer = headers.Referer;
  let filename: string | undefined;
  try {
    filename = safeFilename(decodeURIComponent(target.pathname));
  } catch {
    filename = safeFilename(target.pathname);
  }
  return result(
    await handoffHttp(target, { headers, referrer }, filename),
    ["transfer_created", "ack"],
  );
}

function startOrReuseProbe(frame: Frame, url: string, playlist: boolean, timeoutMs: number): Probe | undefined {
  if (frame.private) return;
  const key = probeKey(frame, url, playlist);
  const existing = cachedProbe(frame, url, playlist);
  if (existing) return existing;
  if (!native.state.connected || activeProbeCount() >= MAX_CONCURRENT_PROBES) return;
  if (probes.size >= 64) {
    const oldest = [...probes].find(([, entry]) => entry.settled);
    if (oldest) probes.delete(oldest[0]);
  }
  const controller = new AbortController();
  const targetUrl = httpUrl(url);
  const headers = targetUrl ? buildMediaHeaders(frame, targetUrl) : undefined;
  const response = native.probe(
    {
      type: "probe_media",
      url,
      ...(playlist ? { playlist: true } : {}),
      ...(headers && Object.keys(headers).length ? { headers } : {}),
    },
    AbortSignal.any([controller.signal, AbortSignal.timeout(timeoutMs)]),
  );
  const active: Probe = { frame, expires: Date.now() + 120_000, controller, response, settled: false };
  probes.set(key, active);
  void response.then((res) => {
    if (
      probes.get(key) === active
      && (!res || res.type !== "media_probed" || !res.probe || typeof res.probe !== "object" || !Array.isArray(res.probe.formats))
    ) {
      probes.delete(key);
    }
  }).catch(() => {
    if (probes.get(key) === active) probes.delete(key);
  }).finally(() => {
    active.settled = true;
  });
  return active;
}

function pillOpen(frame: Frame, message: Extract<BrowserMessage, { kind: "pill-open" }>): ActionResult {
  const stream = observedStream(frame);
  const url = parseMediaTarget(frame, message.url, stream, message.embedded === true);
  if (frame.private || !url || (message.requestId !== undefined && !Number.isSafeInteger(message.requestId)) || blockedSite(frame.page, settings)
    || blockedSite(httpUrl(frame.tabUrl), settings) || blockedSite(url, settings)) return { ok: false };
  if (stream?.frameId === frame.frameId && stream.url === url.href && isHls(url, stream.mime)) {
    if (!stream.variants) readHls(stream);
    return { ok: true };
  }
  const streamAllowed = stream?.kind === "direct" && !blockedSite(httpUrl(stream.url), settings)
    && (stream.url === url.href || (url.href === frame.page.href && !isKnownVideoPage(url.href)));
  const directTarget = streamAllowed
    ? { url: stream!.url, mime: stream!.mime, sizeBytes: stream!.sizeBytes }
    : isDirectMediaUrl(url.href)
      ? { url: url.href, mime: "", sizeBytes: undefined }
      : undefined;
  const directChoice = directTarget ? directStreamChoice(directTarget.url, directTarget.mime, directTarget.sizeBytes) : undefined;
  if (directChoice) {
    const immediate: PillFormatsMessage = {
      kind: "pill-formats",
      url: url.href,
      requestId: message.requestId,
      formats: [directChoice],
      isPlaylist: false,
    };
    postToFrame(frame.tabId, frame.frameId, immediate, frame.documentId);
  }
  const playlist = message.isPlaylist === true && isPlaylistPage(url.href);
  const key = probeKey(frame, url.href, playlist);
  const active = startOrReuseProbe(frame, url.href, playlist, 12_000);
  if (!active) return { ok: Boolean(directChoice) };
  void active.response.then((response) => {
    if (probes.get(key) !== active || !response || response.type !== "media_probed") return;
    const formats = probeFormats(response.probe);
    if (!formats.length && directChoice) return;
    const mergedFormats = directChoice ? [directChoice, ...formats] : formats;
    const update: PillFormatsMessage = {
      kind: "pill-formats", url: url.href, requestId: message.requestId,
      formats: mergedFormats, isPlaylist: playlist || response.probe.is_playlist,
    };
    postToFrame(frame.tabId, frame.frameId, update, frame.documentId);
  }).catch(() => {});
  return { ok: true };
}

async function mediaAction(action: MediaAction, source: string, links: unknown[], frame: Frame, embedded = false): Promise<ActionResult> {
  const { page, tabId, tabUrl, frameId } = frame;
  if (action === "mute") {
    if (frame.private) return { ok: false };
    await updateSettings((current) => matchesSite(page, current.mutedSites)
      ? current : { ...current, mutedSites: [...current.mutedSites, page.hostname] });
    return { ok: true };
  }
  if ([page, httpUrl(tabUrl)].some((url) => blockedSite(url, settings))) {
    return { ok: false, message: "SlipStream is disabled for this site." };
  }
  if (action === "links") return grabLinks(links);
  if (source.length > MAX_URL_LENGTH) return { ok: false, message: "Media URL is too long." };
  const observed = observedStream(frame);
  const parsedSource = source ? parseMediaTarget(frame, source, observed, embedded) : undefined;
  if (source && !parsedSource) return { ok: false, message: "SlipStream cannot use this media URL." };
  let url = parsedSource?.href ?? observed?.url ?? page.href;
  if (url.length > MAX_URL_LENGTH) return { ok: false, message: "Media URL is too long." };
  if (blockedSite(httpUrl(url), settings)) return { ok: false, message: "SlipStream is disabled for this site." };
  if (action === "picker" || action === "playlist") {
    return result(await native.send({ type: "open_desktop_window", open_picker_url: url }), ["ack"]);
  }
  if (action.startsWith("direct:")) {
    const directUrl = httpUrl(action.slice("direct:".length));
    if (!directUrl || blockedSite(directUrl, settings)
      || (directUrl.href !== url && observed?.url !== directUrl.href && !isPublicHttpUrl(directUrl.href))) {
      return { ok: false, message: "That video stream is unavailable." };
    }
    return handoffDirectVideo(frame, directUrl);
  }

  const audio = action === "mp3_320" || action === "flac" ? action : undefined;
  let formatId: string | undefined;
  let height: number | undefined;
  if (action.startsWith("format:")) {
    formatId = action.slice("format:".length);
  } else if (action.startsWith("hls:")) {
    const variant = observed?.url === url ? observed.variants?.find((entry) => entry.url === action.slice(4)) : undefined;
    if (!variant || blockedSite(httpUrl(variant.url), settings)) return { ok: false, message: "That stream quality is unavailable." };
    if (observed?.separateAudio) height = variant.height ?? 0;
    else url = variant.url;
  } else if (action === "4k" || action === "1080p") {
    if (frame.private) {
      return { ok: false, message: "Automatic format probing is disabled in private windows." };
    }
    const useObservedDirect = observed?.kind === "direct" && !blockedSite(httpUrl(observed.url), settings)
      && (observed.url === url || (url === page.href && !isKnownVideoPage(url)));
    const directTarget = useObservedDirect
      ? httpUrl(observed!.url)
      : isDirectMediaUrl(url)
        ? httpUrl(url)
        : undefined;
    if (directTarget && !blockedSite(directTarget, settings)) {
      return handoffDirectVideo(frame, directTarget);
    }
    height = action === "4k" ? 2160 : 1080;
  }

  if (height !== undefined) {
    if (frame.private) {
      return { ok: false, message: "Automatic format probing is disabled in private windows." };
    }
    const active = startOrReuseProbe(frame, url, false, 30_000);
    if (!active) {
      return {
        ok: false,
        message: native.state.connected ? "Too many probes in progress. Try again shortly." : "SlipStream desktop is disconnected.",
      };
    }
    const response = await active.response;
    if (!response || response.type !== "media_probed") {
      return response?.type === "error" ? result(response, ["media_probed"]) : { ok: false, message: "SlipStream returned no formats." };
    }
    formatId = height > 0 ? selectPillFormatId(response.probe, height) : "best";
    if (!formatId) return { ok: false, message: "That quality is unavailable. Open the format picker for more options." };
  }
  if (formatId !== undefined && !validFormatId(formatId)) return { ok: false, message: "Invalid media format." };
  return result(await native.send({
    type: "add_media_download",
    request: {
      url, format_id: formatId, audio_only_preset: audio,
      embed_subtitles_langs: [], lossless_remux: !audio, normalize_audio: false,
      headers: frame.private ? {} : network.mediaHeaders(tabId, url, tabUrl, frameId, page.href),
    },
  }), ["transfer_created", "ack"]);
}

const actions = new Set<MediaAction>(["4k", "1080p", "mp3_320", "flac", "picker", "playlist", "mute", "links"]);
function isMediaAction(value: unknown): value is MediaAction {
  return typeof value === "string" && (actions.has(value as MediaAction)
    || (value.startsWith("format:") && validFormatId(value.slice(7)))
    || (value.startsWith("hls:") && value.length <= 8196 && !!httpUrl(value.slice(4)))
    || (value.startsWith("direct:") && value.length <= 8199 && !!httpUrl(value.slice(7))));
}

api.runtime.onMessage.addListener((message: BrowserMessage, sender, reply) => {
  if (sender.id !== api.runtime.id || !message || typeof message !== "object") return;
  const frame = getFrame(sender);

  if (message.kind === "frame-reset" && frame) {
    clearFrame(frame.tabId, frame.frameId === 0 ? undefined : frame.frameId);
    reply({ ok: true });
    return;
  }
  if (message.kind === "alt" && frame?.frameId === 0 && typeof message.held === "boolean") {
    if (message.held) heldAlt.add(frame.tabId);
    else heldAlt.delete(frame.tabId);
    reply({ ok: true });
    return;
  }
  if (message.kind === "stream" && frame) {
    const stream = observedStream(frame);
    const formats: PillFormat[] | undefined = stream?.variants?.length
      ? stream.variants.map(({ url, label }) => ({ id: ("hls:" + url) as MediaAction, label }))
      : stream?.kind === "direct"
        ? [directStreamChoice(stream.url, stream.mime, stream.sizeBytes)]
        : undefined;
    reply({
      url: stream?.url,
      formats,
      childActive: frame.frameId === 0 && childPlayers.has(frame.tabId),
    });
    return;
  }

  const fromPopup = sender.url?.split(/[?#]/, 1)[0] === popupUrl;
  if (fromPopup && message.kind === "wake") {
    native.wake();
    reply({ ok: true, state: native.state });
    return;
  }
  let response: Promise<ActionResult> | undefined;
  if (message.kind === "media" && frame && isMediaAction(message.action) && typeof message.url === "string") {
    response = settingsReady.then(() => mediaAction(
      message.action, message.url, Array.isArray(message.links) ? message.links : [], frame, message.embedded === true,
    ));
  } else if (message.kind === "pill-open" && frame && !frame.private) {
    response = settingsReady.then(() => pillOpen(frame, message));
  } else if (message.kind === "pill-spot" && frame && !frame.private) {
    response = siteSpot(frame, message.spot);
  } else if (fromPopup && message.kind === "desktop") {
    const request = message.request;
    if (request?.type === "open_desktop_window") {
      response = native.send({ type: request.type }).then((res) => result(res, ["ack"]));
    } else if ((request?.type === "pause_transfer" || request?.type === "resume_transfer")
      && typeof request.transfer_id === "string") {
      response = native.send({ type: request.type, transfer_id: request.transfer_id }).then((res) => result(res, ["ack"]));
    }
  } else if (fromPopup && message.kind === "save-settings") {
    response = (async () => {
      const [activeTab] = sender.tab ? [sender.tab] : await api.tabs.query({ active: true, currentWindow: true });
      if (!canUseSiteStorage(api.extension?.inIncognitoContext, sender.tab?.incognito ?? activeTab?.incognito)) {
        return { ok: false, message: "Settings cannot be changed from an incognito window." };
      }
      await updateSettings(() => readSettings(message.settings));
      return { ok: true, message: "Settings saved." };
    })();
  } else if (fromPopup && message.kind === "refresh" && typeof message.id === "string" && typeof message.transferId === "string") {
    const senderIncognito = Boolean(api.extension?.inIncognitoContext || sender.tab?.incognito);
    response = refreshDownload(message.id, message.transferId, senderIncognito);
  }
  if (!response) return;
  void response.then(reply, () => reply({ ok: false, message: "SlipStream could not complete the request." }));
  return true;
});

const REFRESH_TTL = 5 * 60_000;
type RefreshIntent = { url: string; tabId?: number; origin?: string; incognito?: boolean; expires: number };

async function refreshDownload(id: string, transferId: string, senderIncognito: boolean): Promise<ActionResult> {
  const key = refreshKey(id);
  const entry = (await api.storage.session.get(key))[key] as RefreshIntent | undefined;
  if (!entry || Date.now() > entry.expires) return { ok: false, message: "This refresh link expired. Open it again from the context menu." };
  if (Boolean(entry.incognito) !== senderIncognito) {
    return { ok: false, message: "This refresh link belongs to a different browsing context." };
  }
  const target = httpUrl(entry.url);
  if (!target) return { ok: false, message: "This refresh link is invalid." };
  const transfer = native.state.batch?.transfers.find((item) => item.id === transferId && isRefreshableHttpTransfer(item));
  if (!transfer) return { ok: false, message: "Select an active or failed HTTP transfer to refresh." };
  const tab = entry.tabId === undefined ? undefined : await api.tabs.get(entry.tabId).catch(() => undefined);
  const tabInScope = entry.origin === httpUrl(tab?.url)?.origin ? tab : undefined;
  const response = result(await native.send({ type: "refresh_download_url", request: {
    transfer_id: transferId, new_url: target.href, ...httpHandoffContext(target, tabInScope, tabReferrer(target, tabInScope)),
  } }), ["ack"]);
  if (response.ok) await api.storage.session.remove(key);
  return response;
}

api.runtime.onInstalled.addListener(() => {
  void api.contextMenus.removeAll().then(() => {
    api.contextMenus.create({ id: "download", title: "Download with SlipStream", contexts: ["link", "video", "audio"] });
    api.contextMenus.create({ id: "links", title: "Download all links on page", contexts: ["page", "link"] });
    api.contextMenus.create({ id: "refresh", title: "Refresh expired link", contexts: ["link", "video", "audio"] });
  }).catch(report);
});

api.contextMenus.onClicked.addListener((info, tab) => {
  void (async () => {
    const rawLink = info.linkUrl ?? info.srcUrl;
    const url = httpUrl(rawLink);
    const magnet = info.menuItemId === "download" ? magnetUri(info.linkUrl) : undefined;
    const page = httpUrl(info.frameUrl ?? info.pageUrl);
    const tabInScope = page && page.origin === httpUrl(tab?.url)?.origin ? tab : undefined;
    if (info.menuItemId === "download" && magnet) {
      await native.send({ type: "add_torrent", request: { source: magnet, sequential: false } });
    } else if (info.menuItemId === "download" && url) {
      await handoffHttp(url, httpHandoffContext(url, tabInScope, tabReferrer(url, tabInScope)));
    } else if (info.menuItemId === "links" && tab?.id !== undefined) {
      const page = httpUrl(tab.url);
      if (page) {
        const links = await api.tabs.sendMessage(tab.id, { kind: "collect-links" }, { frameId: 0 });
        if (Array.isArray(links)) await grabLinks(links);
      }
    } else if (info.menuItemId === "refresh" && url) {
      const id = crypto.randomUUID();
      const incognito = Boolean(api.extension?.inIncognitoContext || tab?.incognito);
      const intent: RefreshIntent = {
        url: url.href, tabId: tabInScope?.id, origin: page?.origin, incognito, expires: Date.now() + REFRESH_TTL,
      };
      const key = refreshKey(id);
      await api.storage.session.set({ [key]: intent });
      api.alarms.create(key, { when: intent.expires });
      const targetUrl = popupUrl + "?refresh=" + id;
      try {
        await api.tabs.create({ url: targetUrl, ...(tab?.windowId !== undefined && { windowId: tab.windowId }) });
      } catch {
        // Chrome spanning mode disallows extension tabs inside incognito windows.
        await api.tabs.create({ url: targetUrl });
      }
    }
  })().catch(report);
});

api.alarms.onAlarm.addListener(({ name }) => {
  if (name.startsWith("refresh:")) void api.storage.session.remove(name).catch(report);
});
