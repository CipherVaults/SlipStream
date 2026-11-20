import { Download, Folder, Link2, Upload } from "lucide-react";
import { useEffect, useRef, useState, type FormEvent } from "react";
import mediaArtwork from "../assets/media-sheet.svg";
import { desktop, isTauri } from "../lib/tauri";
import { bytes, clock, errorMessage, isHttpUrl, isTorrentSource, isWildcard, sourceName } from "../lib/transfers";
import type { AppSettings, AudioPreset, MediaFormatOption, MediaProbeResult } from "../types/ipc";
import { Dialog, FormatRow, Switch, Tabs } from "./ui";

const tabs = [["video", "Video"], ["audio", "Audio Only"], ["subs", "Subtitles"], ["torrent", "Torrent / Magnet"]] as const;
type AddTab = typeof tabs[number][0];
const audioPresets: Array<[AudioPreset, string, string]> = [
  ["mp3_320", "MP3 · High Quality", "320 kbps"], ["m4a_aac", "M4A · AAC", "AAC"],
  ["opus", "Opus", "Opus"], ["flac", "FLAC · Lossless", "FLAC"], ["wav", "WAV · Uncompressed", "PCM"],
];

export interface AddSeed {
  url?: string;
  probe?: MediaProbeResult;
  format?: MediaFormatOption;
  autoProbe?: boolean;
}

export function AddDialog({ seed, droppedSource, settings, onClose, onAdded, onBusyChange }: {
  seed: AddSeed; droppedSource?: { url: string; seq: number }; settings: AppSettings; onClose: () => void;
  onAdded: (message: string, id?: string) => void; onBusyChange: (busy: boolean) => void;
}) {
  const native = isTauri();
  const [url, setUrl] = useState(seed.url ?? "");
  const [tab, setTab] = useState<AddTab>(() => seed.format?.is_audio_only ? "audio"
    : isTorrentSource(seed.url ?? "") || isWildcard(seed.url ?? "") ? "torrent" : "video");
  const [probe, setProbe] = useState<MediaProbeResult | undefined>(seed.probe);
  const [videoFormat, setVideoFormat] = useState<string | null>(seed.format?.is_audio_only ? null : seed.format?.format_id ?? null);
  const [audio, setAudio] = useState<{ preset: AudioPreset } | { format: string }>(
    seed.format?.is_audio_only ? { format: seed.format.format_id } : { preset: "mp3_320" }
  );
  const [languages, setLanguages] = useState<string[]>([]);
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
  const [submitting, setSubmitting] = useState(false);
  const [message, setMessage] = useState("");
  const probeRequest = useRef(0);
  const activeProbe = useRef<{ id: number; source: string } | null>(null);
  const submitLock = useRef(false);
  const batchProgress = useRef({ pattern: "", count: 0 });
  const appliedDropSeq = useRef(0);

  const source = url.trim();
  const wildcard = isWildcard(source);
  const torrent = !wildcard && (tab === "torrent" || isTorrentSource(source));
  const direct = !torrent && (wildcard || (tab === "video" && !videoFormat));
  const mediaOptions = [...new Map(probe?.subtitles.map(track => [track.lang, track]) ?? []).values()];

  function changeSource(value: string) {
    setUrl(value);
    setMessage("");
    setProbe(undefined);
    setVideoFormat(null);
    setAudio({ preset: "mp3_320" });
    setLanguages([]);
    setWildcardUrls([]);
    setProbeBusy(false);
    activeProbe.current = null;
    probeRequest.current++;
    batchProgress.current = { pattern: "", count: 0 };
    const next = value.trim();
    if (isTorrentSource(next) || isWildcard(next)) setTab("torrent");
    else if (isHttpUrl(next) && tab === "torrent") setTab("video");
  }

  async function probeSource(value: string) {
    if (activeProbe.current?.source === value) return;
    const request = ++probeRequest.current;
    activeProbe.current = { id: request, source: value };
    setMessage("");
    setProbeBusy(true);
    try {
      if (isWildcard(value)) {
        const urls = await desktop.expandWildcardUrls(value);
        if (request !== probeRequest.current) return;
        setWildcardUrls(urls);
        setTab("torrent");
      } else {
        if (!isHttpUrl(value)) throw new Error("Enter an HTTP or HTTPS media URL to probe.");
        const result = await desktop.probeMedia(value);
        if (request !== probeRequest.current) return;
        setProbe(result);
        setVideoFormat(result.formats.find(format => format.has_video)?.format_id ?? null);
        setLanguages(result.subtitles.some(track => track.lang === "en") ? ["en"] : []);
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

  useEffect(() => {
    const initial = seed.url?.trim();
    if (seed.autoProbe && initial && !isTorrentSource(initial)) void probeSource(initial);
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
      setMessage("Probe the URL and select at least one subtitle language.");
      return;
    }
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
        id = await desktop.addHttpDownload({ url: source, save_dir: saveDir, max_connections: threads,
          filename: outputName, expected_sha256: checksum.trim() || null, headers: {} });
      } else {
        let format = tab === "audio" ? ("format" in audio ? audio.format : null) : videoFormat;
        if (tab !== "audio" && format && probe) {
          const picked = probe.formats.find(item => item.format_id === format);
          if (picked && !picked.has_audio && probe.formats.some(item => item.has_audio)) {
            const combined = format + "+bestaudio";
            if (combined.length <= 256) format = combined;
          }
        }
        id = await desktop.addMediaDownload({
          url: source, format_id: format,
          audio_only_preset: tab === "audio" && "preset" in audio ? audio.preset : null,
          embed_subtitles_langs: tab === "audio" ? [] : tab === "subs" || embed ? languages : [],
          lossless_remux: remux, normalize_audio: normalize, save_dir: saveDir, filename: outputName,
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

  return <Dialog title="Add Download" onClose={onClose} className="add-sheet" busy={submitting} header={
    probe && <div className="sheet-thumb"><img src={probe.thumbnail_url?.startsWith("https://") ? probe.thumbnail_url : mediaArtwork} alt="" /></div>
  }>
    {probe && <div className="probe-summary tabular"><strong>{probe.title}</strong>
      <span>{sourceName(probe.source_url)}{probe.duration_seconds !== null ? " · " + clock(probe.duration_seconds) : ""}</span>
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

        <Tabs<AddTab> label="Download format" value={tab} onChange={setTab} options={tabs} className="sheet-tabs" buttonClass="sheet-tab">
          {tab === "video" && <div className="format-list">
            {probe?.formats.filter(format => format.has_video).map(format => formatRow(format, videoFormat === format.format_id,
              () => setVideoFormat(format.format_id)))}
            <FormatRow name="Direct HTTP download" tags={["HTTP"]} selected={!videoFormat} onSelect={() => setVideoFormat(null)} />
          </div>}
          {tab === "audio" && <div className="format-list">
            {audioPresets.map(([preset, name, tag]) => <FormatRow key={preset} name={name} tags={[tag]}
              selected={"preset" in audio && audio.preset === preset} onSelect={() => setAudio({ preset })} />)}
            {probe?.formats.filter(format => format.is_audio_only).map(format =>
              formatRow(format, "format" in audio && audio.format === format.format_id, () => setAudio({ format: format.format_id })))}
          </div>}
          {tab === "subs" && <div className="format-list">
            {mediaOptions.map(track => <label className="check-row" key={track.lang}>
              <input type="checkbox" checked={languages.includes(track.lang)} onChange={event => setLanguages(event.target.checked
                ? [...languages, track.lang] : languages.filter(language => language !== track.lang))} />
              {track.name} <span className="light-pill">{track.lang.toUpperCase()}</span>
              {track.is_auto && <span className="field-note">Automatic</span>}
            </label>)}
            {mediaOptions.length === 0 && <p className="empty-state">{probe ? "No subtitle tracks detected." : "Probe a media URL to see available subtitles."}</p>}
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
            <label className="field">Filename<input value={filename} disabled={wildcard || torrent} onChange={event => setFilename(event.target.value)} /></label>
            {direct && !wildcard && <label className="field">Expected SHA-256<input value={checksum} spellCheck={false}
              onChange={event => setChecksum(event.target.value)} /></label>}
          </div>
        </details>
      </fieldset>
      {message && <p className="inline-error" role="alert">{message}</p>}
      <div className="sheet-footer">
        <span className="sheet-foot-note tabular">{torrent ? "BitTorrent" : direct ? threads + " HTTP threads" : "Media download"}</span>
        <div className="sheet-foot-btns">
          <button type="button" className="btn-ghost form-button" disabled={submitting} onClick={onClose}>Cancel</button>
          <button type="submit" className="btn-primary" disabled={submitting || probeBusy || !source}>
            <Download size={14} />{submitting ? "Adding…" : "Download"}
          </button>
        </div>
      </div>
    </form>
  </Dialog>;
}
