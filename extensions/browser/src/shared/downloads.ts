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
  extensions: ["zip", "7z", "rar", "iso", "exe", "msi", "mp4", "webm", "mkv"],
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

export function shouldIntercept(
  item: { url: string; finalUrl?: string; filename: string; fileSize: number; referrer?: string },
  settings: InterceptionSettings,
  context: { connected: boolean; altHeld: boolean; tabUrl?: string },
): boolean {
  const source = httpUrl(item.url);
  const target = httpUrl(item.finalUrl || item.url);
  if (!context.connected || context.altHeld || !settings.enabled || !source || !target) return false;
  if (settings.minBytes > 0 && !(item.fileSize >= settings.minBytes)) return false;
  if ([source, target, httpUrl(item.referrer), httpUrl(context.tabUrl)].some((url) => blockedSite(url, settings))) {
    return false;
  }
  if (!settings.extensions.length) return true;
  const filename = item.filename || target.pathname;
  const extension = /\.([^.\\/]+)$/.exec(filename)?.[1].toLowerCase();
  return settings.extensions.some((value) => value.trim().toLowerCase().replace(/^\./, "") === extension);
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
