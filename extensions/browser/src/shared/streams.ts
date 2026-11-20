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
