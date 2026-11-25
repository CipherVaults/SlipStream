import { Download, Info, Plug, Save, Settings, Timer } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { version } from "../../package.json";
import { desktop } from "../lib/tauri";
import { MB, errorMessage } from "../lib/transfers";
import { DEFAULT_CHROMIUM_EXTENSION_ID, DEFAULT_FIREFOX_EXTENSION_ID } from "../types/ipc";
import type { AppSettings, BrowserKind, BrowserSetupResult, BrowserStatus, ToolchainStatus, ToolSource, UpdateProgress, UpdateStatus } from "../types/ipc";
import { Dialog, Switch } from "./ui";

export type SettingsLoadState = "loading" | "ready" | "error";

const toolSources: Record<ToolSource, string> = {
  bundled: "Bundled", managed: "Managed", custom: "Custom", system_path: "System PATH", missing: "Missing",
};
const manualBrowsers = [
  ["chrome", "Chrome"], ["edge", "Edge"], ["firefox", "Firefox"],
] as const;

function downloadFraction(progress: UpdateProgress) {
  if (progress.stage !== "downloading" || !progress.total_bytes || progress.total_bytes <= 0) return undefined;
  return Math.max(0, Math.min(1, progress.downloaded_bytes / progress.total_bytes));
}

export function formatUpdateProgress(progress: UpdateProgress) {
  if (progress.stage === "verifying") return "Verifying…";
  if (progress.stage === "installing") return "Installing…";
  const downloaded = (progress.downloaded_bytes / MB).toFixed(1) + " MB";
  const fraction = downloadFraction(progress);
  if (fraction === undefined) return "Downloading… " + downloaded;
  const total = ((progress.total_bytes ?? 0) / MB).toFixed(1) + " MB";
  return `Downloading… ${Math.round(fraction * 100)}% (${downloaded} / ${total})`;
}

function ToolRow({ name, path, version, source }: {
  name: string; path: string | null; version?: string | null; source: ToolSource;
}) {
  const firstLine = version?.trim().split(/\r?\n/, 1)[0]?.replace(/\s+Copyright\b.*$/i, "").trim();
  return <li className="tool-row">
    <strong>{name}</strong>
    <span className="tool-version tabular" title={firstLine}>{firstLine || (path ? "Version unavailable" : "Unavailable")}</span>
    <span className={"badge " + (source === "missing" || !path ? "amber" : "")}>
      {toolSources[source] ?? (path ? "Unknown source" : "Missing")}
    </span>
  </li>;
}

async function copyText(value: string) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(value);
      return true;
    }
  } catch {
    // WebViews can deny access to the Clipboard API.
  }
  const previous = document.activeElement;
  const field = document.createElement("textarea");
  field.value = value;
  field.readOnly = true;
  field.className = "clipboard-fallback";
  try {
    (previous?.closest(".settings-sheet") ?? document.body).append(field);
    field.select();
    return document.execCommand?.("copy") === true;
  } catch {
    return false;
  } finally {
    field.remove();
    if (previous instanceof HTMLElement && previous.isConnected) previous.focus({ preventScroll: true });
  }
}

export function SettingsDialog({
  settings, loadState, loadError, focusUpdates, updateInfo, updateProgress, updateInstalling,
  onCheckForUpdates, onInstallUpdate, onRetryLoad, onClose, onSave, onHibernate, onBusyChange,
}: {
  settings: AppSettings; loadState: SettingsLoadState; loadError?: string; onRetryLoad: () => void;
  focusUpdates?: boolean; updateInfo?: UpdateStatus; updateProgress: UpdateProgress | null; updateInstalling: boolean;
  onCheckForUpdates: () => Promise<void>; onInstallUpdate: () => Promise<void>;
  onClose: () => void; onSave: (patch: Partial<AppSettings>) => Promise<void>;
  onHibernate: () => Promise<void>; onBusyChange: (busy: boolean) => void;
}) {
  const [draft, setDraft] = useState(settings);
  const [limit, setLimit] = useState(settings.global_speed_limit_bps === null ? "" : String(settings.global_speed_limit_bps / MB));
  const [tools, setTools] = useState<ToolchainStatus>();
  const [browsers, setBrowsers] = useState<BrowserStatus[]>();
  const [setup, setSetup] = useState<BrowserSetupResult>();
  const [ids, setIds] = useState<Partial<Record<BrowserKind, string>>>({});
  const [busy, setBusy] = useState("");
  const busyLock = useRef(false);
  const dirtyFields = useRef<Set<keyof AppSettings>>(new Set());
  const [message, setMessage] = useState("");
  const [problem, setProblem] = useState("");
  const [toolError, setToolError] = useState("");
  const [toolUpdate, setToolUpdate] = useState<{ message: string; failed?: boolean }>();
  const [browserError, setBrowserError] = useState("");
  const [copyFeedback, setCopyFeedback] = useState("");

  useEffect(() => {
    if (loadState !== "ready") return;
    setDraft(previous => {
      const merged: AppSettings = { ...previous };
      for (const key of Object.keys(settings) as Array<keyof AppSettings>) {
        if (!dirtyFields.current.has(key)) {
          (merged as Record<keyof AppSettings, unknown>)[key] = settings[key];
        }
      }
      if (!dirtyFields.current.has("update_mode")) merged.update_mode = settings.update_mode ?? "auto";
      return merged;
    });
    if (!dirtyFields.current.has("global_speed_limit_bps")) {
      setLimit(settings.global_speed_limit_bps === null ? "" : String(settings.global_speed_limit_bps / MB));
    }
  }, [loadState, settings]);

  const refreshIntegration = useCallback(async (isCurrent: () => boolean = () => true) => {
    await Promise.all([
      desktop.getToolchainStatus().then(value => {
        if (!isCurrent()) return;
        setTools(value);
        setToolError("");
      }, error => { if (isCurrent()) setToolError(errorMessage(error)); }),
      desktop.listBrowsers().then(value => {
        if (!isCurrent()) return;
        setBrowsers(value);
        setBrowserError("");
      }, error => { if (isCurrent()) setBrowserError(errorMessage(error)); }),
    ]);
  }, []);

  useEffect(() => {
    let mounted = true;
    void refreshIntegration(() => mounted);
    return () => { mounted = false; };
  }, [refreshIntegration]);

  useEffect(() => {
    if (!copyFeedback) return;
    const timeout = window.setTimeout(() => setCopyFeedback(""), 3000);
    return () => window.clearTimeout(timeout);
  }, [copyFeedback]);

  function update<K extends keyof AppSettings>(key: K, value: AppSettings[K]) {
    dirtyFields.current.add(key);
    setDraft(previous => ({ ...previous, [key]: value }));
  }

  async function run(action: string, operation: () => Promise<void>) {
    if (busyLock.current) return;
    busyLock.current = true;
    setBusy(action);
    onBusyChange(true);
    setMessage("");
    setProblem("");
    try {
      await operation();
    } catch (error) {
      setProblem(errorMessage(error));
    } finally {
      busyLock.current = false;
      setBusy("");
      onBusyChange(false);
    }
  }

  async function registerHosts() {
    try {
      const names = await desktop.installBrowserHosts(ids.chrome?.trim() || null, ids.edge?.trim() || null, ids.firefox?.trim() || null);
      setMessage(names.length ? "Browser hosts installed for " + names.join(", ") + "." : "Browser host installation finished.");
    } finally {
      await refreshIntegration();
    }
  }

  async function openBrowserSetup(browser: BrowserKind) {
    setCopyFeedback("");
    setSetup(undefined);
    try {
      setSetup(await desktop.openBrowserExtensionSetup(browser, ids[browser]?.trim() || null));
    } finally {
      await refreshIntegration();
    }
  }

  async function updateYtdlp() {
    setToolUpdate(undefined);
    try {
      setTools(await desktop.updateYtdlp());
      setToolError("");
      setToolUpdate({ message: "yt-dlp update completed." });
    } catch (error) {
      setToolUpdate({ message: "Could not update yt-dlp. " + errorMessage(error), failed: true });
    }
  }

  async function copySetupValue(label: "path" | "ID", value: string) {
    setCopyFeedback(await copyText(value) ? `Copied ${label}` : `Select the ${label} to copy it manually.`);
  }

  const ready = loadState === "ready";
  const currentVersion = updateInfo?.current_version ?? version;
  const checkingTools = !tools && !toolError;
  const detectingBrowsers = !browsers && !browserError;

  return <Dialog title="Settings & Browser Integration" className="settings-sheet" onClose={onClose} busy={Boolean(busy)}
    header={<Settings className="dialog-symbol" size={19} />}>
    <form className="dialog-form" onSubmit={event => {
      event.preventDefault();
      if (!ready) return;
      const trimmedLimit = limit.trim();
      let speedLimitBps: number | null = null;
      if (trimmedLimit !== "") {
        const parsed = Number(trimmedLimit);
        const scaled = parsed * MB;
        const rounded = Math.round(scaled);
        if (!Number.isFinite(parsed) || parsed <= 0 || !Number.isFinite(scaled) || rounded < 1 || rounded > Number.MAX_SAFE_INTEGER) {
          setProblem("Enter a positive finite speed limit in MB/s, or leave blank for unlimited.");
          return;
        }
        speedLimitBps = rounded;
      }
      if (!Number.isInteger(draft.max_concurrent_downloads) || draft.max_concurrent_downloads < 1 || draft.max_concurrent_downloads > 32) {
        setProblem("Enter a whole number between 1 and 32 for max concurrent downloads.");
        return;
      }
      const patch: Partial<AppSettings> = {};
      const normalized: AppSettings = {
        ...draft,
        ytdlp_path: draft.ytdlp_path?.trim() || null,
        ffmpeg_path: draft.ffmpeg_path?.trim() || null,
        default_download_dir: draft.default_download_dir.trim(),
        global_speed_limit_bps: speedLimitBps,
      };
      for (const key of dirtyFields.current) {
        (patch as Record<keyof AppSettings, unknown>)[key] = normalized[key];
      }
      void run("save", async () => {
        await onSave(patch);
      });
    }}>
      {loadState === "loading" && <p className="field-note" role="status">Loading settings…</p>}
      {loadState === "error" && <div className="inline-error" role="alert">
        <p>{loadError || "Failed to load saved settings."}</p>
        <button type="button" className="btn-ghost form-button" onClick={() => { dirtyFields.current.clear(); setProblem(""); onRetryLoad(); }}>Retry</button>
      </div>}
      <fieldset className="dialog-fields" disabled={Boolean(busy) || !ready}>
        <div className="fields-grid">
          <label className="field">Theme<select data-initial-focus={focusUpdates ? undefined : ""} value={draft.theme}
            onChange={event => update("theme", event.target.value as AppSettings["theme"])}>
            <option value="system">System</option><option value="dark">Graphite</option><option value="light">Porcelain</option>
          </select></label>
          <label className="field">Accent<select value={draft.accent} onChange={event => update("accent", event.target.value as AppSettings["accent"])}>
            <option value="blue">Blue</option><option value="mono">Monochrome</option>
          </select></label>
          <label className="field full-width">Default download folder<input value={draft.default_download_dir}
            onChange={event => update("default_download_dir", event.target.value)} /></label>
          <label className="field">Max connections<select value={draft.default_connections} onChange={event => update("default_connections", Number(event.target.value))}>
            {Array.from({ length: 32 }, (_, index) => <option key={index}>{index + 1}</option>)}
          </select></label>
          <label className="field">Max concurrent downloads<input required type="number" min={1} max={32} value={draft.max_concurrent_downloads || ""}
            onChange={event => update("max_concurrent_downloads", Number(event.target.value))} /></label>
          <label className="field full-width">Global speed limit in MB/s<input type="number" min={0} max={9_007_199_254} step="any"
            placeholder="Unlimited" value={limit} onChange={event => {
              dirtyFields.current.add("global_speed_limit_bps");
              setLimit(event.target.value);
            }} /></label>
        </div>
        <div className="settings-switches">
          <Switch label="Hibernate to tray on close" checked={draft.close_to_tray_hibernate} onChange={value => update("close_to_tray_hibernate", value)} />
          <Switch label="Scan with Windows Defender" checked={draft.auto_defender_scan} onChange={value => update("auto_defender_scan", value)} />
          <Switch label="Monitor clipboard" checked={draft.monitor_clipboard} onChange={value => update("monitor_clipboard", value)} />
          <button className="btn-ghost form-button" type="button" onClick={() => void run("hibernate", onHibernate)}><Timer size={14} />Hibernate Now</button>
        </div>
        <section className="settings-group" aria-label="Toolchain">
          <div className="settings-section-header">
            <h3 className="settings-heading"><Download size={14} />Toolchain</h3>
            <button type="button" className="btn-ghost form-button" disabled={checkingTools}
              onClick={() => void run("update-ytdlp", updateYtdlp)}>{busy === "update-ytdlp" ? "Updating…" : "Update yt-dlp"}</button>
          </div>
          {checkingTools && <p className="field-note" role="status">Checking toolchain…</p>}
          {toolError && <p className="inline-error" role="alert">Could not read toolchain status. {toolError}</p>}
          {tools && <ul className="tool-list">
            <ToolRow name="yt-dlp" path={tools.ytdlp_path} version={tools.ytdlp_version} source={tools.ytdlp_source} />
            <ToolRow name="FFmpeg" path={tools.ffmpeg_path} version={tools.ffmpeg_version} source={tools.ffmpeg_source} />
            <ToolRow name="ffprobe" path={tools.ffprobe_path} version={tools.ffprobe_version} source={tools.ffprobe_source} />
          </ul>}
          {toolUpdate && <p role={toolUpdate.failed ? "alert" : "status"}
            className={toolUpdate.failed ? "inline-error" : "integrity-ok"}>{toolUpdate.message}</p>}
          <details className="settings-advanced">
            <summary>Advanced: Custom Paths</summary>
            <div className="fields-grid">
              <label className="field">yt-dlp path<input value={draft.ytdlp_path ?? ""} placeholder={tools?.ytdlp_path ?? "Automatic"}
                onChange={event => update("ytdlp_path", event.target.value)} /></label>
              <label className="field">FFmpeg path<input value={draft.ffmpeg_path ?? ""} placeholder={tools?.ffmpeg_path ?? "Automatic"}
                onChange={event => update("ffmpeg_path", event.target.value)} /></label>
            </div>
          </details>
        </section>
        <section className="settings-group" aria-label="Browser Integration">
          <h3 className="settings-heading"><Plug size={14} />Browser Integration</h3>
          {detectingBrowsers && <p className="field-note" role="status">Detecting browsers…</p>}
          {browserError && <p className="inline-error" role="alert">Could not read browser status. {browserError}</p>}
          {browsers && <div className="browser-cards">
            {browsers.map(browser => {
              const action = browser.native_host_registered ? "Open Setup" : "Add Extension";
              return <article className="browser-card" key={browser.id} aria-label={browser.name}>
                <h4 className="settings-heading">{browser.name}</h4>
                <div className="browser-status">
                  <span className={"badge " + (browser.installed ? "green" : "")}>
                    {browser.installed ? "Detected" : "Not detected"}
                  </span>
                  <span className={"badge " + (browser.native_host_registered ? "green" : "amber")}>
                    {browser.native_host_registered ? "Host ready" : "Host pending"}
                  </span>
                </div>
                <button type="button" className="btn-ghost form-button" disabled={detectingBrowsers} aria-label={action + " for " + browser.name}
                  onClick={() => void run(browser.id, () => openBrowserSetup(browser.id))}>
                  {busy === browser.id ? "Opening…" : action}
                </button>
              </article>;
            })}
          </div>}
          {setup && <section className="browser-setup" aria-label="Extension setup">
            <h4 className="settings-heading">{browsers?.find(browser => browser.id === setup.browser_id)?.name} extension setup</h4>
            <dl className="browser-setup-values">
              <dt>Extension folder</dt>
              <dd><code>{setup.extension_dir}</code><button type="button" className="btn-ghost form-button"
                onClick={() => void copySetupValue("path", setup.extension_dir)}>Copy Path</button></dd>
              <dt>Extension ID</dt>
              <dd><code>{setup.extension_id}</code><button type="button" className="btn-ghost form-button"
                onClick={() => void copySetupValue("ID", setup.extension_id)}>Copy ID</button></dd>
            </dl>
            {!setup.launched_browser && <p className="field-note">Open <code>{setup.extension_page_url}</code> in your browser.</p>}
            {!setup.revealed_extension_dir && <p className="field-note">Open the extension folder in File Explorer using the path above.</p>}
            {!setup.native_host_registered && <p className="field-note">Use Re-register Native Hosts in Advanced to connect the extension.</p>}
            {setup.browser_id === "firefox" ? <ol className="browser-steps field-note">
              <li>Choose <strong>Load Temporary Add-on</strong> on the Firefox debugging page.</li>
              <li>Select <strong>manifest.json</strong> in the extension folder above.</li>
            </ol> : <ol className="browser-steps field-note">
              <li>Turn on <strong>Developer mode</strong> on the extensions page.</li>
              <li>Choose <strong>Load unpacked</strong> and select the extension folder above.</li>
            </ol>}
            {copyFeedback && <p className="field-note" role="status">{copyFeedback}</p>}
          </section>}
          <details className="settings-advanced">
            <summary>Advanced: Manual Host &amp; Extension IDs</summary>
            <div className="fields-grid browser-fields">
              {manualBrowsers.map(([browser, name]) => <label className="field" key={browser}>
                {name} extension ID<input spellCheck={false} value={ids[browser] ?? ""}
                  placeholder={browser === "firefox" ? DEFAULT_FIREFOX_EXTENSION_ID : DEFAULT_CHROMIUM_EXTENSION_ID}
                  onChange={event => setIds(previous => ({ ...previous, [browser]: event.target.value }))} />
              </label>)}
            </div>
            <button type="button" className="btn-ghost form-button" disabled={checkingTools || detectingBrowsers}
              onClick={() => void run("register-hosts", registerHosts)}>
              {busy === "register-hosts" ? "Registering…" : "Re-register Native Hosts"}
            </button>
          </details>
        </section>
        <section className="settings-group" aria-label="About & Updates">
          <h3 className="settings-heading"><Info size={14} />About &amp; Updates</h3>
          <div className="settings-about">
            <div>
              <p className="settings-heading tabular">SlipStream v{currentVersion}</p>
              <p className="field-note">CipherVaults · Windows 10/11 · MIT License</p>
              <p className="field-note">Signed NSIS Auto-Update (GitHub Releases)</p>
              <p className="field-note" role="status">
                {updateInfo && (updateInfo.available
                  ? <>Update v{updateInfo.version} available.</>
                  : <>Up to date (v{currentVersion}).</>)}
              </p>
            </div>
            <div className="settings-update-actions">
              <button type="button" className="btn-ghost form-button" disabled={updateInstalling}
                onClick={() => void run("update-check", onCheckForUpdates)}>
                {busy === "update-check" ? "Checking…" : "Check for Updates"}
              </button>
              {updateInfo?.available && <button type="button" className="btn-primary form-button" disabled={updateInstalling}
                onClick={() => void run("update-install", onInstallUpdate)}>
                {updateInstalling ? "Update in progress…" : "Install Update & Restart"}
              </button>}
            </div>
          </div>
          <div className="settings-update-mode">
            <label className="field">Update Checks<select data-initial-focus={focusUpdates ? "" : undefined}
              aria-describedby="update-mode-note" value={draft.update_mode ?? "auto"}
              onChange={event => update("update_mode", event.target.value === "manual" ? "manual" : "auto")}>
              <option value="auto">Automatic on startup</option>
              <option value="manual">Manual only</option>
            </select></label>
            <p className="field-note" id="update-mode-note">Automatic checks run when SlipStream starts. You choose when to install.</p>
          </div>
          {updateInstalling && <div className="settings-update-progress">
            <p className="field-note tabular" role="status">{updateProgress ? formatUpdateProgress(updateProgress) : "Preparing update…"}</p>
            <progress aria-label="Update progress" max={1} value={updateProgress ? downloadFraction(updateProgress) : undefined} />
          </div>}
          {updateInfo?.available && updateInfo.pub_date &&
            <p className="field-note">Released {updateInfo.pub_date.slice(0, 10)}</p>}
          {updateInfo?.available && updateInfo.notes &&
            <p className="field-note settings-update-notes">{updateInfo.notes}</p>}
        </section>
      </fieldset>
      {message && <p role="status" className="integrity-ok">{message}</p>}
      {problem && problem !== loadError && <p role="alert" className="inline-error">{problem}</p>}
      <div className="sheet-footer align-right"><div className="sheet-foot-btns">
        <button className="btn-ghost form-button" type="button" onClick={onClose} disabled={Boolean(busy)}>Cancel</button>
        <button className="btn-primary" type="submit" disabled={Boolean(busy) || !ready}><Save size={14} />{busy === "save" ? "Saving…" : "Save Settings"}</button>
      </div></div>
    </form>
  </Dialog>;
}
