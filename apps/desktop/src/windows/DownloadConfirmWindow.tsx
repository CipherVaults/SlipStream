import {
  AlertTriangle, Archive, Clock, Download, File, FileText, Film, FolderOpen, Music, Package, Plus,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import { DEFAULT_CATEGORIES } from "../lib/demo";
import { desktop } from "../lib/tauri";
import { bytes, errorMessage, folderSeparator, statusLabel } from "../lib/transfers";
import type { DownloadCategory, PendingDownloadPreview } from "../types/ipc";
import { DialogTitlebar, useDialogTheme } from "./dialogChrome";

type DuplicateChoice = "numbered" | "overwrite" | "existing";

function joinPath(dir: string, filename: string): string {
  const cleanDir = dir.replace(/[\\/]+$/, "");
  if (!cleanDir) return filename;
  const sep = folderSeparator(cleanDir);
  return `${cleanDir}${sep}${filename}`;
}

function splitPath(fullPath: string, fallbackDir: string, fallbackName: string): { saveDir: string; filename: string } {
  const trimmed = fullPath.trim();
  const lastSep = Math.max(trimmed.lastIndexOf("\\"), trimmed.lastIndexOf("/"));
  if (lastSep <= 0) {
    return { saveDir: fallbackDir, filename: trimmed || fallbackName };
  }
  const dirPart = trimmed.slice(0, lastSep);
  const saveDir = /^[A-Za-z]:$/.test(dirPart) ? `${dirPart}\\` : dirPart;
  const filename = trimmed.slice(lastSep + 1).trim() || fallbackName;
  return { saveDir, filename };
}

function resolveCategoryDir(
  category: DownloadCategory,
  baseDir: string,
): string {
  if (category.save_dir?.trim()) return category.save_dir.trim();
  const cleanBase = baseDir.replace(/[\\/]+$/, "");
  if (!cleanBase || category.id === "general") return cleanBase;
  const sep = folderSeparator(cleanBase);
  return `${cleanBase}${sep}${category.name}`;
}

function CategoryIcon({ categoryId }: { categoryId: string }) {
  switch (categoryId) {
    case "programs": return <Package size={32} strokeWidth={1.6} />;
    case "compressed": return <Archive size={32} strokeWidth={1.6} />;
    case "video": return <Film size={32} strokeWidth={1.6} />;
    case "music": return <Music size={32} strokeWidth={1.6} />;
    case "documents": return <FileText size={32} strokeWidth={1.6} />;
    default: return <File size={32} strokeWidth={1.6} />;
  }
}

export function DownloadConfirmWindow({ pendingId }: { pendingId: string }) {
  const { settings } = useDialogTheme();
  const [preview, setPreview] = useState<PendingDownloadPreview | null>(null);
  const [categories, setCategories] = useState<DownloadCategory[]>(DEFAULT_CATEGORIES);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  // Duplicate resolution state
  const [duplicateDismissed, setDuplicateDismissed] = useState(false);
  const [duplicateChoice, setDuplicateChoice] = useState<DuplicateChoice>("numbered");
  const [overwriteExisting, setOverwriteExisting] = useState(false);

  // File info form state
  const [categoryId, setCategoryId] = useState("general");
  const [saveAsPath, setSaveAsPath] = useState("");
  const [rememberPath, setRememberPath] = useState(false);
  const [description, setDescription] = useState("");
  const [addingCategory, setAddingCategory] = useState(false);
  const [newCategoryName, setNewCategoryName] = useState("");
  const [newCategoryExts, setNewCategoryExts] = useState("");

  useEffect(() => {
    if (settings.categories?.length) {
      setCategories(settings.categories);
    }
  }, [settings.categories]);

  useEffect(() => {
    let active = true;
    setLoading(true);
    setError("");
    void desktop.getPendingDownload(pendingId).then(data => {
      if (!active) return;
      setPreview(data);
      setCategoryId(data.suggested_category_id || "general");
      setSaveAsPath(joinPath(data.suggested_save_dir, data.filename));
      setLoading(false);
    }).catch(err => {
      if (!active) return;
      setError(errorMessage(err));
      setLoading(false);
    });
    return () => { active = false; };
  }, [pendingId]);

  const showDuplicate = Boolean(preview?.duplicate && !duplicateDismissed);

  useEffect(() => {
    void desktop.setWindowTitle(showDuplicate ? "Duplicate download link" : "Download File Info");
  }, [showDuplicate]);

  const handleCancel = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      await desktop.cancelPendingDownload(pendingId);
    } catch {
      // Best-effort cleanup before closing.
    } finally {
      await desktop.close();
    }
  }, [busy, pendingId]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        void handleCancel();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [handleCancel]);

  const baseDownloadDir = useMemo(() => {
    if (settings.default_download_dir?.trim()) return settings.default_download_dir.trim();
    if (!preview?.suggested_save_dir) return "";
    const suggested = preview.suggested_save_dir.replace(/[\\/]+$/, "");
    const suggestedCat = categories.find(cat => cat.id === preview.suggested_category_id);
    if (suggestedCat && suggestedCat.id !== "general") {
      const sep = folderSeparator(suggested);
      const suffix = sep + suggestedCat.name;
      if (suggested.toLowerCase().endsWith(suffix.toLowerCase())) {
        return suggested.slice(0, -suffix.length);
      }
    }
    return suggested;
  }, [categories, preview, settings.default_download_dir]);

  const selectedCategory = categories.find(cat => cat.id === categoryId) ?? categories[0];
  const { saveDir: currentDir, filename: currentFilename } = splitPath(
    saveAsPath,
    preview?.suggested_save_dir ?? baseDownloadDir,
    preview?.filename ?? "download",
  );

  function handleCategoryChange(nextId: string) {
    setCategoryId(nextId);
    const nextCat = categories.find(cat => cat.id === nextId);
    if (!nextCat) return;
    const nextDir = resolveCategoryDir(nextCat, baseDownloadDir || currentDir);
    if (nextDir) {
      setSaveAsPath(joinPath(nextDir, currentFilename));
    }
  }

  async function handleBrowseFolder() {
    if (busy) return;
    try {
      const picked = await desktop.pickDownloadFolder(currentDir || null);
      if (picked?.trim()) {
        const dir = picked.trim();
        const fallbackDir = preview?.suggested_save_dir ?? baseDownloadDir;
        const fallbackName = preview?.filename ?? "download";
        setSaveAsPath(current => joinPath(dir, splitPath(current, fallbackDir, fallbackName).filename));
      }
    } catch (err) {
      setError(errorMessage(err));
    }
  }

  async function handleCreateCategory() {
    const name = newCategoryName.trim();
    if (!name || busy) return;
    setBusy(true);
    setError("");
    try {
      const extensions = newCategoryExts
        .split(/[,\s;]+/)
        .map(ext => ext.trim().replace(/^\./, "").toLowerCase())
        .filter(Boolean);
      const created = await desktop.addDownloadCategory(name, extensions, null);
      setCategories(prev => [...prev.filter(cat => cat.id !== created.id), created]);
      setCategoryId(created.id);
      const nextDir = resolveCategoryDir(created, baseDownloadDir || currentDir);
      if (nextDir) setSaveAsPath(joinPath(nextDir, currentFilename));
      setAddingCategory(false);
      setNewCategoryName("");
      setNewCategoryExts("");
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleDuplicateConfirm() {
    if (busy) return;
    if (duplicateChoice === "existing") {
      setBusy(true);
      setError("");
      try {
        await desktop.resolveDuplicateExisting(pendingId);
        await desktop.close();
      } catch (err) {
        setError(errorMessage(err));
        setBusy(false);
      }
      return;
    }
    setOverwriteExisting(duplicateChoice === "overwrite");
    setDuplicateDismissed(true);
  }

  async function submitDownload(startImmediately: boolean) {
    if (!preview || busy) return;
    setBusy(true);
    setError("");
    try {
      const { saveDir, filename } = splitPath(saveAsPath, preview.suggested_save_dir, preview.filename);
      await desktop.confirmPendingDownload({
        pending_id: pendingId,
        filename,
        save_dir: saveDir,
        category_id: categoryId,
        remember_category_path: rememberPath,
        start_immediately: startImmediately,
        overwrite_existing: overwriteExisting,
        description: description.trim() ? description.trim() : null,
      });
      await desktop.close();
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  }

  function handleFormSubmit(event: FormEvent) {
    event.preventDefault();
    void submitDownload(true);
  }

  return (
    <div className="float-window">
      <DialogTitlebar
        title={showDuplicate ? "Duplicate download link" : "Download File Info"}
        icon={showDuplicate ? <AlertTriangle size={14} /> : <Download size={14} />}
        onClose={() => void handleCancel()}
      />

      <main className="float-body">
        {loading && <p className="float-muted" role="status">Loading download details…</p>}
        {!loading && !preview && (
          <div className="float-error-box" role="alert">
            <p>{error || "Pending download is no longer available."}</p>
            <div className="float-actions">
              <button type="button" className="btn-ghost form-button" onClick={() => void desktop.close()}>Close</button>
            </div>
          </div>
        )}

        {!loading && preview && showDuplicate && preview.duplicate && (
          <section className="float-duplicate" aria-label="Duplicate download link">
            <div className="float-dup-header">
              <AlertTriangle size={22} className="float-dup-icon" aria-hidden="true" />
              <div className="float-dup-copy">
                <p className="float-dup-lead">This address is already in the list of downloads:</p>
                <p className="float-dup-file">
                  <strong>{preview.duplicate.filename}</strong>
                  <span className="badge">{statusLabel[preview.duplicate.status]}</span>
                </p>
              </div>
            </div>

            <fieldset className="float-radio-group" disabled={busy}>
              <label className="float-radio">
                <input
                  type="radio"
                  name="duplicate-action"
                  value="numbered"
                  checked={duplicateChoice === "numbered"}
                  onChange={() => setDuplicateChoice("numbered")}
                />
                <span>Add the duplicate with a numbered file name</span>
              </label>
              <label className="float-radio">
                <input
                  type="radio"
                  name="duplicate-action"
                  value="overwrite"
                  checked={duplicateChoice === "overwrite"}
                  onChange={() => setDuplicateChoice("overwrite")}
                />
                <span>Add the duplicate and overwrite the existing file</span>
              </label>
              <label className="float-radio">
                <input
                  type="radio"
                  name="duplicate-action"
                  value="existing"
                  checked={duplicateChoice === "existing"}
                  onChange={() => setDuplicateChoice("existing")}
                />
                <span>If existing file is complete, show download complete dialog; otherwise resume it.</span>
              </label>
            </fieldset>

            {error && <p className="inline-error" role="alert">{error}</p>}

            <div className="float-actions">
              <button type="button" className="btn-primary form-button" disabled={busy} onClick={() => void handleDuplicateConfirm()}>
                OK
              </button>
              <button type="button" className="btn-ghost form-button" disabled={busy} onClick={() => void handleCancel()}>
                Cancel
              </button>
            </div>
          </section>
        )}

        {!loading && preview && !showDuplicate && (
          <form className="float-confirm-form" onSubmit={handleFormSubmit}>
            <div className="float-confirm-grid">
              <div className="float-confirm-fields">
                <label className="float-field">
                  <span>URL</span>
                  <input type="text" readOnly value={preview.url} aria-label="URL" className="float-mono" />
                </label>

                <div className="float-field">
                  <label htmlFor="confirm-category">Category</label>
                  <div className="float-inline-row">
                    <select
                      id="confirm-category"
                      aria-label="Category"
                      value={categoryId}
                      disabled={busy}
                      onChange={event => handleCategoryChange(event.target.value)}
                    >
                      {categories.map(cat => (
                        <option key={cat.id} value={cat.id}>{cat.name}</option>
                      ))}
                    </select>
                    <button
                      type="button"
                      className="btn-ghost float-square-btn"
                      aria-label="Add custom category"
                      title="Add custom category"
                      disabled={busy}
                      onClick={() => setAddingCategory(prev => !prev)}
                    >
                      <Plus size={14} />
                    </button>
                  </div>
                </div>

                {addingCategory && (
                  <div className="float-inline-category">
                    <input
                      type="text"
                      placeholder="Category name"
                      aria-label="New category name"
                      value={newCategoryName}
                      onChange={event => setNewCategoryName(event.target.value)}
                    />
                    <input
                      type="text"
                      placeholder="Extensions (e.g. iso, dmg)"
                      aria-label="New category extensions"
                      value={newCategoryExts}
                      onChange={event => setNewCategoryExts(event.target.value)}
                    />
                    <button
                      type="button"
                      className="btn-primary form-button"
                      disabled={busy || !newCategoryName.trim()}
                      onClick={() => void handleCreateCategory()}
                    >
                      Add
                    </button>
                  </div>
                )}

                <div className="float-field">
                  <label htmlFor="confirm-save-as">Save As</label>
                  <div className="float-inline-row">
                    <input
                      id="confirm-save-as"
                      type="text"
                      aria-label="Save As"
                      value={saveAsPath}
                      disabled={busy}
                      onChange={event => setSaveAsPath(event.target.value)}
                      className="float-mono"
                    />
                    <button
                      type="button"
                      className="btn-ghost float-browse-btn"
                      aria-label="Browse folder"
                      title="Choose folder"
                      disabled={busy}
                      onClick={() => void handleBrowseFolder()}
                    >
                      <FolderOpen size={13} />
                      <span>...</span>
                    </button>
                  </div>
                </div>

                <label className="float-checkbox">
                  <input
                    type="checkbox"
                    checked={rememberPath}
                    disabled={busy}
                    onChange={event => setRememberPath(event.target.checked)}
                  />
                  <span>Remember this path for &ldquo;{selectedCategory?.name ?? "General"}&rdquo; category</span>
                </label>

                <div className="float-dim-path" title={currentDir}>
                  {currentDir || baseDownloadDir}
                </div>

                <label className="float-field">
                  <span>Description</span>
                  <input
                    type="text"
                    aria-label="Description"
                    placeholder="Optional note"
                    value={description}
                    disabled={busy}
                    onChange={event => setDescription(event.target.value)}
                  />
                </label>
              </div>

              <aside className="float-confirm-side" aria-label="File summary">
                <div className="float-file-badge">
                  <CategoryIcon categoryId={categoryId} />
                </div>
                <strong className="float-file-size tabular">
                  {preview.expected_size_bytes !== null && preview.expected_size_bytes > 0
                    ? bytes(preview.expected_size_bytes, 2)
                    : "Unknown size"}
                </strong>
                <span className="float-file-domain" title={preview.source_domain}>{preview.source_domain}</span>
              </aside>
            </div>

            {error && <p className="inline-error" role="alert">{error}</p>}

            <div className="float-actions">
              <button
                type="button"
                className="btn-ghost form-button"
                disabled={busy}
                onClick={() => void submitDownload(false)}
              >
                <Clock size={13} />
                Download Later
              </button>
              <button
                type="submit"
                className="btn-primary form-button"
                disabled={busy}
              >
                <Download size={13} />
                Start Download
              </button>
              <button
                type="button"
                className="btn-ghost form-button"
                disabled={busy}
                onClick={() => void handleCancel()}
              >
                Cancel
              </button>
            </div>
          </form>
        )}
      </main>
    </div>
  );
}
