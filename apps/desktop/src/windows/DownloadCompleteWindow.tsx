import { Check, CheckCircle2, ExternalLink, FolderOpen } from "lucide-react";
import { useEffect, useState } from "react";
import { desktop } from "../lib/tauri";
import { bytes, errorMessage, folderSeparator } from "../lib/transfers";
import type { TelemetryBatch, TransferSnapshot } from "../types/ipc";
import { DialogTitlebar, useDialogTheme } from "./dialogChrome";

export function DownloadCompleteWindow({ transferId }: { transferId: string }) {
  useDialogTheme();
  const [transfer, setTransfer] = useState<TransferSnapshot | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    void desktop.setWindowTitle("Download complete");
  }, []);

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

    return () => {
      active = false;
      if (unlisten) void Promise.resolve(unlisten()).catch(() => {});
    };
  }, [transferId]);

  async function handleOpenFile() {
    if (busy) return;
    setBusy(true);
    setError("");
    try {
      await desktop.openCompletedFile(transferId);
      await desktop.close();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleOpenFolder() {
    if (busy) return;
    setBusy(true);
    setError("");
    try {
      await desktop.revealInExplorer(transferId);
      await desktop.close();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  const savedPath = transfer
    ? `${transfer.save_dir.replace(/[\\/]+$/, "")}${folderSeparator(transfer.save_dir)}${transfer.filename}`
    : "";

  return (
    <div className="float-window">
      <DialogTitlebar title="Download complete" icon={<CheckCircle2 size={14} />} />

      <main className="float-body float-complete-body">
        {!transfer ? (
          <p className="float-muted" role="status">Loading completed transfer…</p>
        ) : (
          <>
            <div className="float-complete-hero">
              <div className="float-complete-icon" aria-hidden="true">
                <CheckCircle2 size={28} strokeWidth={1.8} />
              </div>
              <div className="float-complete-meta">
                <div className="float-complete-title-row">
                  <strong className="float-complete-name" title={transfer.filename}>{transfer.filename}</strong>
                  {transfer.sha256_verified === true && (
                    <span className="verified-pill"><Check size={10} strokeWidth={2.8} />SHA-256 Verified</span>
                  )}
                </div>
                <span className="float-complete-size tabular">
                  {bytes(transfer.total_bytes ?? transfer.downloaded_bytes, 2)}
                </span>
              </div>
            </div>

            <div className="float-dim-path" title={savedPath}>
              {savedPath}
            </div>
          </>
        )}

        {error && <p className="inline-error" role="alert">{error}</p>}

        <div className="float-actions">
          <button type="button" className="btn-primary form-button" disabled={busy || !transfer} onClick={() => void handleOpenFile()}>
            <ExternalLink size={13} />
            Open
          </button>
          <button type="button" className="btn-ghost form-button" disabled={busy || !transfer} onClick={() => void handleOpenFolder()}>
            <FolderOpen size={13} />
            Open folder
          </button>
          <button type="button" className="btn-ghost form-button" disabled={busy} onClick={() => void desktop.close()}>
            Close
          </button>
        </div>
      </main>
    </div>
  );
}
