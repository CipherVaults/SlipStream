import { Download, Folder, Link2, Search, Upload } from "lucide-react";
import { useEffect, useRef, useState, type FormEvent } from "react";
import mediaArtwork from "../assets/media-sheet.svg";
import { desktop, isTauri } from "../lib/tauri";
import {
  bytes, clock, errorMessage, folderSeparator, hasListParam, isHttpUrl, isPurePlaylistUrl,
  isRemoteTorrentUrl, isSafeThumbnailUrl, isTorrentSource, isWildcard, parsePlaylistRanges, sanitizeMediaStem, sourceName,
} from "../lib/transfers";
import type { AppSettings, AudioPreset, MediaFormatOption, MediaProbeResult, SelectedPlaylistEntry } from "../types/ipc";
import { Dialog, FormatRow, Switch, Tabs } from "./ui";

const tabs = [["video", "Video"], ["audio", "Audio Only"], ["subs", "Subtitles"], ["torrent", "Torrent / Magnet"]] as const;
type AddTab = typeof tabs[number][0];
const audioPresets: Array<[AudioPreset, string, string]> = [
  ["mp3_320", "MP3 · High Quality", "320 kbps"], ["m4a_aac", "M4A · AAC", "AAC"],
  ["opus", "Opus", "Opus"], ["flac", "FLAC · Lossless", "FLAC"], ["wav", "WAV · Uncompressed", "PCM"],
];
const playlistQualities = [
  [null, "Best Available"], [2160, "2160p · 4K UHD"], [1080, "1080p · Full HD"],
  [720, "720p · HD"], [480, "480p · SD"], [360, "360p"],
] as const;

const formatPart = /^[a-z0-9_][a-z0-9_.-]*$/i;
const subtitleLanguage = /^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$/;
const quickSubtitleLanguages = ["en", "ar", "es", "fr", "de", "ja"];
const audioTrackPriority = { descriptive: 0, dubbed: 1, unspecified: 2, default: 3, original: 4 };

function pairVideoWithAudio(video: MediaFormatOption, formats: MediaFormatOption[]) {
  const id = video.format_id;
  const fallback = id + "+bestaudio/" + id;
  if (!formatPart.test(id) || fallback.length > 256) return id;
  const container = video.container.toLowerCase();
  const priority = (audio: MediaFormatOption) => audioTrackPriority[audio.audio_track_kind ?? "unspecified"];
  const tracks = formats.filter(audio => audio.is_audio_only && formatPart.test(audio.format_id))
    .sort((a, b) => priority(b) - priority(a));
  const preferred = tracks[0];
  if (!preferred) return fallback;
  const compatible = tracks.filter(audio => {
    if (priority(audio) !== priority(preferred)) return false;
    const codec = audio.audio_codec?.toLowerCase() ?? "";
    const ext = audio.container.toLowerCase();
    return (container === "mp4" && (codec ? ["aac", "alac", "mp3"].includes(codec) : ["m4a", "mp4"].includes(ext)))
      || (container === "webm" && (codec ? ["opus", "vorbis"].includes(codec) : ["webm", "opus", "ogg"].includes(ext)));
  });
  if (priority(preferred) > audioTrackPriority.unspecified && !compatible.includes(preferred)) {
    compatible.push(preferred);
  }
  for (const audio of compatible) {
    const selector = id + "+" + audio.format_id + "/" + fallback;
    if (selector.length <= 256) return selector;
  }
  return fallback;
}

export interface AddSeed {
  url?: string;
  headers?: Record<string, string>;
  probe?: MediaProbeResult;
  format?: MediaFormatOption;
  autoProbe?: boolean;
}

function sourceOrigin(raw: string | undefined) {
  const trimmed = raw?.trim();
  if (!trimmed) return "";
  try {
    return new URL(trimmed).origin;
  } catch {
    return "";
  }
}

function cleanSeedHeaders(headers: Record<string, string> | undefined) {
  if (!headers) return undefined;
  const entries = Object.entries(headers).filter(([, value]) => value.length > 0);
  return entries.length ? Object.fromEntries(entries) : undefined;
}

const pendingAutoProbes = new WeakMap<AddSeed, Promise<MediaProbeResult>>();

export function AddDialog({ seed, droppedSource, settings, onClose, onAdded, onBusyChange }: {
  seed: AddSeed; droppedSource?: { url: string; seq: number }; settings: AppSettings; onClose: () => void;
  onAdded: (message: string, id?: string) => void; onBusyChange: (busy: boolean) => void;
}) {
  const native = isTauri();
  const [url, setUrl] = useState(seed.url ?? "");
  const [tab, setTab] = useState<AddTab>(() => seed.format?.is_audio_only ? "audio"
    : isTorrentSource(seed.url ?? "") || isWildcard(seed.url ?? "") ? "torrent" : "video");
  const [probe, setProbe] = useState<MediaProbeResult | undefined>(seed.probe);
  const [playlistMode, setPlaylistMode] = useState(() => {
    const initial = seed.url?.trim() ?? "";
    return isPurePlaylistUrl(initial) || (!hasListParam(initial) && Boolean(seed.probe?.is_playlist));
  });
  const [maxHeight, setMaxHeight] = useState<number | null>(null);
  const [playlistItems, setPlaylistItems] = useState("");
  const [selectedIndices, setSelectedIndices] = useState(() => new Set(seed.probe?.playlist_items?.map(item => item.index)));
  const [playlistFilter, setPlaylistFilter] = useState("");
  const [playlistFolder, setPlaylistFolder] = useState<string | null>(null);
  const [videoFormat, setVideoFormat] = useState<string | null>(seed.format?.is_audio_only ? null : seed.format?.format_id ?? null);
  const [audio, setAudio] = useState<{ preset: AudioPreset } | { format: string }>(
    seed.format?.is_audio_only ? { format: seed.format.format_id } : { preset: "mp3_320" }
  );
  const [languages, setLanguages] = useState<string[]>([]);
  const [customLanguage, setCustomLanguage] = useState("");
  const [embed, setEmbed] = useState(true);
  const [remux, setRemux] = useState(true);
  const [normalize, setNormalize] = useState(false);
  const [sequential, setSequential] = useState(false);
  const [folder, setFolder] = useState(settings.default_download_dir);
  const [threads, setThreads] = useState(settings.default_connections);
  const [filename, setFilename] = useState("");
  const [checksum, setChecksum] = useState("");
  const [wildcardUrls, setWildcardUrls] = useState<string[]>([]);
  const [probeBusy, setProbeBusy] = useState(false);
  const [thumbFailed, setThumbFailed] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [message, setMessage] = useState("");
  const [seedHeaders, setSeedHeaders] = useState<Record<string, string> | undefined>(() => cleanSeedHeaders(seed.headers));
  const seedOrigin = useRef(sourceOrigin(seed.url));
  const probeRequest = useRef(0);
  const activeProbe = useRef<{ id: number; source: string; playlist?: boolean } | null>(null);
  const submitLock = useRef(false);
  const batchProgress = useRef({ pattern: "", count: 0 });
  const appliedDropSeq = useRef(0);

  const source = url.trim();
  const activeHeaders = seedHeaders && (!source || (seedOrigin.current && sourceOrigin(source) === seedOrigin.current))
    ? seedHeaders
    : undefined;
  const isPlaylistActive = playlistMode;
  const canTogglePlaylist = hasListParam(source) && !isPurePlaylistUrl(source);
  const wildcard = !isPlaylistActive && isWildcard(source);
  const torrent = !wildcard && (tab === "torrent" || isTorrentSource(source));
  const direct = !isPlaylistActive && !torrent && (wildcard || (tab === "video" && !videoFormat));
  const mediaOptions = [...new Map(probe?.subtitles.map(track => [track.lang, track]) ?? []).values()];
  const choosePlaylistLanguages = isPlaylistActive && mediaOptions.length === 0;
  const entries = isPlaylistActive && !torrent ? probe?.playlist_items ?? [] : [];
  const previewIncomplete = probe?.playlist_count != null && probe.playlist_count > entries.length;
  const normalizedRange = playlistItems.trim().replace(/\s*([,-])\s*/g, "$1");
  const parsedRanges = normalizedRange ? parsePlaylistRanges(normalizedRange) : null;
  const rangeHasOutOfPreview = Boolean(parsedRanges && entries.length && parsedRanges.some(([, last]) => last > entries.at(-1)!.index));
  const noPlaylistSelection = entries.length > 0 && selectedIndices.size === 0 && !playlistItems.trim();
  const filter = playlistFilter.trim().toLowerCase();
  const indexFilter = /^#?\s*(\d+)$/.exec(filter)?.[1];
  const visibleEntries = filter ? entries.filter(entry => entry.title.toLowerCase().includes(filter)
    || (indexFilter && String(entry.index).includes(indexFilter))) : entries;
  const defaultPlaylistFolder = sanitizeMediaStem(probe?.title ?? "");
  const playlistFolderValue = playlistFolder ?? (probe ? defaultPlaylistFolder : "");
  const destination = folder.trim() || settings.default_download_dir.trim() || "Default download folder";
  const separator = folderSeparator(destination);
  const playlistPath = destination.replace(/[\\/]+$/, "") + separator
    + (playlistFolderValue.trim() ? sanitizeMediaStem(playlistFolderValue) : defaultPlaylistFolder) + separator;

  function resetPlaylistSelection(next?: MediaProbeResult) {
    setSelectedIndices(new Set(next?.playlist_items?.map(item => item.index)));
    setPlaylistItems("");
    setPlaylistFilter("");
  }

  function selectPlaylistIndices(indices: Set<number>) {
    setSelectedIndices(indices);
    setPlaylistItems("");
    setMessage("");
  }

  function changePlaylistRange(value: string) {
    setPlaylistItems(value);
    setMessage("");
    if (!entries.length) return;
    const normalized = value.trim().replace(/\s*([,-])\s*/g, "$1");
    if (!normalized) {
      setSelectedIndices(new Set(entries.map(entry => entry.index)));
      return;
    }
    const ranges = parsePlaylistRanges(normalized);
    if (ranges) {
      setSelectedIndices(new Set(entries.filter(entry => ranges.some(([first, last]) =>
        entry.index >= first && entry.index <= last)).map(entry => entry.index)));
    }
  }

  function changeSource(value: string) {
    setUrl(value);
    if (sourceOrigin(value) !== seedOrigin.current) {
      seedOrigin.current = "";
      setSeedHeaders(undefined);
    }
    setMessage("");
    setProbe(undefined);
    setVideoFormat(null);
    setAudio({ preset: "mp3_320" });
    setLanguages([]);
    setCustomLanguage("");
    setWildcardUrls([]);
    setProbeBusy(false);
    activeProbe.current = null;
    probeRequest.current++;
    batchProgress.current = { pattern: "", count: 0 };
    const next = value.trim();
    const playlist = isPurePlaylistUrl(next);
    setPlaylistMode(playlist);
    setMaxHeight(null);
    resetPlaylistSelection();
    setPlaylistFolder(null);
    if (!playlist && (isTorrentSource(next) || isWildcard(next))) setTab("torrent");
    else if (isRemoteTorrentUrl(next)) setTab("video");
    else if (isHttpUrl(next) && tab === "torrent") setTab("video");
  }

  async function probeSource(
    value: string,
    playlist: boolean | undefined = playlistMode || undefined,
    coalesceSeed = false,
    explicitHeaders?: Record<string, string>,
  ) {
    if (activeProbe.current?.source === value && activeProbe.current.playlist === playlist) return;
    const request = ++probeRequest.current;
    activeProbe.current = { id: request, source: value, playlist };
    setMessage("");
    setProbeBusy(true);
    try {
      if (!playlist && isWildcard(value)) {
        const urls = await desktop.expandWildcardUrls(value);
        if (request !== probeRequest.current) return;
        setWildcardUrls(urls);
        setTab("torrent");
      } else {
        if (!isHttpUrl(value)) throw new Error("Enter an HTTP or HTTPS media URL to probe.");
        const probeHeaders = explicitHeaders ?? (sourceOrigin(value) === seedOrigin.current ? activeHeaders : undefined);
        const runProbe = () => probeHeaders
          ? desktop.probeMedia(value, playlist, probeHeaders)
          : playlist === undefined ? desktop.probeMedia(value) : desktop.probeMedia(value, playlist);
        let probePromise: Promise<MediaProbeResult>;
        if (coalesceSeed) {
          let cached = pendingAutoProbes.get(seed);
          if (!cached) {
            cached = runProbe();
            pendingAutoProbes.set(seed, cached);
            void cached.finally(() => {
              if (pendingAutoProbes.get(seed) === cached) pendingAutoProbes.delete(seed);
            }).catch(() => {});
          }
          probePromise = cached;
        } else {
          probePromise = runProbe();
        }
        const result = await probePromise;
        if (request !== probeRequest.current) return;
        setProbe(result);
        setThumbFailed(false);
        if (probe?.is_playlist && probe.playlist_items?.length && result.is_playlist
          && value === source && (playlist === undefined || playlist === playlistMode)) {
          const indices = result.playlist_items?.map(item => item.index) ?? [];
          setSelectedIndices(current => new Set(indices.filter(index => current.has(index))));
        } else {
          resetPlaylistSelection(result);
        }
        if (result.is_playlist) {
          setPlaylistMode(true);
          setAudio({ preset: "mp3_320" });
        }
        setVideoFormat(result.formats.find(format => format.has_video)?.format_id ?? null);
        if (!result.is_playlist || result.subtitles.length > 0) {
          setLanguages(result.subtitles.some(track => track.lang === "en") ? ["en"] : []);
        }
      }
    } catch (error) {
      if (request === probeRequest.current) setMessage(errorMessage(error));
    } finally {
      if (activeProbe.current?.id === request) {
        activeProbe.current = null;
        setProbeBusy(false);
      }
    }
  }

  function changePlaylistMode(playlist: boolean) {
    if (playlist === playlistMode) return;
    setPlaylistMode(playlist);
    setProbe(undefined);
    resetPlaylistSelection();
    setPlaylistFolder(null);
    setVideoFormat(null);
    setLanguages([]);
    setCustomLanguage("");
    setAudio({ preset: "mp3_320" });
    void probeSource(source, playlist);
  }

  useEffect(() => {
    const initial = seed.url?.trim();
    const initialHeaders = cleanSeedHeaders(seed.headers);
    seedOrigin.current = sourceOrigin(seed.url);
    setSeedHeaders(initialHeaders);
    if (seed.autoProbe && initial && !isTorrentSource(initial) && !isRemoteTorrentUrl(initial)) {
      void probeSource(initial, isPurePlaylistUrl(initial) || undefined, true, initialHeaders);
    }
    return () => {
      probeRequest.current++;
      activeProbe.current = null;
      setProbeBusy(false);
    };
  }, [seed]);

  useEffect(() => {
    if (!droppedSource || droppedSource.seq === appliedDropSeq.current) return;
    appliedDropSeq.current = droppedSource.seq;
    changeSource(droppedSource.url);
  }, [droppedSource]);

  function useFile(file: File) {
    if (!file.name.toLowerCase().endsWith(".torrent")) {
      setMessage("Choose a .torrent file.");
      return;
    }
    const path = (file as File & { path?: string }).path;
    if (native && !path) {
      setMessage("Drop the .torrent file from Explorer or paste its path above.");
      return;
    }
    changeSource(path ?? file.name);
  }

  function toggleLanguage(language: string) {
    setLanguages(current => current.includes(language)
      ? current.filter(item => item !== language) : [...current, language]);
    setMessage("");
  }

  function addCustomLanguage() {
    const language = customLanguage.trim();
    if (!subtitleLanguage.test(language)) {
      setMessage("Enter a subtitle language code such as en-US.");
      return;
    }
    setLanguages(current => current.includes(language) ? current : [...current, language]);
    setCustomLanguage("");
    setMessage("");
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (submitLock.current) return;
    setMessage("");
    if (torrent ? !isTorrentSource(source) : !wildcard && !isHttpUrl(source)) {
      setMessage(torrent ? "Enter a magnet link or the path to a .torrent file." : "Enter a valid HTTP or HTTPS URL.");
      return;
    }
    if (checksum.trim() && direct && !wildcard && !/^[a-f\d]{64}$/i.test(checksum.trim())) {
      setMessage("Enter a SHA-256 checksum with 64 hexadecimal characters.");
      return;
    }
    if (tab === "subs" && !wildcard && !torrent && languages.length === 0) {
      setMessage(choosePlaylistLanguages ? "Select at least one subtitle language."
        : "Probe the URL and select at least one subtitle language.");
      return;
    }
    if (noPlaylistSelection) return;
    const items = isPlaylistActive && !torrent ? playlistItems.trim().replace(/\s*([,-])\s*/g, "$1") : "";
    if (!parsePlaylistRanges(items)) {
      setMessage("Enter playlist item numbers or ranges, such as 1-5, 8.");
      return;
    }
    const selectedEntries: SelectedPlaylistEntry[] | null = !items && entries.length
      && !(previewIncomplete && selectedIndices.size === entries.length)
      ? entries.filter(item => selectedIndices.has(item.index)).map(({ index, title, url }) => ({ index, title, url: url ?? null }))
      : null;
    submitLock.current = true;
    setSubmitting(true);
    onBusyChange(true);
    const saveDir = folder.trim() || null;
    const outputName = filename.trim() || null;
    try {
      let id: string | undefined;
      if (wildcard) {
        const urls = wildcardUrls.length ? wildcardUrls : await desktop.expandWildcardUrls(source);
        if (!urls.length) throw new Error("No URLs matched this pattern.");
        if (batchProgress.current.pattern !== source) batchProgress.current = { pattern: source, count: 0 };
        for (const next of urls.slice(batchProgress.current.count)) {
          id = await desktop.addHttpDownload({ url: next, save_dir: saveDir, max_connections: threads, headers: {} });
          batchProgress.current.count++;
        }
        onAdded(urls.length + " downloads added.", id);
        return;
      }
      if (torrent) {
        id = await desktop.addTorrent({ source, save_dir: saveDir, sequential });
      } else if (direct) {
        id = await desktop.addHttpDownload({
          url: source, save_dir: saveDir, max_connections: threads,
          filename: outputName, expected_sha256: checksum.trim() || null,
          headers: activeHeaders ?? {},
          ...(activeHeaders?.Referer ? { referrer: activeHeaders.Referer } : {}),
        });
      } else {
        let format = isPlaylistActive ? null : tab === "audio" ? ("format" in audio ? audio.format : null) : videoFormat;
        if (tab !== "audio" && format && probe) {
          const picked = probe.formats.find(item => item.format_id === format);
          if (picked?.has_video && !picked.has_audio) {
            format = pairVideoWithAudio(picked, probe.formats);
          }
        }
        id = await desktop.addMediaDownload({
          url: source, format_id: format,
          audio_only_preset: tab === "audio" && "preset" in audio ? audio.preset : null,
          embed_subtitles_langs: tab === "audio" ? [] : tab === "subs" || embed ? languages : [],
          lossless_remux: remux, normalize_audio: normalize, save_dir: saveDir,
          filename: isPlaylistActive ? null : outputName,
          ...(activeHeaders && { headers: activeHeaders }),
          ...(isPlaylistActive && {
            download_playlist: true, max_height: tab === "audio" ? null : maxHeight,
            playlist_items: items || null,
            playlist_folder_name: playlistFolderValue.trim() ? sanitizeMediaStem(playlistFolderValue) : null,
            selected_entries: selectedEntries,
          }),
        });
      }
      onAdded("Download added.", id);
    } catch (error) {
      const count = batchProgress.current.count;
      setMessage((wildcard && count ? count + " downloads added. Retry to add the remaining URLs. " : "") + errorMessage(error));
    } finally {
      submitLock.current = false;
      setSubmitting(false);
      onBusyChange(false);
    }
  }

  function formatRow(format: MediaFormatOption, selected: boolean, select: () => void) {
    const codec = format.video_codec ?? format.audio_codec;
    const container = format.container.toUpperCase();
    const rawTags = [codec, container].filter((tag): tag is string => Boolean(tag));
    const tags = rawTags.filter((tag, index) =>
      rawTags.findIndex(item => item.toUpperCase() === tag.toUpperCase()) === index);
    return <FormatRow key={format.format_id} name={format.label} size={bytes(format.filesize_bytes)}
      tags={tags} selected={selected} onSelect={select} />;
  }

  const safeThumb = !thumbFailed ? isSafeThumbnailUrl(probe?.thumbnail_url) : null;

  return <Dialog title="Add Download" onClose={onClose} className="add-sheet" busy={submitting} header={
    probe && <div className="sheet-thumb"><img src={safeThumb ?? mediaArtwork} referrerPolicy="no-referrer" alt=""
      onError={() => { if (safeThumb) setThumbFailed(true); }} /></div>
  }>
    {probe && <div className="probe-summary tabular"><strong>{probe.title}</strong>
      <span>{sourceName(probe.source_url)}{probe.duration_seconds !== null ? " · " + clock(probe.duration_seconds) : ""}</span>
      {isPlaylistActive && <span className="light-pill">Playlist{probe.playlist_count !== null ? " · " + probe.playlist_count + " videos" : ""}</span>}
      <span className="sheet-url-pill">{probe.extractor}</span></div>}
    <form onSubmit={submit} className="dialog-form">
      <fieldset disabled={submitting} className="dialog-fields">
        <div className="field"><label htmlFor="download-source">URL, magnet link, .torrent path, or wildcard pattern</label>
          <span className="field-inline"><Link2 size={15} /><input id="download-source" data-initial-focus aria-label="Download source" value={url}
            placeholder="Paste a URL or magnet link" onChange={event => changeSource(event.target.value)} onKeyDown={event => {
              if (event.key === "Enter" && !isTorrentSource(source)) {
                event.preventDefault();
                if (!probeBusy && source) void probeSource(source);
              }
            }} />
            <button type="button" className="btn-ghost form-button" disabled={!source || probeBusy || isTorrentSource(source)}
              onClick={() => void probeSource(source)}>{probeBusy ? "Probing…" : "Probe"}</button></span>
        </div>
        {canTogglePlaylist && <div className="sheet-tabs playlist-mode" role="group" aria-label="Download scope">
          {([[false, "Single Video"], [true, "Whole Playlist"]] as const).map(([playlist, label]) =>
            <button key={label} type="button" className={"sheet-tab" + (playlistMode === playlist ? " active" : "")}
              aria-pressed={playlistMode === playlist} onClick={() => changePlaylistMode(playlist)}>{label}</button>)}
        </div>}

        <Tabs<AddTab> label="Download format" value={tab} onChange={setTab} options={tabs} className="sheet-tabs" buttonClass="sheet-tab">
          {tab === "video" && isPlaylistActive && <div className="field">
            <span>Max Quality</span>
            <div className="format-list">
              {playlistQualities.map(([height, name]) => <FormatRow key={name} name={name}
                selected={maxHeight === height} onSelect={() => setMaxHeight(height)} />)}
            </div>
          </div>}
          {tab === "video" && !isPlaylistActive && <div className="format-list">
            {probe?.formats.filter(format => format.has_video).map(format => formatRow(format, videoFormat === format.format_id,
              () => setVideoFormat(format.format_id)))}
            <FormatRow name="Direct HTTP download" tags={["HTTP"]} selected={!videoFormat} onSelect={() => setVideoFormat(null)} />
          </div>}
          {tab === "audio" && <div className="format-list">
            {audioPresets.map(([preset, name, tag]) => <FormatRow key={preset} name={name} tags={[tag]}
              selected={"preset" in audio && audio.preset === preset} onSelect={() => setAudio({ preset })} />)}
            {!isPlaylistActive && probe?.formats.filter(format => format.is_audio_only).map(format =>
              formatRow(format, "format" in audio && audio.format === format.format_id, () => setAudio({ format: format.format_id })))}
          </div>}
          {tab === "subs" && <div className="format-list">
            {mediaOptions.map(track => <label className="check-row" key={track.lang}>
              <input type="checkbox" checked={languages.includes(track.lang)} onChange={() => toggleLanguage(track.lang)} />
              {track.name} <span className="light-pill">{track.lang.toUpperCase()}</span>
              {track.is_auto && <span className="field-note">Automatic</span>}
            </label>)}
            {choosePlaylistLanguages ? <div className="field playlist-subtitles">
              <div className="subtitle-chips" role="group" aria-label="Subtitle languages">
                {[...new Set([...quickSubtitleLanguages, ...languages])].map(language =>
                  <button key={language} type="button" className="btn-ghost subtitle-chip"
                    aria-pressed={languages.includes(language)} onClick={() => toggleLanguage(language)}>{language}</button>)}
              </div>
              <div className="field-inline">
                <input aria-label="Custom subtitle language code" placeholder="e.g. en-US" value={customLanguage}
                  onChange={event => setCustomLanguage(event.target.value)}
                  onKeyDown={event => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      addCustomLanguage();
                    }
                  }} />
                <button type="button" className="btn-ghost form-button" onClick={addCustomLanguage}>Add Language</button>
              </div>
            </div> : mediaOptions.length === 0 && <p className="empty-state">{probe ? "No subtitle tracks detected." : "Probe a media URL to see available subtitles."}</p>}
          </div>}
          {tab === "torrent" && (wildcard ? <div className="format-list wildcard-panel">
            <div className="section-head"><strong>Wildcard Batch Preview</strong>
              <span className="tabular">{wildcardUrls.length} URLs</span></div>
            {wildcardUrls.length ? <ol>{wildcardUrls.slice(0, 20).map((item, index) => <li key={item + index} title={item}>{item}</li>)}</ol>
              : <p className="field-note">Probe the pattern to preview expanded URLs, such as file_[001-010].zip.</p>}
            {wildcardUrls.length > 20 && <p className="field-note">Showing the first 20 URLs.</p>}
          </div> : <div className="torrent-entry">
            {native ? <div className="file-chooser" onDragOver={event => { event.preventDefault(); event.stopPropagation(); }}
              onDrop={event => {
                event.preventDefault(); event.stopPropagation();
                const file = event.dataTransfer.files[0];
                const text = event.dataTransfer.getData("text/uri-list") || event.dataTransfer.getData("text/plain");
                if (file) useFile(file);
                else if (isTorrentSource(text.trim())) changeSource(text.trim());
              }}>
              <Upload size={15} />Drop a .torrent file from Explorer, or paste its path above
            </div> : <label className="file-chooser" onDragOver={event => { event.preventDefault(); event.stopPropagation(); }}
              onDrop={event => {
                event.preventDefault(); event.stopPropagation();
                const file = event.dataTransfer.files[0];
                const text = event.dataTransfer.getData("text/uri-list") || event.dataTransfer.getData("text/plain");
                if (file) useFile(file);
                else if (isTorrentSource(text.trim())) changeSource(text.trim());
              }}>
              <Upload size={15} />Choose or drop a .torrent file
              <input aria-label="Choose a .torrent file" type="file" accept=".torrent,application/x-bittorrent"
                onChange={event => {
                  if (event.target.files?.[0]) useFile(event.target.files[0]);
                  event.target.value = "";
                }} />
            </label>}
            <Switch label="Sequential" checked={sequential} onChange={setSequential} />
          </div>)}
        </Tabs>
        {isPlaylistActive && !torrent && <>
          {entries.length > 0 && <div className="playlist-checklist" role="group" aria-labelledby="playlist-videos-label">
            <div className="playlist-checklist-toolbar">
              <div className="playlist-checklist-heading">
                <h3 id="playlist-videos-label">Playlist Videos</h3>
                <span className="field-note tabular" role="status">
                  {rangeHasOutOfPreview && selectedIndices.size === 0
                    ? `Custom range: ${normalizedRange}`
                    : `${selectedIndices.size} of ${entries.length} selected`}
                </span>
              </div>
              <div className="playlist-checklist-actions">
                <button type="button" className="btn-ghost form-button"
                  onClick={() => {
                    const next = new Set(selectedIndices);
                    for (const entry of visibleEntries) next.add(entry.index);
                    selectPlaylistIndices(next);
                  }}>Select All</button>
                <button type="button" className="btn-ghost form-button"
                  onClick={() => {
                    const next = new Set(selectedIndices);
                    for (const entry of visibleEntries) next.delete(entry.index);
                    selectPlaylistIndices(next);
                  }}>Clear</button>
                <button type="button" className="btn-ghost form-button"
                  onClick={() => {
                    const next = new Set(selectedIndices);
                    for (const entry of visibleEntries) {
                      if (next.has(entry.index)) next.delete(entry.index);
                      else next.add(entry.index);
                    }
                    selectPlaylistIndices(next);
                  }}>Invert</button>
              </div>
              <label className="field-inline playlist-filter"><Search size={14} aria-hidden="true" />
                <input type="search" aria-label="Filter videos" placeholder="Filter videos…" value={playlistFilter}
                  onChange={event => setPlaylistFilter(event.target.value)}
                  onKeyDown={event => { if (event.key === "Enter") event.preventDefault(); }} />
              </label>
            </div>
            <div className="playlist-checklist-items">
              {visibleEntries.map(entry => <label className="playlist-checklist-row" key={entry.index}>
                <input type="checkbox" checked={selectedIndices.has(entry.index)} onChange={event => {
                  const next = new Set(selectedIndices);
                  if (event.target.checked) next.add(entry.index);
                  else next.delete(entry.index);
                  selectPlaylistIndices(next);
                }} />
                <span className="playlist-entry-index tabular">#{entry.index}</span>
                <span className="playlist-entry-title" title={entry.title}>{entry.title}</span>
                {entry.duration_seconds != null && <span className="field-note tabular">{clock(entry.duration_seconds)}</span>}
              </label>)}
              {!visibleEntries.length && <p className="field-note playlist-checklist-empty">No videos match your filter.</p>}
            </div>
            {noPlaylistSelection && <p className="field-note playlist-checklist-empty">Select at least one video to download.</p>}
          </div>}
          {previewIncomplete && <p className="field-note playlist-checklist-empty tabular">
            {entries.length
              ? `Showing ${entries.length} of ${probe.playlist_count} videos. Select all previewed videos to download the full playlist.`
              : `Preview unavailable for this ${probe.playlist_count}-video playlist. Leave Playlist Items empty to download it in full.`}
            {" "}Use Playlist Items to specify a range outside the preview.
          </p>}
          <label className="field">Playlist Items
            <input value={playlistItems} onChange={event => changePlaylistRange(event.target.value)}
              placeholder={entries.length ? "Select by number (e.g. 1-5, 8)" : "All videos (e.g. 1-5, 8)"} />
          </label>
          <div className="field">
            <label htmlFor="playlist-folder">Playlist Folder</label>
            <input id="playlist-folder" value={playlistFolderValue} aria-describedby="playlist-path"
              placeholder={probe ? defaultPlaylistFolder : "Playlist title"} onChange={event => setPlaylistFolder(event.target.value)} />
            <span id="playlist-path" className="field-note playlist-path" title={playlistPath}>{playlistPath}</span>
          </div>
        </>}

        {tab !== "torrent" && <div className="sheet-toggles">
          {tab !== "audio" && <Switch label="Embed Subtitles" checked={embed} onChange={setEmbed} />}
          <Switch label="Lossless Remux" checked={remux} onChange={setRemux} />
          <Switch label="Audio Track Normalization" checked={normalize} onChange={setNormalize} />
        </div>}
        <div className="dest-bar">
          <label className="dest-path"><Folder size={14} /><span className="sr-only">Save folder</span>
            <input value={folder} onChange={event => setFolder(event.target.value)} placeholder="Default download folder" />
          </label>
          <label className="thread-select"><span>Threads</span><select value={threads} disabled={!direct} title={direct ? undefined : "HTTP downloads only"} onChange={event => setThreads(Number(event.target.value))}>
            {Array.from({ length: 32 }, (_, index) => <option key={index} value={index + 1}>{index + 1}</option>)}
          </select></label>
        </div>
        <details className="optional-fields">
          <summary>{direct && !wildcard ? "Filename / Expected SHA-256" : "Filename"}</summary>
          <div className="fields-grid">
            <label className="field">Filename<input value={filename} disabled={wildcard || torrent || isPlaylistActive} onChange={event => setFilename(event.target.value)} /></label>
            {direct && !wildcard && <label className="field">Expected SHA-256<input value={checksum} spellCheck={false}
              onChange={event => setChecksum(event.target.value)} /></label>}
          </div>
        </details>
      </fieldset>
      {message && <p className="inline-error" role="alert">{message}</p>}
      <div className="sheet-footer">
        <span className="sheet-foot-note tabular">{torrent ? "BitTorrent" : isPlaylistActive ? "Playlist download" : direct ? threads + " HTTP threads" : "Media download"}</span>
        <div className="sheet-foot-btns">
          <button type="button" className="btn-ghost form-button" disabled={submitting} onClick={onClose}>Cancel</button>
          <button type="submit" className="btn-primary" disabled={submitting || probeBusy || !source || noPlaylistSelection}>
            <Download size={14} />{submitting ? "Adding…" : isPlaylistActive && !torrent ? "Download Playlist" : "Download"}
          </button>
        </div>
      </div>
    </form>
  </Dialog>;
}
