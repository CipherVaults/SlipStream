import { extension as api } from "./shared/browser";
import { blockedSite, defaults, downloadLinks, httpUrl, readSettings } from "./shared/downloads";
import { createMediaPill, pillAnchor, PILL_HEIGHT, PILL_WIDTH } from "./shared/pill";
import { isPillHost } from "./shared/pillGuard";
import {
  APPEARANCE_KEY, canUseSiteStorage, PLAYER_PORT, readAppearance, SETTINGS_KEY,
  type ActionResult, type BrowserMessage, type PillFormat, type PillFormatsMessage, type PillSpot,
} from "./shared/state";
import { isKnownVideoPage, isPlaylistPage, resolveVideoPostUrl, StreamNavigationState } from "./shared/streams";

type Player = HTMLVideoElement | HTMLIFrameElement;
type Pill = ReturnType<typeof createMediaPill>;
type Entry = {
  pill: Pill;
  corner?: PillSpot;
  url?: string;
  requestId?: number;
  formats?: PillFormat[];
  isPlaylist?: boolean;
};

const topFrame = window === window.top;
const siteStorage = canUseSiteStorage(api.extension?.inIncognitoContext);
const systemTheme = matchMedia("(prefers-color-scheme: dark)");
const pills = new Map<Player, Entry>();
const navigation = new StreamNavigationState(location.href);
const observedPlayers = new Set<Player>();
let dismissed = new WeakMap<Player, string>();
let appearance = readAppearance(undefined);
let settings = defaults;
let savedSpot: PillSpot | undefined;
let hlsFormats: PillFormat[] = [];
let playerPort: chrome.runtime.Port | undefined;
let childActive = false;
let privateMuted = false;
let nextRequestId = 0;
let lastStreamRequest = 0;
let scheduled = false;
let altHeld = false;
let optionsReady = false;
let lastScan = -Infinity;
let lastMutationLayout = -Infinity;
let needsScan = false;
let videos: HTMLVideoElement[] = [];
let frames: HTMLIFrameElement[] = [];
let roots: ShadowRoot[] = [];
let scanTimer: ReturnType<typeof setTimeout> | undefined;
const postLinks = new WeakMap<HTMLVideoElement, { scan: number; url?: string }>();

function send<T>(message: BrowserMessage): Promise<T> {
  return api.runtime.sendMessage(message);
}

function collectLinks(): string[] {
  return downloadLinks([
    ...Array.from(document.querySelectorAll<HTMLAnchorElement>("a[href]"), (anchor) => anchor.href),
    ...Array.from(document.querySelectorAll<HTMLMediaElement | HTMLSourceElement>("video[src], audio[src], source[src]"), (el) => el.src),
  ]);
}

function reportAlt(held: boolean): void {
  if (!topFrame || altHeld === held) return;
  altHeld = held;
  void send({ kind: "alt", held }).catch(() => {});
}

function keyEvent(event: KeyboardEvent | MouseEvent): void {
  if (event.isTrusted) reportAlt(event.altKey);
}

addEventListener("keydown", keyEvent, true);
addEventListener("keyup", keyEvent, true);
addEventListener("mousedown", keyEvent, true);
addEventListener("blur", () => reportAlt(false));
addEventListener("pagehide", () => {
  reportAlt(false);
  reportPlayer(false);
});
addEventListener("pageshow", schedule);
document.addEventListener("visibilitychange", () => {
  if (document.hidden) reportAlt(false);
  schedule();
});

function updateStream(): void {
  const href = location.href;
  const epoch = navigation.epoch;
  const requestId = ++lastStreamRequest;
  void send<{ url?: string; formats?: PillFormat[]; childActive?: boolean }>({ kind: "stream" }).then((reply) => {
    if (href !== location.href || epoch !== navigation.epoch || requestId !== lastStreamRequest) return;
    navigation.update(epoch, href, location.href, reply?.url);
    hlsFormats = reply?.formats ?? [];
    if (topFrame && typeof reply?.childActive === "boolean") childActive = reply.childActive;
    schedule();
  }).catch(() => {});
}

function syncNavigation(): void {
  if (!navigation.sync(location.href)) return;
  hlsFormats = [];
  dismissed = new WeakMap();
  for (const [player, entry] of pills) removePill(player, entry);
  void send({ kind: "frame-reset" }).catch(() => {}).then(updateStream);
}

api.runtime.onMessage.addListener((message, sender, reply) => {
  if (sender.id !== api.runtime.id || sender.tab) return;
  if (message.kind === "stream-changed" || message.kind === "child-player-changed") updateStream();
  if (message.kind === "collect-links" && topFrame) reply(collectLinks());
  if (message.kind === "pill-formats") {
    const update = message as PillFormatsMessage;
    for (const entry of pills.values()) {
      if (entry.requestId !== update.requestId || entry.url !== update.url) continue;
      entry.formats = update.formats;
      entry.isPlaylist = update.isPlaylist;
      renderFormats(entry);
    }
  }
});

function reportPlayer(active: boolean): void {
  if (topFrame) return;
  if (!active) {
    const old = playerPort;
    playerPort = undefined;
    old?.disconnect();
  } else if (!playerPort && api.runtime.connect) {
    const port = api.runtime.connect({ name: PLAYER_PORT });
    playerPort = port;
    port.onDisconnect.addListener(() => {
      if (playerPort !== port) return;
      playerPort = undefined;
      // A restarted background can recover ownership at the next layout.
      setTimeout(schedule, 500);
    });
  }
}

const resizeObserver = new ResizeObserver(schedule);
const mutations = new MutationObserver((records) => {
  let relayout = false;
  for (const record of records) {
    if (record.type === "childList") {
      if ([...record.addedNodes, ...record.removedNodes].some((node) => !isPillHost(node))) {
        requestScan();
        return;
      }
    } else if (!isPillHost(record.target)) {
      if (record.attributeName === "src" || record.attributeName === "href") {
        requestScan();
        return;
      }
      relayout = true;
    }
  }
  if (relayout && observedPlayers.size) requestScan(false);
});
const observerOptions = { childList: true, subtree: true, attributes: true, attributeFilter: ["src", "href", "class", "style", "hidden"] };

function scanPlayers(): void {
  lastScan = performance.now();
  const foundRoots: ShadowRoot[] = [];
  const foundVideos: HTMLVideoElement[] = [];
  function scan(root: Document | ShadowRoot, depth: number): void {
    foundVideos.push(...root.querySelectorAll("video"));
    if (depth >= 6) return;
    for (const element of root.querySelectorAll("*")) {
      if (!element.shadowRoot) continue;
      foundRoots.push(element.shadowRoot);
      scan(element.shadowRoot, depth + 1);
    }
  }
  scan(document, 0);
  videos = foundVideos;
  frames = topFrame ? [...document.querySelectorAll("iframe")] : [];
  if (foundRoots.length !== roots.length || foundRoots.some((root, index) => root !== roots[index])) {
    roots = foundRoots;
    mutations.disconnect();
    mutations.observe(document.documentElement, observerOptions);
    for (const root of roots) mutations.observe(root, observerOptions);
  }
  const current = new Set<Player>([...videos, ...frames]);
  for (const player of observedPlayers) {
    if (!current.has(player)) {
      resizeObserver.unobserve(player);
      observedPlayers.delete(player);
    }
  }
  for (const player of current) {
    if (!observedPlayers.has(player)) {
      observedPlayers.add(player);
      resizeObserver.observe(player);
    }
  }
}

function requestScan(deep = true): void {
  needsScan ||= deep;
  if (scanTimer !== undefined) return;
  scanTimer = setTimeout(() => {
    scanTimer = undefined;
    lastMutationLayout = performance.now();
    if (needsScan) {
      needsScan = false;
      scanPlayers();
    }
    schedule();
  }, Math.max(0, 300 - (performance.now() - Math.max(lastScan, lastMutationLayout))));
}

function visibleBox(player: Player): DOMRect | undefined {
  const bounds = player.getBoundingClientRect();
  if (Math.min(innerWidth, bounds.right) - Math.max(0, bounds.left) < 220
    || Math.min(innerHeight, bounds.bottom) - Math.max(0, bounds.top) < 120) return;
  const style = getComputedStyle(player);
  if (style.display === "none" || style.visibility !== "visible" || Number(style.opacity || 1) === 0
    || (player.checkVisibility && !player.checkVisibility({ opacityProperty: true, visibilityProperty: true }))) return;
  return bounds;
}

function pickCorner(player: Player, bounds: DOMRect): PillSpot {
  const right = { x: 1, y: 0 };
  const left = { x: 0, y: 0 };
  const obstructed = (spot: PillSpot) => {
    const { x, y } = pillAnchor(bounds, spot);
    for (const offset of [4, PILL_WIDTH / 2, PILL_WIDTH - 4]) {
      for (const hit of document.elementsFromPoint?.(x + offset, y + PILL_HEIGHT / 2) ?? []) {
        if (isPillHost(hit)) continue;
        if (hit === player || hit.contains(player)) break;
        const control = hit.closest("button, [role=button], a[href], input, select");
        if (control && !control.contains(player)) {
          const box = control.getBoundingClientRect();
          if (box.width * box.height < bounds.width * bounds.height / 5) return true;
        }
      }
    }
    return false;
  };
  return obstructed(right) && !obstructed(left) ? left : right;
}

function* coveredPlayers(): Generator<[HTMLIFrameElement, DOMRect]> {
  for (const frame of frames) {
    const bounds = visibleBox(frame);
    if (!bounds || bounds.width < 300 || bounds.height < 170) continue;
    const x = bounds.right - 40;
    const y = bounds.top + 16;
    const hits = document.elementsFromPoint?.(x, y) ?? [];
    const top = document.elementFromPoint?.(x, y);
    const overlay = top && (isPillHost(top) ? hits.find((hit) => !isPillHost(hit)) : top);
    if (!overlay || overlay === frame || !hits.includes(frame) || overlay.contains(frame)) continue;
    const style = getComputedStyle(overlay);
    if (!overlay.matches("video, img, canvas, iframe") && (style.opacity === "0"
      || (style.backgroundImage === "none" && /^(transparent|rgba\([^)]*,\s*0\))$/.test(style.backgroundColor)))) {
      yield [frame, bounds];
    }
  }
}

function sourceIdentity(player: Player): string {
  return player instanceof HTMLVideoElement ? player.currentSrc || player.src : player.src;
}

function resolveTarget(player: Player): string {
  if (player instanceof HTMLIFrameElement) return httpUrl(player.src)?.href ?? location.href;
  if (isKnownVideoPage(location.href) || isPlaylistPage(location.href)) return location.href;
  let post = postLinks.get(player);
  if (post?.scan !== lastScan) {
    post = { scan: lastScan, url: resolveVideoPostUrl(player) };
    postLinks.set(player, post);
  }
  return post.url ?? navigation.resolveTarget(sourceIdentity(player), location.href);
}

function renderFormats(entry: Entry): void {
  entry.pill.updateFormats(entry.formats?.length ? entry.formats : entry.url === navigation.streamUrl ? hlsFormats : [],
    entry.isPlaylist ?? isPlaylistPage(entry.url ?? ""));
}

function updateTarget(entry: Entry, url: string, canProbe: boolean): void {
  if (entry.url !== url) {
    entry.url = url;
    entry.formats = undefined;
    entry.isPlaylist = undefined;
    entry.requestId = undefined;
    if (canProbe && siteStorage) void send({ kind: "video-seen", url }).catch(() => {});
  }
  renderFormats(entry);
}

function removePill(player: Player, entry: Entry): void {
  entry.pill.destroy();
  pills.delete(player);
}

function composedContains(ancestor: Element, element: Element): boolean {
  for (let node: Node | null = element; node;) {
    if (node === ancestor) return true;
    node = node instanceof ShadowRoot ? node.host : node.parentNode;
  }
  return false;
}

function showPlayer(player: Player, bounds: DOMRect, fullscreen: Element | null): boolean {
  if (dismissed.get(player) === sourceIdentity(player)) return false;
  let entry = pills.get(player);
  const spot = savedSpot ?? entry?.corner ?? pickCorner(player, bounds);
  const { x, y } = pillAnchor(bounds, spot);
  if (x < 0 || y < 0 || x + PILL_WIDTH > innerWidth || y + PILL_HEIGHT > innerHeight) return false;
  if (!entry) {
    const pill = createMediaPill(document, async (action) => {
      if (action === "mute" && !siteStorage) {
        privateMuted = true;
        schedule();
        return { ok: true };
      }
      const url = pills.get(player)?.url ?? resolveTarget(player);
      return send<ActionResult>({ kind: "media", action, url, links: action === "links" ? collectLinks() : undefined });
    }, {
      onOpen() {
        const current = pills.get(player);
        const url = current?.url;
        if (!siteStorage || !current || !url) return;
        const requestId = ++nextRequestId;
        current.requestId = requestId;
        void send({ kind: "pill-open", url, isPlaylist: isPlaylistPage(url), requestId }).catch(() => {});
      },
      onDismiss() {
        dismissed.set(player, sourceIdentity(player));
        const current = pills.get(player);
        if (current) removePill(player, current);
        schedule();
      },
      onMove(next) {
        savedSpot = next;
        if (siteStorage) void send({ kind: "pill-spot", spot: next }).catch(() => {});
        schedule();
      },
      onInteractionEnd: schedule,
      onVisibilityChange: schedule,
    });
    entry = { pill, corner: spot };
    pills.set(player, entry);
  }
  const { pill } = entry;
  const nativeFullscreen = fullscreen instanceof HTMLVideoElement;
  const container = nativeFullscreen ? document.documentElement : fullscreen ?? document.documentElement;
  if (!pill.isLocked()) {
    entry.corner ??= spot;
    if (pill.host.parentElement !== container) container.append(pill.host);
    if (nativeFullscreen && !pill.host.matches(":popover-open") && typeof pill.host.showPopover === "function") {
      pill.host.setAttribute("popover", "manual");
      pill.host.showPopover();
    } else if (!nativeFullscreen && pill.host.hasAttribute("popover")) {
      pill.host.removeAttribute("popover");
    }
  }
  pill.appearance(appearance, systemTheme.matches);
  pill.position(x, y, innerHeight - y < 250 && y > innerHeight / 2, bounds);
  updateTarget(entry, entry.pill.isLocked() && entry.url ? entry.url : resolveTarget(player), player instanceof HTMLVideoElement);
  return true;
}

function layout(): void {
  syncNavigation();
  const page = httpUrl(location.href);
  const enabled = optionsReady && !!page && !privateMuted && !blockedSite(page, settings);
  for (const [player, entry] of pills) {
    if (!enabled || !player.isConnected) removePill(player, entry);
  }
  const fullscreen = document.fullscreenElement;
  if (!enabled || document.hidden || (fullscreen && !settings.showInFullscreen)) {
    for (const { pill } of pills.values()) pill.hide(true);
    reportPlayer(false);
    return;
  }

  const active = new Set<Player>();
  let directPlayer = false;
  const knownPage = isKnownVideoPage(location.href);
  for (const video of videos) {
    if (!video.isConnected || (fullscreen && !composedContains(fullscreen, video))) continue;
    const bounds = visibleBox(video);
    if (!bounds || !(httpUrl(sourceIdentity(video)) || video.readyState >= 1
      || (Number.isFinite(video.duration) && video.duration > 3) || navigation.streamUrl || knownPage)) continue;
    directPlayer = true;
    if (showPlayer(video, bounds, fullscreen)) active.add(video);
  }
  if (topFrame && !directPlayer && !childActive && !fullscreen) {
    for (const [frame, bounds] of coveredPlayers()) {
      if (showPlayer(frame, bounds, fullscreen)) active.add(frame);
    }
  }
  for (const [player, entry] of pills) {
    if (!active.has(player)) {
      entry.pill.requestHide();
    }
  }
  reportPlayer([...pills].some(([player, { pill }]) => player instanceof HTMLVideoElement && pill.isVisible()));
}

function schedule(): void {
  if (scheduled) return;
  scheduled = true;
  requestAnimationFrame(() => {
    scheduled = false;
    layout();
  });
}

mutations.observe(document.documentElement, observerOptions);
scanPlayers();
addEventListener("scroll", schedule, { capture: true, passive: true });
addEventListener("resize", () => {
  for (const entry of pills.values()) entry.corner = undefined;
  schedule();
});
addEventListener("popstate", schedule);
addEventListener("hashchange", schedule);
document.addEventListener("fullscreenchange", schedule);
for (const type of ["loadedmetadata", "loadstart", "playing"]) document.addEventListener(type, () => {
  requestScan();
  schedule();
}, true);
systemTheme.addEventListener("change", schedule);
updateStream();

void api.storage.local.get([APPEARANCE_KEY, SETTINGS_KEY]).then((values) => {
  appearance = readAppearance(values[APPEARANCE_KEY]);
  settings = readSettings(values[SETTINGS_KEY]);
}).catch(() => {}).finally(() => {
  optionsReady = true;
  schedule();
});
if (siteStorage) void send<{ spot?: PillSpot }>({ kind: "pill-spot" }).then((reply) => {
  savedSpot ??= reply?.spot;
  schedule();
}).catch(() => {});
api.storage.onChanged.addListener((changes, area) => {
  if (area !== "local") return;
  if (APPEARANCE_KEY in changes) appearance = readAppearance(changes[APPEARANCE_KEY].newValue);
  if (SETTINGS_KEY in changes) settings = readSettings(changes[SETTINGS_KEY].newValue);
  schedule();
});
