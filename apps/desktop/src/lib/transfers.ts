import type { TransferSnapshot, TransferStatus } from "../types/ipc";

export const SEGMENT_COUNT = 16;
export const MB = 1_000_000;
export const SPEED_CAP = 50 * MB;

export const statusLabel: Record<TransferStatus, string> = {
  queued: "Queued", probing: "Probing", active: "Downloading", remuxing: "Remuxing",
  paused: "Paused", seeding: "Seeding", completed: "Completed", failed: "Failed", canceled: "Canceled",
};

export function isRunning(transfer: TransferSnapshot) {
  return ["active", "probing", "remuxing", "seeding"].includes(transfer.status);
}

export function isDone(transfer: TransferSnapshot) {
  return transfer.status === "completed";
}

export function bytes(value: number | null, mbDecimals = 1) {
  if (value === null) return "—";
  if (value >= 1e12) return (value / 1e12).toFixed(1) + " TB";
  if (value >= 1e9) return (value / 1e9).toFixed(2) + " GB";
  if (value >= MB) return (value / MB).toFixed(mbDecimals) + " MB";
  if (value >= 1_000) return (value / 1_000).toFixed(1) + " KB";
  return value + " B";
}

export function speed(value: number) {
  return (value / MB).toFixed(1) + " MB/s";
}

export function clock(seconds: number) {
  const total = Math.max(0, Math.floor(seconds));
  const minutes = Math.floor(total / 60);
  return (minutes >= 60 ? Math.floor(minutes / 60) + ":" : "")
    + String(minutes >= 60 ? minutes % 60 : minutes).padStart(2, "0")
    + ":" + String(total % 60).padStart(2, "0");
}

export function ratio(value: number) {
  return Math.min(1, Math.max(0, Number.isFinite(value) ? value : 0));
}

export function percentage(transfer: TransferSnapshot) {
  if (isDone(transfer)) return 100;
  return transfer.total_bytes ? Math.round(ratio(transfer.downloaded_bytes / transfer.total_bytes) * 100) : 0;
}

export function bytePair(transfer: TransferSnapshot) {
  const { downloaded_bytes: downloaded, total_bytes: total } = transfer;
  if (total === null) return bytes(downloaded);
  if (total >= 1e9) return (downloaded / 1e9).toFixed(2) + " / " + bytes(total);
  if (total >= MB) return Number((downloaded / MB).toFixed(1)) + " / " + bytes(total, 0);
  return bytes(downloaded) + " / " + bytes(total);
}

export function isHttpUrl(input: string) {
  try {
    return /^https?:\/\//i.test(input) && /^https?:$/.test(new URL(input).protocol);
  } catch {
    return false;
  }
}

export function isTorrentSource(input: string) {
  return /^magnet:\?/i.test(input) || /\.torrent$/i.test(input);
}

export function isWildcard(input: string) {
  const withoutIpv6Host = input.replace(/^(https?:\/\/(?:[^/@]+@)?)\[[\da-f:.]+\]/i, "$1localhost");
  return /\[(?:\d+-\d+|[a-z]-[a-z]|[A-Z]-[A-Z])(?::\d+)?\]/.test(withoutIpv6Host);
}

export function isDownloadSource(input: string) {
  return isHttpUrl(input) || isTorrentSource(input) || (/^https?:\/\//i.test(input) && isWildcard(input));
}

export function fileCategory(transfer: TransferSnapshot) {
  const extension = transfer.filename.split(".").at(-1)?.toLowerCase() ?? "";
  if (transfer.kind === "torrent") return "torrents";
  if (["mp3", "flac", "wav", "m4a", "aac", "ogg", "opus"].includes(extension)) return "audio";
  if (transfer.kind === "media" || ["mp4", "mkv", "webm", "mov", "avi"].includes(extension)) return "videos";
  return "archives";
}

export function sourceName(source: string) {
  try {
    return new URL(source).hostname || source;
  } catch {
    return source;
  }
}

export function errorMessage(error: unknown) {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (error && typeof error === "object" && "message" in error && typeof error.message === "string") {
    return error.message;
  }
  return "The operation could not be completed.";
}

export function parentFolder(path: string) {
  return path.split(/[\\/]/).filter(Boolean).slice(-2).join(path.includes("\\") ? "\\" : "/");
}
