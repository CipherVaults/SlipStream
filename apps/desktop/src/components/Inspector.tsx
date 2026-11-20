import { FileCheck2, Folder, Pause, Play, RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import alpinePreview from "../assets/alpine-preview.svg";
import { desktop } from "../lib/tauri";
import { bytes, errorMessage, isDone, isRunning, parentFolder, percentage, ratio, SEGMENT_COUNT, speed, statusLabel } from "../lib/transfers";
import type { MediaFormatOption, MediaProbeResult, SegmentState, TorrentFileEntry, TransferSnapshot } from "../types/ipc";
import { Sparkline } from "./Sparkline";
import { primaryAction, TransferIcon, type OnTransferAction } from "./TransferList";
import { Tabs } from "./ui";

const threadStates: Record<SegmentState, string> = {
  pending: "Pending", active: "Active", split: "Split", completed: "Done", paused: "Paused", error: "Error",
};

function Segments({ transfer }: { transfer: TransferSnapshot }) {
  const segments = new Map(transfer.segments.map(segment => [segment.index, segment]));
  return <div className="insp-content">
    <div className="seg-grid-head tabular"><span>{transfer.kind === "torrent" ? "16-Segment Piece Map" : "16-Thread Dynamic Split Map"}</span>
      <strong className="accent-text">{speed(transfer.download_speed_bps)}</strong></div>
    <div className="seg-grid tabular">
      {Array.from({ length: SEGMENT_COUNT }, (_, index) => {
        const segment = segments.get(index);
        const length = segment ? Math.max(1, segment.end_byte - segment.start_byte + 1) : 0;
        const progress = segment ? ratio(segment.downloaded_bytes / length) : ratio(transfer.ui_segments[index] ?? 0);
        let state = segment?.state ?? (progress === 1 ? "completed" : progress > 0 ? "active" : "pending");
        if (transfer.status === "paused" && state !== "completed") state = "paused";
        if (transfer.status === "failed" && state !== "completed") state = "error";
        const className = state === "completed" ? "done" : state;
        const label = "T" + String(index + 1).padStart(2, "0");
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
      {!torrent && <button type="button" className="btn-ghost form-button" onClick={() => onAction("refresh", transfer)}>Refresh Link</button>}
    </div>}
  </div>;
}

type DetailData = { files?: TorrentFileEntry[]; probe?: MediaProbeResult };
type DetailState = { status: "loading" } | { status: "ready"; data: DetailData } | { status: "error"; message: string };

function Details({ transfer, preview, onFormat, onThumbnail }: {
  transfer: TransferSnapshot; preview: boolean; onFormat: (probe: MediaProbeResult, format: MediaFormatOption) => void;
  onThumbnail: (url: string | null) => void;
}) {
  const { id, kind, source_url } = transfer;
  const torrent = kind === "torrent";
  const [state, setState] = useState<DetailState>({ status: "loading" });
  const [retry, setRetry] = useState(0);
  const [saving, setSaving] = useState(false);
  const [selectionError, setSelectionError] = useState("");

  useEffect(() => {
    let current = true;
    setState({ status: "loading" });
    const request: Promise<DetailData> = torrent ? desktop.listTorrentFiles(id).then(files => ({ files }))
      : desktop.probeMedia(source_url).then(probe => ({ probe }));
    request.then(data => {
      if (!current) return;
      setState({ status: "ready", data });
      if (data.probe) onThumbnail(data.probe.thumbnail_url);
    }, error => {
      if (current) setState({ status: "error", message: errorMessage(error) });
    });
    return () => { current = false; };
  }, [id, torrent, source_url, retry, onThumbnail]);

  async function select(files: TorrentFileEntry[], index: number, selected: boolean) {
    if (saving) return;
    setSaving(true);
    setSelectionError("");
    const next = files.map(file => file.index === index ? { ...file, selected } : file);
    try {
      await desktop.setTorrentFiles(id, next.filter(file => file.selected).map(file => file.index));
      setState({ status: "ready", data: { files: next } });
    } catch (error) {
      setSelectionError(errorMessage(error));
    } finally {
      setSaving(false);
    }
  }

  if (state.status === "loading") return <p className="empty-state" role="status">{torrent ? "Loading torrent files…" : "Probing available streams…"}</p>;
  if (state.status === "error") return <div className="inline-error"><p role="alert">{state.message}</p>
    <button type="button" className="btn-ghost form-button" onClick={() => setRetry(retry + 1)}>Retry</button></div>;
  if (state.data.files) {
    const files = state.data.files;
    return <div className="insp-content">
      <div className="seg-grid-head tabular"><span>Select files to download</span>
        <span>{files.filter(file => file.selected).length} / {files.length}</span></div>
      <div className="mini-format-list">
        {files.map(file => <label className="mini-format-row file-option" key={file.index}>
          <input type="checkbox" checked={file.selected} disabled={saving} onChange={event => void select(files, file.index, event.target.checked)} />
          <span title={file.relative_path}>{file.relative_path}</span><small>{bytes(file.size_bytes)}</small>
        </label>)}
        {!files.length && <p className="empty-state">File metadata is unavailable.</p>}
      </div>
      {selectionError && <p className="inline-error" role="alert">{selectionError}</p>}
    </div>;
  }

  const probe = state.data.probe;
  return <div className="insp-content">
    <div className="seg-grid-head"><span>Available Streams & Presets</span><span>yt-dlp + FFmpeg</span></div>
    <div className="mini-format-list tabular">
      {probe?.formats.map((format, index) => <button className={"mini-format-row" + (preview && index === 0 ? " active" : "")}
        type="button" onClick={() => onFormat(probe, format)} title={"Download " + format.label} key={format.format_id}>
        <span><strong>{format.label}</strong> · {format.video_codec ?? format.audio_codec ?? format.container}</span>
        <span>{bytes(format.filesize_bytes)}</span>
      </button>)}
      {!probe?.formats.length && <p className="empty-state">No media formats detected.</p>}
    </div>
  </div>;
}

type InspectorTab = "overview" | "segments" | "details";

export function Inspector({ transfer, samples, preview, onAction, onFormat }: {
  transfer: TransferSnapshot | undefined; samples: readonly number[]; preview: boolean; onAction: OnTransferAction;
  onFormat: (probe: MediaProbeResult, format: MediaFormatOption) => void;
}) {
  const [tab, setTab] = useState<InspectorTab>("overview");
  const [thumbnail, setThumbnail] = useState<string | null>(null);
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
      {tab === "details" && <Details transfer={transfer} preview={preview} onFormat={onFormat} onThumbnail={setThumbnail} />}
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
        {!torrent && !isDone(transfer) && <button type="button" className="footer-btn" onClick={() => onAction("refresh", transfer)}><RefreshCw size={12} />Refresh URL</button>}
        <button type="button" className="footer-btn" onClick={() => onAction("verify", transfer)}><FileCheck2 size={13} />Verify SHA-256</button>
      </div>
    </div>
  </aside>;
}
