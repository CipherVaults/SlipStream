use std::{collections::HashSet, time::Duration};

use serde_json::Value;
use slipstream_protocol::{
    validate_http_url, AudioPreset, MediaContainer, MediaFormatOption, MediaProbeResult,
    SubtitleTrack,
};

use crate::{
    args::{validate_format_id, validate_subtitle_language},
    toolchain::{capture, command},
    BuildYtDlpArgs, MediaError, MediaToolchain,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct ProbeOptions {
    pub playlist: bool,
}

pub async fn probe_url(
    tools: &MediaToolchain,
    url: &str,
    options: ProbeOptions,
) -> Result<MediaProbeResult, MediaError> {
    let args = BuildYtDlpArgs::probe(options.playlist)
        .ffmpeg_location(&tools.ffmpeg().path)?
        .build(url)?;
    let binary = tools.require_ytdlp()?;
    let mut cmd = command(&binary.path, "yt-dlp");
    cmd.args(args);
    let output = capture(
        cmd,
        &binary.path,
        "yt-dlp",
        Duration::from_secs(90),
        32 * 1024 * 1024,
    )
    .await?;
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

    let mut formats: Vec<_> = value["formats"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_format)
        .collect();
    if formats.is_empty() && !is_playlist {
        formats.extend(parse_format(&value));
    }
    formats.sort_by(|a, b| {
        b.height
            .cmp(&a.height)
            .then_with(|| b.fps.unwrap_or(0.0).total_cmp(&a.fps.unwrap_or(0.0)))
            .then_with(|| b.has_audio.cmp(&a.has_audio))
    });

    let mut subtitles = Vec::new();
    for (key, is_auto) in [("subtitles", false), ("automatic_captions", true)] {
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
                    name: name.to_owned(),
                    is_auto,
                });
            }
        }
    }

    Ok(MediaProbeResult {
        title: value["title"]
            .as_str()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .unwrap_or("download")
            .to_owned(),
        source_url,
        extractor: value["extractor"]
            .as_str()
            .or_else(|| value["extractor_key"].as_str())
            .unwrap_or_default()
            .to_owned(),
        duration_seconds: duration_seconds(&value["duration"]),
        thumbnail_url: value["thumbnail"]
            .as_str()
            .and_then(|url| validate_http_url(url).ok())
            .map(|url| url.to_string()),
        is_playlist,
        playlist_count,
        formats,
        subtitles,
    })
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

fn parse_format(value: &Value) -> Option<MediaFormatOption> {
    let format_id = value["format_id"].as_str()?;
    validate_format_id(format_id).ok()?;
    let raw_ext = value["ext"].as_str().unwrap_or_default();
    if raw_ext.eq_ignore_ascii_case("mhtml")
        || value["protocol"].as_str() == Some("mhtml")
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
        if is_audio_ext && !acodec_none {
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
    Some(MediaFormatOption {
        format_id: format_id.into(),
        label,
        container,
        video_codec,
        audio_codec,
        height,
        fps,
        filesize_bytes: byte_count(&value["filesize"])
            .or_else(|| byte_count(&value["filesize_approx"])),
        is_audio_only: has_audio && !has_video,
        has_video,
        has_audio,
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
    Some(match fps.filter(|fps| *fps >= 49.0) {
        Some(fps) => format!("{height}p{:.0}", fps.round()),
        None => format!("{height}p"),
    })
}

fn codec(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let first = raw.split(['.', ' ']).next()?.to_ascii_lowercase();
    let normalized = match first.as_str() {
        "" | "none" => return None,
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

    const URL: &str = "https://example.com/watch?v=[123]&t=%32";

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
            r#"{"_type":"playlist","title":"Clips","entries":[{},null,{}],"thumbnail":"file:///clip.mp4"}"#, URL,
        ).unwrap();
        assert!(flat.is_playlist);
        assert_eq!(flat.playlist_count, Some(3));
        assert!(flat.thumbnail_url.is_none());
        assert!(flat.formats.is_empty());
        let direct = parse_ytdlp_probe_json(
            r#"{"duration":12,"format_id":"http-480","height":480,"vcodec":"avc1","ext":"mp4"}"#,
            URL,
        )
        .unwrap();
        assert_eq!(direct.duration_seconds, Some(12));
        assert_eq!(direct.formats.len(), 1);
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
