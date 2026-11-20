// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMediaPill } from "./pill";
import { emptyState, formatSpeed, type MediaAction } from "./state";
import pillCss from "./pill.css?raw";
import themeCss from "./theme.css?raw";

function mount(send = vi.fn(async (_action: MediaAction) => ({ ok: true }))) {
  const attachShadow = vi.spyOn(Element.prototype, "attachShadow");
  const pill = createMediaPill(document, send);
  const root = attachShadow.mock.results[0].value as ShadowRoot;
  document.body.append(pill.host);
  return { ...pill, root, attachShadow, send };
}

afterEach(() => {
  document.body.replaceChildren();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("floating media pill", () => {
  it("uses a closed root, follows theme and accent, and uses the design system tokens", () => {
    const pill = mount();
    expect(pill.attachShadow).toHaveBeenCalledWith({ mode: "closed" });
    expect(pill.host.shadowRoot).toBeNull();
    expect(pill.host.querySelector("button")).toBeNull();
    const surface = pill.root.querySelector<HTMLElement>(".surface")!;
    pill.appearance({ ...emptyState, theme: "system", accent: "mono" }, true);
    expect(surface.dataset.theme).toBe("dark");
    expect(surface.dataset.accent).toBe("mono");
    pill.appearance({ ...emptyState, theme: "light" }, true);
    expect(surface.dataset.theme).toBe("light");
    expect(surface.dataset.accent).toBe("blue");
    const inline = pill.root.querySelector("style")?.textContent;
    if (!inline) expect(pill.root.adoptedStyleSheets[0].cssRules.length).toBeGreaterThan(0);
    const css = (inline ?? pillCss + "\n" + themeCss).toLowerCase();
    for (const token of ["#1a1c22", "#262a34", "#f0f2f6", "#3b8cff", "#e4e7ee",
      "#ffffff", "#f7f8fb", "#14171f", "#0071e3", "#181b22", "tabular-nums", "backdrop-filter"]) {
      expect(css).toContain(token);
    }
    expect(css).not.toContain("#000000");
    pill.position(12, 24, true);
    expect(pill.host.style.top).toBe("24px");
    expect(pill.host.style.right).toBe("12px");
    expect(surface.dataset.side).toBe("above");
    pill.hide();
    expect(pill.host.style.display).toBe("none");
    pill.destroy();
  });

  it("offers each requested one-click action and dispatches the corresponding command", async () => {
    vi.useFakeTimers();
    const pill = mount();
    const toggle = pill.root.querySelector<HTMLButtonElement>(".pill")!;
    const menu = pill.root.querySelector<HTMLElement>(".menu")!;
    const items = Array.from(menu.querySelectorAll("button"));
    expect(menu.hidden).toBe(true);
    expect(items.map((item) => item.textContent)).toEqual([
      "4K", "1080p", "MP3 320k", "FLAC", "Open Full Format Picker…", "Mute site", "Grab Page Links",
    ]);
    for (const item of items) {
      toggle.click();
      expect(toggle.getAttribute("aria-expanded")).toBe("true");
      item.click();
      await Promise.resolve();
      expect(menu.hidden).toBe(true);
    }
    expect(pill.send.mock.calls.map(([action]) => action)).toEqual(["4k", "1080p", "mp3_320", "flac", "picker", "mute", "links"]);
    pill.destroy();
  });

  it("formats speeds with stable units", () => {
    expect([0, 512, 1536, 1024 * 1024].map(formatSpeed)).toEqual(["0 B/s", "512 B/s", "1.5 KB/s", "1.0 MB/s"]);
  });
});
