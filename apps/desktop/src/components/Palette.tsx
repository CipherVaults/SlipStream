import { ArrowDownToLine, ArrowUpRight, Command, Search } from "lucide-react";
import { useEffect, useState } from "react";
import { isDownloadSource } from "../lib/transfers";
import type { TransferSnapshot } from "../types/ipc";
import { Dialog } from "./ui";

export interface QuickCommand {
  label: string;
  shortcut?: string;
  run: () => void;
}

export function Palette({ transfers, commands, onAdd, onSelect, onFilter, onClose, initial }: {
  transfers: TransferSnapshot[]; commands: QuickCommand[]; onAdd: (source: string) => void;
  onSelect: (id: string) => void; onFilter: (value: string) => void; onClose: () => void; initial: string;
}) {
  const [query, setQuery] = useState(initial);
  const [cursor, setCursor] = useState(0);
  const text = query.trim();
  const needle = text.toLowerCase();
  const results = transfers.filter(transfer => (transfer.filename + " " + transfer.source_domain).toLowerCase().includes(needle)).slice(0, 6);
  const entries = [
    ...(isDownloadSource(text) ? [{ label: "Open download picker", detail: text, run: () => onAdd(text), Icon: ArrowDownToLine }] : []),
    ...(text && results.length ? [{ label: "Filter transfers", detail: text, run: () => onFilter(text), Icon: Search }] : []),
    ...commands.filter(command => command.label.toLowerCase().includes(needle)).map(command => ({ ...command, detail: command.shortcut, Icon: Command })),
    ...results.map(transfer => ({ label: transfer.filename, detail: transfer.source_domain,
      run: () => onSelect(transfer.id), Icon: ArrowUpRight })),
  ];
  useEffect(() => {
    document.getElementById("command-option-" + cursor)?.scrollIntoView?.({ block: "nearest" });
  }, [cursor]);

  return <Dialog title="Command Palette" className="palette-sheet" onClose={onClose} header={<Command className="dialog-symbol" size={19} />}>
    <form className="field-inline palette-search" onSubmit={event => { event.preventDefault(); entries[cursor]?.run(); }}>
      <Search size={16} />
      <input data-initial-focus aria-label="Search commands and transfers" value={query} placeholder="Type a command or paste a link…"
        onChange={event => { setQuery(event.target.value); setCursor(0); }}
        onKeyDown={event => {
          if (!entries.length || (event.key !== "ArrowDown" && event.key !== "ArrowUp")) return;
          event.preventDefault();
          setCursor((cursor + entries.length + (event.key === "ArrowDown" ? 1 : -1)) % entries.length);
        }} />
    </form>
    <div className="palette-results" aria-label="Commands and downloads">
      {entries.map(({ label, detail, run, Icon }, index) => <button type="button" id={"command-option-" + index}
        key={label + index} className={"palette-result" + (index === cursor ? " active" : "")}
        onMouseEnter={() => setCursor(index)} onClick={run}>
        <Icon size={15} /><span className="palette-label">{label}</span>{detail && <span className="palette-detail">{detail}</span>}
      </button>)}
      {!entries.length && <p className="empty-state">No matching commands or downloads.</p>}
    </div>
  </Dialog>;
}
