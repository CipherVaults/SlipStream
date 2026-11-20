import type { TransferSnapshot } from "@slipstream/ipc-types";
import { extension as api } from "../shared/browser";
import { readSettings } from "../shared/downloads";
import {
  applyAppearance, emptyState, formatSpeed, isRefreshableHttpTransfer, ratio, readState, SETTINGS_KEY, STATE_KEY,
  type ActionResult, type BridgeState, type BrowserMessage, type PopupAction,
} from "../shared/state";
import "../shared/theme.css";
import "./style.css";

const element = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const theme = matchMedia("(prefers-color-scheme: dark)");
const feedback = element("feedback");
const transfers = element("transfers");
const openDesktop = element<HTMLButtonElement>("open-desktop");
const refreshId = new URL(location.href).searchParams.get("refresh");
const refresh = element<HTMLFormElement>("refresh-form");
const refreshSelect = element<HTMLSelectElement>("refresh-transfer");
const commands = new Set<string>();
let state: BridgeState = emptyState;
let refreshOptions = "";

async function perform(message: BrowserMessage, key: string): Promise<void> {
  if (commands.has(key)) return;
  commands.add(key);
  feedback.textContent = "";
  render();
  try {
    const result: ActionResult = await api.runtime.sendMessage(message);
    feedback.textContent = result?.message ?? (result?.ok ? "Sent to SlipStream." : "SlipStream is unavailable.");
  } catch {
    feedback.textContent = "SlipStream is unavailable.";
  } finally {
    commands.delete(key);
    render();
  }
}

function desktop(request: PopupAction): void {
  void perform({ kind: "desktop", request }, "transfer_id" in request ? request.transfer_id : request.type);
}

function transferRow(item: TransferSnapshot): HTMLLIElement {
  const row = document.createElement("li");
  const heading = document.createElement("div");
  heading.className = "transfer-title";
  const filename = document.createElement("strong");
  filename.title = filename.textContent = item.filename;
  const fraction = item.total_bytes ? ratio(item.downloaded_bytes / item.total_bytes) : undefined;
  const progress = document.createElement("span");
  progress.textContent = fraction === undefined ? "—" : Math.floor(fraction * 100) + "%";
  heading.append(filename, progress);

  const bar = document.createElement("div");
  bar.className = "progress";
  bar.setAttribute("role", "progressbar");
  bar.setAttribute("aria-label", item.filename);
  if (fraction !== undefined) bar.setAttribute("aria-valuenow", String(Math.floor(fraction * 100)));
  const fill = document.createElement("span");
  fill.style.width = (fraction ?? 0) * 100 + "%";
  bar.append(fill);

  const segments = document.createElement("div");
  segments.className = "segments";
  segments.setAttribute("aria-hidden", "true");
  for (let index = 0; index < 16; index++) {
    const segment = document.createElement("span");
    const fill = document.createElement("i");
    fill.style.width = ratio(item.ui_segments?.[index] ?? 0) * 100 + "%";
    segment.append(fill);
    segments.append(segment);
  }
  const footer = document.createElement("div");
  footer.className = "transfer-footer";
  const speed = document.createElement("span");
  const paused = item.status === "paused";
  speed.textContent = item.status === "active" || item.status === "seeding"
    ? formatSpeed(item.download_speed_bps) : item.status[0].toUpperCase() + item.status.slice(1);
  footer.append(speed);
  if (["queued", "probing", "active", "paused", "seeding"].includes(item.status)) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = paused ? "Resume" : "Pause";
    button.disabled = !state.connected || commands.has(item.id);
    button.dataset.transfer = item.id;
    button.addEventListener("click", () => desktop({
      type: paused ? "resume_transfer" : "pause_transfer", transfer_id: item.id,
    }));
    footer.append(button);
  }
  row.append(heading, bar, segments, footer);
  return row;
}

function render(): void {
  applyAppearance(document.documentElement, state, theme.matches);
  const badge = element("connection");
  badge.textContent = state.connected ? "Connected" : "Disconnected";
  badge.dataset.connected = String(state.connected);
  const items = state.batch?.transfers ?? [];
  if (refreshId) {
    const eligible = items.filter(isRefreshableHttpTransfer);
    const signature = JSON.stringify(eligible.map(({ id, filename }) => [id, filename]));
    if (refreshOptions !== signature) {
      const selected = refreshSelect.value;
      const placeholder = new Option("Choose a download", "");
      refreshSelect.replaceChildren(placeholder, ...eligible.map(({ id, filename }) => new Option(filename, id)));
      if (eligible.some(({ id }) => id === selected)) refreshSelect.value = selected;
      refreshOptions = signature;
    }
    refresh.querySelector("button")!.disabled = !state.connected || !eligible.length || commands.has(refreshId);
    return;
  }
  element("down").textContent = state.connected ? formatSpeed(state.batch?.aggregate_down_bps ?? 0) : "—";
  element("up").textContent = state.connected ? formatSpeed(state.batch?.aggregate_up_bps ?? 0) : "—";
  element("active-count").textContent = state.connected ? (state.batch?.active_count ?? 0) + " active" : "";
  const current = items.filter(({ status }) => !["completed", "failed", "canceled"].includes(status));
  const empty = element("empty");
  empty.hidden = current.length > 0;
  empty.textContent = state.connected ? "No active transfers" : "Open SlipStream on your desktop to connect.";
  const focused = (document.activeElement as HTMLElement | null)?.dataset.transfer;
  const scroll = transfers.scrollTop;
  transfers.replaceChildren(...current.map(transferRow));
  transfers.scrollTop = scroll;
  if (focused) {
    Array.from(transfers.querySelectorAll("button")).find((button) => button.dataset.transfer === focused)?.focus();
  }
  openDesktop.disabled = !state.connected || commands.has("open_desktop_window");
}

openDesktop.addEventListener("click", () => desktop({ type: "open_desktop_window" }));
theme.addEventListener("change", render);
api.storage.onChanged.addListener((changes, area) => {
  if (area !== "session" || !(STATE_KEY in changes)) return;
  state = readState(changes[STATE_KEY].newValue);
  render();
});
void api.storage.session.get(STATE_KEY).then((values) => {
  state = readState(values[STATE_KEY]);
  render();
}).catch(render);
void api.runtime.sendMessage({ kind: "wake" } satisfies BrowserMessage).then((reply?: { ok?: boolean; state?: unknown }) => {
  if (reply?.state) {
    state = readState(reply.state);
    render();
  }
}).catch(() => {});

if (refreshId) {
  element("dashboard").hidden = true;
  element("refresh-panel").hidden = false;
  refresh.addEventListener("submit", (event) => {
    event.preventDefault();
    if (refreshSelect.value) void perform({ kind: "refresh", id: refreshId, transferId: refreshSelect.value }, refreshId);
  });
} else {
  const settingsForm = element<HTMLFormElement>("settings");
  const input = (id: string) => element<HTMLInputElement>(id);
  const parseList = (id: string) => [...new Set(input(id).value.split(/[,\s]+/).map((item) => item.toLowerCase()).filter(Boolean))];
  const mib = 1024 * 1024;
  void api.storage.local.get(SETTINGS_KEY).then((values) => {
    const preferences = readSettings(values[SETTINGS_KEY]);
    input("enabled").checked = preferences.enabled;
    input("extensions").value = preferences.extensions.join(", ");
    input("min-size").value = String(preferences.minBytes / mib);
    input("excluded").value = preferences.excludedSites.join(", ");
    input("muted").value = preferences.mutedSites.join(", ");
  }).catch(() => { feedback.textContent = "Could not load preferences."; });
  settingsForm.addEventListener("submit", (event) => {
    event.preventDefault();
    void perform({
      kind: "save-settings",
      settings: {
        enabled: input("enabled").checked,
        extensions: parseList("extensions"),
        minBytes: Math.min(Number.MAX_SAFE_INTEGER, Math.round(input("min-size").valueAsNumber * mib)),
        excludedSites: parseList("excluded"),
        mutedSites: parseList("muted"),
      },
    }, "settings");
  });
}
render();
