import {
  Archive, Check, CircleArrowDown, Clock3, Download, Gauge, Grid2X2, ListOrdered, Magnet,
  Minus, Moon, Music, Plus, Search, Settings, Square, Sun, Video, X,
} from "lucide-react";
import type { RefObject } from "react";
import { version } from "../../package.json";
import type { TelemetryBatch, TransferSnapshot } from "../types/ipc";
import { bytes, fileCategory, isDone, isRunning, playlistBuckets, speed, type PlaylistBucket } from "../lib/transfers";
import { IconButton } from "./ui";
import { Sparkline } from "./Sparkline";

const navigation = [
  ["all", "All Downloads", Grid2X2], ["active", "Active", CircleArrowDown], ["videos", "Videos & Media", Video],
  ["audio", "Audio", Music], ["torrents", "Torrents", Magnet], ["archives", "Archives & Files", Archive],
  ["queues", "Queues", ListOrdered], ["schedule", "Night Schedule", Clock3], ["completed", "Completed", Check],
] as const;

export type View = typeof navigation[number][0];
export const filters = ["All", "Active", "Paused", "Done"] as const;
export type StatusFilter = typeof filters[number];

export function matchesView(transfer: TransferSnapshot, view: View, playlists?: ReadonlyMap<string, PlaylistBucket>) {
  if (transfer.playlist_id && playlists) {
    const bucket = playlists.get(transfer.playlist_id);
    if (bucket) {
      if (view === "completed") return bucket === "completed";
      if (view === "active" || view === "queues") return bucket === "active" && transfer.status !== "canceled";
    }
  }
  switch (view) {
    case "all": return true;
    case "active": return !isDone(transfer) && transfer.status !== "canceled";
    case "completed": return isDone(transfer);
    case "queues": return !isDone(transfer) && transfer.status !== "canceled";
    case "schedule": return /night/i.test(transfer.queue_id) && transfer.status !== "canceled";
    default: return fileCategory(transfer) === view;
  }
}

export function matchesStatus(transfer: TransferSnapshot, filter: StatusFilter) {
  switch (filter) {
    case "All": return true;
    case "Active": return isRunning(transfer);
    case "Paused": return transfer.status === "paused";
    case "Done": return isDone(transfer);
  }
}

export function Sidebar({ view, select, transfers, batch, folder, samples, limit, preview, toggleLimit, openSettings }: {
  view: View; select: (view: View) => void; transfers: TransferSnapshot[]; batch: TelemetryBatch | null;
  folder: string; samples: number[]; limit: number | null; preview: boolean; toggleLimit: () => void; openSettings: () => void;
}) {
  const playlists = playlistBuckets(transfers);
  const torrentActive = transfers.some(transfer => transfer.kind === "torrent" && isRunning(transfer));
  const queueCount = new Set(transfers.filter(transfer => matchesView(transfer, "queues", playlists)).map(transfer => transfer.queue_id)).size;

  function item([key, label, Icon]: typeof navigation[number]) {
    const count = key === "queues" ? queueCount : transfers.filter(transfer => matchesView(transfer, key, playlists)).length;
    return <button type="button" key={key} className={"nav-item" + (view === key ? " active" : "")}
      aria-current={view === key ? "page" : undefined} onClick={() => select(key)}>
      <span className="nav-item-left"><Icon size={15} strokeWidth={1.75} />{label}</span>
      <span className="nav-item-right">{key === "torrents" && torrentActive && <span className="nav-dot" title="Torrent activity" />}
        <span className="nav-count tabular">{count}</span></span>
    </button>;
  }

  return <aside className="sidebar" aria-label="Sidebar">
    <div className="sidebar-top">
      <div className="brand-bar" data-tauri-drag-region>
        <span className="brand-left"><span className="brand-mark"><Download size={14} strokeWidth={2.2} /></span>
          <span className="brand-name">SlipStream</span></span>
        <span className="brand-ver">v{version}</span>
      </div>
      <nav className="nav-scroll" aria-label="Download views">
        {navigation.slice(0, 6).map(item)}
        <div className="nav-divider" />
        <p className="nav-section-label">Organize</p>
        {navigation.slice(6).map(item)}
      </nav>
    </div>
    <div className="sidebar-footer">
      <div className="spark-card" aria-label="Throughput">
        <div className="spark-rates tabular">
          <span className="rate-down"><span className="arrow">↓</span> {speed(batch?.aggregate_down_bps ?? 0)}</span>
          <span className="rate-up">↑ {speed(batch?.aggregate_up_bps ?? 0)}</span>
        </div>
        <Sparkline samples={samples} />
      </div>
      <div className="disk-row">
        <div className="disk-meta tabular">
          <span title={folder} className="disk-path selectable">{folder || "Default download folder"}</span>
          {preview && <span>{bytes(1.4e12)} free</span>}
        </div>
        <div className="disk-track" aria-hidden="true">{preview && <div className="disk-fill" />}</div>
      </div>
      <div className="sidebar-actions">
        <button className={"footer-btn tabular" + (limit !== null ? " limit-on" : "")} type="button" title="Toggle speed limit"
          aria-pressed={limit !== null} onClick={toggleLimit}>
          <Gauge size={13} />{limit === null ? "Unlimited" : "Cap: " + speed(limit).replace(".0 MB", " MB")}
        </button>
        <button className="footer-btn" type="button" onClick={openSettings}><Settings size={13} />Settings</button>
      </div>
    </div>
  </aside>;
}

export function Toolbar({ search, searchInputRef, setSearch, filter, setFilter, theme, accent, toggleTheme, toggleAccent,
  openAdd, openPalette, submitSearch, windowAction }: {
  search: string; searchInputRef: RefObject<HTMLInputElement | null>; setSearch: (value: string) => void; filter: StatusFilter;
  setFilter: (value: StatusFilter) => void; theme: "dark" | "light"; accent: "blue" | "mono";
  toggleTheme: () => void; toggleAccent: () => void; openAdd: () => void; openPalette: () => void;
  submitSearch: () => void; windowAction: (action: "minimize" | "toggleMaximize" | "close") => void;
}) {
  return <header className="toolbar" data-tauri-drag-region>
    <form className="search-box" data-tauri-drag-region="false" onSubmit={event => { event.preventDefault(); submitSearch(); }}>
      <Search size={14} />
      <input ref={searchInputRef} type="search" aria-label="Search downloads" placeholder="Paste URL, magnet link, or search…"
        value={search} onChange={event => setSearch(event.target.value)} />
      <button type="button" title="Open Command Palette" className="kbd" onClick={openPalette}>Ctrl K</button>
    </form>
    <div className="toolbar-right" data-tauri-drag-region="false">
      <div className="seg-control" role="group" aria-label="Transfer status">
        {filters.map(value => <button key={value} type="button" className={"seg-btn" + (value === filter ? " active" : "")}
          aria-pressed={value === filter} onClick={() => setFilter(value)}>{value}</button>)}
      </div>
      <button className="theme-pill" type="button" onClick={toggleTheme} title="Switch app theme">
        {theme === "dark" ? <Moon size={13} /> : <Sun size={13} />}
        <span>{theme === "dark" ? "Graphite" : "Porcelain"}</span>
      </button>
      <IconButton label={accent === "blue" ? "Switch to Monochrome accent" : "Switch to Blue accent"}
        className="accent-dot-btn" onClick={toggleAccent}><span className="accent-swatch" /></IconButton>
      <button type="button" className="btn-primary" aria-keyshortcuts="Control+n" onClick={openAdd}><Plus size={13} strokeWidth={2.4} />Add</button>
      <div className="win-chrome" role="group" aria-label="Window controls">
        <IconButton className="win-btn" label="Minimize" onClick={() => windowAction("minimize")}><Minus size={11} /></IconButton>
        <IconButton className="win-btn" label="Maximize or restore" onClick={() => windowAction("toggleMaximize")}><Square size={9} /></IconButton>
        <IconButton className="win-btn close" label="Close window" onClick={() => windowAction("close")}><X size={11} /></IconButton>
      </div>
    </div>
  </header>;
}
