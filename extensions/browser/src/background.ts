import type { NativeBridgeResponse } from "@slipstream/ipc-types";
import { extension as api } from "./shared/browser";
import {
  blockedSite, canAttributeTab, defaults, downloadLinks, httpUrl, magnetUri, matchesSite,
  readSettings, safeFilename, sameDownloadOrigin, shouldIntercept, type InterceptionSettings,
} from "./shared/downloads";
import { NativeClient } from "./shared/native";
import { isPublicHttpUrl, ObservedNetwork } from "./shared/network";
import {
  APPEARANCE_KEY, canUseSiteStorage, isRefreshableHttpTransfer, PILL_SPOTS_KEY, PLAYER_PORT,
  readPillSpot, readState, rememberPillSpot, SETTINGS_KEY, STATE_KEY,
  type ActionResult, type BrowserMessage, type MediaAction, type PillFormatsMessage,
} from "./shared/state";
import { isHls, isMediaStream, isPlaylistPage, mediaLength, mediaMime, parseHlsMaster, probeFormats, selectPillFormatId, validFormatId } from "./shared/streams";

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

type Frame = { page: URL; tabId: number; tabUrl: string; frameId: number; documentId?: string; private: boolean };
type Probe = { frame: Frame; expires: number; controller: AbortController; response: Promise<NativeBridgeResponse>; settled: boolean };
const probes = new Map<string, Probe>();
const childPlayers = new Map<number, Set<chrome.runtime.Port>>();
const hlsRequests = new Map<string, AbortController>();
const tabLookups = new Map<number, Promise<chrome.tabs.Tab | undefined>>();

function getFrame(sender: chrome.runtime.MessageSender): Frame | undefined {
  const page = httpUrl(sender.url);
  const tab = sender.tab;
  const top = httpUrl(tab?.url);
  const frameId = sender.frameId;
  if (sender.id !== api.runtime.id || !page || !top || tab?.id === undefined || frameId === undefined || frameId < 0
    || (sender.origin !== undefined && sender.origin !== page.origin) || (frameId === 0 && top.origin !== page.origin)
    || (sender.documentLifecycle !== undefined && sender.documentLifecycle !== "active")) return;
  return {
    page, tabId: tab.id, tabUrl: top.href, frameId, documentId: sender.documentId,
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
  if (details.tabId < 0 || origin.incognito || !page || !target || details.statusCode < 200 || details.statusCode >= 300
    || details.statusCode === 206 || headers.some(({ name }) => name.toLowerCase() === "content-range")
    || !isMediaStream(details.url, mime, mediaLength(headers))) return;
  void observedTab(details.tabId).then((tab) => {
    if (!tab?.url) return;
    if (network.recordStream(origin, tab.url, headers)) notifyStream(details.tabId, Math.max(0, details.frameId));
  }).catch(() => {});
  return undefined;
}, webFilter, ["responseHeaders"]);

function httpHandoffContext(url: URL, tab: chrome.tabs.Tab | undefined, fallback?: URL, allowCookie = true) {
  const canUsePageHeaders = canUseSiteStorage(api.extension?.inIncognitoContext, tab?.incognito);
  const headers = canUsePageHeaders ? { ...network.headers(tab?.id, url.href, tab?.url) } : {};
  if (!allowCookie) delete headers.Cookie;
  const userAgent = typeof navigator === "undefined" ? undefined : navigator.userAgent;
  if (!headers["User-Agent"] && userAgent?.trim() && !/[\u0000-\u001f\u007f]/.test(userAgent)) {
    headers["User-Agent"] = userAgent;
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

api.downloads.onCreated.addListener((item) => {
  void (async () => {
    if (!native.state.connected) return;
    await settingsReady;
    const [[active], [current]] = await Promise.all([
      api.tabs.query({ active: true, lastFocusedWindow: true }),
      api.downloads.search({ id: item.id }),
    ]);
    if (!current || current.state !== "in_progress" || current.paused) return;
    const target = httpUrl(current.finalUrl || current.url);
    if (!target) return;
    const downloadContext = current as typeof current & { cookieStoreId?: string };
    const tabContext = active as (typeof active & { cookieStoreId?: string }) | undefined;
    const tab = canAttributeTab(downloadContext, tabContext) ? active : undefined;
    if (!shouldIntercept(current, settings, {
      connected: native.state.connected,
      altHeld: tab?.id !== undefined && heldAlt.has(tab.id),
      tabUrl: tab?.url,
    })) return;
    const referrer = httpUrl(current.referrer);
    const page = httpUrl(tab?.url);
    const fallback = tab || (!active && !current.incognito)
      ? referrer ?? (page?.origin === target.origin ? page : undefined)
      : undefined;
    const context = httpHandoffContext(target, tab, fallback, !!referrer && sameDownloadOrigin(current));
    const response = await handoffHttp(target, context, safeFilename(current.filename));
    if (response.type === "transfer_created") await api.downloads.cancel(item.id);
  })().catch(report);
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

function pillOpen(frame: Frame, message: Extract<BrowserMessage, { kind: "pill-open" }>): ActionResult {
  const url = typeof message.url === "string" && message.url.length <= 8192 ? httpUrl(message.url) : undefined;
  const stream = observedStream(frame);
  if (frame.private || !url || (message.requestId !== undefined && !Number.isSafeInteger(message.requestId)) || blockedSite(frame.page, settings)
    || blockedSite(httpUrl(frame.tabUrl), settings) || blockedSite(url, settings)
    || (url.origin !== frame.page.origin && stream?.url !== url.href)) return { ok: false };
  if (stream?.frameId === frame.frameId && stream.url === url.href && isHls(url, stream.mime)) {
    if (!stream.variants) readHls(stream);
    return { ok: true };
  }
  const playlist = message.isPlaylist === true && isPlaylistPage(url.href);
  const key = probeKey(frame, url.href, playlist);
  let probe = cachedProbe(frame, url.href, playlist);
  if (!probe) {
    if (!native.state.connected || [...probes.values()].filter((entry) => !entry.settled).length >= 3) return { ok: false };
    if (probes.size >= 64) {
      const oldest = [...probes].find(([, entry]) => entry.settled);
      if (oldest) probes.delete(oldest[0]);
    }
    const controller = new AbortController();
    const response = native.probe({ type: "probe_media", url: url.href, ...(playlist ? { playlist: true } : {}) },
      AbortSignal.any([controller.signal, AbortSignal.timeout(12_000)]));
    probe = { frame, expires: Date.now() + 120_000, controller, response, settled: false };
    probes.set(key, probe);
  }
  const active = probe;
  void active.response.then((response) => {
    if (probes.get(key) !== active) return;
    active.settled = true;
    if (response.type !== "media_probed") {
      probes.delete(key);
      return;
    }
    const update: PillFormatsMessage = {
      kind: "pill-formats", url: url.href, requestId: message.requestId,
      formats: probeFormats(response.probe), isPlaylist: playlist || response.probe.is_playlist,
    };
    postToFrame(frame.tabId, frame.frameId, update, frame.documentId);
  }).catch(() => {});
  return { ok: true };
}

async function mediaAction(action: MediaAction, source: string, links: unknown[], frame: Frame): Promise<ActionResult> {
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
  const observed = observedStream(frame);
  let url = httpUrl(source)?.href ?? observed?.url ?? page.href;
  if (blockedSite(httpUrl(url), settings)) return { ok: false, message: "SlipStream is disabled for this site." };
  if (action === "picker" || action === "playlist") {
    return result(await native.send({ type: "open_desktop_window", open_picker_url: url }), ["ack"]);
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
    height = action === "4k" ? 2160 : 1080;
  }

  if (height !== undefined) {
    const response = await (cachedProbe(frame, url, false)?.response
      ?? native.probe({ type: "probe_media", url, playlist: false }, AbortSignal.timeout(30_000)));
    if (response.type !== "media_probed") {
      return response.type === "error" ? result(response, ["media_probed"]) : { ok: false, message: "SlipStream returned no formats." };
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
    || (value.startsWith("hls:") && value.length <= 8196 && !!httpUrl(value.slice(4))));
}

api.runtime.onMessage.addListener((message: BrowserMessage, sender, reply) => {
  if (sender.id !== api.runtime.id || !message || typeof message !== "object") return;
  const frame = getFrame(sender);

  if (message.kind === "frame-reset" && frame) {
    clearFrame(frame.tabId, frame.frameId === 0 ? undefined : frame.frameId);
    reply({ ok: true });
    return;
  }
  if (message.kind === "video-seen" && frame) {
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
    reply({
      url: stream?.url,
      formats: stream?.variants?.map(({ url, label }) => ({ id: "hls:" + url, label })),
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
      message.action, message.url, Array.isArray(message.links) ? message.links : [], frame,
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
      const [activeTab] = await api.tabs.query({ active: true, lastFocusedWindow: true });
      if (!canUseSiteStorage(api.extension?.inIncognitoContext, sender.tab?.incognito) || activeTab?.incognito) {
        return { ok: false, message: "Settings cannot be changed from an incognito window." };
      }
      await updateSettings(() => readSettings(message.settings));
      return { ok: true, message: "Settings saved." };
    })();
  } else if (fromPopup && message.kind === "refresh" && typeof message.id === "string" && typeof message.transferId === "string") {
    response = refreshDownload(message.id, message.transferId);
  }
  if (!response) return;
  void response.then(reply, () => reply({ ok: false, message: "SlipStream could not complete the request." }));
  return true;
});

const REFRESH_TTL = 5 * 60_000;
type RefreshIntent = { url: string; tabId?: number; origin?: string; expires: number };

async function refreshDownload(id: string, transferId: string): Promise<ActionResult> {
  const key = refreshKey(id);
  const entry = (await api.storage.session.get(key))[key] as RefreshIntent | undefined;
  if (!entry || Date.now() > entry.expires) return { ok: false, message: "This refresh link expired. Open it again from the context menu." };
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
      const intent: RefreshIntent = { url: url.href, tabId: tabInScope?.id, origin: page?.origin, expires: Date.now() + REFRESH_TTL };
      const key = refreshKey(id);
      await api.storage.session.set({ [key]: intent });
      api.alarms.create(key, { when: intent.expires });
      await api.tabs.create({ url: popupUrl + "?refresh=" + id });
    }
  })().catch(report);
});

api.alarms.onAlarm.addListener(({ name }) => {
  if (name.startsWith("refresh:")) void api.storage.session.remove(name).catch(report);
});
