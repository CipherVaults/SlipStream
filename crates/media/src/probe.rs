use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use serde_json::Value;
use slipstream_protocol::{
    sanitize_title_to_filename, validate_http_url, AudioPreset, AudioTrackKind, MediaContainer,
    MediaFormatOption, MediaProbeResult, NativeBridgeResponse, PlaylistEntry, SubtitleTrack,
    MAX_NATIVE_MESSAGE_BYTES,
};

use crate::{
    args::{
        ensure_default_user_agent, is_http_403_error, validate_format_id,
        validate_subtitle_language,
    },
    toolchain::{capture, command},
    BuildYtDlpArgs, MediaError, MediaToolchain, ResolvedBinary,
};

#[derive(Debug, Clone, Default)]
pub struct ProbeOptions {
    pub playlist: bool,
    pub cache_dir: Option<PathBuf>,
    pub headers: HashMap<String, String>,
}

// Keep the preview comfortably below the native IPC frame limit. The count
// is calculated from the full probe before taking the preview.
pub const MAX_PLAYLIST_PREVIEW_ENTRIES: usize = 512;
pub const MAX_PLAYLIST_TITLE_CHARS: usize = 160;
const MAX_FORMATS: usize = 64;
const MAX_SUBTITLES: usize = 64;
// The filename sanitizer caps each legacy title at 200 bytes. Reserve 128 KiB
// for those titles and their array overhead before budgeting structured items.
const MAX_PLAYLIST_ITEMS_BYTES: usize = MAX_NATIVE_MESSAGE_BYTES / 2 - 128 * 1024;
const MAX_PLAYLIST_THUMBNAIL_URL_BYTES: usize = 512;

pub async fn probe_url(
    tools: &MediaToolchain,
    url: &str,
    options: ProbeOptions,
) -> Result<MediaProbeResult, MediaError> {
    probe_url_with_binary(tools.require_ytdlp()?, url, options).await
}

/// Probes media using a discovered yt-dlp binary.
pub async fn probe_url_with_binary(
    binary: &ResolvedBinary,
    url: &str,
    options: ProbeOptions,
) -> Result<MediaProbeResult, MediaError> {
    let effective_headers = ensure_default_user_agent(&options.headers);
    let mut builder = BuildYtDlpArgs::probe(options.playlist).headers(&effective_headers)?;
    if let Some(dir) = &options.cache_dir {
        builder = builder.cache_dir(dir)?;
    }
    let run_probe = |args| {
        let mut cmd = command(&binary.path, "yt-dlp");
        cmd.args(args);
        capture(
            cmd,
            &binary.path,
            "yt-dlp",
            Duration::from_secs(90),
            32 * 1024 * 1024,
        )
    };
    let output = match run_probe(builder.clone().build(url)?).await {
        Err(err) if is_http_403_error(&err) => run_probe(builder.force_ipv4().build(url)?).await?,
        result => result?,
    };
    let json = std::str::from_utf8(&output).map_err(|_| MediaError::InvalidToolOutput {
        tool: "yt-dlp",
        kind: "UTF-8 JSON",
    })?;
    parse_ytdlp_probe_json(json, url)
}

pub fn parse_ytdlp_probe_json(
    json: &str,
    source_url: &str,
) -> Result<MediaProbeResult, MediaError> {
    let source_url = validate_http_url(source_url)?.to_string();
    let value: Value = serde_json::from_str(json)?;
    if !value.is_object() {
        return Err(MediaError::InvalidToolOutput {
            tool: "yt-dlp",
            kind: "probe object",
        });
    }
    let entries = value["entries"].as_array();
    let is_playlist =
        matches!(value["_type"].as_str(), Some("playlist" | "multi_video")) || entries.is_some();
    let playlist_count = is_playlist
        .then(|| {
            u32_value(&value["playlist_count"])
                .or_else(|| u32_value(&value["n_entries"]))
                .or_else(|| entries.and_then(|items| items.len().try_into().ok()))
        })
        .flatten();
    let youtube_playlist =
        is_youtube_extractor(&value["extractor"]) || is_youtube_extractor(&value["extractor_key"]);
    let mut playlist_entries = Vec::new();
    let mut playlist_items = Vec::new();
    let mut playlist_items_bytes = 0;
    for (position, entry) in entries
        .into_iter()
        .flatten()
        .take(MAX_PLAYLIST_PREVIEW_ENTRIES)
        .enumerate()
    {
        let title = playlist_entry_title(entry);
        if entry.is_object() && playlist_items_bytes < MAX_PLAYLIST_ITEMS_BYTES {
            let item = parse_playlist_entry(entry, (position + 1) as u32, &title, youtube_playlist);
            playlist_items_bytes += serde_json::to_vec(&item)?.len() + 1;
            if playlist_items_bytes <= MAX_PLAYLIST_ITEMS_BYTES {
                playlist_items.push(item);
            }
        }
        // The legacy array uses positions to map titles to staged downloads.
        playlist_entries.push(title);
    }

    let mut formats: Vec<_> = value["formats"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_format)
        .collect();
    if formats.is_empty() && !is_playlist {
        formats.extend(parse_format(&value));
    }
    let mut formats = rank_formats(formats);
    if formats.len() > MAX_FORMATS
        && !formats[..MAX_FORMATS]
            .iter()
            .any(|format| format.is_audio_only)
    {
        if let Some(audio) = formats[MAX_FORMATS..]
            .iter()
            .position(|format| format.is_audio_only)
        {
            formats.swap(MAX_FORMATS - 1, MAX_FORMATS + audio);
        }
    }
    formats.truncate(MAX_FORMATS);

    let mut subtitles = Vec::new();
    'tracks: for (key, is_auto) in [("subtitles", false), ("automatic_captions", true)] {
        if let Some(tracks) = value[key].as_object() {
            for (lang, versions) in tracks {
                if validate_subtitle_language(lang).is_err()
                    || !versions.as_array().is_some_and(|list| !list.is_empty())
                {
                    continue;
                }
                let name = versions
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find_map(|track| track["name"].as_str().filter(|name| !name.is_empty()))
                    .unwrap_or(lang);
                subtitles.push(SubtitleTrack {
                    lang: lang.clone(),
                    name: name.chars().take(80).collect(),
                    is_auto,
                });
                if subtitles.len() == MAX_SUBTITLES {
                    break 'tracks;
                }
            }
        }
    }

    let mut response = NativeBridgeResponse::MediaProbed {
        probe: MediaProbeResult {
            title: value["title"]
                .as_str()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or("download")
                .chars()
                .take(240)
                .collect(),
            source_url,
            extractor: value["extractor"]
                .as_str()
                .or_else(|| value["extractor_key"].as_str())
                .unwrap_or_default()
                .chars()
                .take(64)
                .collect(),
            duration_seconds: duration_seconds(&value["duration"]),
            thumbnail_url: value["thumbnail"]
                .as_str()
                .and_then(|url| bounded_http_url(url, 1024)),
            is_playlist,
            playlist_count,
            playlist_entries,
            playlist_items,
            formats,
            subtitles,
        },
    };
    let mut size = serde_json::to_vec(&response)?.len();
    let NativeBridgeResponse::MediaProbed { probe } = &mut response else {
        unreachable!();
    };
    while size >= MAX_NATIVE_MESSAGE_BYTES - 16 * 1024 {
        if !probe.playlist_items.is_empty() || !probe.playlist_entries.is_empty() {
            size -= pop_wire_bytes(&mut probe.playlist_items)?
                + pop_wire_bytes(&mut probe.playlist_entries)?;
        } else if !probe.subtitles.is_empty() {
            size -= pop_wire_bytes(&mut probe.subtitles)?;
        } else if !probe.formats.is_empty() {
            size -= pop_wire_bytes(&mut probe.formats)?;
        } else {
            return Err(MediaError::InvalidToolOutput {
                tool: "yt-dlp",
                kind: "probe frame size",
            });
        }
    }
    let NativeBridgeResponse::MediaProbed { probe } = response else {
        unreachable!();
    };
    Ok(probe)
}

fn pop_wire_bytes<T: serde::Serialize>(items: &mut Vec<T>) -> Result<usize, MediaError> {
    let Some(item) = items.pop() else {
        return Ok(0);
    };
    Ok(serde_json::to_vec(&item)?.len() + usize::from(!items.is_empty()))
}

fn playlist_entry_title(entry: &Value) -> String {
    entry["title"]
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .and_then(|title| sanitize_title_to_filename(clamp_playlist_title(title), "txt").ok())
        .and_then(|name| {
            let stem = name.strip_suffix(".txt")?;
            // Sanitization can prepend an underscore to a reserved Windows name.
            Some(
                clamp_playlist_title(stem)
                    .trim_end_matches(['.', ' '])
                    .to_owned(),
            )
        })
        .unwrap_or_default()
}

fn clamp_playlist_title(title: &str) -> &str {
    let end = title
        .char_indices()
        .nth(MAX_PLAYLIST_TITLE_CHARS)
        .map_or(title.len(), |(end, _)| end);
    &title[..end]
}

fn parse_playlist_entry(entry: &Value, position: u32, title: &str, youtube: bool) -> PlaylistEntry {
    let id = entry["id"].as_str().map(str::trim).filter(|id| {
        !id.is_empty() && id.chars().count() <= 128 && !id.chars().any(char::is_control)
    });
    let url = ["webpage_url", "url"]
        .into_iter()
        .filter_map(|key| entry[key].as_str())
        .find_map(|url| bounded_http_url(url, MAX_PLAYLIST_ITEMS_BYTES))
        .or_else(|| {
            let id = id.filter(|id| {
                (6..=32).contains(&id.len())
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            })?;
            if youtube || is_youtube_extractor(&entry["ie_key"]) {
                bounded_http_url(
                    &format!("https://www.youtube.com/watch?v={id}"),
                    MAX_PLAYLIST_ITEMS_BYTES,
                )
            } else {
                None
            }
        });
    let thumbnail_url = entry["thumbnail"]
        .as_str()
        .and_then(|url| bounded_http_url(url, MAX_PLAYLIST_THUMBNAIL_URL_BYTES))
        .or_else(|| {
            entry["thumbnails"]
                .as_array()?
                .iter()
                .rev()
                .find_map(|thumbnail| {
                    bounded_http_url(thumbnail["url"].as_str()?, MAX_PLAYLIST_THUMBNAIL_URL_BYTES)
                        .filter(|url| url.starts_with("https://"))
                })
        });

    PlaylistEntry {
        index: u32_value(&entry["playlist_index"])
            .filter(|&index| index > 0)
            .unwrap_or(position),
        id: id.map(str::to_owned),
        title: title.to_owned(),
        url,
        duration_seconds: duration_seconds(&entry["duration"]),
        thumbnail_url,
    }
}

fn is_youtube_extractor(value: &Value) -> bool {
    value.as_str().is_some_and(|name| {
        name.eq_ignore_ascii_case("youtube") || name.eq_ignore_ascii_case("youtube:tab")
    })
}

fn bounded_http_url(raw: &str, max_bytes: usize) -> Option<String> {
    if raw.len() > max_bytes {
        return None;
    }
    let url = validate_http_url(raw).ok()?;
    (url.as_str().len() <= max_bytes).then(|| url.to_string())
}

fn u32_value(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|n| n.try_into().ok())
}

fn duration_seconds(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|n| *n >= 0.0 && *n < u64::MAX as f64)
            .map(|n| n.floor() as u64)
    })
}

struct ProbedFormat {
    option: MediaFormatOption,
    language: Option<String>,
    is_dubbed: bool,
    is_direct: bool,
    is_hls: bool,
    is_drc: bool,
    exact_size: bool,
}

impl ProbedFormat {
    fn video_key(&self) -> (Option<u32>, Option<u32>, Option<String>, bool) {
        (
            self.option.height,
            fps_bucket(self.option.fps),
            self.option.video_codec.clone(),
            self.option.has_audio,
        )
    }

    fn preference(&self, other: &Self) -> Ordering {
        self.option
            .audio_track_kind
            .unwrap_or_default()
            .cmp(&other.option.audio_track_kind.unwrap_or_default())
            .then_with(|| self.is_direct.cmp(&other.is_direct))
            .then_with(|| other.is_hls.cmp(&self.is_hls))
            .then_with(|| other.is_drc.cmp(&self.is_drc))
            .then_with(|| self.exact_size.cmp(&other.exact_size))
            .then_with(|| {
                self.option
                    .filesize_bytes
                    .is_some()
                    .cmp(&other.option.filesize_bytes.is_some())
            })
            .then_with(|| {
                self.option
                    .bitrate_kbps
                    .unwrap_or_default()
                    .total_cmp(&other.option.bitrate_kbps.unwrap_or_default())
            })
            .then_with(|| self.option.filesize_bytes.cmp(&other.option.filesize_bytes))
    }
}

fn rank_formats(mut formats: Vec<ProbedFormat>) -> Vec<MediaFormatOption> {
    let direct_video: HashSet<_> = formats
        .iter()
        .filter(|f| f.is_direct && f.option.has_video)
        .map(ProbedFormat::video_key)
        .collect();
    let direct_audio = formats
        .iter()
        .any(|f| f.is_direct && f.option.is_audio_only && f.option.audio_codec.is_some());
    let audio_ids: HashSet<_> = formats
        .iter()
        .filter(|f| f.option.is_audio_only && !f.is_drc)
        .map(|f| (f.option.format_id.clone(), f.language.clone()))
        .collect();

    formats.sort_by(|a, b| b.preference(a));
    let mut seen_video = HashSet::new();
    let mut seen_audio = HashSet::new();
    formats.retain(|f| {
        let opt = &f.option;
        if opt.has_video {
            let key = f.video_key();
            !(f.is_hls && direct_video.contains(&key))
                && seen_video.insert((key, opt.container.clone()))
        } else {
            !(f.is_hls && direct_audio && opt.audio_codec.is_none() && f.language.is_none())
                && !(f.is_drc
                    && opt
                        .format_id
                        .split_once("-drc")
                        .is_some_and(|(base, suffix)| {
                            audio_ids.contains(&(format!("{base}{suffix}"), f.language.clone()))
                        }))
                && seen_audio.insert((
                    opt.audio_codec.clone(),
                    opt.container.clone(),
                    f.language.clone(),
                    f.is_dubbed,
                    opt.audio_track_kind == Some(AudioTrackKind::Descriptive),
                ))
        }
    });
    formats.sort_by(|a, b| {
        let (video_a, video_b) = (&a.option, &b.option);
        if video_a.is_audio_only && video_b.is_audio_only {
            return b.preference(a);
        }
        video_a
            .is_audio_only
            .cmp(&video_b.is_audio_only)
            .then_with(|| video_b.height.cmp(&video_a.height))
            .then_with(|| {
                video_b
                    .fps
                    .unwrap_or_default()
                    .total_cmp(&video_a.fps.unwrap_or_default())
            })
            .then_with(|| b.is_direct.cmp(&a.is_direct))
            .then_with(|| {
                video_b
                    .filesize_bytes
                    .is_some()
                    .cmp(&video_a.filesize_bytes.is_some())
            })
            .then_with(|| video_b.has_audio.cmp(&video_a.has_audio))
            .then_with(|| video_b.filesize_bytes.cmp(&video_a.filesize_bytes))
    });
    formats.into_iter().map(|f| f.option).collect()
}

fn parse_format(value: &Value) -> Option<ProbedFormat> {
    let format_id = value["format_id"].as_str()?;
    validate_format_id(format_id).ok()?;
    let raw_ext = value["ext"].as_str().unwrap_or_default();
    let protocol = value["protocol"].as_str().unwrap_or_default();
    let is_hls = protocol.starts_with("m3u8");
    if raw_ext.eq_ignore_ascii_case("mhtml")
        || protocol == "mhtml"
        || value["resolution"].as_str() == Some("storyboard")
    {
        return None;
    }
    let video_codec = codec(value["vcodec"].as_str());
    let audio_codec = codec(value["acodec"].as_str());
    let height = u32_value(&value["height"]).filter(|height| *height > 0);
    let fps = value["fps"]
        .as_f64()
        .filter(|fps| *fps > 0.0 && *fps <= f32::MAX as f64)
        .map(|fps| fps as f32);
    let has_ext = |key: &str| {
        value[key]
            .as_str()
            .is_some_and(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"))
    };
    let vcodec_none = value["vcodec"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("none"));
    let acodec_none = value["acodec"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("none"));
    let mut has_video =
        video_codec.is_some() || (!vcodec_none && (height.is_some() || has_ext("video_ext")));
    let mut has_audio = audio_codec.is_some()
        || (!acodec_none
            && (value["abr"].as_f64().is_some_and(|abr| abr > 0.0) || has_ext("audio_ext")));
    if !has_video && !has_audio && !raw_ext.is_empty() {
        let is_audio_ext = matches!(
            raw_ext.to_ascii_lowercase().as_str(),
            "mp3" | "m4a" | "aac" | "opus" | "ogg" | "oga" | "flac" | "wav" | "mka"
        );
        if (is_audio_ext || (is_hls && vcodec_none)) && !acodec_none {
            has_audio = true;
        } else if !is_audio_ext && !vcodec_none {
            has_video = true;
        }
    }
    if !has_video && !has_audio {
        return None;
    }
    let container = value["ext"]
        .as_str()
        .or_else(|| value["container"].as_str())
        .map(container_name)
        .unwrap_or_default();
    let mut label = if has_video {
        quality_label(height, fps).unwrap_or_else(|| "Video".to_owned())
    } else {
        "Audio".to_owned()
    };
    for tag in [&video_codec, &audio_codec].into_iter().flatten() {
        label.push_str(" · ");
        label.push_str(tag);
    }
    if !container.is_empty() {
        label.push_str(" · ");
        label.push_str(&container);
    }
    let language = value["language"]
        .as_str()
        .map(str::trim)
        .filter(|lang| !has_video && validate_subtitle_language(lang).is_ok())
        .map(|lang| lang.to_ascii_lowercase().replace('_', "-"));
    if let Some(lang) = &language {
        label.push_str(" · ");
        label.push_str(lang);
    }
    let note = value["format_note"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let is_drc = !has_video && format_id.split('-').any(|part| part == "drc");
    let preference = value["language_preference"].as_f64().unwrap_or_default();
    let is_dubbed =
        !has_video && (note.contains("dubbed") || (language.is_some() && preference < 0.0));
    let audio_track_kind = if has_video {
        None
    } else if note.contains("descript")
        || language
            .as_deref()
            .is_some_and(|lang| lang.ends_with("-desc"))
    {
        label.push_str(" · Descriptive");
        Some(AudioTrackKind::Descriptive)
    } else if note.contains("original") {
        Some(AudioTrackKind::Original)
    } else if note.contains("default") || preference > 0.0 {
        Some(AudioTrackKind::Default)
    } else if is_dubbed {
        Some(AudioTrackKind::Dubbed)
    } else {
        None
    };
    if is_dubbed && audio_track_kind != Some(AudioTrackKind::Descriptive) {
        label.push_str(" · Dubbed");
    }
    let exact = byte_count(&value["filesize"]);
    let bitrate = |key: &str| value[key].as_f64().filter(|rate| *rate > 0.0);
    let bitrate_kbps = if has_video {
        bitrate("tbr").or_else(|| bitrate("vbr"))
    } else {
        bitrate("abr").or_else(|| bitrate("tbr"))
    };
    Some(ProbedFormat {
        language,
        is_dubbed,
        is_direct: matches!(protocol, "http" | "https"),
        is_hls,
        is_drc,
        exact_size: exact.is_some(),
        option: MediaFormatOption {
            format_id: format_id.into(),
            label,
            container,
            video_codec,
            audio_codec,
            height,
            fps,
            filesize_bytes: exact.or_else(|| byte_count(&value["filesize_approx"])),
            bitrate_kbps,
            audio_track_kind,
            is_audio_only: has_audio && !has_video,
            has_video,
            has_audio,
        },
    })
}

fn byte_count(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0 && *n < u64::MAX as f64)
            .map(|n| n as u64)
    })
}

fn quality_label(height: Option<u32>, fps: Option<f32>) -> Option<String> {
    let height = height?;
    Some(match fps_bucket(fps) {
        Some(fps) => format!("{height}p{fps}"),
        None => format!("{height}p"),
    })
}

fn fps_bucket(fps: Option<f32>) -> Option<u32> {
    fps.filter(|fps| *fps >= 49.0).map(|fps| fps.round() as u32)
}

fn codec(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let first = raw.split(['.', ' ']).next()?.to_ascii_lowercase();
    let normalized = match first.as_str() {
        "" | "none" | "unknown" => return None,
        "av01" | "av1" => "AV1",
        "vp09" | "vp9" => "VP9",
        "vp08" | "vp8" => "VP8",
        "avc1" | "avc3" | "avc" | "h264" | "h"
            if first != "h" || raw.eq_ignore_ascii_case("h.264") =>
        {
            "H.264"
        }
        "hvc1" | "hev1" | "hevc" | "h265" | "dvhe" | "dvh1" => "HEVC",
        "h" if raw.eq_ignore_ascii_case("h.265") => "HEVC",
        "mp4a" | "aac" => "AAC",
        "ec-3" | "eac3" => "E-AC-3",
        "ac-3" | "ac3" => "AC-3",
        "alac" => "ALAC",
        "opus" => "Opus",
        "flac" => "FLAC",
        "mp3" => "MP3",
        "vorbis" => "Vorbis",
        _ if first.starts_with("pcm") => "PCM",
        _ => raw,
    };
    Some(normalized.to_owned())
}

fn container_name(raw: &str) -> String {
    let ext = raw.split('_').next().unwrap_or(raw);
    match ext.to_ascii_lowercase().as_str() {
        "webm" => "WebM".to_owned(),
        "opus" => "Opus".to_owned(),
        "matroska" => "MKV".to_owned(),
        _ => ext.to_ascii_uppercase(),
    }
}

/// Finds a concrete probed format by ID. Selector expressions are accepted by the download builder.
pub fn pick_format_option<'a>(
    probe: &'a MediaProbeResult,
    format_id: &str,
) -> Option<&'a MediaFormatOption> {
    validate_format_id(format_id).ok()?;
    probe
        .formats
        .iter()
        .find(|option| option.format_id == format_id)
}

pub fn quality_summary_tags(probe: &MediaProbeResult) -> Vec<String> {
    let mut qualities: Vec<_> = probe
        .formats
        .iter()
        .filter(|format| format.has_video)
        .filter_map(|format| Some((format.height?, format.fps.unwrap_or(0.0))))
        .collect();
    qualities.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.total_cmp(&a.1)));
    let mut tags = Vec::new();
    let mut seen = HashSet::new();
    for (height, fps) in qualities {
        if let Some(tag) = quality_label(Some(height), Some(fps)) {
            if seen.insert(tag.clone()) {
                tags.push(tag);
            }
        }
    }
    for format in &probe.formats {
        for tag in [
            format.video_codec.as_ref(),
            format.audio_codec.as_ref(),
            Some(&format.container),
        ]
        .into_iter()
        .flatten()
        {
            if !tag.is_empty() && seen.insert(tag.clone()) {
                tags.push(tag.clone());
            }
        }
    }
    tags
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPresetRecommendation {
    pub preset: AudioPreset,
    pub label: &'static str,
    pub container: MediaContainer,
    pub source_format_id: Option<String>,
}

pub fn audio_preset_recommendations(probe: &MediaProbeResult) -> Vec<AudioPresetRecommendation> {
    use AudioPreset::*;
    [
        (Mp3_320, "MP3 320 kbps", MediaContainer::Mp3, "MP3"),
        (M4aAac, "M4A (AAC)", MediaContainer::M4a, "AAC"),
        (Opus, "Opus", MediaContainer::Opus, "Opus"),
        (Flac, "FLAC", MediaContainer::Flac, "FLAC"),
        (Wav, "WAV", MediaContainer::Wav, "PCM"),
    ]
    .into_iter()
    .map(|(preset, label, container, codec)| {
        let source = probe
            .formats
            .iter()
            .filter(|format| format.has_audio)
            .max_by_key(|format| {
                (
                    format.audio_codec.as_deref() == Some(codec),
                    format.is_audio_only,
                    format.filesize_bytes,
                )
            });
        AudioPresetRecommendation {
            preset,
            label,
            container,
            source_format_id: source.map(|format| format.format_id.clone()),
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_support::RetryTool, ToolchainConfig};

    const URL: &str = "https://example.com/watch?v=[123]&t=%32";

    #[tokio::test]
    async fn probe_sets_user_agent_and_retries_http_403_once_over_ipv4() {
        let tool = RetryTool::build().await;
        let binary = crate::discover_ytdlp_only(ToolchainConfig {
            ytdlp_path: Some(tool.executable()),
            ..ToolchainConfig::default()
        })
        .await
        .unwrap();
        let mut options = ProbeOptions {
            headers: HashMap::from([("referer".into(), "https://example.com/watch".into())]),
            ..ProbeOptions::default()
        };

        for (case, attempts) in [
            ("probe-default", "default\n"),
            ("probe-retry", "default\nipv4\n"),
            ("probe-always-forbidden", "default\nipv4\n"),
            ("probe-custom", "default\n"),
        ] {
            if case == "probe-custom" {
                options
                    .headers
                    .insert("uSeR-aGeNt".into(), "Fixture browser".into());
            }
            let url = format!("https://example.com/{case}");
            let result = probe_url_with_binary(&binary, &url, options.clone()).await;
            if case == "probe-always-forbidden" {
                assert!(is_http_403_error(&result.unwrap_err()));
            } else {
                assert_eq!(result.unwrap().title, "Fixture video");
            }
            assert_eq!(tool.attempts(case), attempts, "{case}");
        }
    }

    #[test]
    fn ranks_direct_youtube_formats_and_drops_equivalent_hls_video() {
        let probe = parse_ytdlp_probe_json(r#"{"formats":[
            {"format_id":"312","protocol":"m3u8_native","height":720,"fps":59.94,"vcodec":"avc1","ext":"mp4"},
            {"format_id":"299","protocol":"https","height":720,"fps":60,"vcodec":"avc1","ext":"mp4","filesize":91000000,"tbr":1500},
            {"format_id":"299-low","protocol":"https","height":720,"fps":60,"vcodec":"avc1","ext":"mp4","filesize":90000000,"vbr":1200},
            {"format_id":"299-approx","protocol":"https","height":720,"fps":60,"vcodec":"avc1","ext":"mp4","filesize_approx":99000000,"tbr":2000},
            {"format_id":"299-segments","protocol":"http_dash_segments","height":720,"fps":60,"vcodec":"avc1","ext":"mp4","filesize":99000000,"tbr":2500},
            {"format_id":"18","protocol":"https","height":720,"fps":60,"vcodec":"avc1","acodec":"aac","ext":"mp4","filesize":92000000},
            {"format_id":"605","protocol":"m3u8","height":360,"fps":30,"vcodec":"vp09","ext":"mp4"},
            {"format_id":"243","protocol":"http","height":360,"fps":25,"vcodec":"vp9","ext":"webm"},
            {"format_id":"600","protocol":"m3u8","height":720,"fps":60,"vcodec":"vp9","ext":"webm","filesize":99000000},
            {"format_id":"301","protocol":"m3u8","height":720,"fps":30,"vcodec":"avc1","ext":"mp4"}
        ]}"#, URL).unwrap();
        let ids: Vec<_> = probe.formats.iter().map(|f| f.format_id.as_str()).collect();
        assert_eq!(ids, ["18", "299", "600", "301", "243"]);
        assert_eq!(probe.formats[1].label, "720p60 · H.264 · MP4");
        assert_eq!(probe.formats[1].filesize_bytes, Some(91_000_000));
    }

    #[test]
    fn keeps_muxed_hls_alongside_direct_video_only() {
        let probe = parse_ytdlp_probe_json(r#"{"formats":[
            {"format_id":"direct","ext":"mp4","protocol":"https","height":720,"fps":30,"vcodec":"avc1","acodec":"none"},
            {"format_id":"hls-video","ext":"mp4","protocol":"m3u8_native","height":720,"fps":30,"vcodec":"avc1","acodec":"none"},
            {"format_id":"hls-muxed","ext":"mp4","protocol":"m3u8_native","height":720,"fps":30,"vcodec":"avc1","acodec":"mp4a.40.2"}
        ]}"#, URL).unwrap();
        assert_eq!(probe.formats.len(), 2);
        assert!(probe
            .formats
            .iter()
            .any(|f| f.format_id == "direct" && !f.has_audio));
        assert!(probe
            .formats
            .iter()
            .any(|f| f.format_id == "hls-muxed" && f.has_audio));
    }

    #[test]
    fn large_playlist_probe_fits_native_frame_and_preserves_count() {
        use slipstream_protocol::{NativeBridgeResponse, MAX_NATIVE_MESSAGE_BYTES};

        let mut entries = vec![serde_json::json!({ "title": "🎬".repeat(512) }); 6000];
        entries[0]["title"] = serde_json::json!(format!("CON.{}", "a".repeat(170)));
        let json = serde_json::json!({ "_type": "playlist", "entries": entries }).to_string();
        let probe = parse_ytdlp_probe_json(&json, URL).unwrap();
        assert_eq!(probe.playlist_count, Some(6000));
        assert_eq!(probe.playlist_entries.len(), MAX_PLAYLIST_PREVIEW_ENTRIES);
        assert_eq!(probe.playlist_items.len(), MAX_PLAYLIST_PREVIEW_ENTRIES);
        assert!(probe.playlist_items[0].title.starts_with("_CON."));
        assert!(probe.playlist_entries.iter().all(|title| {
            !title.is_empty() && title.chars().count() <= MAX_PLAYLIST_TITLE_CHARS
        }));
        let response = serde_json::to_vec(&NativeBridgeResponse::MediaProbed { probe }).unwrap();
        assert!(response.len() < MAX_NATIVE_MESSAGE_BYTES / 2);
    }

    #[test]
    fn oversized_playlist_metadata_preserves_legacy_preview_and_frame_headroom() {
        let entry = serde_json::json!({
            "id": "🎬".repeat(128), "title": "🎬".repeat(160),
            "url": format!("https://example.com/{}", "a".repeat(2000)),
            "thumbnail": format!("https://example.com/{}", "b".repeat(450))
        });
        let json = serde_json::json!({ "entries": vec![entry; 768] }).to_string();
        let probe = parse_ytdlp_probe_json(&json, URL).unwrap();
        assert_eq!(probe.playlist_count, Some(768));
        assert_eq!(probe.playlist_entries.len(), MAX_PLAYLIST_PREVIEW_ENTRIES);
        assert!(!probe.playlist_items.is_empty());
        assert!(probe.playlist_items.len() < MAX_PLAYLIST_PREVIEW_ENTRIES);
        assert_eq!(
            probe.playlist_items.last().unwrap().index as usize,
            probe.playlist_items.len()
        );
        let response =
            serde_json::to_vec(&slipstream_protocol::NativeBridgeResponse::MediaProbed { probe })
                .unwrap();
        assert!(response.len() < MAX_NATIVE_MESSAGE_BYTES / 2);
    }

    #[test]
    fn probe_wire_budget_covers_all_fields_and_prefers_manual_subtitles() {
        use serde_json::{json, Map};
        let tracks = (0..200)
            .map(|i| (format!("en-{i}"), json!([{ "name": "\u{1}".repeat(4000) }])))
            .collect::<Map<_, _>>();
        let formats = (1..=300)
            .map(|height| {
                json!({
                    "format_id": format!("id-{height}"), "height": height, "vcodec": "avc1",
                    "ext": "mp4", "format_note": "format ".repeat(512)
                })
            })
            .collect::<Vec<_>>();
        let json = json!({
            "title": "🎬".repeat(300), "extractor": "x".repeat(300),
            "thumbnail": format!("https://example.com/{}", "t".repeat(2048)),
            "entries": vec![json!({ "title": "🎬".repeat(160) }); 600],
            "formats": formats, "subtitles": &tracks, "automatic_captions": &tracks,
        });
        // This long source forces the overall frame budget to trim the
        // playlist even after each metadata field has been capped.
        let source = format!("https://example.com/{}", "a".repeat(930_000));
        let probe = parse_ytdlp_probe_json(&json.to_string(), &source).unwrap();
        assert_eq!(probe.title.chars().count(), 240);
        assert_eq!(probe.extractor.chars().count(), 64);
        assert!(probe.thumbnail_url.is_none());
        assert_eq!(probe.formats.len(), 64);
        assert_eq!(probe.subtitles.len(), 64);
        assert!(probe
            .subtitles
            .iter()
            .all(|track| !track.is_auto && track.name.chars().count() == 80));
        assert_eq!(probe.playlist_count, Some(600));
        assert!(probe.playlist_items.len() < MAX_PLAYLIST_PREVIEW_ENTRIES);
        let encoded = serde_json::to_vec(&NativeBridgeResponse::MediaProbed { probe }).unwrap();
        assert!(encoded.len() < MAX_NATIVE_MESSAGE_BYTES - 16 * 1024);
    }

    #[test]
    fn parses_playlist_entry_links_and_skips_invalid_fields() {
        let playlist = serde_json::json!({
            "extractor": "YouTube:Tab",
            "entries": [
                {"id":" abc_123 ", "title":"First/Clip", "url":"abc_123", "duration":42.9},
                null,
                {"id":"xyz_456", "title":"Explicit URL", "playlist_index":7,
                 "webpage_url":"https://example.com/watch", "url":"https://example.com/direct",
                 "thumbnail":"https://example.com/primary.jpg"},
                {"id":"bad\u{7f}id", "title":"Last", "playlist_index":0,
                 "webpage_url":"file:///bad", "url":"https://example.com/fallback",
                 "duration":-1, "thumbnail":format!("https://example.com/{}", "x".repeat(512)),
                 "thumbnails":[{"url":"https://example.com/first.jpg"}, {"url":"https://example.com/last.jpg"},
                               {"url":"http://example.com/plain.jpg"}, {"url":"file:///bad.jpg"}]},
                {"id":"bad?id", "url":"javascript:alert(1)", "thumbnail":"file:///bad.jpg"},
                {"id":"a".repeat(129)}
            ]
        });
        let probe = parse_ytdlp_probe_json(&playlist.to_string(), URL).unwrap();
        assert_eq!(probe.playlist_count, Some(6));
        assert_eq!(
            probe.playlist_entries,
            ["First_Clip", "", "Explicit URL", "Last", "", ""]
        );
        let [first, explicit, last, unsafe_entry, overlong] = probe.playlist_items.as_slice()
        else {
            panic!("expected five structured entries");
        };
        assert_eq!(first.index, 1);
        assert_eq!(first.id.as_deref(), Some("abc_123"));
        assert_eq!(first.title, "First_Clip");
        assert_eq!(
            first.url.as_deref(),
            Some("https://www.youtube.com/watch?v=abc_123")
        );
        assert_eq!(first.duration_seconds, Some(42));
        assert_eq!(explicit.index, 7);
        assert_eq!(explicit.url.as_deref(), Some("https://example.com/watch"));
        assert_eq!(
            explicit.thumbnail_url.as_deref(),
            Some("https://example.com/primary.jpg")
        );
        assert_eq!(last.index, 4);
        assert!(last.id.is_none());
        assert!(last.duration_seconds.is_none());
        assert_eq!(last.url.as_deref(), Some("https://example.com/fallback"));
        assert_eq!(
            last.thumbnail_url.as_deref(),
            Some("https://example.com/last.jpg")
        );
        assert!(unsafe_entry.url.is_none());
        assert!(unsafe_entry.thumbnail_url.is_none());
        assert!(overlong.id.is_none());

        let untyped = parse_ytdlp_probe_json(
            r#"{"entries":[{"id":"abc123"},{"id":"abc123","ie_key":"Youtube"}]}"#,
            URL,
        )
        .unwrap();
        assert!(untyped.playlist_items[0].url.is_none());
        assert_eq!(
            untyped.playlist_items[1].url.as_deref(),
            Some("https://www.youtube.com/watch?v=abc123")
        );
    }

    #[test]
    fn deduplicates_audio_variants_without_discarding_hls_only_choices() {
        let probe = parse_ytdlp_probe_json(r#"{"formats":[
            {"format_id":"233","protocol":"m3u8","vcodec":"none","acodec":"unknown","ext":"mp4"},
            {"format_id":"234","protocol":"m3u8_native","vcodec":"none","acodec":null,"ext":"mp4"},
            {"format_id":"140","protocol":"https","vcodec":"none","acodec":"aac","ext":"m4a","filesize":100,"abr":128},
            {"format_id":"140-drc","protocol":"https","vcodec":"none","acodec":"aac","ext":"m4a","filesize":200,"abr":160},
            {"format_id":"140-hls","protocol":"m3u8","vcodec":"none","acodec":"aac","ext":"m4a","filesize":300,"abr":300},
            {"format_id":"249","protocol":"https","vcodec":"none","acodec":"opus","ext":"webm","filesize":400,"abr":50},
            {"format_id":"251","protocol":"https","vcodec":"none","acodec":"opus","ext":"webm","filesize":300,"abr":160},
            {"format_id":"251-drc","protocol":"https","vcodec":"none","acodec":"opus","ext":"webm","filesize":500,"abr":170},
            {"format_id":"flac-drc","protocol":"https","vcodec":"none","acodec":"flac","ext":"flac"}
        ]}"#, URL).unwrap();
        let ids: Vec<_> = probe.formats.iter().map(|f| f.format_id.as_str()).collect();
        assert_eq!(ids, ["251", "140", "flac-drc"]);
        assert_eq!(probe.formats[0].bitrate_kbps, Some(160.0));

        let hls = parse_ytdlp_probe_json(r#"{"formats":[
            {"format_id":"high","protocol":"m3u8_native","height":720,"vcodec":"avc1","ext":"mp4","tbr":2000},
            {"format_id":"low","protocol":"m3u8","height":720,"vcodec":"avc1","ext":"mp4","tbr":1000},
            {"format_id":"medium","protocol":"m3u8","height":480,"vcodec":"avc1","ext":"mp4"},
            {"format_id":"audio","protocol":"m3u8","vcodec":"none","acodec":"unknown","ext":"mp4"}
        ]}"#, URL).unwrap();
        let ids: Vec<_> = hls.formats.iter().map(|f| f.format_id.as_str()).collect();
        assert_eq!(ids, ["high", "medium", "audio"]);
        assert!(hls.formats[2].is_audio_only);
    }

    #[test]
    fn preserves_distinct_audio_languages_and_ranks_original_first() {
        let probe = parse_ytdlp_probe_json(
            r#"{"formats":[
                {"format_id":"140-2","protocol":"https","vcodec":"none","acodec":"aac","ext":"m4a","language":"es","format_note":"Spanish (dubbed) (default)","language_preference":5,"abr":256},
                {"format_id":"140-1","protocol":"https","vcodec":"none","acodec":"aac","ext":"m4a","language":" EN_us ","format_note":"English (original)","language_preference":10,"abr":128},
                {"format_id":"139-1","protocol":"https","vcodec":"none","acodec":"aac","ext":"m4a","language":"en-US","abr":48},
                {"format_id":"140-drc-1","protocol":"https","vcodec":"none","acodec":"aac","ext":"m4a","language":"en-US","language_preference":10,"abr":320},
                {"format_id":"140-3","protocol":"https","vcodec":"none","acodec":"aac","ext":"m4a","language":"fr","language_preference":-1,"abr":288},
                {"format_id":"140-5","protocol":"https","vcodec":"none","acodec":"aac","ext":"m4a","language":"en-US","format_note":"English (dubbed)","abr":320},
                {"format_id":"140-4","protocol":"https","vcodec":"none","acodec":"aac","ext":"m4a","language":"de","format_note":"German (descriptive)","abr":296}
            ]}"#,
            URL,
        )
        .unwrap();
        let ids: Vec<_> = probe.formats.iter().map(|f| f.format_id.as_str()).collect();
        assert_eq!(ids, ["140-1", "140-2", "140-5", "140-3", "140-4"]);
        assert_eq!(probe.formats[0].label, "Audio · AAC · M4A · en-us");
        assert_eq!(
            probe.formats[0].audio_track_kind,
            Some(AudioTrackKind::Original)
        );
        assert_eq!(probe.formats[1].label, "Audio · AAC · M4A · es · Dubbed");
        assert_eq!(probe.formats[2].label, "Audio · AAC · M4A · en-us · Dubbed");
    }

    #[test]
    fn normalizes_video_audio_and_subtitles() {
        let probe = parse_ytdlp_probe_json(r#"{
            "title": "東京建築_4K_HDR",
            "extractor": "youtube",
            "duration": 98.75,
            "thumbnail": "https://example.com/img?a=1&b=2",
            "formats": [
                {"format_id":"18","height":480,"fps":30,"vcodec":"avc1.0","acodec":"mp4a.40.2","ext":"mp4","filesize_approx":2400},
                {"format_id":"22","height":720,"fps":30,"vcodec":"h264","acodec":"mp3","ext":"mkv"},
                {"format_id":"140","vcodec":"none","acodec":"mp4a.40.2","ext":"m4a","filesize":1500},
                {"format_id":"251","vcodec":"none","acodec":"opus","ext":"webm"},
                {"format_id":"313","height":2160,"fps":59.94,"vcodec":"av01.0.12M.08","acodec":"none","ext":"webm","filesize":5000,"filesize_approx":4500},
                {"format_id":"315","height":2160,"fps":30,"vcodec":"vp09.02","acodec":"none","ext":"webm"},
                {"format_id":"308","height":1440,"fps":60,"vcodec":"hvc1.1.6","ext":"mp4"},
                {"format_id":"271","height":1440,"fps":30,"vcodec":"hev1.1.6","ext":"mp4"},
                {"format_id":"299","height":1080,"fps":60,"vcodec":"avc1.0","ext":"mp4"},
                {"format_id":"137","height":1080,"fps":30,"vcodec":"avc1.0","ext":"mp4"},
                {"format_id":"298","height":720,"fps":60,"vcodec":"avc1.0","ext":"mp4"},
                {"format_id":"flac-audio","vcodec":"none","acodec":"flac","ext":"flac"},
                {"format_id":"vorbis-audio","vcodec":"none","acodec":"vorbis","ext":"ogg"},
                {"format_id":"-f bad","height":2160,"vcodec":"av01"}
            ],
            "subtitles":{"en":[{"name":"English","ext":"vtt"}],"en.*":[{"ext":"srt"}]},
            "automatic_captions":{"ar":[{"ext":"srt"}],"en":[{"name":"English (automatic)","ext":"vtt"}]}
        }"#, URL).unwrap();
        assert_eq!(probe.title, "東京建築_4K_HDR");
        assert_eq!(probe.source_url, URL);
        assert_eq!(probe.extractor, "youtube");
        assert_eq!(probe.duration_seconds, Some(98));
        assert_eq!(
            probe.thumbnail_url.as_deref(),
            Some("https://example.com/img?a=1&b=2")
        );
        assert_eq!(probe.formats.len(), 13);
        assert_eq!(probe.formats[0].label, "2160p60 · AV1 · WebM");
        assert_eq!(probe.formats[0].filesize_bytes, Some(5000));
        let video = pick_format_option(&probe, "18").unwrap();
        assert_eq!(video.filesize_bytes, Some(2400));
        assert!(video.has_video && video.has_audio);
        assert_eq!(video.video_codec.as_deref(), Some("H.264"));
        assert!(pick_format_option(&probe, "251").unwrap().is_audio_only);
        assert!(pick_format_option(&probe, "-f bad").is_none());
        let tags = quality_summary_tags(&probe);
        assert_eq!(
            &tags[..9],
            [
                "2160p60", "2160p", "1440p60", "1440p", "1080p60", "1080p", "720p60", "720p",
                "480p"
            ]
        );
        for tag in [
            "AV1", "VP9", "H.264", "HEVC", "AAC", "Opus", "FLAC", "MP3", "Vorbis", "MP4", "MKV",
            "WebM", "M4A",
        ] {
            assert_eq!(
                tags.iter().filter(|candidate| *candidate == tag).count(),
                1,
                "{tag}"
            );
        }
        assert_eq!(probe.subtitles.len(), 3);
        assert!(probe
            .subtitles
            .iter()
            .any(|track| track.lang == "en" && track.name == "English" && !track.is_auto));
        assert!(probe
            .subtitles
            .iter()
            .any(|track| track.lang == "ar" && track.name == "ar" && track.is_auto));
        let audio = audio_preset_recommendations(&probe);
        assert_eq!(
            audio.iter().map(|option| option.preset).collect::<Vec<_>>(),
            [
                AudioPreset::Mp3_320,
                AudioPreset::M4aAac,
                AudioPreset::Opus,
                AudioPreset::Flac,
                AudioPreset::Wav
            ]
        );
        assert_eq!(audio[1].source_format_id.as_deref(), Some("140"));
        assert_eq!(audio[2].source_format_id.as_deref(), Some("251"));
    }

    #[test]
    fn parses_flat_playlists_and_direct_media() {
        let flat = parse_ytdlp_probe_json(
            r#"{"_type":"playlist","title":"Clips","entries":[{"title":"  First: Clip  "},null,{"title":"../CON"}],"thumbnail":"file:///clip.mp4"}"#, URL,
        ).unwrap();
        assert!(flat.is_playlist);
        assert_eq!(flat.playlist_count, Some(3));
        assert!(flat.thumbnail_url.is_none());
        assert!(flat.formats.is_empty());
        assert_eq!(flat.playlist_entries, ["First_ Clip", "", "_CON"]);
        let direct = parse_ytdlp_probe_json(
            r#"{"duration":12,"format_id":"http-480","height":480,"vcodec":"avc1","ext":"mp4"}"#,
            URL,
        )
        .unwrap();
        assert_eq!(direct.duration_seconds, Some(12));
        assert_eq!(direct.formats.len(), 1);
        assert!(direct.playlist_entries.is_empty());
        let generic_direct = parse_ytdlp_probe_json(
            r#"{"title":"","extractor":"generic","formats":[{"format_id":"mp4","ext":"mp4","vcodec":null,"video_ext":"mp4","audio_ext":"none"},{"format_id":"sb0","ext":"mhtml","vcodec":"none","acodec":"none","protocol":"mhtml"}]}"#,
            URL,
        )
        .unwrap();
        assert_eq!(generic_direct.title, "download");
        assert_eq!(generic_direct.formats.len(), 1);
        assert!(generic_direct.formats[0].has_video);
        assert!(!generic_direct.formats[0].is_audio_only);
        assert_eq!(generic_direct.formats[0].label, "Video · MP4");
        let sparse = parse_ytdlp_probe_json(
            r#"{"extractor":"youtube","formats":[
                {"format_id":"140","ext":"m4a","vcodec":"none","acodec":null},
                {"format_id":"137","ext":"mp4","vcodec":null,"acodec":"none"},
                {"format_id":"bad-audio","ext":"m4a","vcodec":null,"acodec":"none"},
                {"format_id":"bad-video","ext":"mp4","vcodec":"none","acodec":"none"}
            ]}"#,
            URL,
        )
        .unwrap();
        assert_eq!(sparse.formats.len(), 2);
        assert!(sparse
            .formats
            .iter()
            .any(|f| f.format_id == "140" && f.is_audio_only && f.has_audio));
        assert!(sparse
            .formats
            .iter()
            .any(|f| f.format_id == "137" && f.has_video && !f.is_audio_only));
        assert!(parse_ytdlp_probe_json("[]", URL).is_err());
        assert!(parse_ytdlp_probe_json("{}", "file:///clip.mp4").is_err());
    }
}
