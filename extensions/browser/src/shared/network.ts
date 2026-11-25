import { httpUrl } from "./downloads";
import { isHls, isManifest, isMediaStream, mediaLength, mediaMime, scopedHeaders, type Header, type HlsVariant } from "./streams";

export function isPublicHttpUrl(raw: string): boolean {
  const url = httpUrl(raw);
  if (!url) return false;
  const host = url.hostname.replace(/\.$/, "");
  if (host.startsWith("[")) {
    const ip = host.slice(1, -1);
    // Global unicast excludes mapped IPv4, loopback, ULA and link-local.
    return /^[23][0-9a-f]{3}:/.test(ip) && !ip.startsWith("2001:db8:");
  }
  if (/^\d+(?:\.\d+){3}$/.test(host)) {
    const [a, b, c] = host.split(".").map(Number);
    return !(a === 0 || a === 10 || a === 127 || a >= 224 || (a === 100 && b >= 64 && b <= 127)
      || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168)
      || (a === 192 && b === 0 && (c === 0 || c === 2)) || (a === 198 && (b === 18 || b === 19 || (b === 51 && c === 100)))
      || (a === 203 && b === 0 && c === 113));
  }
  const labels = host.split(".");
  return labels.length > 1 && labels.every(label => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label))
    && !["localhost", "local", "internal"].includes(labels.at(-1)!) && !host.endsWith(".home.arpa");
}

type Request = {
  tabId: number;
  frameId?: number;
  requestId: string;
  url: string;
  initiator?: string;
  documentUrl?: string;
  originUrl?: string;
  incognito?: boolean;
};
type Scope = { tabId: number; frameId: number; origin: string; topOrigin: string; time: number };
type RequestHeaders = Scope & { target: string; http: Record<string, string>; media: Record<string, string> };
type Detection = Scope & { url: string; mime: string; variants?: HlsVariant[]; separateAudio?: boolean };
const TTL = 120_000;
const frameKey = (tabId: number, frameId: number) => tabId + ":" + frameId;
const requestPage = (details: Request) => httpUrl(details.documentUrl ?? details.initiator ?? details.originUrl);

export class ObservedNetwork {
  private requests = new Map<string, RequestHeaders>();
  private streams = new Map<string, Detection>();

  get cacheSize(): { requests: number; streams: number } {
    return { requests: this.requests.size, streams: this.streams.size };
  }

  prune(now = Date.now()): void {
    for (const [id, entry] of this.requests) {
      if (now - entry.time >= TTL) this.requests.delete(id);
    }
    for (const [key, entry] of this.streams) {
      if (now - entry.time >= TTL) this.streams.delete(key);
    }
  }

  private scope(details: Request, tabUrl: string | undefined, now: number): Scope | undefined {
    const topOrigin = httpUrl(tabUrl)?.origin;
    const page = requestPage(details);
    const initiator = httpUrl(details.initiator ?? details.originUrl ?? details.documentUrl);
    const frameId = Math.max(0, details.frameId ?? 0);
    if (!topOrigin || !page || initiator?.origin !== page.origin || details.incognito || details.tabId < 0
      || (frameId === 0 && page.origin !== topOrigin)) return;
    return { tabId: details.tabId, frameId, origin: page.origin, topOrigin, time: now };
  }

  recordHeaders(details: Request, headers: Header[], tabUrl = requestPage(details)?.href): void {
    const now = Date.now();
    this.prune(now);
    this.requests.delete(details.requestId);
    const scope = this.scope(details, tabUrl, now);
    const target = httpUrl(details.url);
    if (!target || !scope) return;
    const http = scopedHeaders(target.href, scope.origin, scope.origin, headers);
    const media = scopedHeaders(target.href, scope.origin, scope.origin, headers, true);
    if (Object.keys(http).length || Object.keys(media).length) {
      this.keep(this.requests, details.requestId, { ...scope, target: target.href, http, media }, 250);
    }
  }

  recordStream(details: Request, tabUrl: string | undefined, headers: Header[]): boolean {
    const now = Date.now();
    this.prune(now);
    const scope = this.scope(details, tabUrl, now);
    const target = httpUrl(details.url);
    const mime = mediaMime(headers);
    if (!scope || !target || !isMediaStream(target.href, mime, mediaLength(headers))) return false;
    const key = frameKey(details.tabId, scope.frameId);
    const current = this.streams.get(key);
    const matchesScope = current?.origin === scope.origin && current.topOrigin === scope.topOrigin;
    if (current && matchesScope && isManifest(new URL(current.url), current.mime) && !isManifest(target, mime)) return false;
    if (current && matchesScope && current.variants?.some(({ url }) => url === target.href)) return false;
    const sameStream = matchesScope && current?.url === target.href;
    this.keep(this.streams, key, { ...(sameStream ? current : undefined), ...scope, url: target.href, mime }, 150);
    return !sameStream;
  }

  stream(tabId: number, tabUrl: string | undefined, frameId?: number, frameUrl = tabUrl): Detection | undefined {
    this.prune();
    const origin = httpUrl(frameUrl)?.origin;
    const topOrigin = httpUrl(tabUrl)?.origin;
    const matches = (entry: Detection) => entry.tabId === tabId && entry.origin === origin && entry.topOrigin === topOrigin;
    const exact = frameId === undefined ? undefined : this.streams.get(frameKey(tabId, frameId));
    if (exact && matches(exact)) return exact;
    if ((frameId !== undefined && frameId > 0) || origin !== topOrigin) return;
    return [...this.streams.values()].reverse().find(matches);
  }

  setHlsVariants(expected: Detection, variants: HlsVariant[], separateAudio: boolean): boolean {
    this.prune();
    const key = frameKey(expected.tabId, expected.frameId);
    const stream = this.streams.get(key);
    if (!stream || stream.origin !== expected.origin || stream.topOrigin !== expected.topOrigin
      || !isHls(new URL(expected.url), expected.mime)
      || (stream.url !== expected.url && !variants.some(({ url }) => url === stream.url))) return false;
    this.keep(this.streams, key, { ...expected, time: stream.time, variants, separateAudio }, 150);
    return true;
  }

  headers(tabId: number | undefined, target: string, tabUrl: string | undefined): Record<string, string> {
    return this.requestHeaders(tabId, target, tabUrl, 0, tabUrl)?.http ?? {};
  }

  mediaHeaders(tabId: number | undefined, target: string, tabUrl: string | undefined, frameId = 0, frameUrl = tabUrl): Record<string, string> {
    return this.requestHeaders(tabId, target, tabUrl, frameId, frameUrl)?.media ?? {};
  }

  private requestHeaders(tabId: number | undefined, target: string, tabUrl: string | undefined, frameId: number, frameUrl: string | undefined) {
    this.prune();
    const origin = httpUrl(frameUrl)?.origin;
    const topOrigin = httpUrl(tabUrl)?.origin;
    const url = httpUrl(target);
    if (tabId === undefined || !topOrigin || !origin || !url) return;
    return [...this.requests.values()].reverse().find((entry) =>
      entry.tabId === tabId && entry.frameId === frameId && entry.target === url.href
      && entry.origin === origin && entry.topOrigin === topOrigin);
  }

  clear(tabId: number, frameId?: number): void {
    for (const [key, entry] of this.streams) {
      if (entry.tabId === tabId && (frameId === undefined || entry.frameId === frameId)) this.streams.delete(key);
    }
    for (const [id, entry] of this.requests) {
      if (entry.tabId === tabId && (frameId === undefined || entry.frameId === frameId)) this.requests.delete(id);
    }
  }

  private keep<K, V>(map: Map<K, V>, key: K, value: V, limit: number): void {
    map.delete(key);
    map.set(key, value);
    if (map.size > limit) {
      const oldest = map.keys().next();
      if (!oldest.done) map.delete(oldest.value);
    }
  }
}
