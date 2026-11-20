export interface InterceptionSettings {
  enabled: boolean;
  extensions: string[];
  minBytes: number;
  excludedSites: string[];
  mutedSites: string[];
}

export const defaults: InterceptionSettings = {
  enabled: true,
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
  if (typeof item.incognito === "boolean" && typeof tab.incognito === "boolean" && item.incognito !== tab.incognito) {
    return false;
  }
  if (item.cookieStoreId && tab.cookieStoreId && item.cookieStoreId !== tab.cookieStoreId) {
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

export function downloadLinks(values: unknown[]): string[] {
  const links = new Set<string>();
  for (const value of values) {
    if (typeof value !== "string" || value.length > 8192) continue;
    const url = httpUrl(value);
    if (!url) continue;
    url.hash = "";
    if (url.href.length <= 8192) links.add(url.href);
  }
  return [...links];
}
