import type { NativeBridgeRequest, NativeBridgeResponse } from "@slipstream/ipc-types";
import { extension as api } from "./shared/browser";
import {
  blockedSite, canAttributeTab, defaults, downloadLinks, httpUrl, magnetUri, matchesSite,
  readSettings, safeFilename, sameDownloadOrigin, shouldIntercept, type InterceptionSettings,
} from "./shared/downloads";
import { NativeClient } from "./shared/native";
import { ObservedNetwork } from "./shared/network";
import {
  APPEARANCE_KEY, isRefreshableHttpTransfer, readState, SETTINGS_KEY, STATE_KEY,
  type ActionResult, type BrowserMessage, type MediaAction,
} from "./shared/state";
import { isMediaStream, mediaMime, selectPillFormatId } from "./shared/streams";

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

function clearTab(tabId: number): void {
  heldAlt.delete(tabId);
  network.clear(tabId);
}
api.tabs.onRemoved.addListener(clearTab);
api.tabs.onUpdated.addListener((id, change) => {
  if (change.status === "loading" || change.url) clearTab(id);
});

const webFilter = { urls: ["http://*/*", "https://*/*"] };
const firefox = Array.isArray((api.runtime.getManifest().background as { scripts?: string[] } | undefined)?.scripts);
api.webRequest.onSendHeaders.addListener((details) => {
  network.recordHeaders(details, details.requestHeaders ?? []);
}, webFilter, firefox ? ["requestHeaders"] : ["requestHeaders", "extraHeaders"]);

api.webRequest.onHeadersReceived.addListener((details): undefined => {
  const headers = details.responseHeaders ?? [];
  if (details.tabId < 0 || details.statusCode < 200 || details.statusCode >= 300
    || !isMediaStream(details.url, mediaMime(headers))) return;
  const firefoxDetails = details as typeof details & { documentUrl?: string; originUrl?: string };
  const initiator = httpUrl(details.initiator ?? firefoxDetails.originUrl ?? firefoxDetails.documentUrl);
  void api.tabs.get(details.tabId).then((tab) => {
    // A cross-origin media URL is valid, but the request must belong to this page.
    if (!initiator || initiator.origin !== httpUrl(tab.url)?.origin) return;
    if (network.recordStream(details, tab.url, headers)) {
      void api.tabs.sendMessage(details.tabId, { kind: "stream-changed" }, { frameId: 0 }).catch(() => {});
    }
  }).catch(() => {});
  return undefined;
}, webFilter, ["responseHeaders"]);

function handoffHttp(url: string, tab: chrome.tabs.Tab | undefined, filename?: string): Promise<NativeBridgeResponse> {
  const headers = network.headers(tab?.id, url, tab?.url);
  return native.send({
    type: "add_http_download",
    request: { url, filename, headers, referrer: headers.Referer },
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
    const downloadContext = current as typeof current & { cookieStoreId?: string };
    const tabContext = active as (typeof active & { cookieStoreId?: string }) | undefined;
    const tab = canAttributeTab(downloadContext, tabContext) ? active : undefined;
    if (!shouldIntercept(current, settings, {
      connected: native.state.connected,
      altHeld: tab?.id !== undefined && heldAlt.has(tab.id),
      tabUrl: tab?.url,
    })) return;
    // A tab-less or cross-origin redirected download cannot carry the previous origin's headers.
    const sourceTab = httpUrl(current.referrer) && sameDownloadOrigin(current) ? tab : undefined;
    const response = await handoffHttp(current.finalUrl || current.url, sourceTab, safeFilename(current.filename));
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

async function mediaAction(action: MediaAction, source: string, links: unknown[], page: URL, tab: chrome.tabs.Tab): Promise<ActionResult> {
  if (action === "mute") {
    await updateSettings((current) => matchesSite(page, current.mutedSites)
      ? current : { ...current, mutedSites: [...current.mutedSites, page.hostname] });
    return { ok: true };
  }
  if (blockedSite(page, settings)) return { ok: false, message: "SlipStream is disabled for this site." };
  if (action === "links") return grabLinks(links);
  const candidate = httpUrl(source);
  const observed = network.stream(tab.id!, tab.url);
  const url = candidate?.href ?? observed?.url ?? page.href;
  if (blockedSite(httpUrl(url), settings)) return { ok: false, message: "SlipStream is disabled for this site." };
  if (action === "picker") return result(await native.send({ type: "open_desktop_window", open_picker_url: url }), ["ack"]);

  const audio = action === "mp3_320" || action === "flac" ? action : undefined;
  let formatId: string | undefined;
  if (!audio) {
    const expectedHeight = action === "4k" ? 2160 : 1080;
    const response = await native.send({ type: "probe_media", url });
    if (response.type !== "media_probed") {
      return response.type === "error" ? result(response, ["media_probed"]) : { ok: false, message: "SlipStream returned no formats." };
    }
    formatId = selectPillFormatId(response.probe, expectedHeight);
    if (!formatId) return { ok: false, message: "That quality is unavailable. Open the format picker for more options." };
  }
  return result(await native.send({
    type: "add_media_download",
    request: {
      url, format_id: formatId, audio_only_preset: audio,
      embed_subtitles_langs: [], lossless_remux: !audio, normalize_audio: false,
    },
  }), ["transfer_created", "ack"]);
}

const actions = new Set<MediaAction>(["4k", "1080p", "mp3_320", "flac", "picker", "mute", "links"]);

api.runtime.onMessage.addListener((message: BrowserMessage, sender, reply) => {
  if (sender.id !== api.runtime.id || !message || typeof message !== "object") return;
  const page = httpUrl(sender.url);
  const tab = sender.tab;
  const hasPage = page && tab?.id !== undefined && sender.frameId === 0
    && httpUrl(tab.url)?.origin === page.origin;

  if (message.kind === "alt" && hasPage && typeof message.held === "boolean") {
    if (message.held) heldAlt.add(tab.id!);
    else heldAlt.delete(tab.id!);
    reply({ ok: true });
    return;
  }
  if (message.kind === "stream" && hasPage) {
    reply({ url: network.stream(tab.id!, page.href)?.url });
    return;
  }

  const fromPopup = sender.url?.split(/[?#]/, 1)[0] === popupUrl;
  if (fromPopup && message.kind === "wake") {
    native.wake();
    reply({ ok: true, state: native.state });
    return;
  }
  let response: Promise<ActionResult> | undefined;
  if (message.kind === "media" && hasPage && actions.has(message.action) && typeof message.url === "string") {
    response = settingsReady.then(() => mediaAction(
      message.action, message.url, Array.isArray(message.links) ? message.links : [], page, tab,
    ));
  } else if (fromPopup && message.kind === "desktop") {
    const request = message.request;
    if (request?.type === "open_desktop_window") {
      response = native.send({ type: request.type }).then((res) => result(res, ["ack"]));
    } else if ((request?.type === "pause_transfer" || request?.type === "resume_transfer")
      && typeof request.transfer_id === "string") {
      response = native.send({ type: request.type, transfer_id: request.transfer_id }).then((res) => result(res, ["ack"]));
    }
  } else if (fromPopup && message.kind === "save-settings") {
    response = updateSettings(() => readSettings(message.settings)).then(() => ({ ok: true, message: "Settings saved." }));
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
  const transfer = native.state.batch?.transfers.find((item) => item.id === transferId && isRefreshableHttpTransfer(item));
  if (!transfer) return { ok: false, message: "Select an active or failed HTTP transfer to refresh." };
  const tab = entry.tabId === undefined ? undefined : await api.tabs.get(entry.tabId).catch(() => undefined);
  const headers = entry.origin === httpUrl(tab?.url)?.origin ? network.headers(tab?.id, entry.url, tab?.url) : {};
  const response = result(await native.send({ type: "refresh_download_url", request: {
    transfer_id: transferId, new_url: entry.url, headers, referrer: headers.Referer,
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
      await handoffHttp(url.href, tabInScope);
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
