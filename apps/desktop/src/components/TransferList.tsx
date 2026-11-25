import {
  Archive, Check, ChevronDown, ChevronRight, Disc, Ellipsis, File, FileCheck2, Film, Folder, ListVideo, Magnet, Music,
  Pause, Play, RefreshCw, Trash2, X,
} from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import tile from "../assets/alpine-tile.svg";
import type { TransferSnapshot } from "../types/ipc";
import {
  SEGMENT_COUNT, bytePair, bytes, clock, fileCategory, isDone, isPlaylistCompleted, isPlaylistPending, isRunning, parentFolder, percentage, ratio, speed, statusLabel,
} from "../lib/transfers";
import type { View } from "./Shell";
import { IconButton } from "./ui";

export type TransferAction = "pause" | "resume" | "refresh" | "reveal" | "cancel" | "remove" | "verify";
export type OnTransferAction = (action: TransferAction, transfer: TransferSnapshot) => void;
export type OnPlaylistAction = (action: "pause_playlist" | "resume_playlist" | "reveal_playlist", playlistId: string) => void;

export function primaryAction(transfer: TransferSnapshot): "pause" | "resume" | null {
  if (transfer.status === "paused") return "resume";
  if (isRunning(transfer) || transfer.status === "queued") return "pause";
  return null;
}

export function TransferIcon({ transfer, size = 20 }: { transfer: TransferSnapshot; size?: number }) {
  const category = fileCategory(transfer);
  const Icon = category === "torrents" ? Magnet : category === "videos" ? Film : category === "audio" ? Music
    : /\.iso$/i.test(transfer.filename) ? Disc : /\.(zip|rar|7z|tar|gz)$/i.test(transfer.filename) ? Archive : File;
  return <Icon size={size} strokeWidth={1.75} />;
}

export function SegmentProgress({ transfer }: { transfer: TransferSnapshot }) {
  const state = transfer.status === "paused" ? "paused" : transfer.kind === "torrent" ? "torrent" : "active";
  return <span className={"seg-progress tabular " + state} role="progressbar" aria-label={transfer.filename + " progress"}
    aria-valuemin={0} aria-valuemax={100} aria-valuenow={transfer.total_bytes !== null ? percentage(transfer) : undefined}>
    {Array.from({ length: SEGMENT_COUNT }, (_, index) => <span className={"seg-cell " + state} key={index}>
      <span className="seg-fill" style={{ width: ratio(transfer.ui_segments[index] ?? 0) * 100 + "%" }} />
    </span>)}
  </span>;
}

function TransferMenu({ transfer, onAction }: { transfer: TransferSnapshot; onAction: OnTransferAction }) {
  const [open, setOpen] = useState(false);
  const wrapper = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    function close(event: PointerEvent) {
      if (!wrapper.current?.contains(event.target as Node)) setOpen(false);
    }
    document.addEventListener("pointerdown", close);
    return () => document.removeEventListener("pointerdown", close);
  }, [open]);
  function action(key: TransferAction, label: string, icon: ReactNode) {
    return <button type="button" onClick={() => { setOpen(false); onAction(key, transfer); }}>{icon}{label}</button>;
  }
  return <div ref={wrapper} className="transfer-menu" onKeyDown={event => {
    if (event.key === "Escape" && open) {
      event.stopPropagation();
      setOpen(false);
      wrapper.current?.querySelector("button")?.focus();
    }
  }}>
    <IconButton label={"More actions for " + transfer.filename} className="folder-ghost more-trigger"
      aria-expanded={open} onClick={() => setOpen(!open)}><Ellipsis size={15} /></IconButton>
    {open && <div className="transfer-popover">
      {!isDone(transfer) && transfer.kind === "http" && action("refresh", "Refresh Link", <RefreshCw size={13} />)}
      {!isDone(transfer) && action("reveal", "Reveal", <Folder size={14} />)}
      {isDone(transfer) && action("verify", "Verify SHA-256", <FileCheck2 size={14} />)}
      {!isDone(transfer) && transfer.status !== "canceled" && action("cancel", "Cancel transfer", <X size={14} />)}
      {action("remove", "Remove", <Trash2 size={13} />)}
    </div>}
  </div>;
}

function TransferRow({ transfer, selected, select, onAction, preview }: {
  transfer: TransferSnapshot; selected: boolean; select: () => void; onAction: OnTransferAction; preview: boolean;
}) {
  const torrent = transfer.kind === "torrent";
  const paused = transfer.status === "paused";
  const running = isRunning(transfer);
  const primary = primaryAction(transfer);
  const tags = preview && transfer.id === "alpine" ? transfer.format_tags.slice(0, 3) : transfer.format_tags;
  let source = [transfer.source_domain, transfer.active_connections ? transfer.active_connections + " threads" : "", transfer.protocol_label]
    .filter(Boolean).join(" · ");
  if (torrent) source = "↑ " + speed(transfer.upload_speed_bps)
    + (preview && transfer.id === "blender" ? " · DHT + PEX" : "");
  if (paused) source = transfer.resumable ? "Resumable Range" : transfer.protocol_label;

  return <article aria-label={transfer.filename} className={"transfer-row" + (selected ? " selected" : "")}>
    <button className="transfer-select" type="button" onClick={select} aria-pressed={selected} aria-label={"Inspect " + transfer.filename}>
      <span className={"thumb-tile" + (torrent ? " torrent-tile" : "")}>
        {preview && transfer.id === "alpine" ? <img src={tile} alt="" /> : <TransferIcon transfer={transfer} />}
      </span>
      <span className="transfer-main">
        <span className="transfer-top">
          <span className="transfer-name-wrap">
            <span className="transfer-name" title={transfer.filename}>{transfer.filename}</span>
            <span className="tag-row tabular">
              {torrent && <><span className="badge green">{transfer.protocol_label}</span><span className="badge">{transfer.active_peers} peers</span></>}
              {tags.map((tag, index) => <span className={"badge" + (index === 0 && transfer.kind === "media" ? " accent" : "")}
                key={tag + index}>{tag}</span>)}
              {transfer.playlist_id && transfer.playlist_index != null && <span className="badge">#{transfer.playlist_index}</span>}
              {paused && <span className="badge amber">Paused</span>}
              <span className="source-text">{source}</span>
            </span>
          </span>
          <span className="transfer-stats tabular">
            <span className={"stat-speed " + transfer.status}>
              {transfer.status === "active" ? speed(transfer.download_speed_bps)
                : transfer.status === "seeding" ? "↑ " + speed(transfer.upload_speed_bps) : statusLabel[transfer.status]}
            </span>
            <span className="stat-sub">{bytePair(transfer)}
              {running && transfer.eta_seconds !== null ? " · " + clock(transfer.eta_seconds) + " left" : ""}
              {paused && transfer.total_bytes ? " (" + percentage(transfer) + "%)" : ""}
            </span>
          </span>
        </span>
        <SegmentProgress transfer={transfer} />
      </span>
    </button>
    <div className="row-actions">
      {primary && <IconButton label={primary === "pause" ? "Pause" : "Resume"} onClick={() => onAction(primary, transfer)}>
        {primary === "pause" ? <Pause size={12} fill="currentColor" /> : <Play size={12} fill="currentColor" />}
      </IconButton>}
      {transfer.status === "failed" && transfer.kind === "http" && <IconButton label="Refresh Link"
        onClick={() => onAction("refresh", transfer)}><RefreshCw size={13} /></IconButton>}
      {transfer.playlist_id && transfer.status === "failed" && <IconButton label="Retry"
        onClick={() => onAction("resume", transfer)}><RefreshCw size={13} /></IconButton>}
      {transfer.playlist_id && isDone(transfer) && <IconButton label="Show in Explorer" className="folder-ghost"
        onClick={() => onAction("reveal", transfer)}><Folder size={14} /></IconButton>}
      <TransferMenu transfer={transfer} onAction={onAction} />
    </div>
  </article>;
}

function CompletedRow({ transfer, select, onAction }: {
  transfer: TransferSnapshot; select: () => void; onAction: OnTransferAction;
}) {
  const tag = transfer.sha256_verified === true ? "SHA-256" : transfer.format_tags.find(item => ["FFmpeg Tags", "MotW Safe"].includes(item));
  const codec = transfer.format_tags.find(item => /kbps/.test(item));
  const time = new Date(transfer.updated_at).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  return <article className="completed-row" aria-label={transfer.filename}>
    <button type="button" className="completed-left" onClick={select} aria-label={"Inspect " + transfer.filename}>
      <span className="completed-icon"><TransferIcon transfer={transfer} size={14} /></span>
      <span className="completed-name" title={transfer.filename}>{transfer.filename}</span>
      <span className="completed-meta tabular">{[codec, bytes(transfer.total_bytes ?? transfer.downloaded_bytes), time].filter(Boolean).join(" · ")}</span>
    </button>
    <div className="completed-right">
      {tag === "SHA-256" ? <button className="verified-pill" type="button" onClick={() => onAction("verify", transfer)}
        title="Verify SHA-256"><Check size={10} strokeWidth={2.8} />{tag}</button>
        : tag && <span className="verified-pill"><Check size={10} strokeWidth={2.8} />{tag}</span>}
      <IconButton label="Show in Explorer" className="folder-ghost" onClick={() => onAction("reveal", transfer)}><Folder size={14} /></IconButton>
      <TransferMenu transfer={transfer} onAction={onAction} />
    </div>
  </article>;
}

type TransferGroup = { playlistId?: string; transfers: TransferSnapshot[] };

function groupTransfers(items: TransferSnapshot[]) {
  const groups: TransferGroup[] = [];
  const playlists = new Map<string, TransferGroup>();
  for (const transfer of items) {
    const playlistId = transfer.playlist_id || undefined;
    const group = playlistId ? playlists.get(playlistId) : undefined;
    if (group) {
      group.transfers.push(transfer);
    } else {
      const next = { playlistId, transfers: [transfer] };
      groups.push(next);
      if (playlistId) playlists.set(playlistId, next);
    }
  }
  return groups;
}

function PlaylistGroup({ playlistId, transfers, members, onAction, renderRow }: {
  playlistId: string; transfers: TransferSnapshot[]; members: TransferSnapshot[]; onAction?: OnPlaylistAction;
  renderRow: (transfer: TransferSnapshot) => ReactNode;
}) {
  const bodyId = useId();
  const [expanded, setExpanded] = useState<boolean>();
  const items = [...transfers].sort((a, b) => (a.playlist_index ?? Infinity) - (b.playlist_index ?? Infinity));
  const current = members.find(isRunning);
  const waiting = members.filter(isPlaylistPending);
  const queued = waiting.filter(item => item.status === "queued").length;
  const paused = waiting.length > 0 && (members.some(item => item.playlist_paused === true)
    || waiting.every(item => item.status === "paused"));
  const done = members.filter(isDone).length;
  const failed = members.filter(item => item.status === "failed").length;
  const canceled = members.filter(item => item.status === "canceled").length;
  const total = members.find(item => item.playlist_total != null)?.playlist_total ?? members.length;
  const title = members.find(item => item.playlist_title?.trim())?.playlist_title?.trim() || "Playlist";
  const directory = members[0].save_dir;
  const folder = parentFolder(directory);
  const open = expanded ?? (waiting.length > 0 || failed > 0);
  const progress = paused ? "Paused" : current
    ? statusLabel[current.status] + (current.playlist_index != null ? ` #${current.playlist_index} of ${total}` : "")
    : "";
  const summary = done === members.length && members.length > 0
    ? (members.length === 1 && members[0].playlist_index == null && total > 1
      ? `${total} videos completed`
      : `${done}/${members.length} completed`)
    : [progress, `${done}/${total} done`, queued ? queued + " queued" : "", failed ? failed + " failed" : "",
      canceled ? canceled + " canceled" : ""].filter(Boolean).join(" · ");

  return <section className="playlist-group" aria-label={"Playlist: " + title}>
    <header className="playlist-group-header">
      <button type="button" className="playlist-group-toggle" aria-expanded={open} aria-controls={bodyId}
        aria-label={(open ? "Collapse " : "Expand ") + title} aria-describedby={bodyId + "-summary"}
        onClick={() => setExpanded(!open)}>
        {open ? <ChevronDown size={16} /> : <ChevronRight size={16} />}
        <span className="playlist-group-icon" aria-hidden="true"><ListVideo size={17} /></span>
        <span className="playlist-group-details">
          <span className="playlist-group-title" title={title}>{title}</span>
          <span id={bodyId + "-summary"} className="playlist-group-summary tabular">{summary}</span>
        </span>
      </button>
      {folder && <span className="badge playlist-group-folder" title={directory}>{folder}</span>}
      {onAction && <div className="playlist-group-actions">
        {paused ? <button type="button" className="btn-ghost" onClick={() => onAction("resume_playlist", playlistId)}>
          <Play size={12} />Resume All
        </button> : (current || queued > 0) && <button type="button" className="btn-ghost"
          onClick={() => onAction("pause_playlist", playlistId)}><Pause size={12} />Pause All</button>}
        <button type="button" className="btn-ghost" onClick={() => onAction("reveal_playlist", playlistId)}>
          <Folder size={14} />Open Folder
        </button>
      </div>}
    </header>
    <div id={bodyId} className="playlist-group-body" hidden={!open}>{open && items.map(renderRow)}</div>
  </section>;
}

function Section({ title, meta, children }: { title: string; meta?: string; children: ReactNode }) {
  return <section aria-label={title}><div className="section-head"><h2 className="section-title">{title}</h2>
    {meta && <span className="section-meta tabular">{meta}</span>}</div>{children}</section>;
}

export function TransferList({ items, selected, setSelected, onAction, onPlaylistAction, allTransfers, view, preview, loading }: {
  items: TransferSnapshot[]; selected: string | undefined; setSelected: (id: string) => void; onAction: OnTransferAction;
  onPlaylistAction?: OnPlaylistAction; allTransfers: TransferSnapshot[]; view: View; preview: boolean; loading: boolean;
}) {
  const groups = groupTransfers(items);
  const playlists = new Map(groupTransfers(allTransfers).map(group => [group.playlistId, group.transfers]));
  const members = (group: TransferGroup) => (group.playlistId && playlists.get(group.playlistId)) || group.transfers;
  const finished = (group: TransferGroup) => group.playlistId
    ? isPlaylistCompleted(members(group)) : group.transfers.every(isDone);
  const active = groups.filter(group => !finished(group));
  const complete = groups.filter(finished);
  const running = items.filter(isRunning);
  const paused = items.filter(transfer => transfer.status === "paused").length;
  const peers = running.reduce((sum, transfer) => sum + transfer.active_peers, 0);
  const threads = running.reduce((sum, transfer) => sum + (transfer.kind === "torrent" ? 0 : transfer.active_connections), 0);
  const activeMeta = [running.length + " downloading", paused + " paused", threads + " HTTP threads" + (peers ? " + " + peers + " peers" : "")].join(" · ");
  const queueGroups = new Map<string, TransferGroup[]>();
  if (view === "queues" || view === "schedule") {
    for (const group of active) {
      const id = group.transfers[0].queue_id;
      const queue = queueGroups.get(id) ?? [];
      queue.push(group);
      queueGroups.set(id, queue);
    }
  }
  const renderRow = (transfer: TransferSnapshot) =>
    <TransferRow key={transfer.id} transfer={transfer} selected={transfer.id === selected} select={() => setSelected(transfer.id)}
      onAction={onAction} preview={preview} />;
  const renderGroup = (group: TransferGroup, completed = false) => {
    const { playlistId, transfers } = group;
    if (playlistId) return <PlaylistGroup key={"playlist:" + playlistId} playlistId={playlistId} transfers={transfers}
      onAction={onPlaylistAction} members={members(group)} renderRow={renderRow} />;
    const [transfer] = transfers;
    return completed
      ? <CompletedRow key={transfer.id} transfer={transfer} select={() => setSelected(transfer.id)} onAction={onAction} />
      : renderRow(transfer);
  };
  const rows = (entries: TransferGroup[]) => <div className="transfer-list">{entries.map(group => renderGroup(group))}</div>;
  const today = new Date().toDateString();
  const completedToday = (group: TransferGroup) =>
    new Date(Math.max(...members(group).map(transfer => Date.parse(transfer.updated_at)))).toDateString() === today;
  const recent = complete.filter(completedToday);
  const older = complete.filter(group => !completedToday(group));
  const completedRows = (title: string, entries: TransferGroup[]) => {
    if (!entries.length) return null;
    const transfers = entries.flatMap(group => group.transfers);
    return <Section title={title} meta={transfers.length + (transfers.length === 1 ? " file" : " files") + " · "
      + bytes(transfers.reduce((sum, transfer) => sum + (transfer.total_bytes ?? transfer.downloaded_bytes), 0))}>
      <div className="completed-list">{entries.map(group => renderGroup(group, true))}</div>
    </Section>;
  };

  return <main className="transfer-pane" aria-label="Transfers" aria-busy={loading}>
    {view === "queues" || view === "schedule"
      ? <Section title={view === "queues" ? "Queues" : "Night Schedule"} meta={queueGroups.size + " queues"}>
        {Array.from(queueGroups, ([id, transfers]) => <div key={id} className="queue-group">
          <h3>{id === "default" ? "Default queue" : id === "night" ? "Night queue" : id}</h3>{rows(transfers)}
        </div>)}
        {queueGroups.size === 0 && <p className="empty-state">{view === "schedule" ? "No transfers in a night queue." : "No queued transfers."}</p>}
      </Section>
      : active.length > 0 && <Section title="Active Transfers" meta={activeMeta}>{rows(active)}</Section>}
    {completedRows("Completed Today", recent)}
    {completedRows(recent.length ? "Earlier Downloads" : "Completed Downloads", older)}
    {!items.length && view !== "queues" && view !== "schedule" &&
      <p className="empty-state">{loading ? "Loading downloads…" : "No matching downloads."}</p>}
  </main>;
}
