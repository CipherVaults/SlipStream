import { extension as api } from "./shared/browser";
import { blockedSite, defaults, downloadLinks, httpUrl, readSettings } from "./shared/downloads";
import { createMediaPill } from "./shared/pill";
import { APPEARANCE_KEY, readAppearance, SETTINGS_KEY, type ActionResult, type BrowserMessage } from "./shared/state";
import { StreamNavigationState } from "./shared/streams";

const systemTheme = matchMedia("(prefers-color-scheme: dark)");
const pills = new Map<HTMLVideoElement, ReturnType<typeof createMediaPill>>();
const navigation = new StreamNavigationState(location.href);
let appearance = readAppearance(undefined);
let settings = defaults;
let scheduled = false;
let altHeld = false;

function send<T>(message: BrowserMessage): Promise<T> {
  return api.runtime.sendMessage(message);
}

function collectLinks(): string[] {
  const candidates = [
    ...Array.from(document.querySelectorAll<HTMLAnchorElement>("a[href]"), (anchor) => anchor.href),
    ...Array.from(document.querySelectorAll<HTMLMediaElement | HTMLSourceElement>("video[src], audio[src], source[src]"), (el) => el.src),
  ];
  return downloadLinks(candidates);
}

function reportAlt(held: boolean): void {
  if (altHeld === held) return;
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
addEventListener("pagehide", () => reportAlt(false));
document.addEventListener("visibilitychange", () => {
  if (document.hidden) reportAlt(false);
});

function updateStream(): void {
  const hrefAtRequest = location.href;
  void send<{ url?: string }>({ kind: "stream" }).then((reply) => {
    navigation.update(hrefAtRequest, location.href, reply?.url);
  }).catch(() => {});
}

function syncNavigation(): void {
  if (navigation.sync(location.href)) updateStream();
}

api.runtime.onMessage.addListener((message, sender, reply) => {
  if (sender.id !== api.runtime.id) return;
  if (message.kind === "stream-changed") updateStream();
  if (message.kind === "collect-links") reply(collectLinks());
});
updateStream();

const resizeObserver = new ResizeObserver(schedule);

function layout(): void {
  syncNavigation();
  const page = httpUrl(location.href);
  const enabled = page && !blockedSite(page, settings);
  for (const [video, pill] of pills) {
    if (video.isConnected && enabled) continue;
    resizeObserver.unobserve(video);
    pill.destroy();
    pills.delete(video);
  }
  if (!enabled) return;

  const fullscreen = document.fullscreenElement;
  for (const video of document.querySelectorAll("video")) {
    const bounds = video.getBoundingClientRect();
    const style = getComputedStyle(video);
    const left = Math.max(0, bounds.left);
    const right = Math.min(innerWidth, bounds.right);
    const top = Math.max(0, bounds.top);
    const bottom = Math.min(innerHeight, bounds.bottom);
    if (right - left < 140 || bottom - top < 60 || style.visibility !== "visible" || style.opacity === "0"
      || (fullscreen && !fullscreen.contains(video))) {
      pills.get(video)?.hide();
      if (!pills.has(video)) resizeObserver.observe(video);
      continue;
    }
    let pill = pills.get(video);
    if (!pill) {
      pill = createMediaPill(document, async (action, event) => {
        if (!event.isTrusted) return { ok: false };
        // Adaptive video elements often use blob: URLs. Recover the HTTP manifest observed in that tab.
        const url = navigation.resolveTarget(video.currentSrc || video.src, location.href);
        return send<ActionResult>({ kind: "media", action, url, links: action === "links" ? collectLinks() : undefined });
      });
      pills.set(video, pill);
      resizeObserver.observe(video);
    }
    const container = fullscreen ?? document.documentElement;
    if (pill.host.parentElement !== container) container.append(pill.host);
    pill.appearance(appearance, systemTheme.matches);
    const y = top + 9;
    pill.position(innerWidth - right + 9, y, innerHeight - y < 265 && y > 265);
  }
}

function schedule(): void {
  if (scheduled) return;
  scheduled = true;
  requestAnimationFrame(() => {
    scheduled = false;
    layout();
  });
}

new MutationObserver(schedule).observe(document.documentElement, { childList: true, subtree: true });
addEventListener("scroll", schedule, { capture: true, passive: true });
addEventListener("resize", schedule);
addEventListener("popstate", () => {
  syncNavigation();
  schedule();
});
addEventListener("hashchange", () => {
  syncNavigation();
  schedule();
});
document.addEventListener("fullscreenchange", schedule);
document.addEventListener("loadedmetadata", schedule, true);
systemTheme.addEventListener("change", schedule);

function updatePreferences(values: Record<string, unknown>): void {
  appearance = readAppearance(values[APPEARANCE_KEY]);
  settings = readSettings(values[SETTINGS_KEY]);
  schedule();
}
void api.storage.local.get([APPEARANCE_KEY, SETTINGS_KEY]).then(updatePreferences).catch(schedule);
api.storage.onChanged.addListener((changes, area) => {
  if (area !== "local") return;
  if (APPEARANCE_KEY in changes) appearance = readAppearance(changes[APPEARANCE_KEY].newValue);
  if (SETTINGS_KEY in changes) settings = readSettings(changes[SETTINGS_KEY].newValue);
  schedule();
});
