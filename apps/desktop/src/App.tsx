import { Download, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type DragEvent } from "react";
import { AddDialog, type AddSeed } from "./components/AddDialog";
import { Inspector } from "./components/Inspector";
import { Palette, type QuickCommand } from "./components/Palette";
import { SettingsDialog } from "./components/SettingsDialog";
import { Sidebar, Toolbar, matchesStatus, matchesView, type StatusFilter, type View } from "./components/Shell";
import { TransferDialog } from "./components/TransferDialog";
import { TransferList, type OnTransferAction } from "./components/TransferList";
import { IconButton } from "./components/ui";
import { DEFAULT_SETTINGS } from "./lib/demo";
import { desktop, isTauri } from "./lib/tauri";
import { errorMessage, isDownloadSource, isRunning, isTorrentSource, MB, SPEED_CAP } from "./lib/transfers";
import type { AppSettings, TelemetryBatch, TransferSnapshot } from "./types/ipc";

type Overlay = { type: "add"; seed: AddSeed; key: number } | { type: "settings" } | { type: "palette" }
  | { type: "refresh" | "verify"; transfer: TransferSnapshot };
type Notice = { tone: "ok" | "error"; message: string };

const systemThemeQuery = "(prefers-color-scheme: dark)";
const removedStorageKey = "slipstream.removed-transfers";
const emptySamples: number[] = [];
const sampleCount = 40;

function readRemoved() {
  try {
    const ids: unknown = JSON.parse(localStorage.getItem(removedStorageKey) ?? "[]");
    return new Set(Array.isArray(ids) ? ids.filter((id): id is string => typeof id === "string") : []);
  } catch {
    return new Set<string>();
  }
}

function useTheme(settings: AppSettings) {
  const [systemDark, setSystemDark] = useState(() => window.matchMedia?.(systemThemeQuery).matches ?? true);
  const theme = settings.theme === "system" ? (systemDark ? "dark" : "light") : settings.theme;
  useEffect(() => {
    if (settings.theme !== "system") return;
    const query = window.matchMedia?.(systemThemeQuery);
    if (!query) return;
    const sync = () => setSystemDark(query.matches);
    sync();
    query.addEventListener("change", sync);
    return () => query.removeEventListener("change", sync);
  }, [settings.theme]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.dataset.accent = settings.accent;
  }, [theme, settings.accent]);
  return theme;
}

export default function App() {
  const [preview] = useState(() => !isTauri());
  const [settings, setSettings] = useState(DEFAULT_SETTINGS);
  const [settingsReady, setSettingsReady] = useState(false);
  const settingsRef = useRef(DEFAULT_SETTINGS);
  const settingsQueue = useRef(Promise.resolve());
  const [batch, setBatch] = useState<TelemetryBatch | null>(null);
  const latest = useRef<TelemetryBatch | null>(null);
  const eventVersion = useRef(0);
  const [samples, setSamples] = useState<number[]>(() => preview ? [18, 30, 46, 40, 74, 70, 92, 86].map(value => value * MB) : []);
  const [history, setHistory] = useState<Record<string, number[]>>((): Record<string, number[]> => preview ? {
    alpine: [37, 42, 45.4, 48, 46, 52.4, 50, 52.4].map(value => value * MB),
  } : {});
  const [view, setView] = useState<View>("all");
  const [filter, setFilter] = useState<StatusFilter>("All");
  const [search, setSearch] = useState("");
  const [selectedId, setSelectedId] = useState<string>();
  const [removed, setRemoved] = useState(() => preview ? new Set<string>() : readRemoved());
  const [inspectorOpen, setInspectorOpen] = useState(true);
  const [overlay, setOverlay] = useState<Overlay>();
  const [busy, setBusy] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [notice, setNotice] = useState<Notice>();
  const modalSequence = useRef(0);
  const busyRef = useRef(false);
  const pendingPicker = useRef<AddSeed | undefined>(undefined);
  const dragDepth = useRef(0);
  const theme = useTheme(settings);
  const transfers = batch?.transfers.filter(transfer => !removed.has(transfer.id)) ?? [];
  const term = search.trim().toLowerCase();
  const items = transfers.filter(transfer => matchesView(transfer, view) && matchesStatus(transfer, filter)
    && [transfer.filename, transfer.source_domain, transfer.protocol_label, ...transfer.format_tags]
      .some(value => value.toLowerCase().includes(term)));
  const selected = items.find(transfer => transfer.id === selectedId) ?? items[0];
  const speedLimit = (batch ? batch.speed_limit_bps : settings.global_speed_limit_bps) ?? null;

  const showError = useCallback((error: unknown) => setNotice({ tone: "error", message: errorMessage(error) }), []);
  const notify = (message: string) => setNotice({ tone: "ok", message });

  const acceptBatch = useCallback((next: TelemetryBatch) => {
    if (latest.current && Date.parse(next.timestamp) < Date.parse(latest.current.timestamp)) return;
    const duplicate = latest.current?.timestamp === next.timestamp;
    latest.current = next;
    setBatch(next);
    if (duplicate) return;
    setSamples(previous => [...previous.slice(1 - sampleCount), next.aggregate_down_bps]);
    setHistory(previous => Object.fromEntries(next.transfers.map(transfer => [
      transfer.id, [...(previous[transfer.id] ?? []).slice(1 - sampleCount), transfer.download_speed_bps],
    ])));
  }, []);

  const pullSnapshot = useCallback(async () => {
    const before = eventVersion.current;
    const next = await desktop.getSnapshot();
    if (eventVersion.current === before || Date.parse(next.timestamp) > Date.parse(latest.current?.timestamp ?? "")) acceptBatch(next);
  }, [acceptBatch]);

  const openAdd = useCallback((seed: AddSeed = {}) => {
    if (busyRef.current) {
      pendingPicker.current = seed;
      return;
    }
    setOverlay({ type: "add", seed, key: ++modalSequence.current });
  }, []);

  const updateBusy = useCallback((value: boolean) => {
    busyRef.current = value;
    setBusy(value);
    if (!value && pendingPicker.current) {
      const pending = pendingPicker.current;
      pendingPicker.current = undefined;
      openAdd(pending);
    }
  }, [openAdd]);

  useEffect(() => {
    let live = true;
    const cleanup: Array<() => void> = [];
    const report = (error: unknown) => { if (live) showError(error); };
    async function connect(listener: Promise<() => void>) {
      try {
        const stop = await listener;
        if (live) cleanup.push(stop);
        else stop();
      } catch (error) {
        report(error);
      }
    }
    void connect(desktop.onTelemetry(next => {
      if (!live) return;
      eventVersion.current++;
      acceptBatch(next);
    })).then(() => {
      if (live) void pullSnapshot().catch(report);
    });
    void connect(desktop.onOpenPicker(url => {
      if (live) openAdd({ url: url ?? undefined, autoProbe: Boolean(url) });
    }));
    void connect(desktop.onFileDrop(event => {
      if (!live) return;
      if (event.type === "enter") setDragging(event.paths.some(isTorrentSource));
      if (event.type === "leave" || event.type === "drop") setDragging(false);
      if (event.type === "drop") {
        const path = event.paths.find(isTorrentSource);
        if (path) openAdd({ url: path });
      }
    }));
    void desktop.getSettings().then(next => {
      if (!live) return;
      settingsRef.current = next;
      setSettings(next);
    }).catch(report).finally(() => { if (live) setSettingsReady(true); });
    return () => { live = false; cleanup.forEach(stop => stop()); };
  }, [acceptBatch, openAdd, pullSnapshot, showError]);

  useEffect(() => {
    if (!notice || notice.tone === "error") return;
    const timeout = window.setTimeout(() => setNotice(undefined), 4400);
    return () => window.clearTimeout(timeout);
  }, [notice]);

  async function perform(action: () => Promise<unknown>, message?: string) {
    try {
      await action();
      if (message) notify(message);
    } catch (error) {
      showError(error);
    }
  }

  function saveAppearance(change: Partial<AppSettings>) {
    if (!settingsReady) return;
    const next = { ...settingsRef.current, ...change };
    settingsRef.current = next;
    setSettings(next);

    const saved = settingsQueue.current.then(() => desktop.saveSettings(next));
    settingsQueue.current = saved.catch(async error => {
      showError(error);
      if (settingsRef.current !== next) return;
      try {
        const current = await desktop.getSettings();
        if (settingsRef.current === next) {
          settingsRef.current = current;
          setSettings(current);
        }
      } catch {
        // Keep the applied appearance until a subsequent settings load succeeds.
      }
    });
  }

  const toggleTheme = () => saveAppearance({ theme: theme === "dark" ? "light" : "dark",
    global_speed_limit_bps: speedLimit });
  const toggleAccent = () => saveAppearance({ accent: settings.accent === "blue" ? "mono" : "blue",
    global_speed_limit_bps: speedLimit });
  const closeDialog = () => setOverlay(undefined);
  const openPalette = () => setOverlay({ type: "palette" });
  const openSettings = () => setOverlay({ type: "settings" });
  const openSource = (url: string) => openAdd({ url, autoProbe: true });
  const toggleInspector = () => setInspectorOpen(open => !open);
  const hibernate = async () => {
    await desktop.hibernateToTray();
    if (preview) notify("Hibernated to the tray in preview.");
  };

  useEffect(() => {
    function handleKey(event: KeyboardEvent) {
      if (event.defaultPrevented || event.repeat) return;
      if (event.key === "Escape" && overlay) {
        event.preventDefault();
        if (!busy) setOverlay(undefined);
        return;
      }
      if (!(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey) return;
      switch (event.key.toLowerCase()) {
        case "k":
          event.preventDefault();
          if (!busy) setOverlay(current => current?.type === "palette" ? undefined : { type: "palette" });
          break;
        case "n":
          event.preventDefault();
          if (!busy) openAdd();
          break;
        case "i":
          event.preventDefault();
          setInspectorOpen(open => !open);
          break;
      }
    }
    document.addEventListener("keydown", handleKey);
    return () => document.removeEventListener("keydown", handleKey);
  }, [overlay, busy, openAdd]);

  const onTransferAction: OnTransferAction = (action, transfer) => {
    if (action === "refresh" || action === "verify") {
      setOverlay({ type: action, transfer });
      return;
    }
    void perform(async () => {
      switch (action) {
        case "pause": await desktop.pauseTransfer(transfer.id); break;
        case "resume": await desktop.resumeTransfer(transfer.id); break;
        case "reveal":
          await desktop.revealInExplorer(transfer.id);
          if (preview) notify("Save folder: " + transfer.save_dir);
          return;
        case "cancel": await desktop.cancelTransfer(transfer.id, false); break;
        case "remove":
          if (transfer.status !== "canceled" && transfer.status !== "completed") await desktop.cancelTransfer(transfer.id, false);
          setRemoved(previous => {
            const next = new Set([...previous, transfer.id]);
            if (!preview) {
              try { localStorage.setItem(removedStorageKey, JSON.stringify([...next])); } catch { /* Storage can be disabled. */ }
            }
            return next;
          });
          break;
      }
      await pullSnapshot();
    });
  };

  function dragEnter(event: DragEvent<HTMLDivElement>) {
    if (!Array.from(event.dataTransfer.types).some(type => ["Files", "text/uri-list", "text/plain"].includes(type))) return;
    event.preventDefault();
    dragDepth.current++;
    setDragging(true);
  }

  function drop(event: DragEvent<HTMLDivElement>) {
    event.preventDefault();
    dragDepth.current = 0;
    setDragging(false);
    const file = Array.from(event.dataTransfer.files).find(item => item.name.toLowerCase().endsWith(".torrent"));
    if (file) {
      const path = (file as File & { path?: string }).path;
      if (path || preview) openAdd({ url: path ?? file.name });
      else showError("Use the full .torrent path or drop the file from Explorer.");
      return;
    }
    const content = event.dataTransfer.getData("text/uri-list") || event.dataTransfer.getData("text/plain");
    const source = content.split(/\r?\n/).map(line => line.trim()).find(isDownloadSource);
    if (source) openSource(source);
    else if (content || event.dataTransfer.files.length) showError("Drop an HTTP, HTTPS, or magnet link, or a .torrent file.");
  }

  async function groupAction(group: "pause" | "resume") {
    const eligible = transfers.filter(transfer => group === "pause" ? isRunning(transfer) || transfer.status === "queued"
      : transfer.status === "paused");
    const operations = await Promise.allSettled(eligible.map(transfer => group === "pause"
      ? desktop.pauseTransfer(transfer.id) : desktop.resumeTransfer(transfer.id)));
    await pullSnapshot();
    const failed = operations.find(result => result.status === "rejected");
    if (failed?.status === "rejected") throw failed.reason;
    notify(eligible.length + (group === "pause" ? " transfers paused." : " transfers resumed."));
  }

  const commands: QuickCommand[] = [
    { label: "Add Download", shortcut: "Ctrl N", run: () => openAdd() },
    { label: "Toggle Theme", run: () => { closeDialog(); toggleTheme(); } },
    { label: "Toggle Accent", run: () => { closeDialog(); toggleAccent(); } },
    { label: "Toggle Inspector", shortcut: "Ctrl I", run: () => { closeDialog(); toggleInspector(); } },
    { label: "Pause All", run: () => { closeDialog(); void perform(() => groupAction("pause")); } },
    { label: "Resume All", run: () => { closeDialog(); void perform(() => groupAction("resume")); } },
    { label: "Settings", run: openSettings },
    { label: "Hibernate to Tray", run: () => { closeDialog(); void perform(hibernate); } },
  ];

  return <div className="app-window" onDragEnter={dragEnter} onDragOver={event => event.preventDefault()} onDropCapture={() => { dragDepth.current = 0; setDragging(false); }}
    onDragLeave={() => { dragDepth.current = Math.max(0, dragDepth.current - 1); if (!dragDepth.current) setDragging(false); }} onDrop={drop}>
    <div className="app-shell" inert={Boolean(overlay)}>
      <Sidebar view={view} select={next => { setView(next); setFilter("All"); }} transfers={transfers} batch={batch}
        folder={settings.default_download_dir} samples={samples} limit={speedLimit} preview={preview} openSettings={openSettings}
        toggleLimit={() => void perform(async () => {
          await settingsQueue.current;
          const limit = speedLimit === null ? SPEED_CAP : null;
          await desktop.setSpeedLimit(limit);
          settingsRef.current = { ...settingsRef.current, global_speed_limit_bps: limit };
          setSettings(settingsRef.current);
          await pullSnapshot();
        })} />
      <div className="main-col">
        <Toolbar search={search} setSearch={setSearch} filter={filter} setFilter={setFilter}
          theme={theme} accent={settings.accent} toggleTheme={toggleTheme} toggleAccent={toggleAccent}
          openAdd={() => openAdd()} openPalette={openPalette} windowAction={action => void perform(desktop[action])}
          submitSearch={() => { if (isDownloadSource(search.trim())) openSource(search.trim()); }} />
        <div className="workspace-body">
          <TransferList items={items} selected={selected?.id} setSelected={id => { setSelectedId(id); setInspectorOpen(true); }}
            onAction={onTransferAction} view={view} preview={preview} loading={!batch && !notice} />
          {inspectorOpen && <Inspector key={selected?.id} transfer={selected} samples={selected ? history[selected.id] ?? emptySamples : emptySamples}
            preview={preview} onAction={onTransferAction}
            onFormat={(probe, format) => openAdd({ url: probe.source_url, probe, format })} />}
        </div>
      </div>
    </div>
    {notice && <div className={"toast " + notice.tone} role={notice.tone === "error" ? "alert" : "status"}>
      <span>{notice.message}</span>
      <IconButton className="folder-ghost" label="Dismiss notification" onClick={() => setNotice(undefined)}><X size={13} /></IconButton>
    </div>}
    {overlay?.type === "add" && <AddDialog key={overlay.key} seed={overlay.seed} settings={settings} onClose={closeDialog}
      onBusyChange={updateBusy} onAdded={(message, id) => {
        closeDialog();
        setView("all"); setFilter("All"); setSearch(""); setSelectedId(id);
        notify(message);
        void pullSnapshot().catch(showError);
      }} />}
    {overlay?.type === "settings" && <SettingsDialog key={String(settingsReady)} settings={{ ...settings, global_speed_limit_bps: speedLimit }}
      onClose={closeDialog} onBusyChange={updateBusy} onHibernate={hibernate} onSave={async next => {
        await settingsQueue.current;
        await desktop.saveSettings(next);
        settingsRef.current = next;
        setSettings(next);
        closeDialog();
        notify("Settings saved.");
        void pullSnapshot().catch(showError);
      }} />}
    {(overlay?.type === "refresh" || overlay?.type === "verify") && <TransferDialog mode={overlay.type}
      transfer={overlay.transfer} onClose={closeDialog} onBusyChange={updateBusy} onChanged={message => {
        if (message) notify(message);
        void pullSnapshot().catch(showError);
      }} />}
    {overlay?.type === "palette" && <Palette transfers={transfers} commands={commands} initial={search} onClose={closeDialog}
      onAdd={openSource} onFilter={value => { setView("all"); setFilter("All"); setSearch(value); closeDialog(); }}
      onSelect={id => { setView("all"); setFilter("All"); setSearch(""); setSelectedId(id); setInspectorOpen(true); closeDialog(); }} />}
    {dragging && <div className="drop-overlay" aria-hidden="true"><div><Download size={25} />Drop a URL, magnet link, or .torrent file</div></div>}
  </div>;
}
