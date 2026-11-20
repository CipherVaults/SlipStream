import { httpUrl } from "./downloads";
import { isMediaStream, mediaMime, scopedHeaders, type Header } from "./streams";

type Request = {
  tabId: number;
  requestId: string;
  url: string;
  initiator?: string;
  documentUrl?: string;
  originUrl?: string;
};
type Scope = { tabId: number; origin: string; target: string; time: number; headers: Record<string, string> };
type Detection = { url: string; origin: string; mime: string; time: number };
const TTL = 120_000;

export class ObservedNetwork {
  private requests = new Map<string, Scope>();
  private streams = new Map<number, Detection>();

  recordHeaders(details: Request, headers: Header[]): void {
    const target = httpUrl(details.url);
    const initiator = details.initiator ?? details.originUrl ?? details.documentUrl;
    const origin = httpUrl(details.documentUrl ?? initiator)?.origin;
    if (!target || !origin || details.tabId < 0) return;
    const values = scopedHeaders(target.href, origin, initiator, headers);
    if (Object.keys(values).length === 0) {
      this.requests.delete(details.requestId);
      return;
    }
    this.keep(this.requests, details.requestId, {
      tabId: details.tabId, origin, target: target.href, time: Date.now(), headers: values,
    }, 250);
  }

  recordStream(details: Request, tabUrl: string | undefined, headers: Header[]): boolean {
    const target = httpUrl(details.url);
    const origin = httpUrl(tabUrl)?.origin;
    const mime = mediaMime(headers);
    if (!origin || !target || !isMediaStream(target.href, mime)) return false;
    const current = this.streams.get(details.tabId);
    const manifest = (url: string, mime: string) => /\.(m3u8|mpd)$/i.test(new URL(url).pathname)
      || /mpegurl|dash\+xml/.test(mime);
    if (current && current.origin === origin && Date.now() - current.time < TTL
      && manifest(current.url, current.mime) && !manifest(target.href, mime)) {
      return false;
    }
    this.keep(this.streams, details.tabId, { url: target.href, origin, mime, time: Date.now() }, 100);
    return true;
  }

  stream(tabId: number, tabUrl: string | undefined): Detection | undefined {
    const stream = this.streams.get(tabId);
    if (stream && Date.now() - stream.time < TTL && stream.origin === httpUrl(tabUrl)?.origin) return stream;
  }

  headers(tabId: number | undefined, target: string, tabUrl: string | undefined): Record<string, string> {
    const origin = httpUrl(tabUrl)?.origin;
    const url = httpUrl(target);
    if (!origin || url?.origin !== origin) return {};
    for (const entry of [...this.requests.values()].reverse()) {
      if (entry.tabId === tabId && entry.target === url.href && entry.origin === origin
        && Date.now() - entry.time < TTL) return entry.headers;
    }
    return {};
  }

  clear(tabId: number): void {
    this.streams.delete(tabId);
    for (const [id, entry] of this.requests) {
      if (entry.tabId === tabId) this.requests.delete(id);
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
