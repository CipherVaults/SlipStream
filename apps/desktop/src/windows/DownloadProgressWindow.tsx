import { ChevronDown, ChevronUp, Download, Pause, Play, X } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { desktop } from "../lib/tauri";
import { bytes, clock, errorMessage, isDone, isRunning, percentage, ratio } from "../lib/transfers";
import type { SegmentSnapshot, TelemetryBatch, TransferCompletionOptions, TransferSnapshot } from "../types/ipc";
import { DialogTitlebar, useDialogTheme } from "./dialogChrome";

type ProgressTab = "status" | "limiter" | "completion";
type SpeedUnit = "KB/s" | "MB/s";

function formatTransferRate(bps: number): string {
  if (bps <= 0) return "0.0 KB/s";
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(2)} MB/s`;
  return `${(bps / 1_000).toFixed(1)} KB/s`;
}

function formatDownloaded(downloaded: number, total: number | null): string {
  const amount = downloaded >= 1_000_000
    ? `${(downloaded / 1_000_000).toFixed(3)} MB`
    : bytes(downloaded, 2);
  if (total && total > 0) {
    const pct = Math.min(100, Math.max(0, (downloaded / total) * 100));
    return `${amount} (${pct.toFixed(2)} %)`;
  }
  return amount;
}

function progressStatusText(transfer: TransferSnapshot): string {
  switch (transfer.status) {
    case "active": return "Receiving data...";
    case "probing":
    case "queued": return "Connecting...";
    case "remuxing": return "Remuxing...";
    case "paused": return "Paused";
    case "seeding": return "Seeding...";
    case "completed": return "Completed";
    case "failed": return transfer.error_message ? `Failed (${transfer.error_message})` : "Failed";
    case "canceled": return "Canceled";
  }
}

function segmentInfoText(segment: SegmentSnapshot): string {
  switch (segment.state) {
    case "active":
    case "split":
      return segment.speed_bps > 0 ? `Receiving data... (${formatTransferRate(segment.speed_bps)})` : "Receiving data...";
    case "completed": return "Completed";
    case "paused": return "Paused";
    case "error": return "Error";
    case "pending": return "Connecting...";
  }
}

export function DownloadProgressWindow({ transferId }: { transferId: string }) {
  useDialogTheme();
  const tabId = useId();
  const [transfer, setTransfer] = useState<TransferSnapshot | null>(null);
  const [tab, setTab] = useState<ProgressTab>("status");
  const [expanded, setExpanded] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);

  // Speed Limiter state
  const [limiterEnabled, setLimiterEnabled] = useState(false);
  const [limiterValue, setLimiterValue] = useState("500");
  const [limiterUnit, setLimiterUnit] = useState<SpeedUnit>("KB/s");
  const [limiterDirty, setLimiterDirty] = useState(false);

  // Completion options state
  const [completionOptions, setCompletionOptions] = useState<TransferCompletionOptions>({
    show_complete_dialog: true,
    auto_close_progress: false,
    open_folder_on_complete: false,
  });
  const completionOptionsRef = useRef(completionOptions);
  const [completionLoaded, setCompletionLoaded] = useState(false);

  useEffect(() => {
    let active = true;
    const applyBatch = (batch: TelemetryBatch) => {
      if (!active) return;
      const found = batch.transfers.find(item => item.id === transferId);
      if (found) setTransfer(found);
    };

    let unlisten: (() => void | Promise<void>) | undefined;
    void desktop.onTelemetry(applyBatch).then(stop => {
      if (active) unlisten = stop;
      else void Promise.resolve(stop()).catch(() => {});
    }).catch(() => {});

    void desktop.getSnapshot().then(applyBatch).catch(err => {
      if (active) setError(errorMessage(err));
    });

    void desktop.getTransferCompletionOptions(transferId).then(opts => {
      if (active) {
        completionOptionsRef.current = opts;
        setCompletionOptions(opts);
        setCompletionLoaded(true);
      }
    }).catch(() => {
      if (active) setCompletionLoaded(true);
    });

    return () => {
      active = false;
      if (unlisten) void Promise.resolve(unlisten()).catch(() => {});
    };
  }, [transferId]);

  useEffect(() => {
    if (!transfer || limiterDirty) return;
    const bps = transfer.speed_limit_bps ?? null;
    if (bps && bps > 0) {
      setLimiterEnabled(true);
      if (bps >= 1_000_000 && bps % 100_000 === 0) {
        setLimiterUnit("MB/s");
        setLimiterValue(String(bps / 1_000_000));
      } else {
        setLimiterUnit("KB/s");
        setLimiterValue(String(bps / 1_000));
      }
    } else {
      setLimiterEnabled(false);
    }
  }, [transfer, limiterDirty]);

  const pct = transfer ? percentage(transfer) : 0;
  const titleText = transfer ? `${pct}% ${transfer.filename}` : "Download Progress";

  useEffect(() => {
    void desktop.setWindowTitle(titleText);
  }, [titleText]);

  async function toggleDetails() {
    const next = !expanded;
    setExpanded(next);
    await desktop.setWindowSize(540, next ? 515 : 335);
  }

  async function handlePauseResume() {
    if (!transfer || busy) return;
    setBusy(true);
    setError("");
    try {
      if (transfer.status === "paused") {
        await desktop.resumeTransfer(transfer.id);
      } else {
        await desktop.pauseTransfer(transfer.id);
      }
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleCancel() {
    if (busy) return;
    if (!transfer || isDone(transfer) || transfer.status === "canceled") {
      await desktop.close();
      return;
    }
    setBusy(true);
    setError("");
    try {
      await desktop.cancelTransfer(transfer.id, false);
      await desktop.close();
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  }

  async function handleApplySpeedLimit() {
    if (busy || !transfer) return;
    setBusy(true);
    setError("");
    setNotice("");
    try {
      let limitBps: number | null = null;
      if (limiterEnabled) {
        const numeric = Number(limiterValue);
        if (!Number.isFinite(numeric) || numeric <= 0) {
          setError("Enter a positive speed limit.");
          setBusy(false);
          return;
        }
        limitBps = Math.round(numeric * (limiterUnit === "MB/s" ? 1_000_000 : 1_000));
      }
      await desktop.setTransferSpeedLimit(transferId, limitBps);
      setLimiterDirty(false);
      setNotice(limitBps === null ? "Speed limit removed." : `Speed limit set to ${limiterValue} ${limiterUnit}.`);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  async function updateCompletionOption<K extends keyof TransferCompletionOptions>(
    key: K,
    value: TransferCompletionOptions[K],
  ) {
    if (!completionLoaded) return;
    const next = { ...completionOptionsRef.current, [key]: value };
    completionOptionsRef.current = next;
    setCompletionOptions(next);
    setError("");
    try {
      await desktop.setTransferCompletionOptions(transferId, next);
    } catch (err) {
      setError(errorMessage(err));
    }
  }

  const tabs: ReadonlyArray<readonly [ProgressTab, string]> = [
    ["status", "Download status"],
    ["limiter", "Speed Limiter"],
    ["completion", "Options on completion"],
  ];

  const canPause = transfer ? (isRunning(transfer) || transfer.status === "queued") : false;
  const canResume = transfer?.status === "paused";
  const serverSummary = transfer
    ? [
        transfer.source_domain,
        transfer.protocol_label,
        transfer.connect_rtt_ms != null ? `${transfer.connect_rtt_ms} ms` : "",
      ].filter(Boolean).join(" · ")
    : "—";

  return (
    <div className="float-window">
      <DialogTitlebar title={titleText} icon={<Download size={14} />} />

      <div className="float-tabs" role="tablist" aria-label="Download progress sections">
        {tabs.map(([key, label]) => (
          <button
            key={key}
            id={`${tabId}-${key}`}
            type="button"
            role="tab"
            aria-selected={tab === key}
            aria-controls={`${tabId}-panel`}
            className={"float-tab" + (tab === key ? " active" : "")}
            onClick={() => setTab(key)}
          >
            {label}
          </button>
        ))}
      </div>

      <main className="float-body float-progress-body" role="tabpanel" id={`${tabId}-panel`} aria-labelledby={`${tabId}-${tab}`}>
        {!transfer ? (
          <p className="float-muted" role="status">Connecting to transfer…</p>
        ) : (
          <>
            {tab === "status" && (
              <div className="float-status-pane">
                <p className="float-url-line float-mono" title={transfer.source_url}>{transfer.source_url}</p>

                <dl className="float-kv-grid tabular">
                  <dt>Status</dt>
                  <dd className={"float-status-val " + transfer.status}>{progressStatusText(transfer)}</dd>
                  <dt>File size</dt>
                  <dd>{transfer.total_bytes !== null ? bytes(transfer.total_bytes, 2) : "Unknown"}</dd>
                  <dt>Downloaded</dt>
                  <dd>{formatDownloaded(transfer.downloaded_bytes, transfer.total_bytes)}</dd>
                  <dt>Transfer rate</dt>
                  <dd>{formatTransferRate(transfer.download_speed_bps)}</dd>
                  <dt>Time left</dt>
                  <dd>{transfer.eta_seconds !== null && isRunning(transfer) ? clock(transfer.eta_seconds) : "—"}</dd>
                  <dt>Resume capability</dt>
                  <dd>{transfer.resumable ? "Yes" : "No"}</dd>
                  <dt>Server</dt>
                  <dd title={serverSummary}>{serverSummary}</dd>
                </dl>

                <div
                  className="float-overall-bar"
                  role="progressbar"
                  aria-label="Overall download progress"
                  aria-valuemin={0}
                  aria-valuemax={100}
                  aria-valuenow={pct}
                >
                  <span className="float-overall-fill" style={{ width: `${pct}%` }} />
                </div>

                <div className="float-progress-actions">
                  <button type="button" className="btn-ghost form-button" onClick={() => void toggleDetails()}>
                    {expanded ? <><ChevronUp size={13} />&lt;&lt; Hide details</> : <><ChevronDown size={13} />Show details &gt;&gt;</>}
                  </button>
                  <div className="float-actions-right">
                    {(canPause || canResume) && (
                      <button
                        type="button"
                        className="btn-ghost form-button"
                        disabled={busy}
                        onClick={() => void handlePauseResume()}
                      >
                        {canResume ? <><Play size={12} fill="currentColor" />Resume</> : <><Pause size={12} fill="currentColor" />Pause</>}
                      </button>
                    )}
                    <button
                      type="button"
                      className="btn-ghost form-button"
                      disabled={busy}
                      onClick={() => void handleCancel()}
                    >
                      <X size={13} />
                      {isDone(transfer) ? "Close" : "Cancel"}
                    </button>
                  </div>
                </div>

                {expanded && (
                  <section className="float-details-panel" aria-label="Connection details">
                    <p className="float-details-heading">Start positions and download progress by connections</p>
                    <div className="float-seg-bar" aria-label="Segment progress">
                      {(transfer.segments.length > 0
                        ? transfer.segments.map(seg => {
                            const span = Math.max(1, seg.end_byte - seg.start_byte);
                            return { key: seg.index, fill: ratio(seg.downloaded_bytes / span) };
                          })
                        : transfer.ui_segments.map((val, idx) => ({ key: idx, fill: ratio(val) }))
                      ).map(cell => (
                        <span key={cell.key} className="float-seg-cell">
                          <span className="float-seg-fill" style={{ width: `${cell.fill * 100}%` }} />
                        </span>
                      ))}
                    </div>

                    <div className="float-conn-table-wrap">
                      <table className="float-conn-table tabular">
                        <thead>
                          <tr>
                            <th>N.</th>
                            <th>Downloaded</th>
                            <th>Info</th>
                          </tr>
                        </thead>
                        <tbody>
                          {transfer.segments.length === 0 ? (
                            <tr>
                              <td colSpan={3} className="float-muted">Waiting for connection segments…</td>
                            </tr>
                          ) : (
                            transfer.segments.map(segment => (
                              <tr key={segment.index}>
                                <td>{segment.index + 1}</td>
                                <td>{bytes(segment.downloaded_bytes, 2)}</td>
                                <td>{segmentInfoText(segment)}</td>
                              </tr>
                            ))
                          )}
                        </tbody>
                      </table>
                    </div>
                  </section>
                )}
              </div>
            )}

            {tab === "limiter" && (
              <div className="float-limiter-pane">
                <label className="float-checkbox">
                  <input
                    type="checkbox"
                    checked={limiterEnabled}
                    disabled={busy}
                    onChange={event => {
                      setLimiterDirty(true);
                      setLimiterEnabled(event.target.checked);
                    }}
                  />
                  <span>Use speed limiter</span>
                </label>

                <div className="float-limiter-row">
                  <label className="float-field">
                    <span>Maximum download speed</span>
                    <div className="float-inline-row">
                      <input
                        type="number"
                        min={1}
                        step="any"
                        aria-label="Speed limit value"
                        disabled={!limiterEnabled || busy}
                        value={limiterValue}
                        onChange={event => {
                          setLimiterDirty(true);
                          setLimiterValue(event.target.value);
                        }}
                      />
                      <select
                        aria-label="Speed limit unit"
                        disabled={!limiterEnabled || busy}
                        value={limiterUnit}
                        onChange={event => {
                          setLimiterDirty(true);
                          setLimiterUnit(event.target.value as SpeedUnit);
                        }}
                      >
                        <option value="KB/s">KB/s</option>
                        <option value="MB/s">MB/s</option>
                      </select>
                      <button
                        type="button"
                        className="btn-primary form-button"
                        disabled={busy || !transfer}
                        onClick={() => void handleApplySpeedLimit()}
                      >
                        Apply
                      </button>
                    </div>
                  </label>
                </div>

                {notice && <p className="integrity-ok" role="status">{notice}</p>}
              </div>
            )}

            {tab === "completion" && (
              <div className="float-completion-pane">
                <label className="float-checkbox">
                  <input
                    type="checkbox"
                    checked={completionOptions.show_complete_dialog}
                    disabled={!completionLoaded}
                    onChange={event => void updateCompletionOption("show_complete_dialog", event.target.checked)}
                  />
                  <span>Show &ldquo;Download complete&rdquo; dialog when finished</span>
                </label>
                <label className="float-checkbox">
                  <input
                    type="checkbox"
                    checked={completionOptions.auto_close_progress}
                    disabled={!completionLoaded}
                    onChange={event => void updateCompletionOption("auto_close_progress", event.target.checked)}
                  />
                  <span>Close this progress window when finished</span>
                </label>
                <label className="float-checkbox">
                  <input
                    type="checkbox"
                    checked={completionOptions.open_folder_on_complete}
                    disabled={!completionLoaded}
                    onChange={event => void updateCompletionOption("open_folder_on_complete", event.target.checked)}
                  />
                  <span>Open containing folder when finished</span>
                </label>
              </div>
            )}
          </>
        )}

        {error && <p className="inline-error" role="alert">{error}</p>}
      </main>
    </div>
  );
}
