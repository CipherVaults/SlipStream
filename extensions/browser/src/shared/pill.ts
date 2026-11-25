import themeCss from "./theme.css?raw";
import pillCss from "./pill.css?raw";
import { isReplayedUserEvent, registerPillHost, setPillPointer, unregisterPillHost } from "./pillGuard";
import { applyAppearance, type ActionResult, type BridgeState, type MediaAction, type PillFormat, type PillSpot } from "./state";

export const PILL_WIDTH = 144;
export const PILL_HEIGHT = 32;
const INSET = 8;
const HIDE_DELAY = 250;
const fallbackFormats: PillFormat[] = [{ id: "4k", label: "4K" }, { id: "1080p", label: "1080p" }];
const audioFormats: PillFormat[] = [{ id: "mp3_320", label: "MP3 320k" }, { id: "flac", label: "FLAC" }];
const commands: PillFormat[] = [
  { id: "picker", label: "Open Full Format Picker…" },
  { id: "links", label: "Grab Page Links" },
  { id: "mute", label: "Mute site" },
];
const trustedPress = (event: Event) => event.isTrusted || isReplayedUserEvent(event);
const clamp = (value: number) => Math.min(1, Math.max(0, value));

function travel(bounds: DOMRectReadOnly) {
  return { x: Math.max(0, bounds.width - PILL_WIDTH - INSET * 2), y: Math.max(0, bounds.height - PILL_HEIGHT - INSET * 2) };
}

export function pillAnchor(bounds: DOMRectReadOnly, spot: PillSpot): PillSpot {
  const distance = travel(bounds);
  return { x: bounds.left + INSET + spot.x * distance.x, y: bounds.top + INSET + spot.y * distance.y };
}

function snapToEdge(bounds: DOMRectReadOnly, x: number, y: number): PillSpot {
  const distance = travel(bounds);
  const at = { x: clamp((x - bounds.left - INSET) / (distance.x || 1)), y: clamp((y - bounds.top - INSET) / (distance.y || 1)) };
  const corner = (value: number) => value < 0.08 ? 0 : value > 0.92 ? 1 : value;
  const dx = Math.min(at.x, 1 - at.x) * distance.x;
  const dy = Math.min(at.y, 1 - at.y) * distance.y;
  return dx < dy ? { x: Math.round(at.x), y: corner(at.y) } : { x: corner(at.x), y: Math.round(at.y) };
}

export function createMediaPill(
  document: Document,
  send: (action: MediaAction, event: MouseEvent) => Promise<ActionResult>,
  options: {
    onOpen?: () => void;
    onMove?: (spot: PillSpot) => void;
    onDismiss?: () => void;
    onInteractionEnd?: () => void;
    onVisibilityChange?: () => void;
    isPlaylist?: boolean;
  } = {},
) {
  const view = document.defaultView;
  const host = document.createElement("div");
  host.style.setProperty("all", "initial", "important");
  for (const [property, value] of Object.entries({
    position: "fixed", "z-index": "2147483647", margin: "0", right: "auto", display: "none",
    width: PILL_WIDTH + "px", height: PILL_HEIGHT + "px",
  })) host.style.setProperty(property, value, "important");
  const root = host.attachShadow({ mode: "closed" });
  registerPillHost(host, root);
  const styles = pillCss + "\n" + themeCss;
  if (typeof CSSStyleSheet.prototype.replaceSync === "function" && "adoptedStyleSheets" in root) {
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(styles);
    root.adoptedStyleSheets = [sheet];
  } else {
    const css = document.createElement("style");
    css.textContent = styles;
    root.append(css);
  }
  const surface = document.createElement("div");
  surface.className = "surface";
  const bar = document.createElement("div");
  bar.className = "bar";
  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = "pill";
  toggle.setAttribute("aria-expanded", "false");
  toggle.setAttribute("aria-controls", "media-formats");
  const mark = document.createElement("span");
  mark.className = "indicator";
  mark.setAttribute("aria-hidden", "true");
  toggle.append(mark, "SlipStream ▾");
  const dismiss = document.createElement("button");
  dismiss.type = "button";
  dismiss.className = "dismiss";
  dismiss.title = "Dismiss on this video";
  dismiss.setAttribute("aria-label", dismiss.title);
  dismiss.textContent = "×";
  const menu = document.createElement("div");
  menu.id = "media-formats";
  menu.className = "menu";
  menu.setAttribute("aria-label", "Download options");
  menu.hidden = true;
  const feedback = document.createElement("div");
  feedback.className = "feedback";
  feedback.setAttribute("role", "status");
  feedback.hidden = true;
  let feedbackTimer: ReturnType<typeof setTimeout> | undefined;
  let hideTimer: ReturnType<typeof setTimeout> | undefined;
  let pendingHide = false;
  let busy = false;
  let hovered = false;
  let last: PillSpot | undefined;
  let bounds: DOMRectReadOnly | undefined;
  let drag: { id: number; downX: number; downY: number; anchor: PillSpot; bounds: DOMRectReadOnly; moved: boolean } | undefined;
  let suppressClickUntil = 0;
  const pressed = new Set<number>();
  let pendingFormats: { formats: readonly PillFormat[]; playlist: boolean } | undefined;

  const locked = () => hovered || !menu.hidden || !!drag || pressed.size > 0;
  const pointerActive = () => !!drag || pressed.size > 0;

  function writePosition(x: number, y: number, above: boolean, force = false): void {
    x = Math.round(x);
    y = Math.round(y);
    if (force || !last || Math.abs(x - last.x) >= 2 || Math.abs(y - last.y) >= 2) {
      host.style.setProperty("left", x + "px", "important");
      host.style.setProperty("top", y + "px", "important");
      last = { x, y };
    }
    const viewportWidth = view?.innerWidth ?? 250;
    const viewportHeight = view?.innerHeight ?? 280;
    const spaceLeft = last.x + PILL_WIDTH - INSET;
    const spaceRight = viewportWidth - last.x - INSET;
    const alignLeft = spaceLeft < 230 && spaceRight > spaceLeft;
    surface.dataset.align = alignLeft ? "left" : "right";
    surface.dataset.side = above ? "above" : "below";
    surface.style.setProperty("--menu-width", Math.max(PILL_WIDTH, alignLeft ? spaceRight : spaceLeft) + "px");
    surface.style.setProperty("--menu-height", Math.max(48, above ? last.y - 10 : viewportHeight - last.y - PILL_HEIGHT - 10) + "px");
    setVisible(true);
  }

  function setVisible(visible: boolean): void {
    const display = visible ? "block" : "none";
    if (host.style.display === display) return;
    host.style.setProperty("display", display, "important");
    options.onVisibilityChange?.();
  }

  function open(show: boolean): void {
    const changed = show === menu.hidden;
    menu.hidden = !show;
    feedback.hidden = true;
    surface.classList.toggle("open", show);
    toggle.setAttribute("aria-expanded", String(show));
    if (changed && !show) interactionEnded();
  }

  function hideNow(): void {
    pendingHide = false;
    clearTimeout(hideTimer);
    hideTimer = undefined;
    hovered = false;
    pressed.clear();
    endDrag();
    open(false);
    setVisible(false);
  }

  function requestHide(): void {
    pendingHide = true;
    if (locked() || hideTimer !== undefined || host.style.display === "none") return;
    hideTimer = setTimeout(() => {
      hideTimer = undefined;
      if (!locked() && pendingHide) hideNow();
    }, HIDE_DELAY);
  }

  function interactionEnded(): void {
    if (!pointerActive() && pendingFormats) {
      const { formats, playlist } = pendingFormats;
      updateFormats(formats, playlist);
    }
    if (locked()) return;
    if (pendingHide) requestHide();
    options.onInteractionEnd?.();
  }

  function dragMove(event: PointerEvent): void {
    if (!trustedPress(event) || drag?.id !== event.pointerId) return;
    const dx = event.clientX - drag.downX;
    const dy = event.clientY - drag.downY;
    if (!drag.moved && Math.hypot(dx, dy) < 5) return;
    if (!drag.moved) open(false);
    drag.moved = true;
    surface.classList.add("dragging");
    writePosition(drag.anchor.x + dx, drag.anchor.y + dy, false);
  }

  function endDrag(): void {
    const id = drag?.id;
    drag = undefined;
    view?.removeEventListener("pointermove", dragMove, true);
    view?.removeEventListener("pointerup", finishDrag, true);
    view?.removeEventListener("pointercancel", finishDrag, true);
    if (id !== undefined) {
      setPillPointer(host, id);
      if (toggle.hasPointerCapture?.(id)) toggle.releasePointerCapture(id);
    }
    surface.classList.remove("dragging");
  }

  function finishDrag(event: PointerEvent): void {
    if (!trustedPress(event) || drag?.id !== event.pointerId) return;
    const { moved, bounds: startBounds } = drag;
    endDrag();
    if (moved && last) {
      suppressClickUntil = Date.now() + 500;
      const spot = snapToEdge(startBounds, last.x, last.y);
      const anchor = pillAnchor(startBounds, spot);
      writePosition(anchor.x, anchor.y, spot.y >= 0.5, true);
      options.onMove?.(spot);
      event.preventDefault();
    }
    interactionEnded();
  }

  toggle.addEventListener("pointerdown", (event) => {
    if (!trustedPress(event) || event.button !== 0 || !last || !bounds || drag) return;
    suppressClickUntil = 0;
    drag = { id: event.pointerId, downX: event.clientX, downY: event.clientY, anchor: last, bounds, moved: false };
    try {
      toggle.setPointerCapture?.(event.pointerId);
      if (toggle.hasPointerCapture?.(event.pointerId)) setPillPointer(host, event.pointerId, toggle);
    } catch { /* A pointer may have ended before capture. */ }
    view?.addEventListener("pointermove", dragMove, true);
    view?.addEventListener("pointerup", finishDrag, true);
    view?.addEventListener("pointercancel", finishDrag, true);
  });
  toggle.addEventListener("pointerup", finishDrag);
  toggle.addEventListener("pointermove", dragMove);
  toggle.addEventListener("lostpointercapture", finishDrag);

  surface.addEventListener("pointerdown", (event) => {
    if (trustedPress(event) && event.button === 0 && (event.target as Element).closest("button")) {
      pressed.add(event.pointerId);
    }
  });
  const releasePress = (event: PointerEvent) => {
    if (!trustedPress(event) || !pressed.has(event.pointerId)) return;
    // Keep the menu stable until the corresponding click has been dispatched.
    const id = event.pointerId;
    setTimeout(() => {
      pressed.delete(id);
      interactionEnded();
    }, 0);
  };
  surface.addEventListener("pointerup", releasePress);
  surface.addEventListener("pointercancel", releasePress);
  view?.addEventListener("pointerup", releasePress, true);
  view?.addEventListener("pointercancel", releasePress, true);

  surface.addEventListener("mouseenter", () => { hovered = true; });
  surface.addEventListener("mouseleave", () => {
    hovered = false;
    interactionEnded();
  });
  surface.addEventListener("mousedown", (event) => {
    if (!trustedPress(event) || event.button !== 0) return;
    const button = (event.target as Element).closest("button");
    if (button) {
      button.focus({ preventScroll: true });
      event.preventDefault();
    }
  });
  toggle.addEventListener("click", (event) => {
    if (!trustedPress(event)) return;
    if (event.detail > 0 && Date.now() < suppressClickUntil) {
      suppressClickUntil = 0;
      return;
    }
    open(menu.hidden);
    if (!menu.hidden) options.onOpen?.();
  });
  dismiss.addEventListener("click", (event) => {
    if (!trustedPress(event)) return;
    hideNow();
    options.onDismiss?.();
  });
  surface.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !menu.hidden) {
      open(false);
      toggle.focus({ preventScroll: true });
      event.stopPropagation();
    }
  });
  surface.addEventListener("focusout", (event) => {
    if (!surface.contains(event.relatedTarget as Node | null)) open(false);
  });

  function menuButton({ id, label, sub }: PillFormat): HTMLButtonElement {
    const button = document.createElement("button");
    button.type = "button";
    button.dataset.action = id;
    button.textContent = label;
    if (sub) {
      const detail = document.createElement("span");
      detail.className = "detail";
      detail.textContent = sub;
      button.append(detail);
    }
    button.addEventListener("click", async (event) => {
      if (!trustedPress(event) || busy) return;
      open(false);
      toggle.focus({ preventScroll: true });
      busy = true;
      clearTimeout(feedbackTimer);
      try {
        if (id !== "mute") {
          feedback.textContent = "Sending to SlipStream…";
          feedback.hidden = false;
        }
        const result = await send(id, event);
        if (id !== "mute" || !result.ok) {
          feedback.textContent = result.message ?? (result.ok ? "Sent to SlipStream" : "Could not send to SlipStream");
          feedback.hidden = false;
        }
      } catch {
        feedback.textContent = "SlipStream is unavailable.";
        feedback.hidden = false;
      } finally {
        busy = false;
        if (host.isConnected) feedbackTimer = setTimeout(() => { feedback.hidden = true; }, 4500);
      }
    });
    return button;
  }

  let menuSignature = "";
  let isPlaylist = !!options.isPlaylist;
  function updateFormats(formats: readonly PillFormat[], playlist = isPlaylist): void {
    isPlaylist = playlist;
    if (pointerActive()) {
      pendingFormats = { formats, playlist };
      return;
    }
    pendingFormats = undefined;
    const choices = [
      formats.length ? formats : fallbackFormats,
      audioFormats,
      [...(playlist ? [{ id: "playlist", label: "Download Playlist…" } as const] : []), ...commands],
    ];
    const signature = JSON.stringify(choices);
    if (signature === menuSignature) return;
    menuSignature = signature;
    const activeEl = root.activeElement as HTMLElement | null;
    const focusedAction = activeEl && menu.contains(activeEl) ? activeEl.dataset.action : undefined;
    const groups = choices.map((items) => {
      const group = document.createElement("div");
      group.className = "group";
      group.append(...items.map(menuButton));
      return group;
    });
    menu.replaceChildren(...groups);
    if (focusedAction && !menu.hidden) {
      const buttons = Array.from(menu.querySelectorAll<HTMLButtonElement>("button"));
      const match = buttons.find((button) => button.dataset.action === focusedAction) ?? buttons[0];
      match?.focus({ preventScroll: true });
    }
  }

  bar.append(toggle, dismiss);
  surface.append(bar, menu, feedback);
  root.append(surface);
  updateFormats([]);
  return {
    host,
    updateFormats,
    isLocked: locked,
    isVisible: () => host.style.display !== "none",
    appearance: (state: Pick<BridgeState, "theme" | "accent">, dark: boolean) => applyAppearance(surface, state, dark),
    position(x: number, y: number, above: boolean, videoBounds?: DOMRectReadOnly) {
      clearTimeout(hideTimer);
      hideTimer = undefined;
      pendingHide = false;
      if (locked()) return;
      bounds = videoBounds;
      writePosition(x, y, above);
    },
    requestHide,
    hide(immediate = false) {
      if (immediate) hideNow();
      else requestHide();
    },
    destroy() {
      clearTimeout(hideTimer);
      clearTimeout(feedbackTimer);
      endDrag();
      pressed.clear();
      view?.removeEventListener("pointerup", releasePress, true);
      view?.removeEventListener("pointercancel", releasePress, true);
      unregisterPillHost(host);
      host.remove();
    },
  };
}
