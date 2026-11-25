import { version } from "../../package.json";
import { DEFAULT_CHROMIUM_EXTENSION_ID, DEFAULT_FIREFOX_EXTENSION_ID } from "../types/ipc";
import type {
  AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings, BrowserKind,
  BrowserSetupResult, BrowserStatus, MediaFormatOption, MediaProbeResult, PlaylistEntry,
  RefreshDownloadUrlRequest, SegmentSnapshot, TelemetryBatch, ToolchainStatus, TorrentFileEntry, TransferSnapshot, UpdateStatus,
} from "../types/ipc";
import {
  MB, SEGMENT_COUNT, folderSeparator, isHttpUrl, isPurePlaylistUrl, isRunning, parsePlaylistRanges, ratio,
  sanitizeMediaStem, sourceName,
} from "./transfers";

export const DEFAULT_SETTINGS: AppSettings = {
  theme: "dark", accent: "blue", default_download_dir: "", default_connections: 16,
  global_speed_limit_bps: null, max_concurrent_downloads: 4, auto_defender_scan: true,
  close_to_tray_hibernate: true, monitor_clipboard: false, ytdlp_path: null, ffmpeg_path: null,
  update_mode: "auto",
};

const demoFolder = "D:\\Downloads";
const mediaUrl = "https://youtube.com/watch?v=alpine-cinema";
const demoPlaylistTitle = "Cinema Playlist";
const demoPlaylistTitles = [
  ["alpine-cinema", "Alpine Cinema in 4K", 362],
  ["tokyo-architecture", "Tokyo Architecture in 4K HDR", 478],
  ["dolomites-dawn", "Dolomites at Dawn", 216],
  ["lisbon-trams", "Lisbon by Tram", 295],
  ["kyoto-rain", "A Rainy Evening in Kyoto", 548],
] as const;
const demoPlaylistItems: (PlaylistEntry & { url: string })[] = demoPlaylistTitles.map(([id, title, duration_seconds], position) => ({
  index: position + 1, id, title, url: "https://www.youtube.com/watch?v=" + id, duration_seconds,
}));
const pausedPlaylists = new Set<string>();
const timestamp = new Date().toISOString();
const telemetryListeners = new Set<(batch: TelemetryBatch) => void>();
const pickerListeners = new Set<(url: string | null) => void>();
let timer: number | undefined;
let tick = 0;
let sequence = 0;
let lastBatchTime = 0;

function baseTransfer(id: string, filename: string, source_url: string): TransferSnapshot {
  return {
    id, filename, source_url, source_domain: sourceName(source_url), kind: "http", status: "active",
    save_dir: demoFolder, total_bytes: null, downloaded_bytes: 0, uploaded_bytes: 0, download_speed_bps: 0,
    upload_speed_bps: 0, eta_seconds: null, active_connections: 0, active_peers: 0, protocol_label: "HTTP/2",
    format_tags: [], ui_segments: Array<number>(SEGMENT_COUNT).fill(0), segments: [], sha256_verified: null,
    resumable: true, queue_id: "default", error_message: null, created_at: timestamp, updated_at: timestamp,
  };
}

function threadMap(threadSize: number, progress: number[], speeds: number[] = []): SegmentSnapshot[] {
  return Array.from({ length: SEGMENT_COUNT }, (_, index) => {
    const fraction = progress[index] ?? 0;
    const speed = speeds[index - 11] ?? 0;
    return {
      index, start_byte: threadSize * index, end_byte: threadSize * (index + 1),
      downloaded_bytes: Math.round(threadSize * fraction), speed_bps: speed * MB,
      state: fraction === 1 ? "completed" : speed ? (index < 13 ? "split" : "active") : "pending",
    };
  });
}

function done(id: string, filename: string, size: number, hour: number, minute: number, tags: string[]) {
  const updatedAt = new Date();
  updatedAt.setHours(hour, minute, 0, 0);
  return {
    ...baseTransfer(id, filename, "https://example.com/" + filename),
    status: "completed" as const, total_bytes: size * MB, downloaded_bytes: size * MB,
    ui_segments: Array<number>(SEGMENT_COUNT).fill(1), updated_at: updatedAt.toISOString(), format_tags: tags,
    sha256_verified: tags.includes("SHA-256") ? true : null,
  };
}

const alpineProgress = [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, .62, .48, 0, 0, 0];
let transfers: TransferSnapshot[] = [
  {
    ...baseTransfer("alpine", "Alpine_Cinema_4K_60fps.mp4", mediaUrl), kind: "media",
    save_dir: demoFolder + "\\Videos\\4K", total_bytes: 1480 * MB, downloaded_bytes: 1040 * MB,
    download_speed_bps: 48.2 * MB, active_connections: 16, eta_seconds: 9, protocol_label: "HTTP/3",
    format_tags: ["4K 60fps", "AV1", "MP4", "Opus 160k", "Subtitles EN", "Lossless Remux"],
    ui_segments: alpineProgress,
    segments: threadMap(92.5 * MB, [...alpineProgress.slice(0, 11), .74, .62, .48, .41, .35], [8.4, 9.2, 10.6, 10.1, 9.9]),
  },
  {
    ...baseTransfer("ubuntu", "Ubuntu-26.04-desktop-amd64.iso", "https://releases.ubuntu.com/26.04/ubuntu.iso"),
    total_bytes: 5600 * MB, downloaded_bytes: 3840 * MB, download_speed_bps: 31.6 * MB, eta_seconds: 55,
    active_connections: 16, format_tags: ["ISO"], ui_segments: [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, .9, .4, 0, 0, 0, 0],
    segments: threadMap(350 * MB, [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, .9, .4], [31.6]),
  },
  {
    ...baseTransfer("blender", "Blender_5.2_Studio_Assets.torrent", "magnet:?xt=urn:btih:example&dn=Blender"),
    kind: "torrent", total_bytes: 8400 * MB, downloaded_bytes: 6120 * MB, uploaded_bytes: 765 * MB,
    download_speed_bps: 18.6 * MB, upload_speed_bps: 4.2 * MB, eta_seconds: 122, active_peers: 42,
    protocol_label: "BitTorrent", ui_segments: [1, 1, 1, .72, 1, 1, 0, 1, 1, .85, 1, 1, .35, 1, 0, 1],
  },
  {
    ...baseTransfer("symphony", "Symphony_Live_Master.flac", "https://example.com/Symphony_Live_Master.flac"),
    status: "paused", total_bytes: 142 * MB, downloaded_bytes: 68 * MB, format_tags: ["FLAC · 24-bit"],
    ui_segments: [1, 1, 1, 1, 1, 1, 1, .6, 0, 0, 0, 0, 0, 0, 0, 0], queue_id: "night",
  },
  done("design-pdf", "Design_System_Specs_2026.pdf", 18.4, 16, 42, ["SHA-256"]),
  done("podcast", "Podcast_Episode_42_Master.mp3", 48, 14, 15, ["320 kbps", "FFmpeg Tags"]),
  done("ffmpeg", "FFmpeg_9.0_Win64_Static.zip", 124, 11, 8, ["MotW Safe"]),
];

const seededFiles: Array<[string, number, number]> = [
  ["Scenes/Studio_Main.blend", 3600, 3600], ["Textures/Studio_Textures.zip", 2900, 2200],
  ["Lighting/HDRI_Collection.zip", 1800, 284], ["Documentation/Studio_Guide.pdf", 100, 36],
];
const torrentFiles = new Map<string, TorrentFileEntry[]>([["blender", seededFiles.map(([relative_path, size, loaded], index) => ({
  index, relative_path, size_bytes: size * MB, downloaded_bytes: loaded * MB, selected: true,
}))]]);
const rates = new Map(transfers.map(transfer => [transfer.id, [
  transfer.download_speed_bps || 12.8 * MB, transfer.upload_speed_bps,
]] as const));
let settings: AppSettings = { ...DEFAULT_SETTINGS, default_download_dir: demoFolder };
let installed = false;
let managedYtdlp = false;
const registeredHosts = new Map<BrowserKind, string>();
const browsers = {
  chrome: { name: "Google Chrome", installed: true, extension_url: "chrome://extensions/" },
  edge: { name: "Microsoft Edge", installed: true, extension_url: "edge://extensions/" },
  brave: { name: "Brave", installed: false, extension_url: "brave://extensions/" },
  firefox: { name: "Mozilla Firefox", installed: true, extension_url: "about:debugging#/runtime/this-firefox" },
};
const browserIds = ["chrome", "edge", "brave", "firefox"] as const;
const manualBrowserIds = ["chrome", "edge", "firefox"] as const;

function defaultExtensionId(browser: BrowserKind) {
  return browser === "firefox" ? DEFAULT_FIREFOX_EXTENSION_ID : DEFAULT_CHROMIUM_EXTENSION_ID;
}

function extensionDirectory(browser: BrowserKind) {
  return "C:\\SlipStream\\extensions\\" + (browser === "firefox" ? "firefox" : "chromium");
}

function toolchain(): ToolchainStatus {
  return {
    ytdlp_path: settings.ytdlp_path || (managedYtdlp
      ? "C:\\SlipStream\\managed\\yt-dlp.exe" : "C:\\SlipStream\\tools\\yt-dlp.exe"),
    ytdlp_version: managedYtdlp ? "2026.11.22" : "2026.11.17",
    ytdlp_source: settings.ytdlp_path ? "custom" : managedYtdlp ? "managed" : "bundled",
    ffmpeg_path: settings.ffmpeg_path || "C:\\SlipStream\\tools\\ffmpeg.exe",
    ffmpeg_version: "9.0",
    ffmpeg_source: settings.ffmpeg_path ? "custom" : "bundled",
    ffprobe_path: "C:\\SlipStream\\tools\\ffprobe.exe",
    ffprobe_version: "9.0",
    ffprobe_source: "bundled",
    native_hosts_installed: installed,
  };
}

const seededFormats: MediaFormatOption[] = [
  makeFormat("401", "2160p 4K · 60fps", "mp4", 2160, "AV1", "Opus", 1480),
  makeFormat("308", "1440p QHD · 60fps", "mkv", 1440, "VP9", "Opus", 780),
  makeFormat("299", "1080p HD · 60fps", "mp4", 1080, "H.264", "AAC", 390),
  makeFormat("flac", "Audio · Lossless Studio", "flac", null, null, "FLAC", 94),
  makeFormat("mp3", "Audio · High Quality", "mp3", null, null, "MP3", 16),
];

function makeFormat(format_id: string, label: string, container: string, height: number | null,
  video_codec: string | null, audio_codec: string, sizeMB: number): MediaFormatOption {
  return {
    format_id, label, container, height, video_codec, audio_codec, fps: height ? 60 : null,
    filesize_bytes: sizeMB * MB, is_audio_only: height === null, has_video: height !== null, has_audio: true,
  };
}

function fileName(source: string) {
  try {
    const url = new URL(source);
    return url.searchParams.get("dn") || decodeURIComponent(url.pathname.split(/[\\/]/).filter(Boolean).at(-1) ?? "") || "Download";
  } catch {
    return source.split(/[\\/]/).at(-1) || "Download";
  }
}

function snapshot(): TelemetryBatch {
  const active = transfers.filter(isRunning);
  const now = Math.max(Date.now(), lastBatchTime + 1);
  lastBatchTime = now;
  return {
    timestamp: new Date(now).toISOString(),
    transfers: transfers.map(transfer => transfer.playlist_id
      ? { ...transfer, playlist_paused: pausedPlaylists.has(transfer.playlist_id) } : transfer),
    aggregate_down_bps: active.reduce((sum, transfer) => sum + transfer.download_speed_bps, 0),
    aggregate_up_bps: active.reduce((sum, transfer) => sum + transfer.upload_speed_bps, 0),
    active_count: active.length, active_threads: active.reduce((sum, transfer) => sum + transfer.active_connections, 0),
    speed_limit_bps: settings.global_speed_limit_bps,
  };
}

function emit() {
  const batch = snapshot();
  for (const listener of telemetryListeners) listener(batch);
}

function update(id: string, change: (transfer: TransferSnapshot) => Partial<TransferSnapshot>) {
  if (!transfers.some(transfer => transfer.id === id)) throw new Error("Transfer not found.");
  transfers = transfers.map(transfer => transfer.id === id
    ? { ...transfer, ...change(transfer), updated_at: new Date().toISOString() } : transfer);
  emit();
}

function activate(transfer: TransferSnapshot) {
  return { ...transfer, status: "active" as const, active_connections: settings.default_connections,
    error_message: null, updated_at: new Date().toISOString() };
}

function advancePlaylist(playlistId: string, resumePaused = false) {
  const items = transfers.filter(transfer => transfer.playlist_id === playlistId);
  if (pausedPlaylists.has(playlistId) || items.some(isRunning)) return;
  const paused = items.find(item => item.status === "paused");
  if (paused && !resumePaused) return;
  const next = paused ?? items.find(item => item.status === "queued");
  if (next) transfers = transfers.map(transfer => transfer.id === next.id ? activate(transfer) : transfer);
  return Boolean(next);
}

function updateRates(advance: boolean) {
  const completedPlaylists = new Set<string>();
  const uncapped = transfers.filter(item => item.status === "active")
    .reduce((sum, item) => sum + (rates.get(item.id)?.[0] ?? 0), 0);
  const limit = settings.global_speed_limit_bps;
  const scale = limit !== null && uncapped > limit ? limit / uncapped : 1;
  transfers = transfers.map(transfer => {
    if (transfer.status !== "active") return transfer;
    const [down, up] = rates.get(transfer.id) ?? [0, 0];
    const variation = advance ? 1 + Math.sin(tick * .8 + down / MB) * .08 : 1;
    const downloadSpeed = Math.round(down * scale * variation);
    const total = transfer.total_bytes;
    // Keep the sample transfers on screen long enough to explore the preview.
    const downloaded = Math.min(total ?? Infinity, transfer.downloaded_bytes + (advance ? Math.floor(downloadSpeed * .06) : 0));
    const finished = total !== null && downloaded >= total;
    if (finished && transfer.playlist_id) completedPlaylists.add(transfer.playlist_id);
    const uniformProgress = total ? downloaded / total * SEGMENT_COUNT : 0;
    const delta = total ? (downloaded - transfer.downloaded_bytes) / total : 0;
    const uiSegments = transfer.ui_segments.map((value, index) => transfer.id.startsWith("demo-")
      ? ratio(uniformProgress - index) : ratio(value + delta));
    return {
      ...transfer, downloaded_bytes: downloaded, uploaded_bytes: transfer.uploaded_bytes + (advance ? Math.floor(up * .06) : 0),
      download_speed_bps: finished ? 0 : downloadSpeed, upload_speed_bps: finished ? 0 : up,
      eta_seconds: !finished && total && downloadSpeed ? Math.ceil((total - downloaded) / downloadSpeed) : null,
      active_connections: finished ? 0 : transfer.active_connections,
      ui_segments: finished ? Array<number>(SEGMENT_COUNT).fill(1) : uiSegments,
      status: finished ? "completed" : "active", updated_at: finished ? new Date().toISOString() : transfer.updated_at,
      segments: transfer.segments.map(segment => {
        if (segment.state === "completed" || segment.speed_bps === 0) return segment;
        const length = segment.end_byte - segment.start_byte;
        const progress = Math.min(length, segment.downloaded_bytes + (advance ? Math.floor(segment.speed_bps * .06 * scale) : 0));
        return {
          ...segment, downloaded_bytes: progress, speed_bps: progress === length ? 0 : segment.speed_bps,
          state: progress === length ? "completed" : segment.state,
        };
      }),
    };
  });
  return completedPlaylists;
}

function refreshRates(advance = false) {
  const completedPlaylists = updateRates(advance);
  let started = false;
  for (const playlistId of completedPlaylists) {
    if (advancePlaylist(playlistId)) started = true;
  }
  if (started) updateRates(false);
  emit();
}

function add(base: TransferSnapshot, overrides: Partial<TransferSnapshot>) {
  const now = new Date().toISOString();
  const transfer = { ...base, ...overrides, created_at: now, updated_at: now };
  rates.set(transfer.id, [transfer.download_speed_bps, transfer.upload_speed_bps]);
  transfers = [transfer, ...transfers];
  refreshRates();
  return transfer.id;
}

function expandWildcard(pattern: string) {
  let parts = [""];
  const ipv6 = pattern.match(/^https?:\/\/\[[\da-f:.]+\]/i)?.[0] ?? "";
  let rest = pattern.slice(ipv6.length);
  parts[0] = ipv6;
  while (rest.includes("[")) {
    const match = /^(.*?)\[(\d+|[a-zA-Z])-(\d+|[a-zA-Z])(?::(\d+))?\](.*)$/.exec(rest);
    if (!match) throw new Error("Use a range such as [001-010] or [a-z:2].");
    const [, prefix, first, last, step = "1", tail] = match;
    const letters = /^[a-z]-[a-z]$|^[A-Z]-[A-Z]$/.test(first + "-" + last);
    if (!letters && !/^\d+-\d+$/.test(first + "-" + last)) throw new Error("Invalid wildcard range.");
    const start = letters ? first.charCodeAt(0) : Number(first);
    const end = letters ? last.charCodeAt(0) : Number(last);
    const stride = Number(step);
    const count = Math.floor((end - start) / stride) + 1;
    if (!Number.isSafeInteger(end) || !Number.isSafeInteger(start) || !Number.isSafeInteger(stride)
      || stride < 1 || count < 1 || parts.length * count > 10_000) {
      throw new Error("Invalid range or more than 10,000 URLs.");
    }
    const width = /^0\d/.test(first) || /^0\d/.test(last) ? Math.max(first.length, last.length) : 0;
    const values = Array.from({ length: count }, (_, index) => {
      const value = start + stride * index;
      return letters ? String.fromCharCode(value) : String(value).padStart(width, "0");
    });
    parts = parts.flatMap(part => values.map(value => part + prefix + value));
    rest = tail;
  }
  const urls = parts.map(part => part + rest);
  if (rest.includes("]") || !urls.every(isHttpUrl)) throw new Error("Enter a valid HTTP or HTTPS URL pattern.");
  return urls;
}

export const demo = {
  snapshot,
  expandWildcard,
  listenTelemetry(listener: (batch: TelemetryBatch) => void) {
    telemetryListeners.add(listener);
    timer ??= window.setInterval(() => { tick++; refreshRates(true); }, 2400);
    return () => {
      telemetryListeners.delete(listener);
      if (telemetryListeners.size === 0) {
        window.clearInterval(timer);
        timer = undefined;
      }
    };
  },
  listenPicker(listener: (url: string | null) => void) {
    pickerListeners.add(listener);
    return () => { pickerListeners.delete(listener); };
  },
  openPicker(url: string | null) {
    for (const listener of pickerListeners) listener(url);
  },
  probe(url: string, playlist = isPurePlaylistUrl(url)): MediaProbeResult {
    if (!isHttpUrl(url)) throw new Error("Enter an HTTP or HTTPS media URL.");
    if (playlist) {
      return {
        title: demoPlaylistTitle, source_url: url, extractor: sourceName(url), duration_seconds: null,
        thumbnail_url: null, is_playlist: true, playlist_count: demoPlaylistItems.length,
        playlist_entries: demoPlaylistItems.map(entry => entry.title), playlist_items: demoPlaylistItems.map(entry => ({ ...entry })),
        formats: [], subtitles: [],
      };
    }
    return {
      title: url === mediaUrl ? "Alpine Cinema in 4K" : "Tokyo Architecture in 4K HDR",
      source_url: url, extractor: sourceName(url), duration_seconds: 1122, thumbnail_url: null,
      is_playlist: false, playlist_count: null, playlist_entries: [], formats: seededFormats,
      subtitles: [{ lang: "en", name: "English", is_auto: false }, { lang: "ar", name: "Arabic", is_auto: false },
        { lang: "es", name: "Spanish", is_auto: true }],
    };
  },
  addHttp(request: AddHttpDownloadRequest) {
    const { url, filename, save_dir, max_connections, queue_id } = request;
    return add(baseTransfer("demo-" + ++sequence, filename || fileName(url), url), {
      save_dir: save_dir || settings.default_download_dir, total_bytes: 376 * MB,
      download_speed_bps: 24 * MB, active_connections: max_connections ?? settings.default_connections,
      queue_id: queue_id ?? "default",
    });
  },
  addMedia(request: AddMediaDownloadRequest) {
    const format = seededFormats.find(item => item.format_id === request.format_id) ?? seededFormats[0];
    const extension = request.audio_only_preset?.split("_")[0] ?? request.target_container ?? format.container;
    const saveDir = request.save_dir || settings.default_download_dir || demoFolder;
    const media: Partial<TransferSnapshot> = {
      kind: "media", save_dir: saveDir, download_speed_bps: 27 * MB,
      active_connections: settings.default_connections, format_tags: [extension.toUpperCase(), format.label],
      total_bytes: request.audio_only_preset ? 48 * MB : format.filesize_bytes,
    };
    if (request.selected_entries || request.download_playlist || request.playlist_items || isPurePlaylistUrl(request.url)) {
      const ranges = parsePlaylistRanges(request.playlist_items ?? "");
      if (!request.selected_entries && !ranges) throw new Error("Enter valid playlist item numbers or ranges.");
      const entries = [...(request.selected_entries ?? demoPlaylistItems.filter(entry => !ranges?.length
        || ranges.some(([first, last]) => entry.index >= first && entry.index <= last)))].sort((a, b) => a.index - b.index);
      if (!entries.length) throw new Error("Select at least one available playlist video.");
      const title = request.playlist_folder_name?.trim() || demoPlaylistTitle;
      const separator = folderSeparator(saveDir);
      const directory = saveDir.replace(/[\\/]+$/, "") + separator + sanitizeMediaStem(title);
      const playlistId = "demo-playlist-" + ++sequence;
      const now = new Date().toISOString();
      const width = Math.max(2, String(entries.at(-1)?.index).length);
      const jobs = entries.map(({ index, title: entryTitle, url }, position): TransferSnapshot => {
        const id = "demo-" + ++sequence;
        const filename = String(index).padStart(width, "0") + " - " + sanitizeMediaStem(entryTitle, "Video") + "." + extension;
        rates.set(id, [27 * MB, 0]);
        return {
          ...baseTransfer(id, filename, url ?? request.url), ...media, save_dir: directory,
          status: position === 0 ? "active" : "queued", total_bytes: (request.audio_only_preset ? 5 : 12) * MB,
          download_speed_bps: 0, active_connections: position === 0 ? settings.default_connections : 0,
          playlist_id: playlistId, playlist_title: title, playlist_index: position + 1, playlist_total: entries.length,
          created_at: now, updated_at: now,
        };
      });
      transfers = [...jobs, ...transfers];
      refreshRates();
      return jobs[0].id;
    }
    return add(baseTransfer("demo-" + ++sequence, request.filename || "Tokyo_Architecture." + extension, request.url), media);
  },
  addTorrent(request: AddTorrentRequest) {
    const id = "demo-" + ++sequence;
    const filename = fileName(request.source);
    const size = 520 * MB;
    torrentFiles.set(id, [{ index: 0, relative_path: filename.replace(/\.torrent$/i, "") + ".zip",
      size_bytes: size, downloaded_bytes: 0, selected: true }]);
    return add(baseTransfer(id, filename, request.source), {
      kind: "torrent", save_dir: request.save_dir || settings.default_download_dir, total_bytes: size,
      download_speed_bps: 18.6 * MB, upload_speed_bps: .8 * MB, protocol_label: "BitTorrent", active_peers: 12,
    });
  },
  listFiles(id: string) {
    return torrentFiles.get(id) ?? [];
  },
  selectFiles(id: string, indices: number[]) {
    torrentFiles.set(id, demo.listFiles(id).map(file => ({ ...file, selected: indices.includes(file.index) })));
  },
  pause(id: string) {
    update(id, () => ({ status: "paused", download_speed_bps: 0, upload_speed_bps: 0, eta_seconds: null }));
    refreshRates();
  },
  resume(id: string) {
    const playlistId = transfers.find(transfer => transfer.id === id)?.playlist_id;
    if (playlistId && (pausedPlaylists.has(playlistId)
      || transfers.some(transfer => transfer.playlist_id === playlistId && isRunning(transfer)))) {
      throw new Error("The playlist is paused or already has a running transfer.");
    }
    update(id, transfer => playlistId ? activate(transfer) : { status: "active", error_message: null });
    refreshRates();
  },
  pausePlaylist(id: string) {
    if (!transfers.some(transfer => transfer.playlist_id === id)) throw new Error("Playlist not found.");
    pausedPlaylists.add(id);
    transfers = transfers.map(transfer => transfer.playlist_id === id && isRunning(transfer)
      ? { ...transfer, status: "paused", download_speed_bps: 0, eta_seconds: null, active_connections: 0,
        updated_at: new Date().toISOString() } : transfer);
    refreshRates();
  },
  resumePlaylist(id: string) {
    if (!transfers.some(transfer => transfer.playlist_id === id)) throw new Error("Playlist not found.");
    pausedPlaylists.delete(id);
    advancePlaylist(id, true);
    refreshRates();
  },
  cancel(id: string) {
    const playlistId = transfers.find(transfer => transfer.id === id)?.playlist_id;
    if (playlistId) {
      transfers = transfers.filter(transfer => transfer.id !== id);
      rates.delete(id);
      if (transfers.some(transfer => transfer.playlist_id === playlistId)) advancePlaylist(playlistId);
      else pausedPlaylists.delete(playlistId);
    } else {
      update(id, () => ({ status: "canceled", download_speed_bps: 0, upload_speed_bps: 0, eta_seconds: null }));
    }
    refreshRates();
  },
  refresh(request: RefreshDownloadUrlRequest) {
    update(request.transfer_id, transfer => ({
      source_url: request.new_url, source_domain: sourceName(request.new_url), error_message: null,
      status: transfer.status === "failed" ? "paused" : transfer.status,
    }));
  },
  verify(id: string, expected: string) {
    const matches = /^([a-f0-9])\1{63}$/i.test(expected);
    update(id, () => ({ sha256_verified: matches }));
    return matches;
  },
  setLimit(limit: number | null) {
    settings = { ...settings, global_speed_limit_bps: limit };
    refreshRates();
  },
  getSettings() { return { ...settings }; },
  saveSettings(next: AppSettings) {
    settings = { ...next };
    refreshRates();
  },
  checkForUpdates(): UpdateStatus {
    return { available: false, current_version: version, version: null, notes: null, pub_date: null };
  },
  installUpdate() { return false; },
  toolchain,
  updateYtdlp(): ToolchainStatus {
    managedYtdlp = true;
    return toolchain();
  },
  listBrowsers(): BrowserStatus[] {
    return browserIds.map(id => ({
      id,
      ...browsers[id],
      native_host_registered: registeredHosts.has(id),
      bundled_extension_dir: extensionDirectory(id),
      extension_id: registeredHosts.get(id) ?? defaultExtensionId(id),
    }));
  },
  openBrowserExtensionSetup(browserId: BrowserKind, customExtensionId?: string | null): BrowserSetupResult {
    const extensionId = customExtensionId?.trim() || defaultExtensionId(browserId);
    installed = true;
    browsers[browserId].installed = true;
    registeredHosts.set(browserId, extensionId);
    return {
      browser_id: browserId,
      launched_browser: true,
      revealed_extension_dir: true,
      native_host_registered: true,
      extension_dir: extensionDirectory(browserId),
      extension_page_url: browsers[browserId].extension_url,
      extension_id: extensionId,
      instructions: browserId === "firefox"
        ? "Choose Load Temporary Add-on and select manifest.json in the extension folder."
        : "Enable Developer mode, choose Load unpacked, and select the extension folder.",
    };
  },
  installHosts(ids: Array<string | null>) {
    installed = true;
    manualBrowserIds.forEach((browser, index) => {
      const id = ids[index]?.trim();
      if (id || browser === "firefox") registeredHosts.set(browser, id || defaultExtensionId(browser));
    });
    return ["Chrome", "Edge", "Firefox"].filter((_, index) => Boolean(ids[index]));
  },
};
