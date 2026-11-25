// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMediaPill, pillAnchor } from "./pill";
import { installPillGuard, isPillHost, isReplayedUserEvent, routePillEvent } from "./pillGuard";
import { emptyState, formatSpeed, type MediaAction } from "./state";
import pillCss from "./pill.css?raw";
import themeCss from "./theme.css?raw";

function mount(options?: Parameters<typeof createMediaPill>[2]) {
  const attachShadow = vi.spyOn(Element.prototype, "attachShadow");
  const send = vi.fn(async (_action: MediaAction) => ({ ok: true }));
  const pill = createMediaPill(document, send, options);
  const root = attachShadow.mock.results.at(-1)?.value as ShadowRoot;
  document.body.append(pill.host);
  return { ...pill, root, attachShadow, send, toggle: root.querySelector<HTMLButtonElement>(".pill")!,
    menu: root.querySelector<HTMLElement>(".menu")!, surface: root.querySelector<HTMLElement>(".surface")! };
}

function hit(pill: ReturnType<typeof mount>, target: Element, type = "click", init: PointerEventInit = {}, trusted = true) {
  const event = {
    type, target: pill.host, isTrusted: trusted, detail: 1, cancelable: true, ...init,
    stopImmediatePropagation: vi.fn(), preventDefault: vi.fn(),
  };
  Object.defineProperty(pill.root, "elementFromPoint", { configurable: true, value: () => target });
  routePillEvent(event as unknown as MouseEvent);
  return event;
}

function press(pill: ReturnType<typeof mount>, target: Element) {
  const pointer = { pointerId: 9, button: 0 };
  hit(pill, target, "pointerdown", pointer);
  hit(pill, target, "pointerup", pointer);
  return hit(pill, target);
}

afterEach(() => {
  document.body.replaceChildren();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("floating media pill", () => {
  it("uses a closed registered root and preserves appearance tokens", () => {
    const pill = mount();
    expect(pill.attachShadow).toHaveBeenCalledWith({ mode: "closed" });
    expect(pill.host.shadowRoot).toBeNull();
    expect(pill.host.querySelector("button")).toBeNull();
    expect(isPillHost(pill.host)).toBe(true);
    pill.appearance({ ...emptyState, theme: "system", accent: "mono" }, true);
    expect(pill.surface.dataset.theme).toBe("dark");
    expect(pill.surface.dataset.accent).toBe("mono");
    pill.appearance({ ...emptyState, theme: "light" }, true);
    expect(pill.surface.dataset.theme).toBe("light");
    expect(pill.surface.dataset.accent).toBe("blue");
    const css = (pill.root.querySelector("style")?.textContent ?? pillCss + "\n" + themeCss).toLowerCase();
    for (const token of ["#1a1c22", "#262a34", "#f0f2f6", "#3b8cff", "#e4e7ee",
      "#ffffff", "#f7f8fb", "#14171f", "#0071e3", "#181b22", "tabular-nums", "backdrop-filter",
      "opacity: 0.85", "opacity 0.15s ease"]) {
      expect(css).toContain(token);
    }
    expect(css).not.toContain("#000000");
    pill.destroy();
    expect(isPillHost(pill.host)).toBe(false);
  });

  it("rounds position, uses a two-pixel deadband, and leaves the anchor unclamped", () => {
    const pill = mount();
    pill.position(12.2, 24.3, true);
    pill.position(13.1, 25.2, true);
    expect(pill.host.style.left).toBe("12px");
    expect(pill.host.style.top).toBe("24px");
    expect(pill.host.style.right).toBe("auto");
    expect(pill.surface.dataset.side).toBe("above");
    pill.position(13.1, 25.6, false);
    expect([pill.host.style.left, pill.host.style.top]).toEqual(["13px", "26px"]);
    expect(pillAnchor(new DOMRect(5, -20, 600, 300), { x: 1, y: 0 }).y).toBe(-12);
    pill.hide(true);
    expect(pill.isVisible()).toBe(false);
    pill.destroy();
  });

  it("debounces hide, cancels it on position, and locks while hovering or open", () => {
    vi.useFakeTimers();
    const pill = mount();
    pill.position(12, 24, false);
    pill.requestHide();
    vi.advanceTimersByTime(249);
    expect(pill.isVisible()).toBe(true);
    pill.position(15, 27, false);
    vi.advanceTimersByTime(250);
    expect(pill.isVisible()).toBe(true);
    pill.requestHide();
    pill.surface.dispatchEvent(new MouseEvent("mouseenter"));
    pill.position(40, 50, false);
    pill.requestHide();
    vi.advanceTimersByTime(300);
    expect(pill.isVisible()).toBe(true);
    expect(pill.host.style.left).toBe("15px");
    press(pill, pill.toggle);
    pill.surface.dispatchEvent(new MouseEvent("mouseleave"));
    pill.position(50, 60, false);
    pill.requestHide();
    vi.advanceTimersByTime(300);
    expect(pill.isVisible()).toBe(true);
    expect(pill.host.style.left).toBe("15px");
    pill.toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(pill.toggle.getAttribute("aria-expanded")).toBe("false");
    vi.advanceTimersByTime(251);
    expect(pill.isVisible()).toBe(false);
    pill.destroy();
  });

  it("snaps on drag, suppresses the drag click and only dismisses on a trusted press", () => {
    const onMove = vi.fn();
    const onDismiss = vi.fn();
    const pill = mount({ onMove, onDismiss });
    const bounds = new DOMRect(0, 0, 640, 400);
    pill.position(488, 8, false, bounds);
    hit(pill, pill.toggle, "pointerdown", { pointerId: 1, button: 0, clientX: 500, clientY: 16 });
    hit(pill, pill.toggle, "pointerup", { pointerId: 1, button: 0, clientX: 503, clientY: 16 });
    expect(onMove).not.toHaveBeenCalled();
    hit(pill, pill.toggle);
    expect(pill.menu.hidden).toBe(false);
    hit(pill, pill.toggle, "click", { detail: 0 });
    hit(pill, pill.toggle, "pointerdown", { pointerId: 2, button: 0, clientX: 500, clientY: 16 });
    for (const clientX of [503, 25]) {
      hit(pill, pill.toggle, "pointermove", { pointerId: 2, clientX, clientY: clientX === 503 ? 16 : 110 });
    }
    hit(pill, pill.toggle, "pointerup", { pointerId: 2, clientX: 25, clientY: 110 });
    expect(onMove).toHaveBeenCalledWith({ x: 0, y: expect.closeTo(94 / 352) });
    expect(pill.host.style.left).toBe("8px");
    hit(pill, pill.toggle);
    expect(pill.menu.hidden).toBe(true);
    const dismiss = pill.root.querySelector<HTMLButtonElement>(".dismiss")!;
    dismiss.click();
    expect(onDismiss).not.toHaveBeenCalled();
    press(pill, dismiss);
    expect(onDismiss).toHaveBeenCalledOnce();
    expect(pill.isVisible()).toBe(false);
    pill.destroy();
  });

  it("offers fallback actions, updates dynamic formats and exposes the playlist picker", async () => {
    const pill = mount();
    const items = () => Array.from(pill.menu.querySelectorAll("button"));
    expect(items().map((item) => item.textContent)).toEqual([
      "4K", "1080p", "MP3 320k", "FLAC", "Open Full Format Picker…", "Grab Page Links", "Mute site",
    ]);
    pill.updateFormats([{ id: "format:313+bestaudio", label: "1080p60 · H.264 · MP4", sub: "390 MB" }], true);
    expect(items().map((item) => item.dataset.action)).toEqual([
      "format:313+bestaudio", "mp3_320", "flac", "playlist", "picker", "links", "mute",
    ]);
    expect(items()[0].querySelector("span")?.textContent).toBe("390 MB");
    expect(items()[3].textContent).toBe("Download Playlist…");
    for (const item of items()) {
      press(pill, pill.toggle);
      expect(pill.toggle.getAttribute("aria-expanded")).toBe("true");
      press(pill, item);
      await Promise.resolve();
      expect(pill.menu.hidden).toBe(true);
    }
    expect(pill.send.mock.calls.map(([action]) => action)).toEqual(items().map((item) => item.dataset.action));
    pill.destroy();
  });

  it("defers format updates through hover, opening and pointer presses", () => {
    vi.useFakeTimers();
    const onOpen = vi.fn();
    const pill = mount({ onOpen });
    pill.toggle.click();
    expect(onOpen).not.toHaveBeenCalled();
    pill.surface.dispatchEvent(new MouseEvent("mouseenter"));
    const old = pill.menu.querySelector("button")!;
    pill.updateFormats([{ id: "format:137", label: "1080p MP4" }]);
    expect(pill.menu.querySelector("button")).toBe(old);
    press(pill, pill.toggle);
    expect(onOpen).toHaveBeenCalledOnce();
    expect(pill.menu.hidden).toBe(false);
    pill.surface.dispatchEvent(new MouseEvent("mouseleave"));
    const pointer = { pointerId: 3, button: 0 };
    hit(pill, old, "pointerdown", pointer);
    const other = pill.menu.querySelectorAll("button")[1];
    hit(pill, other, "pointerup", pointer);
    hit(pill, other);
    expect(pill.send).not.toHaveBeenCalled();
    pill.toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(pill.menu.querySelector("button")).toBe(old);
    vi.runAllTimers();
    expect(pill.menu.querySelector("button")?.dataset.action).toBe("format:137");
    pill.toggle.focus();
    hit(pill, pill.toggle, "click", { detail: 0 });
    expect(onOpen).toHaveBeenCalledTimes(2);
    pill.destroy();
  });

  it("formats speeds with stable units", () => {
    expect([0, 512, 1536, 1024 * 1024].map(formatSpeed)).toEqual(["0 B/s", "512 B/s", "1.5 KB/s", "1.0 MB/s"]);
  });
});

describe("document-start click capture", () => {
  it("installs once and only replays trusted input inside the closed root", () => {
    const spy = vi.spyOn(window, "addEventListener");
    installPillGuard(window);
    installPillGuard(window);
    expect(spy.mock.calls.map(([type]) => type)).toEqual([
      "pointerdown", "pointerup", "pointercancel", "mousedown", "mouseup", "click", "dblclick", "auxclick", "contextmenu",
    ]);
    expect(spy.mock.calls.every(([, , capture]) => capture === true)).toBe(true);

    const pill = mount();
    const inside = vi.fn((event: Event) => {
      expect(event.composed).toBe(false);
      expect(event.isTrusted).toBe(false);
      expect(isReplayedUserEvent(event)).toBe(true);
    });
    const outside = vi.fn();
    pill.toggle.addEventListener("click", inside);
    pill.host.addEventListener("click", outside);
    const rejected = hit(pill, pill.toggle, "click", {}, false);
    expect(rejected.stopImmediatePropagation).toHaveBeenCalledOnce();
    expect(inside).not.toHaveBeenCalled();
    const trusted = press(pill, pill.toggle);
    expect(trusted.stopImmediatePropagation).toHaveBeenCalledOnce();
    expect(inside).toHaveBeenCalledOnce();
    expect(outside).not.toHaveBeenCalled();
    pill.destroy();
  });
});
