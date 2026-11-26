export interface InterceptionSettings {
  enabled: boolean;
  showInFullscreen: boolean;
  extensions: string[];
  minBytes: number;
  excludedSites: string[];
  mutedSites: string[];
}

export const defaults: InterceptionSettings = {
  enabled: true,
  showInFullscreen: false,
  extensions: [
    "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso", "img", "bin", "cab",
    "exe", "msi", "msix", "appx", "dmg", "pkg", "deb", "rpm", "apk",
    "mp4", "webm", "mkv", "avi", "mov", "m4v",
    "mp3", "flac", "wav", "m4a", "ogg", "opus",
    "pdf", "torrent",
  ],
  minBytes: 0,
  excludedSites: [],
  mutedSites: [],
};

export function readSettings(value: unknown): InterceptionSettings {
  const saved = value as Partial<InterceptionSettings> | undefined;
  const strings = (list: unknown, fallback: string[]) => Array.isArray(list)
    ? list.filter((item): item is string => typeof item === "string") : fallback;
  const minBytes = saved?.minBytes;
  return {
    enabled: typeof saved?.enabled === "boolean" ? saved.enabled : defaults.enabled,
    showInFullscreen: saved?.showInFullscreen === true,
    extensions: strings(saved?.extensions, defaults.extensions),
    minBytes: typeof minBytes === "number" && Number.isSafeInteger(minBytes) && minBytes >= 0 ? minBytes : defaults.minBytes,
    excludedSites: strings(saved?.excludedSites, defaults.excludedSites),
    mutedSites: strings(saved?.mutedSites, defaults.mutedSites),
  };
}

export function httpUrl(value?: string): URL | undefined {
  if (!value || !/^https?:\/\//i.test(value) || /[\u0000-\u0020\u007f\\]/.test(value)) return;
  try {
    const url = new URL(value);
    if (url.hostname && !url.username && !url.password) return url;
  } catch {
    return;
  }
}

export function matchesSite(url: URL | undefined, sites: readonly string[]): boolean {
  if (!url) return false;
  const host = url.hostname.replace(/\.$/, "");
  return sites.some((entry) => {
    const site = entry.trim().toLowerCase().replace(/\.$/, "");
    return site !== "" && (host === site || host.endsWith("." + site));
  });
}

export function blockedSite(url: URL | undefined, settings: InterceptionSettings): boolean {
  return matchesSite(url, settings.excludedSites) || matchesSite(url, settings.mutedSites);
}

export function canAttributeTab(
  item: { referrer?: string; incognito?: boolean; cookieStoreId?: string },
  tab: { url?: string; incognito?: boolean; cookieStoreId?: string } | undefined,
): boolean {
  if (!tab) return false;
  if ((item.incognito !== undefined || tab.incognito !== undefined) && item.incognito !== tab.incognito) {
    return false;
  }
  if ((item.cookieStoreId !== undefined || tab.cookieStoreId !== undefined) && item.cookieStoreId !== tab.cookieStoreId) {
    return false;
  }
  const referrer = httpUrl(item.referrer);
  const activeOrigin = httpUrl(tab.url)?.origin;
  return !referrer || activeOrigin === referrer.origin;
}

export function sameDownloadOrigin(item: { url: string; finalUrl?: string }): boolean {
  const initial = httpUrl(item.url);
  const resolved = httpUrl(item.finalUrl || item.url);
  return Boolean(initial && resolved && initial.origin === resolved.origin);
}

const DOWNLOADABLE_MIMES = new Set([
  "application/octet-stream",
  "application/binary",
  "application/x-msdownload",
  "application/x-msi",
  "application/zip",
  "application/x-7z-compressed",
  "application/x-rar-compressed",
  "application/gzip",
  "application/x-gzip",
  "application/x-tar",
  "application/x-bzip2",
  "application/x-xz",
  "application/x-iso9660-image",
  "application/vnd.android.package-archive",
  "application/pdf",
  "application/x-bittorrent",
]);

const MIME_TO_EXTENSIONS: Record<string, readonly string[]> = {
  "application/zip": ["zip"],
  "application/x-zip-compressed": ["zip"],
  "application/x-7z-compressed": ["7z"],
  "application/x-rar-compressed": ["rar"],
  "application/vnd.rar": ["rar"],
  "application/x-tar": ["tar"],
  "application/gzip": ["gz", "tgz"],
  "application/x-gzip": ["gz", "tgz"],
  "application/x-bzip2": ["bz2"],
  "application/x-xz": ["xz"],
  "application/zstd": ["zst"],
  "application/x-iso9660-image": ["iso"],
  "application/x-msdownload": ["exe", "msi"],
  "application/x-dosexec": ["exe"],
  "application/x-msi": ["msi"],
  "application/vnd.apple.installer+xml": ["pkg"],
  "application/x-apple-diskimage": ["dmg"],
  "application/vnd.debian.binary-package": ["deb"],
  "application/x-rpm": ["rpm"],
  "application/vnd.android.package-archive": ["apk"],
  "application/pdf": ["pdf"],
  "application/x-bittorrent": ["torrent"],
  "video/mp4": ["mp4", "m4v"],
  "video/webm": ["webm"],
  "video/x-matroska": ["mkv"],
  "video/x-msvideo": ["avi"],
  "video/quicktime": ["mov"],
  "audio/mpeg": ["mp3"],
  "audio/flac": ["flac"],
  "audio/wav": ["wav"],
  "audio/x-wav": ["wav"],
  "audio/mp4": ["m4a"],
  "audio/ogg": ["ogg"],
  "audio/opus": ["opus"],
};

const GENERIC_BINARY_EXTENSIONS = new Set([
  "exe", "msi", "msix", "appx", "dmg", "pkg", "deb", "rpm", "apk",
  "bin", "iso", "img", "zip", "7z", "rar", "tar", "gz", "tgz", "xz", "bz2", "zst", "cab",
]);

function matchesAllowedMime(rawMime: string | undefined, allowed: ReadonlySet<string>): boolean {
  const mime = rawMime?.split(";")[0].trim().toLowerCase() ?? "";
  if (!mime || !DOWNLOADABLE_MIMES.has(mime) && !mime.startsWith("video/") && !mime.startsWith("audio/")) {
    return false;
  }
  const inferred = MIME_TO_EXTENSIONS[mime];
  if (inferred) {
    return inferred.some((ext) => allowed.has(ext));
  }
  if (mime === "application/octet-stream" || mime === "application/binary") {
    return [...GENERIC_BINARY_EXTENSIONS].some((ext) => allowed.has(ext));
  }
  if (mime.startsWith("video/") || mime.startsWith("audio/")) {
    const sub = mime.split("/")[1]?.replace(/^x-/, "");
    return Boolean(sub && allowed.has(sub));
  }
  return false;
}

export function shouldIntercept(
  item: { url: string; finalUrl?: string; filename: string; fileSize: number; referrer?: string; mime?: string },
  settings: InterceptionSettings,
  context: { connected: boolean; altHeld: boolean; tabUrl?: string },
): boolean {
  const source = httpUrl(item.url);
  const target = httpUrl(item.finalUrl || item.url);
  if (!context.connected || context.altHeld || !settings.enabled || !source || !target
    || !isPublicHttpUrl(source.href) || !isPublicHttpUrl(target.href)) return false;
  if (settings.minBytes > 0 && !(item.fileSize >= settings.minBytes)) return false;
  if ([source, target, httpUrl(item.referrer), httpUrl(context.tabUrl)].some((url) => blockedSite(url, settings))) {
    return false;
  }
  if (!settings.extensions.length) return true;
  const allowed = new Set(settings.extensions.map((value) => value.trim().toLowerCase().replace(/^\./, "")).filter(Boolean));
  const fileExt = item.filename ? /\.([^.\\/]+)$/.exec(item.filename)?.[1].toLowerCase() : undefined;
  const urlExt = /\.([^.\\/]+)$/.exec(target.pathname)?.[1].toLowerCase();
  if ((fileExt && allowed.has(fileExt)) || (urlExt && allowed.has(urlExt))) return true;
  // Never override an explicit non-matching filename extension via MIME heuristics.
  if (fileExt) return false;
  return matchesAllowedMime(item.mime, allowed);
}

export function safeFilename(path: string): string | undefined {
  const filename = path.split(/[\\/]/).pop();
  if (!filename || /[<>:"|?*\u0000-\u001f\u007f\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069]/i.test(filename)) return;
  if (/^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])$/i.test(filename.split(".", 1)[0].trim())) return;
  if (/^\s|[.\s]$/.test(filename) || new TextEncoder().encode(filename).length > 200) return;
  return filename;
}

export function magnetUri(value?: string): string | undefined {
  if (!value || /[\u0000-\u001f\u007f]/.test(value)) return;
  const trimmed = value.trim();
  try {
    const parsed = new URL(trimmed);
    if (parsed.protocol !== "magnet:") return;
    for (const xt of parsed.searchParams.getAll("xt")) {
      if (xt.startsWith("urn:btih:")) {
        const hash = xt.slice("urn:btih:".length);
        if (/^[a-fA-F0-9]{40}$/.test(hash) || /^[A-Za-z2-7]{32}$/.test(hash)) return trimmed;
      } else if (xt.startsWith("urn:btmh:1220")) {
        const hash = xt.slice("urn:btmh:1220".length);
        if (/^[a-fA-F0-9]{64}$/.test(hash)) return trimmed;
      }
    }
  } catch {
    return;
  }
}

export const MAX_DOWNLOAD_LINKS = 512;

export function isPublicHttpUrl(raw: string): boolean {
  const url = httpUrl(raw);
  if (!url) return false;
  const host = url.hostname.replace(/\.$/, "");
  if (host.startsWith("[")) {
    const ip = host.slice(1, -1);
    // Global unicast excludes mapped IPv4, loopback, ULA, link-local, documentation, 6to4 and Teredo.
    return /^[23][0-9a-f]{3}:/.test(ip)
      && !ip.startsWith("2001:db8:")
      && !ip.startsWith("2002:")
      && !/^2001:(?:0{0,4}:|$)/.test(ip);
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

export function downloadLinks(values: unknown[], limit = MAX_DOWNLOAD_LINKS): string[] {
  const links = new Set<string>();
  for (const value of values) {
    if (links.size >= limit) break;
    if (typeof value !== "string" || value.length > 8192) continue;
    const url = httpUrl(value);
    if (!url) continue;
    url.hash = "";
    if (url.href.length <= 8192) links.add(url.href);
  }
  return [...links];
}
