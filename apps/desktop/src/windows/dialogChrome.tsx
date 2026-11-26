import { Minus, X } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { DEFAULT_SETTINGS } from "../lib/demo";
import { desktop } from "../lib/tauri";
import type { AccentMode, AppSettings, ThemeMode } from "../types/ipc";

const systemThemeQuery = "(prefers-color-scheme: dark)";

export function useDialogTheme(): { theme: "dark" | "light"; accent: AccentMode; settings: AppSettings } {
  const [settings, setSettings] = useState<AppSettings>(DEFAULT_SETTINGS);
  const [systemDark, setSystemDark] = useState(() => window.matchMedia?.(systemThemeQuery).matches ?? true);

  useEffect(() => {
    let active = true;
    void desktop.getSettings().then(loaded => {
      if (active) setSettings(loaded);
    }).catch(() => {});
    return () => { active = false; };
  }, []);

  const mode: ThemeMode = settings.theme ?? "dark";
  const theme: "dark" | "light" = mode === "system" ? (systemDark ? "dark" : "light") : mode;

  useEffect(() => {
    if (mode !== "system") return;
    const query = window.matchMedia?.(systemThemeQuery);
    if (!query) return;
    const sync = () => setSystemDark(query.matches);
    sync();
    query.addEventListener("change", sync);
    return () => query.removeEventListener("change", sync);
  }, [mode]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.dataset.accent = settings.accent ?? "blue";
  }, [theme, settings.accent]);

  return { theme, accent: settings.accent ?? "blue", settings };
}

export function DialogTitlebar({
  title,
  icon,
  onClose,
}: {
  title: string;
  icon?: ReactNode;
  onClose?: () => void;
}) {
  return (
    <header className="float-titlebar" data-tauri-drag-region="">
      <div className="float-title-left" data-tauri-drag-region="">
        {icon && <span className="float-title-icon" aria-hidden="true">{icon}</span>}
        <span className="float-title-text" title={title} data-tauri-drag-region="">{title}</span>
      </div>
      <div className="float-win-controls" data-tauri-drag-region="false">
        <button
          type="button"
          className="float-win-btn"
          aria-label="Minimize"
          title="Minimize"
          data-tauri-drag-region="false"
          onClick={() => void desktop.minimize()}
        >
          <Minus size={13} />
        </button>
        <button
          type="button"
          className="float-win-btn close"
          aria-label="Close window"
          title="Close window"
          data-tauri-drag-region="false"
          onClick={() => (onClose ? onClose() : void desktop.close())}
        >
          <X size={13} />
        </button>
      </div>
    </header>
  );
}
