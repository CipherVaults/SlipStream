import { Download, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type DragEvent } from "react";
import { AddDialog, type AddSeed } from "./components/AddDialog";
import { Inspector, type TorrentFilesEntry } from "./components/Inspector";
import { Palette, type QuickCommand } from "./components/Palette";
import { formatUpdateProgress, SettingsDialog, type SettingsLoadState } from "./components/SettingsDialog";
import { Sidebar, Toolbar, matchesStatus, matchesView, type StatusFilter, type View } from "./components/Shell";
import { TransferDialog } from "./components/TransferDialog";
import { TransferList, type OnTransferAction } from "./components/TransferList";
import { IconButton } from "./components/ui";
import { DEFAULT_SETTINGS } from "./lib/demo";
import { desktop, isTauri } from "./lib/tauri";
import { errorMessage, isDownloadSource, isRemoteTorrentUrl, isRunning, isTorrentSource, MB, SPEED_CAP } from "./lib/transfers";
import type { AppSettings, TelemetryBatch, TorrentFileEntry, TransferSnapshot, UpdateProgress, UpdateStatus } from "./types/ipc";

type Overlay = { type: "add"; seed: AddSeed; key: number } | { type: "settings"; focusUpdates?: boolean } | { type: "palette" }
  | { type: "refresh" | "verify"; transfer: TransferSnapshot };
type Notice = { tone: "ok" | "error"; message: string };
type Unsubscribe = () => void | Promise<void>;

const systemThemeQuery = "(prefers-color-scheme: dark)";
const emptySamples: number[] = [];
const sampleCount = 40;

function stopSafely(unsubscribe: Unsubscribe) {
  try {
    void Promise.resolve(unsubscribe()).catch(() => {});
  } catch {
    // Ignore synchronous listener cleanup failures on teardown.
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
  const [settingsLoadState, setSettingsLoadState] = useState<SettingsLoadState>("loading");
  const [settingsError, setSettingsError] = useState("");
  const settingsRef = useRef(DEFAULT_SETTINGS);
  const settingsLoadedRef = useRef(false);
  const settingsLoadRef = useRef<Promise<AppSettings> | null>(null);
  const settingsQueue = useRef(Promise.resolve());
  const settingsErrorMessagesRef = useRef<Set<string>>(new Set());
  const mountedRef = useRef(true);
  const startupUpdateChecked = useRef(false);
  const updateCheckSequence = useRef(0);
  const [updateInfo, setUpdateInfo] = useState<UpdateStatus>();
  const [updateBannerDismissed, setUpdateBannerDismissed] = useState(false);
  const [updateProgress, setUpdateProgress] = useState<UpdateProgress | null>(null);
  const [updateInstalling, setUpdateInstalling] = useState(false);
  const updateInstallLock = useRef(false);
  const [batch, setBatch] = useState<TelemetryBatch | null>(null);
  const latest = useRef<TelemetryBatch | null>(null);
  const eventVersion = useRef(0);
  const [samples, setSamples] = useState<number[]>(() => preview ? [18, 30, 46, 40, 74, 70, 92, 86].map(value => value * MB) : []);
  const [history, setHistory] = useState<Record<string, number[]>>((): Record<string, number[]> =>
    preview ? { alpine: [37, 42, 45.4, 48, 46, 52.4, 50, 52.4].map(value => value * MB) } : {});
  const [view, setView] = useState<View>("all");
  const [filter, setFilter] = useState<StatusFilter>("All");
  const [search, setSearch] = useState("");
  const [selectedId, setSelectedId] = useState<string>();
  const [removed, setRemoved] = useState<Set<string>>(() => new Set());
  const [torrentFilesMap, setTorrentFilesMap] = useState<Record<string, TorrentFilesEntry>>({});
  const torrentFilesRef = useRef<Record<string, TorrentFilesEntry>>({});
  torrentFilesRef.current = torrentFilesMap;
  const torrentActiveIdsRef = useRef(new Set<string>());
  const torrentQueuesRef = useRef(new Map<string, Promise<void>>());
  const torrentPendingRef = useRef(new Map<string, number>());
  const torrentLatestFilesRef = useRef(new Map<string, TorrentFileEntry[]>());
  const torrentGenRef = useRef(new Map<string, number>());
  const [inspectorOpen, setInspectorOpen] = useState(true);
  const [overlay, setOverlay] = useState<Overlay>();
  const [droppedSource, setDroppedSource] = useState<{ url: string; seq: number }>();
  const [busy, setBusy] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [notice, setNotice] = useState<Notice>();
  const modalSequence = useRef(0);
  const dropSequence = useRef(0);
  const busyRef = useRef(false);
  const overlayRef = useRef<Overlay | undefined>(undefined);
  const pendingPickers = useRef<AddSeed[]>([]);
  const dragDepth = useRef(0);
  const theme = useTheme(settings);
  const transfers = batch?.transfers.filter(transfer => !removed.has(transfer.id)) ?? [];
  const term = search.trim().toLowerCase();
  const items = transfers.filter(transfer => matchesView(transfer, view) && matchesStatus(transfer, filter)
    && [transfer.filename, transfer.source_domain, transfer.protocol_label, ...transfer.format_tags]
      .some(value => value.toLowerCase().includes(term)));
  const selected = items.find(transfer => transfer.id === selectedId) ?? items[0];
  const speedLimit = (batch ? batch.speed_limit_bps : settings.global_speed_limit_bps) ?? null;

  useEffect(() => {
    mountedRef.current = true;
    return () => { mountedRef.current = false; };
  }, []);

  const commitOverlay = useCallback((next: Overlay | undefined | ((current: Overlay | undefined) => Overlay | undefined)) => {
    const resolved = typeof next === "function" ? next(overlayRef.current) : next;
    overlayRef.current = resolved;
    setOverlay(resolved);
  }, []);

  const showError = useCallback((error: unknown) => setNotice({ tone: "error", message: errorMessage(error) }), []);
  const notify = (message: string) => setNotice({ tone: "ok", message });

  useEffect(() => {
    setUpdateBannerDismissed(false);
  }, [updateInfo?.version]);

  const checkForUpdates = useCallback(async () => {
    const sequence = ++updateCheckSequence.current;
    const result = await desktop.checkForUpdates();
    if (mountedRef.current && sequence === updateCheckSequence.current) setUpdateInfo(result);
  }, []);

  async function installUpdate() {
    if (updateInstallLock.current) return;
    updateInstallLock.current = true;
    updateCheckSequence.current++;
    setUpdateInstalling(true);
    setUpdateProgress(null);
    try {
      if (!await desktop.installUpdate() && mountedRef.current) {
        setUpdateInfo(previous => previous && { available: false, current_version: previous.current_version });
      }
    } finally {
      updateInstallLock.current = false;
      if (mountedRef.current) {
        setUpdateProgress(null);
        setUpdateInstalling(false);
      }
    }
  }

  const bumpTorrentGen = useCallback((transferId: string) => {
    const nextGen = (torrentGenRef.current.get(transferId) ?? 0) + 1;
    torrentGenRef.current.set(transferId, nextGen);
    return nextGen;
  }, []);

  const clearTorrentCache = useCallback((transferId: string) => {
    bumpTorrentGen(transferId);
    torrentActiveIdsRef.current.delete(transferId);
    torrentLatestFilesRef.current.delete(transferId);
    torrentPendingRef.current.delete(transferId);
    torrentQueuesRef.current.delete(transferId);
    if (transferId in torrentFilesRef.current) {
      const next = { ...torrentFilesRef.current };
      delete next[transferId];
      torrentFilesRef.current = next;
    }
    setTorrentFilesMap(previous => {
      if (!(transferId in previous)) return previous;
      const next = { ...previous };
      delete next[transferId];
      return next;
    });
  }, [bumpTorrentGen]);

  const loadSettings = useCallback(async () => {
    if (settingsLoadedRef.current) return settingsRef.current;
    if (!settingsLoadRef.current) {
      if (mountedRef.current) {
        setSettingsLoadState("loading");
        setSettingsError("");
      }
      settingsLoadRef.current = desktop.getSettings().then(next => {
        settingsLoadedRef.current = true;
        settingsRef.current = next;
        const trackedErrors = new Set(settingsErrorMessagesRef.current);
        settingsErrorMessagesRef.current.clear();
        if (mountedRef.current) {
          setSettings(next);
          setSettingsLoadState("ready");
          setSettingsError("");
          if (trackedErrors.size > 0) {
            setNotice(current => current?.tone === "error" && trackedErrors.has(current.message) ? undefined : current);
          }
          if (!startupUpdateChecked.current) {
            startupUpdateChecked.current = true;
            if ((next.update_mode ?? "auto") === "auto") void checkForUpdates().catch(() => {});
          }
        }
        return next;
      }).catch(error => {
        settingsLoadRef.current = null;
        const msg = errorMessage(error);
        settingsErrorMessagesRef.current.add(msg);
        if (mountedRef.current) {
          setSettingsLoadState("error");
          setSettingsError(msg);
        }
        throw error;
      });
    }
    return settingsLoadRef.current;
  }, [checkForUpdates]);

  const acceptBatch = useCallback((next: TelemetryBatch) => {
    if (latest.current && Date.parse(next.timestamp) < Date.parse(latest.current.timestamp)) return;
    const duplicate = latest.current?.timestamp === next.timestamp;
    latest.current = next;
    setBatch(next);
    const present = new Set(next.transfers.map(transfer => transfer.id));
    setRemoved(previous => {
      if (previous.size === 0) return previous;
      let changed = false;
      const retained = new Set<string>();
      previous.forEach(id => {
        if (present.has(id)) retained.add(id);
        else changed = true;
      });
      return changed ? retained : previous;
    });
    const staleIds = Array.from(torrentActiveIdsRef.current).filter(id => !present.has(id));
    for (const id of staleIds) {
      clearTorrentCache(id);
    }
    if (duplicate) return;
    setSamples(previous => [...previous.slice(1 - sampleCount), next.aggregate_down_bps]);
    setHistory(previous => Object.fromEntries(next.transfers.map(transfer => [
      transfer.id, [...(previous[transfer.id] ?? []).slice(1 - sampleCount), transfer.download_speed_bps],
    ])));
  }, [clearTorrentCache]);

  const pullSnapshot = useCallback(async () => {
    const before = eventVersion.current;
    const next = await desktop.getSnapshot();
    if (eventVersion.current === before || Date.parse(next.timestamp) > Date.parse(latest.current?.timestamp ?? "")) acceptBatch(next);
  }, [acceptBatch]);

  const openAdd = useCallback((seed: AddSeed = {}, deferIfOpen = false) => {
    if (busyRef.current || (deferIfOpen && overlayRef.current && overlayRef.current.type !== "palette")) {
      pendingPickers.current.push(seed);
      return;
    }
    setDroppedSource(undefined);
    commitOverlay({ type: "add", seed, key: ++modalSequence.current });
  }, [commitOverlay]);

  const handleDroppedSource = useCallback((source: string, autoProbe: boolean) => {
    if (overlayRef.current?.type === "add" && !busyRef.current && !autoProbe) {
      setDroppedSource({ url: source, seq: ++dropSequence.current });
      return;
    }
    openAdd({ url: source, autoProbe }, true);
  }, [openAdd]);

  const updateBusy = useCallback((value: boolean) => {
    busyRef.current = value;
    setBusy(value);
  }, []);

  const loadTorrentFiles = useCallback((transferId: string, force = false) => {
    const existing = torrentFilesRef.current[transferId];
    if (!force && existing && (existing.status === "loading" || existing.status === "ready")) return;
    if ((torrentPendingRef.current.get(transferId) ?? 0) > 0) return;
    const gen = torrentGenRef.current.get(transferId) ?? 0;
    torrentActiveIdsRef.current.add(transferId);

    setTorrentFilesMap(previous => {
      if ((torrentGenRef.current.get(transferId) ?? 0) !== gen) return previous;
      return {
        ...previous,
        [transferId]: {
          status: "loading",
          files: previous[transferId]?.files,
          saving: previous[transferId]?.saving ?? false,
          selectionError: "",
        },
      };
    });

    void desktop.listTorrentFiles(transferId).then(files => {
      if (!mountedRef.current || (torrentGenRef.current.get(transferId) ?? 0) !== gen) return;
      if ((torrentPendingRef.current.get(transferId) ?? 0) > 0) return;
      torrentLatestFilesRef.current.set(transferId, files);
      setTorrentFilesMap(previous => {
        if ((torrentGenRef.current.get(transferId) ?? 0) !== gen) return previous;
        return {
          ...previous,
          [transferId]: {
            status: "ready",
            files,
            saving: (torrentPendingRef.current.get(transferId) ?? 0) > 0,
            selectionError: previous[transferId]?.selectionError ?? "",
          },
        };
      });
    }, error => {
      if (!mountedRef.current || (torrentGenRef.current.get(transferId) ?? 0) !== gen) return;
      setTorrentFilesMap(previous => {
        if ((torrentGenRef.current.get(transferId) ?? 0) !== gen) return previous;
        return {
          ...previous,
          [transferId]: {
            status: "error",
            files: previous[transferId]?.files,
            error: errorMessage(error),
            saving: (torrentPendingRef.current.get(transferId) ?? 0) > 0,
            selectionError: previous[transferId]?.selectionError ?? "",
          },
        };
      });
    });
  }, []);

  const selectTorrentFile = useCallback((transferId: string, index: number, checked: boolean) => {
    const currentFiles = torrentLatestFilesRef.current.get(transferId) ?? torrentFilesRef.current[transferId]?.files;
    if (!currentFiles) return;
    const gen = torrentGenRef.current.get(transferId) ?? 0;
    torrentActiveIdsRef.current.add(transferId);
    const nextFiles = currentFiles.map(file => file.index === index ? { ...file, selected: checked } : file);
    torrentLatestFilesRef.current.set(transferId, nextFiles);
    const nextPending = (torrentPendingRef.current.get(transferId) ?? 0) + 1;
    torrentPendingRef.current.set(transferId, nextPending);

    setTorrentFilesMap(previous => {
      if ((torrentGenRef.current.get(transferId) ?? 0) !== gen) return previous;
      return {
        ...previous,
        [transferId]: {
          status: "ready",
          files: nextFiles,
          saving: true,
          selectionError: "",
        },
      };
    });

    const indices = nextFiles.filter(file => file.selected).map(file => file.index);
    const prior = torrentQueuesRef.current.get(transferId) ?? Promise.resolve();
    const nextQueue = prior.then(async () => {
      try {
        if ((torrentGenRef.current.get(transferId) ?? 0) !== gen) return;
        await desktop.setTorrentFiles(transferId, indices);
      } catch (error) {
        if ((torrentGenRef.current.get(transferId) ?? 0) !== gen) return;
        if (mountedRef.current) {
          const msg = errorMessage(error);
          setTorrentFilesMap(previous => {
            if ((torrentGenRef.current.get(transferId) ?? 0) !== gen) return previous;
            return {
              ...previous,
              [transferId]: {
                ...previous[transferId],
                status: "ready",
                files: previous[transferId]?.files ?? nextFiles,
                saving: (torrentPendingRef.current.get(transferId) ?? 0) > 1,
                selectionError: msg,
              },
            };
          });
        }
        try {
          if ((torrentGenRef.current.get(transferId) ?? 0) === gen
            && (torrentPendingRef.current.get(transferId) ?? 0) <= 1) {
            const fresh = await desktop.listTorrentFiles(transferId);
            if (mountedRef.current
              && (torrentGenRef.current.get(transferId) ?? 0) === gen
              && (torrentPendingRef.current.get(transferId) ?? 0) <= 1) {
              torrentLatestFilesRef.current.set(transferId, fresh);
              setTorrentFilesMap(previous => {
                if ((torrentGenRef.current.get(transferId) ?? 0) !== gen) return previous;
                return {
                  ...previous,
                  [transferId]: {
                    status: "ready",
                    files: fresh,
                    saving: false,
                    selectionError: previous[transferId]?.selectionError ?? "",
                  },
                };
              });
            }
          }
        } catch {
          // Retain optimistic state and keep selectionError visible.
        }
      } finally {
        if ((torrentGenRef.current.get(transferId) ?? 0) === gen) {
          const remaining = Math.max(0, (torrentPendingRef.current.get(transferId) ?? 1) - 1);
          torrentPendingRef.current.set(transferId, remaining);
          if (remaining === 0) {
            const stillPresent = latest.current?.transfers.some(transfer => transfer.id === transferId) ?? true;
            if (!stillPresent) {
              clearTorrentCache(transferId);
            } else if (mountedRef.current) {
              setTorrentFilesMap(previous => {
                if ((torrentGenRef.current.get(transferId) ?? 0) !== gen || !previous[transferId]) return previous;
                return {
                  ...previous,
                  [transferId]: { ...previous[transferId], saving: false },
                };
              });
            }
          }
        }
      }
    });
    torrentQueuesRef.current.set(transferId, nextQueue);
  }, [clearTorrentCache]);

  useEffect(() => {
    let cancelled = false;
    const cleanup: Unsubscribe[] = [];
    const report = (error: unknown) => { if (!cancelled) showError(error); };
    async function connect(listener: Promise<Unsubscribe>, onError = report) {
      try {
        const stop = await listener;
        if (!cancelled) cleanup.push(stop);
        else stopSafely(stop);
      } catch (error) {
        onError(error);
      }
    }
    void connect(desktop.onTelemetry(next => {
      if (cancelled) return;
      eventVersion.current++;
      acceptBatch(next);
    })).then(() => {
      if (!cancelled) void pullSnapshot().catch(report);
    });
    void connect(desktop.onOpenPicker(url => {
      if (!cancelled) openAdd({ url: url ?? undefined, autoProbe: Boolean(url) }, true);
    }));
    void connect(desktop.onUpdateProgress(progress => {
      if (!cancelled && updateInstallLock.current) setUpdateProgress(progress);
    }), () => {});
    void connect(desktop.onFileDrop(event => {
      if (cancelled) return;
      if (event.type === "enter") setDragging(event.paths.some(isTorrentSource));
      if (event.type === "leave" || event.type === "drop") setDragging(false);
      if (event.type === "drop") {
        const path = event.paths.find(isTorrentSource);
        if (path) handleDroppedSource(path, false);
      }
    }));
    void loadSettings().catch(report);
    return () => {
      cancelled = true;
      cleanup.forEach(stopSafely);
    };
  }, [acceptBatch, handleDroppedSource, loadSettings, openAdd, pullSnapshot, showError]);

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

  function enqueueSettings(operation: () => Promise<void>) {
    const task = settingsQueue.current.then(async () => {
      await loadSettings();
      await operation();
    });
    settingsQueue.current = task.catch(() => {});
    return task;
  }

  async function persistAndSyncSettings(next: AppSettings) {
    await desktop.saveSettings(next);
    if (!isTauri()) {
      settingsRef.current = next;
      if (mountedRef.current) setSettings(next);
      return;
    }
    try {
      const synced = await desktop.getSettings();
      settingsRef.current = synced;
      if (mountedRef.current) setSettings(synced);
    } catch (error) {
      settingsLoadedRef.current = false;
      settingsLoadRef.current = null;
      const detail = errorMessage(error);
      const refreshMsg = "Settings saved, but failed to refresh canonical settings: " + detail;
      settingsErrorMessagesRef.current.add(refreshMsg);
      settingsErrorMessagesRef.current.add(detail);
      settingsRef.current = next;
      if (mountedRef.current) {
        setSettings(next);
        setSettingsLoadState("error");
        setSettingsError(refreshMsg);
      }
      throw new Error(refreshMsg);
    }
  }

  function saveAppearance(change: Partial<AppSettings>) {
    void enqueueSettings(async () => {
      const next = { ...settingsRef.current, ...change };
      await persistAndSyncSettings(next);
    }).catch(showError);
  }

  const toggleTheme = () => saveAppearance({ theme: theme === "dark" ? "light" : "dark" });
  const toggleAccent = () => saveAppearance({ accent: settings.accent === "blue" ? "mono" : "blue" });
  const closeDialog = useCallback(() => {
    const next = pendingPickers.current.shift();
    if (next) {
      setDroppedSource(undefined);
      commitOverlay({ type: "add", seed: next, key: ++modalSequence.current });
      return;
    }
    setDroppedSource(undefined);
    commitOverlay(undefined);
  }, [commitOverlay]);
  const openPalette = () => commitOverlay({ type: "palette" });
  const openSettings = () => {
    if (settingsLoadState === "error") void loadSettings().catch(() => {});
    commitOverlay({ type: "settings" });
  };
  const openSource = (url: string) => openAdd({ url, autoProbe: true });
  const toggleInspector = () => setInspectorOpen(open => !open);
  const hibernate = async () => {
    await desktop.hibernateToTray();
    if (preview) notify("Hibernated to the tray in preview.");
  };

  useEffect(() => {
    function handleKey(event: KeyboardEvent) {
      if (event.defaultPrevented || event.repeat) return;
      if (event.key === "Escape" && overlayRef.current) {
        event.preventDefault();
        if (!busyRef.current) closeDialog();
        return;
      }
      if (!(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey) return;
      switch (event.key.toLowerCase()) {
        case "k":
          event.preventDefault();
          if (busyRef.current) break;
          if (overlayRef.current && overlayRef.current.type !== "palette") break;
          commitOverlay(current => current?.type === "palette" ? undefined : { type: "palette" });
          break;
        case "n":
          event.preventDefault();
          if (busyRef.current) break;
          if (overlayRef.current && overlayRef.current.type !== "palette") break;
          openAdd();
          break;
        case "i":
          event.preventDefault();
          setInspectorOpen(open => !open);
          break;
      }
    }
    document.addEventListener("keydown", handleKey);
    return () => document.removeEventListener("keydown", handleKey);
  }, [openAdd, closeDialog, commitOverlay]);

  const onTransferAction: OnTransferAction = (action, transfer) => {
    if (action === "refresh" || action === "verify") {
      commitOverlay({ type: action, transfer });
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
          await desktop.cancelTransfer(transfer.id, false);
          clearTorrentCache(transfer.id);
          setRemoved(previous => new Set([...previous, transfer.id]));
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
      if (path || preview) handleDroppedSource(path ?? file.name, false);
      else showError("Use the full .torrent path or drop the file from Explorer.");
      return;
    }
    const content = event.dataTransfer.getData("text/uri-list") || event.dataTransfer.getData("text/plain");
    const source = content.split(/\r?\n/).map(line => line.trim()).find(isDownloadSource);
    if (source) handleDroppedSource(source, !isTorrentSource(source) && !isRemoteTorrentUrl(source));
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
        toggleLimit={() => void enqueueSettings(async () => {
          const currentLimit = (latest.current ? latest.current.speed_limit_bps : settingsRef.current.global_speed_limit_bps) ?? null;
          const limit = currentLimit === null ? SPEED_CAP : null;
          await desktop.setSpeedLimit(limit);
          settingsRef.current = { ...settingsRef.current, global_speed_limit_bps: limit };
          if (mountedRef.current) setSettings(settingsRef.current);
          await pullSnapshot();
        }).catch(showError)} />
      <div className="main-col">
        <Toolbar search={search} setSearch={setSearch} filter={filter} setFilter={setFilter}
          theme={theme} accent={settings.accent} toggleTheme={toggleTheme} toggleAccent={toggleAccent}
          openAdd={() => openAdd()} openPalette={openPalette} windowAction={action => void perform(desktop[action])}
          submitSearch={() => { if (isDownloadSource(search.trim())) openSource(search.trim()); }} />
        {updateInfo?.available && updateInfo.version && !updateBannerDismissed &&
          <section className="update-banner" aria-label="App update">
            <Download size={16} aria-hidden="true" />
            <div className="update-banner-copy" role="status">
              <strong>Update v{updateInfo.version} available</strong>
              {updateInstalling && <span>{updateProgress ? formatUpdateProgress(updateProgress) : "Preparing update…"}</span>}
            </div>
            <div className="update-banner-actions">
              <button type="button" className="btn-ghost form-button"
                onClick={() => commitOverlay({ type: "settings", focusUpdates: true })}>Details</button>
              <button type="button" className="btn-primary form-button" disabled={updateInstalling}
                onClick={() => void perform(installUpdate)}>Install Update</button>
              <IconButton className="folder-ghost" label="Dismiss update banner"
                onClick={() => setUpdateBannerDismissed(true)}><X size={14} /></IconButton>
            </div>
          </section>}
        <div className="workspace-body">
          <TransferList items={items} selected={selected?.id} setSelected={id => { setSelectedId(id); setInspectorOpen(true); }}
            onAction={onTransferAction} view={view} preview={preview} loading={!batch && !notice} />
          {inspectorOpen && <Inspector key={selected?.id} transfer={selected} samples={selected ? history[selected.id] ?? emptySamples : emptySamples}
            preview={preview} torrentEntry={selected ? torrentFilesMap[selected.id] : undefined}
            onLoadTorrentFiles={loadTorrentFiles} onSelectTorrentFile={selectTorrentFile}
            onAction={onTransferAction} onFormat={(probe, format) => openAdd({ url: probe.source_url, probe, format })} />}
        </div>
      </div>
    </div>
    {notice && <div className={"toast " + notice.tone} role={notice.tone === "error" ? "alert" : "status"}>
      <span>{notice.message}</span>
      <IconButton className="folder-ghost" label="Dismiss notification" onClick={() => setNotice(undefined)}><X size={13} /></IconButton>
    </div>}
    {overlay?.type === "add" && <AddDialog key={overlay.key} seed={overlay.seed} droppedSource={droppedSource} settings={settings} onClose={closeDialog}
      onBusyChange={updateBusy} onAdded={(message, id) => {
        if (id) {
          clearTorrentCache(id);
          setRemoved(previous => {
            if (!previous.has(id)) return previous;
            const next = new Set(previous);
            next.delete(id);
            return next;
          });
        }
        closeDialog();
        setView("all"); setFilter("All"); setSearch(""); setSelectedId(id);
        notify(message);
        void pullSnapshot().catch(showError);
      }} />}
    {overlay?.type === "settings" && <SettingsDialog settings={{ ...settings, global_speed_limit_bps: speedLimit }}
      loadState={settingsLoadState} loadError={settingsError} onRetryLoad={() => void loadSettings().catch(() => {})}
      focusUpdates={overlay.focusUpdates} updateInfo={updateInfo} updateProgress={updateProgress} updateInstalling={updateInstalling}
      onCheckForUpdates={checkForUpdates} onInstallUpdate={installUpdate}
      onClose={closeDialog} onBusyChange={updateBusy} onHibernate={hibernate} onSave={async patch => {
        await enqueueSettings(async () => {
          const next = { ...settingsRef.current, ...patch };
          await persistAndSyncSettings(next);
        });
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
