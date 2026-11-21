import { FileCheck2, RefreshCw } from "lucide-react";
import { useRef, useState, type FormEvent } from "react";
import { desktop } from "../lib/tauri";
import { errorMessage, isHttpUrl } from "../lib/transfers";
import type { TransferSnapshot } from "../types/ipc";
import { Dialog } from "./ui";

export function TransferDialog({ mode, transfer, onClose, onBusyChange, onChanged }: {
  mode: "refresh" | "verify"; transfer: TransferSnapshot; onClose: () => void;
  onBusyChange: (busy: boolean) => void; onChanged: (message?: string) => void;
}) {
  const refresh = mode === "refresh";
  const title = refresh ? "Refresh Expired Link" : "Verify SHA-256 Checksum";
  const [value, setValue] = useState("");
  const [referrer, setReferrer] = useState("");
  const [headers, setHeaders] = useState("");
  const [status, setStatus] = useState<boolean | null>(null);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const busyLock = useRef(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busyLock.current) return;
    const input = value.trim();
    setStatus(null);
    setMessage("");
    if (refresh ? !isHttpUrl(input) : !/^[a-f\d]{64}$/i.test(input)) {
      setMessage(refresh ? "Enter a valid HTTP or HTTPS URL." : "Enter a SHA-256 checksum with 64 hexadecimal characters.");
      return;
    }
    if (referrer.trim() && !isHttpUrl(referrer.trim())) {
      setMessage("Enter a valid HTTP or HTTPS referrer.");
      return;
    }
    busyLock.current = true;
    setBusy(true);
    onBusyChange(true);
    try {
      if (refresh) {
        const pairs = headers.split(/\r?\n/).filter(line => line.trim()).map(line => {
          const separator = line.indexOf(":");
          const key = line.slice(0, separator).trim();
          if (separator < 1 || !/^[\w-]+$/.test(key)) throw new Error("Enter one HTTP header per line using Name: value.");
          return [key, line.slice(separator + 1).trim()];
        });
        await desktop.refreshDownloadUrl({ transfer_id: transfer.id, new_url: input, headers: Object.fromEntries(pairs),
          referrer: referrer.trim() || null });
        onChanged("Download link refreshed.");
        onClose();
      } else {
        setStatus(await desktop.verifyTransferChecksum(transfer.id, input));
        onChanged();
      }
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      busyLock.current = false;
      setBusy(false);
      onBusyChange(false);
    }
  }

  return <Dialog title={title} className="compact-sheet" onClose={onClose} busy={busy}
    header={refresh ? <RefreshCw size={18} className="dialog-symbol" /> : <FileCheck2 size={18} className="dialog-symbol" />}>
    <p className="dialog-filename" title={transfer.filename}>{transfer.filename}</p>
    <form className="dialog-form" onSubmit={event => void submit(event)}>
      <fieldset disabled={busy} className="dialog-fields">
        <label className="field">{refresh ? "New URL" : "Expected SHA-256"}
          <input data-initial-focus value={value} required spellCheck={false} autoComplete="off"
            placeholder={refresh ? "https://" : "64 hexadecimal characters"}
            onChange={event => { setValue(event.target.value); setStatus(null); setMessage(""); }} />
        </label>
        {refresh && <details className="optional-fields"><summary>Request options</summary>
          <label className="field">Referrer<input value={referrer} onChange={event => setReferrer(event.target.value)} /></label>
          <label className="field">HTTP headers<textarea rows={3} value={headers} onChange={event => setHeaders(event.target.value)} /></label>
        </details>}
      </fieldset>
      {status !== null && <p role="status" className={status ? "integrity-ok" : "inline-error"}>
        {status ? "SHA-256 checksum matches." : "SHA-256 checksum does not match."}
      </p>}
      {message && <p role="alert" className="inline-error">{message}</p>}
      <div className="sheet-footer align-right"><div className="sheet-foot-btns">
        <button className="btn-ghost form-button" type="button" onClick={onClose} disabled={busy}>{status === null ? "Cancel" : "Done"}</button>
        <button className="btn-primary" type="submit" disabled={busy}>{busy ? (refresh ? "Refreshing…" : "Verifying…")
          : refresh ? "Refresh URL" : "Verify SHA-256"}</button>
      </div></div>
    </form>
  </Dialog>;
}
