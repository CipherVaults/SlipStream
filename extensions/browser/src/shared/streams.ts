import type { MediaProbeResult } from "@slipstream/ipc-types";
import { httpUrl } from "./downloads";

export interface Header {
  name: string;
  value?: string;
}

const mediaTypes = new Set(["application/vnd.apple.mpegurl", "application/x-mpegurl", "application/dash+xml"]);
const forwardedNames = new Map([
  ["cookie", "Cookie"],
  ["referer", "Referer"],
  ["user-agent", "User-Agent"],
]);

export function mediaMime(headers: Header[]): string {
  return headers.find(({ name }) => name.toLowerCase() === "content-type")?.value
    ?.split(";")[0].trim().toLowerCase() ?? "";
}

export function isMediaStream(value: string, mime = ""): boolean {
  const url = httpUrl(value);
  if (!url) return false;
  const type = mime.split(";")[0].trim().toLowerCase();
  return /\.(mp4|webm|m3u8|mpd)$/i.test(url.pathname)
    || /^(video|audio)\/[\w.+-]+$/.test(type) || mediaTypes.has(type);
}

export function scopedHeaders(
  requestUrl: string,
  tabUrl: string | undefined,
  initiator: string | undefined,
  headers: Header[],
): Record<string, string> {
  const target = httpUrl(requestUrl);
  const origin = httpUrl(tabUrl)?.origin;
  if (!target || target.origin !== origin || httpUrl(initiator)?.origin !== origin) return {};
  const result: Record<string, string> = {};
  for (const { name, value } of headers) {
    const canonical = forwardedNames.get(name.toLowerCase());
    if (!canonical || !value || /[\u0000-\u001f\u007f]/.test(value)) continue;
    if (canonical === "Referer" && httpUrl(value)?.origin !== origin) continue;
    result[canonical] = value;
  }
  return result;
}

const MAX_FORMAT_ID_LEN = 256;
const BEST_AUDIO_SUFFIX = "+bestaudio";

export function selectPillFormatId(probe: MediaProbeResult, expectedHeight: 2160 | 1080): string | undefined {
  const formats = probe.formats.filter(
    (format) => format.has_video && format.height === expectedHeight && format.format_id.length <= MAX_FORMAT_ID_LEN,
  );
  const muxed = formats.find((format) => format.has_audio);
  if (muxed) return muxed.format_id;
  if (probe.formats.some((format) => format.has_audio)) {
    const combinable = formats.find((format) => (format.format_id + BEST_AUDIO_SUFFIX).length <= MAX_FORMAT_ID_LEN);
    if (combinable) return combinable.format_id + BEST_AUDIO_SUFFIX;
  }
  return formats[0]?.format_id;
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
