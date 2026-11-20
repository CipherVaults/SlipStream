import { Check, Download, Plug, Save, Settings, Timer } from "lucide-react";
import { useEffect, useState } from "react";
import { desktop } from "../lib/tauri";
import { MB, errorMessage } from "../lib/transfers";
import type { AppSettings, ToolchainStatus } from "../types/ipc";
import { Dialog, Switch } from "./ui";

export function SettingsDialog({ settings, onClose, onSave, onHibernate, onBusyChange }: {
  settings: AppSettings; onClose: () => void; onSave: (next: AppSettings) => Promise<void>;
  onHibernate: () => Promise<void>; onBusyChange: (busy: boolean) => void;
}) {
  const [draft, setDraft] = useState(settings);
  const [limit, setLimit] = useState(settings.global_speed_limit_bps === null ? "" : String(settings.global_speed_limit_bps / MB));
  const [tools, setTools] = useState<ToolchainStatus>();
  const [ids, setIds] = useState({ chrome: "", edge: "", firefox: "" });
  const [busy, setBusy] = useState("");
  const [message, setMessage] = useState("");
  const [problem, setProblem] = useState("");
  const [toolError, setToolError] = useState("");

  useEffect(() => {
    let mounted = true;
    desktop.getToolchainStatus().then(value => { if (mounted) setTools(value); },
      error => { if (mounted) setToolError(errorMessage(error)); });
    return () => { mounted = false; };
  }, []);

  function update<K extends keyof AppSettings>(key: K, value: AppSettings[K]) {
    setDraft(previous => ({ ...previous, [key]: value }));
  }

  async function run(action: string, operation: () => Promise<void>) {
    if (busy) return;
    setBusy(action);
    onBusyChange(true);
    setMessage("");
    setProblem("");
    try {
      await operation();
    } catch (error) {
      setProblem(errorMessage(error));
    } finally {
      setBusy("");
      onBusyChange(false);
    }
  }

  async function install() {
    const names = await desktop.installBrowserHosts(ids.chrome.trim() || null, ids.edge.trim() || null, ids.firefox.trim() || null);
    setMessage(names.length ? "Browser hosts installed for " + names.join(", ") + "." : "Browser host installation finished.");
    setTools(await desktop.getToolchainStatus());
    setToolError("");
  }

  return <Dialog title="Settings & Browser Integration" className="settings-sheet" onClose={onClose} busy={Boolean(busy)}
    header={<Settings className="dialog-symbol" size={19} />}>
    <form className="dialog-form" onSubmit={event => {
      event.preventDefault();
      const amount = limit.trim() && Number(limit);
      void run("save", async () => {
        await onSave({ ...draft, ytdlp_path: draft.ytdlp_path?.trim() || null, ffmpeg_path: draft.ffmpeg_path?.trim() || null,
          default_download_dir: draft.default_download_dir.trim(),
          global_speed_limit_bps: amount ? Math.round(amount * MB) : null });
      });
    }}>
      <fieldset className="dialog-fields" disabled={Boolean(busy)}>
        <div className="fields-grid">
          <label className="field">Theme<select data-initial-focus value={draft.theme} onChange={event => update("theme", event.target.value as AppSettings["theme"])}>
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
          <label className="field">Max concurrent downloads<input required type="number" min={1} max={255} value={draft.max_concurrent_downloads || ""}
            onChange={event => update("max_concurrent_downloads", Number(event.target.value))} /></label>
          <label className="field full-width">Global speed limit in MB/s<input type="number" min={0} max={9_007_199_254} step="any"
            placeholder="Unlimited" value={limit} onChange={event => setLimit(event.target.value)} /></label>
        </div>
        <div className="settings-switches">
          <Switch label="Hibernate to tray on close" checked={draft.close_to_tray_hibernate} onChange={value => update("close_to_tray_hibernate", value)} />
          <Switch label="Scan with Windows Defender" checked={draft.auto_defender_scan} onChange={value => update("auto_defender_scan", value)} />
          <Switch label="Monitor clipboard" checked={draft.monitor_clipboard} onChange={value => update("monitor_clipboard", value)} />
          <button className="btn-ghost form-button" type="button" onClick={() => void run("hibernate", onHibernate)}><Timer size={14} />Hibernate Now</button>
        </div>
        <section className="settings-group" aria-label="Toolchain">
          <h3 className="settings-heading"><Download size={14} />Toolchain</h3>
          <div className="fields-grid">
            <label className="field">yt-dlp path<input value={draft.ytdlp_path ?? ""} placeholder={tools?.ytdlp_path ?? "Automatic"}
              onChange={event => update("ytdlp_path", event.target.value)} /></label>
            <label className="field">FFmpeg path<input value={draft.ffmpeg_path ?? ""} placeholder={tools?.ffmpeg_path ?? "Automatic"}
              onChange={event => update("ffmpeg_path", event.target.value)} /></label>
          </div>
          <p className="field-note tabular" role="status">{toolError || (tools
            ? "yt-dlp " + (tools.ytdlp_version ?? "unavailable") + " · FFmpeg " + (tools.ffmpeg_version ?? "unavailable")
            : "Checking toolchain…")}</p>
        </section>
        <section className="settings-group" aria-label="Browser Integration">
          <h3 className="settings-heading"><Plug size={14} />Browser Integration</h3>
          <div className="fields-grid browser-fields">
            {(["chrome", "edge", "firefox"] as const).map(browser => <label className="field" key={browser}>
              {browser[0].toUpperCase() + browser.slice(1)} extension ID<input spellCheck={false} value={ids[browser]}
                placeholder={browser === "firefox" ? "slipstream@ciphervaults.com" : "chrome-extension://…"}
                onChange={event => setIds({ ...ids, [browser]: event.target.value })} />
            </label>)}
          </div>
          <div className="integration-status">
            <span className={"field-note " + (tools?.native_hosts_installed ? "integrity-ok" : "")}>
              {tools?.native_hosts_installed ? <><Check size={13} />Native hosts installed</>
                : tools ? "Native hosts pending installation" : "Native host status pending"}
            </span>
            <button type="button" className="btn-ghost form-button"
              onClick={() => void run("install", install)}>{busy === "install" ? "Installing…" : "Install Browser Native Hosts"}</button>
          </div>
        </section>
      </fieldset>
      {message && <p role="status" className="integrity-ok">{message}</p>}
      {problem && <p role="alert" className="inline-error">{problem}</p>}
      <div className="sheet-footer align-right"><div className="sheet-foot-btns">
        <button className="btn-ghost form-button" type="button" onClick={onClose} disabled={Boolean(busy)}>Cancel</button>
        <button className="btn-primary" type="submit" disabled={Boolean(busy)}><Save size={14} />{busy === "save" ? "Saving…" : "Save Settings"}</button>
      </div></div>
    </form>
  </Dialog>;
}
