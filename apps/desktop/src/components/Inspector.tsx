import { FileCheck2, Folder, Pause, Play, RefreshCw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import alpinePreview from "../assets/alpine-preview.svg";
import { desktop } from "../lib/tauri";
import { bytes, errorMessage, isDone, isRunning, parentFolder, percentage, ratio, SEGMENT_COUNT, speed, statusLabel } from "../lib/transfers";
import type { MediaFormatOption, MediaProbeResult, SegmentState, TorrentFileEntry, TransferSnapshot } from "../types/ipc";
import { Sparkline } from "./Sparkline";
import { primaryAction, TransferIcon, type OnTransferAction } from "./TransferList";
import { Tabs } from "./ui";

export interface TorrentFilesEntry {
  status: "idle" | "loading" | "ready" | "error";
  files?: TorrentFileEntry[];
  error?: string;
  saving: boolean;
  selectionError: string;
}

const threadStates: Record<SegmentState | "idle", string> = {
  idle: "Idle", pending: "Pending", active: "Active", split: "Split", completed: "Done", paused: "Paused", error: "Error",
};

function Segments({ transfer }: { transfer: TransferSnapshot }) {
  const segments = new Map(transfer.segments.map(segment => [segment.index, segment]));
  const hasExplicit = transfer.segments.length > 0;
  return <div className="insp-content">
    <div className="seg-grid-head tabular"><span>{transfer.kind === "torrent" ? "16-Segment Piece Map" : "16-Thread Dynamic Split Map"}</span>
      <strong className="accent-text">{speed(transfer.download_speed_bps)}</strong></div>
    <div className="seg-grid tabular">
      {Array.from({ length: SEGMENT_COUNT }, (_, index) => {
        const segment = segments.get(index);
        const label = "T" + String(index + 1).padStart(2, "0");
        if ((hasExplicit && !segment) || (!hasExplicit && transfer.kind !== "torrent")) {
          return <div className="thread-tile" key={index}>
            <div className="thread-top"><span>{label}</span><span className="thread-state idle">Idle</span></div>
            <div className="thread-bar idle" role="progressbar" aria-label={label + " progress"}
              aria-valuenow={0}><div style={{ width: "0%" }} /></div>
            <div className="thread-spd">—</div>
          </div>;
        }
        const length = segment ? Math.max(1, segment.end_byte - segment.start_byte) : 0;
        const progress = segment ? ratio(segment.downloaded_bytes / length) : ratio(transfer.ui_segments[index] ?? 0);
        let state: SegmentState = segment?.state ?? (progress === 1 ? "completed" : progress > 0 ? "active" : "pending");
        if (transfer.status === "paused" && state !== "completed") state = "paused";
        if (transfer.status === "failed" && state !== "completed") state = "error";
        const className = state === "completed" ? "done" : state;
        const range = segment ? bytes(segment.start_byte) + " – " + bytes(segment.end_byte) : undefined;

        return <div className="thread-tile" key={index} title={range}>
          <div className="thread-top"><span>{label}</span><span className={"thread-state " + className}>{threadStates[state]}</span></div>
          <div className={"thread-bar " + className} role="progressbar" aria-label={label + " progress"}
            aria-valuenow={Math.round(progress * 100)}><div style={{ width: progress * 100 + "%" }} /></div>
          <div className="thread-spd">{segment && segment.speed_bps > 0 && isRunning(transfer) ? speed(segment.speed_bps)
            : segment ? bytes(segment.downloaded_bytes) : Math.round(progress * 100) + "%"}</div>
        </div>;
      })}
    </div>
  </div>;
}

function Overview({ transfer, samples, preview, onAction }: {
  transfer: TransferSnapshot; samples: readonly number[]; preview: boolean; onAction: OnTransferAction;
}) {
  const average = samples.length ? samples.reduce((sum, sample) => sum + sample, 0) / samples.length : 0;
  const peak = Math.max(0, ...samples);
  const torrent = transfer.kind === "torrent";
  const example = preview && transfer.id === "alpine";
  const threads = Math.max(transfer.active_connections, transfer.segments.length);
  const integrity = transfer.sha256_verified === true ? "SHA-256 Verified"
    : transfer.sha256_verified === false ? "SHA-256 Mismatch" : "SHA-256 Ready";
  const specs = [
    ["Protocol", example ? "HTTP/3 QUIC" : transfer.protocol_label],
    [torrent ? "Peers" : "Threads", torrent ? transfer.active_peers + " Active" : transfer.active_connections + " / " + threads + " Active"],
    ["Integrity", integrity, transfer.sha256_verified === false ? "error-text" : "integrity-ok"],
    ["Save Folder", parentFolder(transfer.save_dir) || "Default"],
  ];

  return <div className="insp-content">
    <div className="kv-grid tabular">
      {specs.map(([label, value, tone]) => <div className="kv-card" key={label}>
        <span className="kv-label">{label}</span><span className={"kv-val " + (tone ?? "")}
          title={label === "Save Folder" ? transfer.save_dir : value}>{value}</span>
      </div>)}
    </div>
    <div className="speed-card">
      <div className="speed-card-top"><span className="speed-card-title">Live Transfer Speed</span>
        <span className="speed-card-nums tabular">Peak {speed(peak).split(" ")[0]} · Avg {speed(average)}</span></div>
      <Sparkline samples={samples} detailed />
    </div>
    {transfer.format_tags.length > 0 && <div className="pill-wrap">
      {(example ? ["4K 2160p60", ...transfer.format_tags.filter(tag => tag !== "MP4").slice(1)] : transfer.format_tags)
        .map((tag, index) => <span className={"badge" + (index === 0 ? " accent" : "")} key={tag + index}>{tag}</span>)}
    </div>}
    {transfer.status === "failed" && <div className="inline-error" role="alert">
      <p>{transfer.error_message || "The transfer failed."}</p>
      {transfer.kind === "http" && <button type="button" className="btn-ghost form-button" onClick={() => onAction("refresh", transfer)}>Refresh Link</button>}
    </div>}
  </div>;
}

type MediaDetailState = { status: "idle" } | { status: "loading" } | { status: "ready"; probe: MediaProbeResult } | { status: "error"; message: string };

function Details({ transfer, preview, torrentEntry, mediaState, onLoadTorrent, onSelectFile, onProbeMedia, onFormat }: {
  transfer: TransferSnapshot; preview: boolean; torrentEntry?: TorrentFilesEntry; mediaState: MediaDetailState;
  onLoadTorrent: () => void; onSelectFile: (index: number, selected: boolean) => void;
  onProbeMedia: () => void; onFormat: (probe: MediaProbeResult, format: MediaFormatOption) => void;
}) {
  if (transfer.kind === "torrent") {
    if (!torrentEntry || torrentEntry.status === "idle" || torrentEntry.status === "loading") {
      return <p className="empty-state" role="status">Loading torrent files…</p>;
    }
    if (torrentEntry.status === "error") {
      return <div className="inline-error"><p role="alert">{torrentEntry.error || "Could not load torrent files."}</p>
        <button type="button" className="btn-ghost form-button" onClick={onLoadTorrent}>Retry</button></div>;
    }
    const files = torrentEntry.files ?? [];
    return <div className="insp-content">
      <div className="seg-grid-head tabular"><span>Select files to download</span>
        <span>{files.filter(file => file.selected).length} / {files.length}</span></div>
      <div className="mini-format-list">
        {files.map(file => <label className="mini-format-row file-option" key={file.index}>
          <input type="checkbox" checked={file.selected} disabled={torrentEntry.saving}
            onChange={event => onSelectFile(file.index, event.target.checked)} />
          <span title={file.relative_path}>{file.relative_path}</span><small>{bytes(file.size_bytes)}</small>
        </label>)}
        {!files.length && <p className="empty-state">File metadata is unavailable.</p>}
      </div>
      {torrentEntry.selectionError && <p className="inline-error" role="alert">{torrentEntry.selectionError}</p>}
    </div>;
  }

  if (mediaState.status === "idle") {
    return <div className="insp-content">
      <div className="seg-grid-head"><span>Direct HTTP Transfer</span><span>{transfer.protocol_label}</span></div>
      <p className="empty-state">Probe this URL with yt-dlp if it points to a media stream.</p>
      <button type="button" className="btn-ghost form-button" onClick={onProbeMedia}>Probe Media Streams</button>
    </div>;
  }
  if (mediaState.status === "loading") return <p className="empty-state" role="status">Probing available streams…</p>;
  if (mediaState.status === "error") return <div className="inline-error"><p role="alert">{mediaState.message}</p>
    <button type="button" className="btn-ghost form-button" onClick={onProbeMedia}>Retry</button></div>;

  const probe = mediaState.probe;
  return <div className="insp-content">
    <div className="seg-grid-head"><span>Available Streams & Presets</span><span>yt-dlp + FFmpeg</span></div>
    <div className="mini-format-list tabular">
      {probe.formats.map((format, index) => <button className={"mini-format-row" + (preview && index === 0 ? " active" : "")}
        type="button" onClick={() => onFormat(probe, format)} title={"Download " + format.label} key={format.format_id}>
        <span><strong>{format.label}</strong> · {format.video_codec ?? format.audio_codec ?? format.container}</span>
        <span>{bytes(format.filesize_bytes)}</span>
      </button>)}
      {!probe.formats.length && <p className="empty-state">No media formats detected.</p>}
    </div>
  </div>;
}

type InspectorTab = "overview" | "segments" | "details";

export function Inspector({ transfer, samples, preview, torrentEntry, onLoadTorrentFiles, onSelectTorrentFile, onAction, onFormat }: {
  transfer: TransferSnapshot | undefined; samples: readonly number[]; preview: boolean;
  torrentEntry?: TorrentFilesEntry; onLoadTorrentFiles: (transferId: string, force?: boolean) => void;
  onSelectTorrentFile: (transferId: string, index: number, selected: boolean) => void;
  onAction: OnTransferAction; onFormat: (probe: MediaProbeResult, format: MediaFormatOption) => void;
}) {
  const [tab, setTab] = useState<InspectorTab>("overview");
  const [thumbnail, setThumbnail] = useState<string | null>(null);
  const [mediaState, setMediaState] = useState<MediaDetailState>({ status: "idle" });
  const mediaRequestToken = useRef(0);
  const mediaRequestedUrl = useRef<string | null>(null);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      mediaRequestToken.current++;
    };
  }, []);

  const transferId = transfer?.id;
  const transferKind = transfer?.kind;
  const transferUrl = transfer?.source_url;

  useEffect(() => {
    mediaRequestToken.current++;
    mediaRequestedUrl.current = null;
    setThumbnail(null);
    setMediaState({ status: "idle" });
  }, [transferId, transferUrl]);

  function probeCurrentMedia(force = false) {
    if (!transferUrl || transferKind === "torrent") return;
    if (!force && mediaRequestedUrl.current === transferUrl) return;
    mediaRequestedUrl.current = transferUrl;
    const token = ++mediaRequestToken.current;
    setMediaState({ status: "loading" });
    void desktop.probeMedia(transferUrl).then(probe => {
      if (!mountedRef.current || token !== mediaRequestToken.current) return;
      setThumbnail(probe.thumbnail_url);
      setMediaState({ status: "ready", probe });
    }, error => {
      if (!mountedRef.current || token !== mediaRequestToken.current) return;
      mediaRequestedUrl.current = null;
      setMediaState({ status: "error", message: errorMessage(error) });
    });
  }

  const prevTabRef = useRef<InspectorTab>(tab);
  const requestTotalBytesRef = useRef<{ transferId: string; totalBytes: number | null } | null>(null);

  const requestTorrentFiles = (force: boolean) => {
    if (!transferId) return;
    requestTotalBytesRef.current = { transferId, totalBytes: transfer?.total_bytes ?? null };
    onLoadTorrentFiles(transferId, force);
  };

  useEffect(() => {
    const enteringDetails = tab === "details" && prevTabRef.current !== "details";
    prevTabRef.current = tab;
    if (tab !== "details" || !transferId || !transferKind) return;
    if (transferKind === "torrent") {
      const currentBytes = transfer?.total_bytes ?? null;
      const switchedTorrent = requestTotalBytesRef.current?.transferId !== transferId;
      const metadataChangedSinceRequest = requestTotalBytesRef.current !== null
        && requestTotalBytesRef.current.transferId === transferId
        && requestTotalBytesRef.current.totalBytes !== currentBytes;
      if (!torrentEntry || torrentEntry.status === "idle") {
        requestTorrentFiles(false);
      } else if (torrentEntry.status === "error" && (enteringDetails || switchedTorrent || metadataChangedSinceRequest)) {
        requestTorrentFiles(true);
      }
      return;
    }
    if (transferKind === "media") {
      probeCurrentMedia(false);
    }
  }, [tab, transferId, transferKind, transferUrl, transfer?.total_bytes, torrentEntry?.status, onLoadTorrentFiles]);

  if (!transfer) return <aside className="inspector" aria-label="Transfer inspector"><p className="empty-state">Select a transfer to inspect.</p></aside>;
  const torrent = transfer.kind === "torrent";
  const primary = primaryAction(transfer);
  const sourcePreview = thumbnail?.startsWith("https://") ? thumbnail
    : preview && transfer.id === "alpine" ? alpinePreview : null;
  const subtitle = transfer.status === "paused"
    ? "Paused at " + percentage(transfer) + "%" + (transfer.resumable ? " · Resumable" : "")
    : torrent ? transfer.active_peers + " Peers"
      : [transfer.active_connections + " Connections", transfer.protocol_label].join(" · ");

  return <aside className="inspector" aria-label="Transfer inspector">
    <div className="insp-preview">{sourcePreview ? <img src={sourcePreview} alt="" onError={() => setThumbnail(null)} />
      : <TransferIcon transfer={transfer} size={42} />}</div>
    <div className="insp-title-block">
      <h2 className="insp-title" title={transfer.filename}>{transfer.filename}</h2>
      <p className="insp-sub tabular">{bytes(transfer.total_bytes)} · {subtitle}</p>
    </div>
    <Tabs<InspectorTab> label="Inspector details" value={tab} onChange={setTab} className="insp-tabs" buttonClass="insp-tab"
      options={[["overview", "Overview"], ["segments", "Segments"], ["details", torrent ? "Torrent Files" : "Media"]]}>
      {tab === "overview" && <Overview transfer={transfer} samples={samples} preview={preview} onAction={onAction} />}
      {tab === "segments" && <Segments transfer={transfer} />}
      {tab === "details" && <Details transfer={transfer} preview={preview} torrentEntry={torrentEntry} mediaState={mediaState}
        onLoadTorrent={() => requestTorrentFiles(true)}
        onSelectFile={(index, checked) => onSelectTorrentFile(transfer.id, index, checked)}
        onProbeMedia={() => probeCurrentMedia(true)} onFormat={onFormat} />}
    </Tabs>
    <div className="insp-footer">
      <div className="insp-status-line">
        <span>{torrent ? statusLabel[transfer.status] : "Dynamic Split: " + (isRunning(transfer) ? "Active" : statusLabel[transfer.status])}</span>
        <span>{transfer.sha256_verified ? "SHA-256 Verified" : preview && transfer.id === "alpine" ? "ETag Verified" : transfer.resumable ? "Resumable" : ""}</span>
      </div>
      <div className="insp-btn-row">
        {primary && <button type="button" className="btn-ghost" onClick={() => onAction(primary, transfer)}>
          {primary === "pause" ? <><Pause size={12} fill="currentColor" />Pause</> : <><Play size={12} fill="currentColor" />Resume</>}
        </button>}
        <button type="button" className="btn-ghost" onClick={() => onAction("reveal", transfer)}><Folder size={13} />Folder</button>
      </div>
      <div className="insp-secondary-actions">
        {transfer.kind === "http" && !isDone(transfer) && <button type="button" className="footer-btn" onClick={() => onAction("refresh", transfer)}><RefreshCw size={12} />Refresh URL</button>}
        <button type="button" className="footer-btn" onClick={() => onAction("verify", transfer)}><FileCheck2 size={13} />Verify SHA-256</button>
      </div>
    </div>
  </aside>;
}
