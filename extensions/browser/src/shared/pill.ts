import themeCss from "./theme.css?raw";
import pillCss from "./pill.css?raw";
import { applyAppearance, type ActionResult, type BridgeState, type MediaAction } from "./state";

const actions: readonly [MediaAction, string][] = [
  ["4k", "4K"],
  ["1080p", "1080p"],
  ["mp3_320", "MP3 320k"],
  ["flac", "FLAC"],
  ["picker", "Open Full Format Picker…"],
  ["mute", "Mute site"],
  ["links", "Grab Page Links"],
];

export function createMediaPill(
  document: Document,
  send: (action: MediaAction, event: MouseEvent) => Promise<ActionResult>,
) {
  const host = document.createElement("div");
  host.style.setProperty("all", "initial", "important");
  for (const [property, value] of Object.entries({ position: "fixed", "z-index": "2147483647", margin: "0" })) {
    host.style.setProperty(property, value, "important");
  }
  const root = host.attachShadow({ mode: "closed" });
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
  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = "pill";
  toggle.setAttribute("aria-expanded", "false");
  toggle.setAttribute("aria-controls", "media-formats");
  const mark = document.createElement("span");
  mark.className = "indicator";
  toggle.append(mark, "SlipStream ▾");
  const menu = document.createElement("div");
  menu.id = "media-formats";
  menu.className = "menu";
  menu.hidden = true;
  const feedback = document.createElement("div");
  feedback.className = "feedback";
  feedback.setAttribute("role", "status");
  feedback.hidden = true;
  let feedbackTimer: ReturnType<typeof setTimeout>;
  let busy = false;

  const open = (show: boolean) => {
    menu.hidden = !show;
    feedback.hidden = true;
    toggle.setAttribute("aria-expanded", String(show));
  };
  toggle.addEventListener("click", () => open(menu.hidden));
  surface.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !menu.hidden) {
      open(false);
      toggle.focus();
      event.stopPropagation();
    }
  });
  surface.addEventListener("focusout", (event) => {
    if (!surface.contains(event.relatedTarget as Node | null)) open(false);
  });
  for (const [action, label] of actions) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.addEventListener("click", async (event) => {
      if (busy) return;
      open(false);
      toggle.focus();
      busy = true;
      clearTimeout(feedbackTimer);
      try {
        if (action !== "mute") {
          feedback.textContent = "Sending to SlipStream…";
          feedback.hidden = false;
        }
        const result = await send(action, event);
        if (action !== "mute" || !result.ok) {
          feedback.textContent = result.message ?? (result.ok ? "Sent to SlipStream" : "Could not send to SlipStream");
          feedback.hidden = false;
        }
      } catch {
        feedback.textContent = "SlipStream is unavailable.";
      } finally {
        busy = false;
        feedbackTimer = setTimeout(() => { feedback.hidden = true; }, 4500);
      }
    });
    menu.append(button);
  }
  surface.append(toggle, menu, feedback);
  root.append(surface);
  return {
    host,
    appearance: (state: Pick<BridgeState, "theme" | "accent">, dark: boolean) => applyAppearance(surface, state, dark),
    position(x: number, y: number, above: boolean) {
      host.style.setProperty("display", "block", "important");
      host.style.setProperty("top", y + "px", "important");
      host.style.setProperty("right", x + "px", "important");
      surface.style.setProperty("--menu-space", Math.max(110, (document.defaultView?.innerWidth ?? 250) - x - 8) + "px");
      surface.dataset.side = above ? "above" : "below";
    },
    hide() {
      host.style.setProperty("display", "none", "important");
      open(false);
    },
    destroy() {
      clearTimeout(feedbackTimer);
      host.remove();
    },
  };
}
