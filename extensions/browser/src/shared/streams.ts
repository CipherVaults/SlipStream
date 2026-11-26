import type { MediaProbeResult } from "@slipstream/ipc-types";
import { httpUrl, isPublicHttpUrl, matchesSite } from "./downloads";
import { formatBytes, type PillFormat } from "./state";

export interface Header { name: string; value?: string }
export interface HlsVariant { url: string; height?: number; bandwidth?: number; label: string }

const hlsTypes = new Set(["application/vnd.apple.mpegurl", "application/x-mpegurl"]);
const forwardedNames = new Map([
  ["cookie", "Cookie"], ["referer", "Referer"], ["origin", "Origin"],
  ["user-agent", "User-Agent"], ["accept-language", "Accept-Language"],
]);

function header(headers: Header[], name: string): string | undefined {
  return headers.find((entry) => entry.name.toLowerCase() === name)?.value;
}

export function mediaMime(headers: Header[]): string {
  return header(headers, "content-type")?.split(";")[0].trim().toLowerCase() ?? "";
}

export function mediaLength(headers: Header[]): number | undefined {
  const value = header(headers, "content-length")?.trim();
  const bytes = value && /^\d+$/.test(value) ? Number(value) : undefined;
  return Number.isSafeInteger(bytes) ? bytes : undefined;
}

export function parseContentRangeTotal(headers: Header[]): number | undefined {
  const value = header(headers, "content-range")?.trim();
  if (!value) return undefined;
  const match = /^bytes\s+\d+-\d+\/(\d+)$/i.exec(value);
  if (!match) return undefined;
  const total = Number(match[1]);
  return Number.isSafeInteger(total) && total > 0 ? total : undefined;
}

export function isHls(url: URL, mime: string): boolean {
  return /\.m3u8$/i.test(url.pathname) || hlsTypes.has(mime);
}

export function isManifest(url: URL, mime: string): boolean {
  return isHls(url, mime) || /\.mpd$/i.test(url.pathname) || mime === "application/dash+xml";
}

export function streamKind(url: URL, mime: string): "hls" | "dash" | "direct" {
  if (isHls(url, mime)) return "hls";
  if (/\.mpd$/i.test(url.pathname) || mime === "application/dash+xml") return "dash";
  return "direct";
}

export function isMediaStream(value: string, mime = "", contentLengthBytes?: number): boolean {
  const url = httpUrl(value);
  if (!url) return false;
  const type = mime.split(";")[0].trim().toLowerCase();
  if (type === "video/mp2t" || /\.(ts|m4s|m4f)$/i.test(url.pathname) || /\/seg-/i.test(url.pathname)
    || [...url.searchParams.keys()].some((param) => /^(range|bytestart|byteend)$/i.test(param))) return false;
  if (isManifest(url, type)) return true;
  if (contentLengthBytes !== undefined && contentLengthBytes < 300_000) return false;
  return /\.(mp4|webm|mkv|mov|m4v)$/i.test(url.pathname) || /^(video|audio)\/[\w.+-]+$/.test(type);
}

export function isDirectMediaUrl(value: string, mime = ""): boolean {
  const url = httpUrl(value);
  if (!url) return false;
  const type = mime.split(";")[0].trim().toLowerCase();
  if (isManifest(url, type)) return false;
  return isMediaStream(value, mime);
}

export function parseHlsMaster(manifestText: string, baseUrl: string): HlsVariant[] {
  if (!httpUrl(baseUrl) || !/^\s*#EXTM3U(?:\s|$)/.test(manifestText)) return [];
  const variants: HlsVariant[] = [];
  let attributes: string | undefined;
  for (const raw of manifestText.split(/\r?\n/)) {
    const line = raw.trim();
    if (line.startsWith("#EXT-X-STREAM-INF:")) {
      attributes = line.slice("#EXT-X-STREAM-INF:".length);
      continue;
    }
    if (!attributes || !line || line.startsWith("#")) continue;
    const values = new Map([...attributes.matchAll(/(?:^|,)\s*([\w-]+)\s*=\s*("[^"]*"|[^,]*)/g)]
      .map(([, key, value]) => [key, value.trim()]));
    attributes = undefined;
    if (/[\u0000-\u0020\u007f\\]/.test(line)) continue;
    try {
      const url = httpUrl(new URL(line, baseUrl).href);
      if (!url || url.href.length > 8192 || !isPublicHttpUrl(url.href)) continue;
      const height = Number(/^\d+x(\d+)$/i.exec(values.get("RESOLUTION") ?? "")?.[1]);
      const bandwidth = Number(/^\d+$/.exec(values.get("BANDWIDTH") ?? "")?.[0]);
      if (!Number.isSafeInteger(bandwidth) || bandwidth <= 0) continue;
      const quality = Number.isSafeInteger(height) && height > 0 ? height : undefined;
      const label = quality ? quality + "p · HLS" : Math.round(bandwidth / 1000) + " kbps · HLS";
      variants.push({ url: url.href, height: quality, bandwidth, label });
    } catch {
      // Skip an invalid variant URI without losing the rest of the master.
    }
  }
  variants.sort((a, b) => (b.height ?? 0) - (a.height ?? 0) || (b.bandwidth ?? 0) - (a.bandwidth ?? 0));
  const labels = new Set<string>();
  return variants.filter(({ label }) => !labels.has(label) && Boolean(labels.add(label)));
}

export function scopedHeaders(
  requestUrl: string,
  tabUrl: string | undefined,
  initiator: string | undefined,
  headers: Header[],
  media = false,
): Record<string, string> {
  const target = httpUrl(requestUrl);
  const origin = httpUrl(tabUrl)?.origin;
  if (!target || !origin || httpUrl(initiator)?.origin !== origin) return {};
  const downgrade = origin.startsWith("https://") && target.protocol === "http:";
  const result: Record<string, string> = {};
  for (const { name, value } of headers) {
    const canonical = forwardedNames.get(name.toLowerCase());
    if (!canonical || !value || /[\u0000-\u001f\u007f]/.test(value)) continue;
    if (canonical === "Cookie" && (media || target.origin !== origin)) continue;
    if (canonical === "Accept-Language" && !media) continue;
    if (canonical === "Referer") {
      const referrer = httpUrl(value);
      if (!referrer || (referrer.origin !== origin && referrer.origin !== target.origin)
        || (referrer.protocol === "https:" && target.protocol === "http:")) continue;
      referrer.hash = "";
      result.Referer = referrer.href;
      continue;
    }
    if (canonical === "Origin" && (value !== origin || downgrade)) continue;
    result[canonical] = value;
  }
  return result;
}

const providers: ReadonlyArray<{ sites: string[]; path: RegExp; query?: (url: URL) => boolean }> = [
  { sites: ["youtube.com"], path: /^\/(shorts|live|embed)\/[^/]+|^\/watch\/?$/, query: (url) => !/^\/watch\/?$/.test(url.pathname) || !!url.searchParams.get("v") },
  { sites: ["youtu.be"], path: /^\/[\w-]+\/?$/ },
  { sites: ["tiktok.com"], path: /^\/(?:@[^/]+\/(?:video|photo)\/\d+|(?:t|v)\/[^/]+)\/?$/ },
  { sites: ["vm.tiktok.com", "vt.tiktok.com"], path: /^\/[\w-]+\/?$/ },
  { sites: ["instagram.com"], path: /^\/(reels?|p|tv)\/[^/]+/ },
  { sites: ["facebook.com"], path: /(?:^|\/)(reels?|videos|posts|permalink)\/[^/]+|^\/(watch\/?|permalink\.php|story\.php)$/, query: (url) => {
    if (/^\/watch\/?$/.test(url.pathname)) return !!url.searchParams.get("v");
    if (/\.php$/.test(url.pathname)) return !!url.searchParams.get("story_fbid");
    return true;
  } },
  { sites: ["fb.watch"], path: /^\/[\w-]+\/?$/ },
  { sites: ["x.com", "twitter.com"], path: /\/status\/\d+(?:\/|$)/ },
  { sites: ["vimeo.com"], path: /\/\d+(?:\/|$)/ },
  { sites: ["dailymotion.com"], path: /\/(video|shorts)\/[^/]+/ },
  { sites: ["dai.ly"], path: /^\/[\w-]+\/?$/ },
  { sites: ["reddit.com"], path: /\/comments\/[^/]+/ },
  { sites: ["twitch.tv"], path: /^\/(videos|clip)\/[^/]+|\/clip\/[^/]+/ },
  { sites: ["clips.twitch.tv"], path: /^\/[\w-]+\/?$/ },
];

export function isKnownVideoPage(value: string): boolean {
  const url = httpUrl(value);
  return Boolean(url && providers.some(({ sites, path, query }) =>
    matchesSite(url, sites) && path.test(url.pathname) && (!query || query(url))));
}

export function isPlaylistPage(value: string): boolean {
  const url = httpUrl(value);
  return Boolean(url && matchesSite(url, ["youtube.com", "youtu.be"]) && url.searchParams.get("list"));
}

function cleanPostUrl(url: URL): string {
  for (const key of [...url.searchParams.keys()]) {
    if (/^(utm_.*|fbclid|igsh|igshid|si|__cft__.*|__tn__.*|mibextid|ref_src)$/i.test(key)) url.searchParams.delete(key);
  }
  url.hash = "";
  return url.href;
}

function enclosingElement(element: Element): Element | null {
  const parent = element.parentNode;
  return element.parentElement ?? (parent instanceof ShadowRoot ? parent.host : null);
}

const article = 'article, [role="article"]';

export function resolveVideoPostUrl(video: HTMLVideoElement): string | undefined {
  let element: Element | null = video;
  let previous: Element | null = null;
  for (let depth = 0; element && depth <= 25; depth++) {
    if (element.matches("body, html")) break;
    for (const link of element.matches("a[href]") ? [element] : element.querySelectorAll("a[href]")) {
      if (link !== element && previous?.contains(link)) continue;
      const enclosingArticle: Element | null = link.closest(article);
      if (enclosingArticle && !enclosingArticle.contains(element)) continue;
      const href = link.getAttribute("href");
      if (!href) continue;
      let url: URL;
      try { url = new URL(href, link.baseURI); } catch { continue; }
      if (isKnownVideoPage(url.href)) return cleanPostUrl(url);
    }
    if (element.matches(article)) break;
    previous = element;
    element = enclosingElement(element);
  }
}

export function validFormatId(id: string): boolean {
  return id.length <= 256 && /^[\w][\w.-]*(?:[+/][\w][\w.-]*)*$/.test(id);
}

export function probeFormats(probe: MediaProbeResult): PillFormat[] {
  const hasAudio = probe.formats.some((format) => format.has_audio);
  return probe.formats.filter((format) => format.has_video && validFormatId(format.format_id))
    .sort((a, b) => (b.height ?? 0) - (a.height ?? 0) || (b.fps ?? 0) - (a.fps ?? 0))
    .slice(0, 20)
    .map(({ format_id, label, has_audio, filesize_bytes }) => ({
      id: "format:" + (hasAudio && !has_audio && validFormatId(format_id + "+bestaudio") ? format_id + "+bestaudio" : format_id) as PillFormat["id"],
      label,
      sub: filesize_bytes && filesize_bytes > 0 ? formatBytes(filesize_bytes) : undefined,
    }));
}

const MAX_FORMAT_ID_LEN = 256;
const BEST_AUDIO_SUFFIX = "+bestaudio";

export function selectPillFormatId(probe: MediaProbeResult, expectedHeight: number): string | undefined {
  const videoFormats = probe.formats.filter(
    (format) => format.has_video && validFormatId(format.format_id),
  );
  if (!videoFormats.length) return undefined;

  const exact = videoFormats.filter((format) => format.height === expectedHeight);
  let candidates: typeof videoFormats;
  if (exact.length) {
    candidates = exact;
  } else {
    const withinCap = videoFormats
      .filter((format) => format.height !== null && format.height <= expectedHeight)
      .sort((a, b) => (b.height ?? 0) - (a.height ?? 0) || (b.fps ?? 0) - (a.fps ?? 0));
    if (withinCap.length) {
      const bestHeight = withinCap[0].height;
      candidates = withinCap.filter((format) => format.height === bestHeight);
    } else {
      const withoutHeight = videoFormats.filter((format) => format.height === null);
      candidates = withoutHeight.length ? withoutHeight : videoFormats;
    }
  }

  const muxed = candidates.find((format) => format.has_audio);
  if (muxed) return muxed.format_id;
  if (probe.formats.some((format) => format.has_audio)) {
    const combinable = candidates.find((format) => (format.format_id + BEST_AUDIO_SUFFIX).length <= MAX_FORMAT_ID_LEN);
    if (combinable) return combinable.format_id + BEST_AUDIO_SUFFIX;
  }
  return candidates[0]?.format_id;
}

export class StreamNavigationState {
  streamUrl: string | undefined;
  epoch = 0;
  private lastHref: string;

  constructor(initialHref: string) {
    this.lastHref = initialHref;
  }

  sync(currentHref: string): boolean {
    if (currentHref === this.lastHref) return false;
    this.lastHref = currentHref;
    this.streamUrl = undefined;
    this.epoch += 1;
    return true;
  }

  update(epochAtRequest: number, hrefAtRequest: string, currentHref: string, observedUrl: string | undefined): void {
    if (epochAtRequest === this.epoch && hrefAtRequest === currentHref) {
      this.streamUrl = observedUrl;
    }
  }

  resolveTarget(videoSrc: string, currentHref: string): string {
    this.sync(currentHref);
    return httpUrl(videoSrc)?.href ?? this.streamUrl ?? currentHref;
  }
}
