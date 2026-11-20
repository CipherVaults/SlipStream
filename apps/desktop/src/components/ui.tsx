import { Check, X } from "lucide-react";
import { useEffect, useId, useRef, type ButtonHTMLAttributes, type ReactNode } from "react";

export function IconButton({ label, children, className = "icon-btn", ...props }:
  ButtonHTMLAttributes<HTMLButtonElement> & { label: string }) {
  return <button type="button" className={className} title={label} aria-label={label} {...props}>{children}</button>;
}

export function Switch({ label, checked, onChange }: { label: string; checked: boolean; onChange: (value: boolean) => void }) {
  return <button type="button" role="switch" aria-checked={checked} className="toggle-item" onClick={() => onChange(!checked)}>
    <span className={checked ? "mac-switch on" : "mac-switch"} aria-hidden="true" />{label}
  </button>;
}

export function Tabs<T extends string>({ label, value, onChange, options, className, buttonClass, children }: {
  label: string;
  value: T;
  onChange: (value: T) => void;
  options: ReadonlyArray<readonly [T, string]>;
  className: string;
  buttonClass: string;
  children: ReactNode;
}) {
  const id = useId();
  return <>
    <div role="tablist" aria-label={label} className={className} onKeyDown={event => {
      const index = options.findIndex(([key]) => key === value);
      const next = { ArrowLeft: (index + options.length - 1) % options.length,
        ArrowRight: (index + 1) % options.length, Home: 0, End: options.length - 1 }[event.key];
      if (next === undefined) return;
      event.preventDefault();
      onChange(options[next][0]);
      event.currentTarget.querySelectorAll("button")[next]?.focus();
    }}>
      {options.map(([key, title]) => <button key={key} id={id + key} type="button" role="tab"
        className={buttonClass + (key === value ? " active" : "")} aria-selected={key === value}
        aria-controls={id + "-panel"} tabIndex={key === value ? 0 : -1} onClick={() => onChange(key)}>{title}</button>)}
    </div>
    <div role="tabpanel" id={id + "-panel"} aria-labelledby={id + value} className="tab-panel">{children}</div>
  </>;
}

export function Dialog({ title, onClose, className = "", header, children, busy = false }: {
  title: string; onClose: () => void; className?: string; header?: ReactNode; children: ReactNode; busy?: boolean;
}) {
  const id = useId();
  const panel = useRef<HTMLElement>(null);
  useEffect(() => {
    const previous = document.activeElement;
    const element = panel.current;
    (element?.querySelector<HTMLElement>("[data-initial-focus]") ?? element)?.focus();
    return () => { if (previous instanceof HTMLElement && previous.isConnected) previous.focus(); };
  }, []);

  return <div className="modal-backdrop open" onMouseDown={event => {
    if (!busy && event.target === event.currentTarget) onClose();
  }}>
    <section className={"sheet-card " + className} role="dialog" aria-modal="true" aria-labelledby={id} tabIndex={-1}
      ref={panel} onKeyDown={event => {
        if (event.key !== "Tab") return;
        const tabbable = Array.from(event.currentTarget.querySelectorAll<HTMLElement>(
          "button:not(:disabled):not([tabindex='-1']), input:not(:disabled):not([type='hidden']), select:not(:disabled), textarea:not(:disabled), summary"
        )).filter(element => !element.closest("details:not([open])") || element.tagName === "SUMMARY");
        const first = tabbable[0];
        const last = tabbable.at(-1);
        if (event.shiftKey && (document.activeElement === first || document.activeElement === panel.current)) {
          event.preventDefault();
          last?.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first?.focus();
        }
      }}>
      <div className="sheet-header">
        {header}
        <h2 className="sheet-title" id={id}>{title}</h2>
        <IconButton label="Close dialog" className="folder-ghost dialog-close" onClick={onClose} disabled={busy}><X size={15} /></IconButton>
      </div>
      {children}
    </section>
  </div>;
}

export function FormatRow({ name, tags = [], size, selected, onSelect }: {
  name: string; tags?: string[]; size?: string; selected: boolean; onSelect: () => void;
}) {
  return <button type="button" className={"format-row" + (selected ? " selected" : "")}
    aria-pressed={selected} onClick={onSelect}>
    <span className="format-left"><span className="radio-check">{selected && <Check size={11} strokeWidth={3} />}</span>
      <span className="format-label">{name}</span></span>
    <span className="format-right">{tags.map((tag, index) => <span className="light-pill" key={`${index}-${tag}`}>{tag}</span>)}
      {size && <span className="format-size">{size}</span>}</span>
  </button>;
}
