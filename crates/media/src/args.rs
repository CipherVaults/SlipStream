use std::{ffi::OsString, path::Path};

use slipstream_protocol::{validate_http_url, validate_safe_filename_component, MediaContainer};

use crate::MediaError;

const YTDLP_FLAGS: &[&str] = &[
    "--ignore-config",
    "--no-config-locations",
    "--no-cache-dir",
    "--no-exec",
    "--no-warnings",
    "--dump-single-json",
    "--no-playlist",
    "--yes-playlist",
    "--flat-playlist",
    "--playlist-items",
    "-f",
    "--ffmpeg-location",
    "-o",
    "--newline",
    "--progress",
    "--progress-template",
    "--write-subs",
    "--write-auto-subs",
    "--sub-langs",
    "--sub-format",
    "--write-thumbnail",
    "--convert-thumbnails",
    "--add-metadata",
    "--",
];
const FFMPEG_FLAGS: &[&str] = &[
    "-hide_banner",
    "-nostdin",
    "-y",
    "-progress",
    "-nostats",
    "-i",
    "-map",
    "-c",
    "-c:v",
    "-c:a",
    "-c:s",
    "-b:a",
    "-compression_level",
    "-id3v2_version",
    "-metadata",
    "-movflags",
    "-vn",
    "-af",
    "-f",
    "--",
];

pub(crate) const DOWNLOAD_PROGRESS_PREFIX: &str = "SLIPSTREAM|";
pub(crate) const LOUDNORM_FILTER: &str = "loudnorm=I=-16:TP=-1.5:LRA=11";
const DOWNLOAD_TEMPLATE: &str = "media-%(autonumber)06d.%(ext)s";

pub fn validate_ytdlp_flag(flag: &str) -> Result<(), MediaError> {
    if YTDLP_FLAGS.contains(&flag) {
        Ok(())
    } else {
        Err(MediaError::argument("yt-dlp flag", flag))
    }
}

pub fn validate_ffmpeg_flag(flag: &str) -> Result<(), MediaError> {
    let indexed = ["-disposition:v:", "-metadata:s:s:"].iter().any(|prefix| {
        flag.strip_prefix(prefix).is_some_and(|index| {
            index.parse::<u32>().is_ok() && index.bytes().all(|b| b.is_ascii_digit())
        })
    });
    if FFMPEG_FLAGS.contains(&flag) || indexed {
        Ok(())
    } else {
        Err(MediaError::argument("FFmpeg flag", flag))
    }
}

pub fn validate_ffmpeg_audio_filter(filter: &str) -> Result<(), MediaError> {
    if filter == LOUDNORM_FILTER {
        Ok(())
    } else {
        Err(MediaError::argument("FFmpeg audio filter", filter))
    }
}

/// Accepts an ID or a selection composed of IDs using yt-dlp's '+' and '/' operators.
pub fn validate_format_id(format: &str) -> Result<(), MediaError> {
    let valid = format.len() <= 256
        && format.split(['+', '/']).all(|part| {
            part.as_bytes()
                .first()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        });
    if valid {
        Ok(())
    } else {
        Err(MediaError::argument("format ID", format))
    }
}

pub fn validate_playlist_items(items: &str) -> Result<(), MediaError> {
    let index =
        |s: &str| s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u32>().is_ok_and(|n| n > 0);
    if items.len() <= 512
        && items.split(',').all(|part| match part.split_once('-') {
            Some((first, last)) => index(first) && index(last),
            None => index(part),
        })
    {
        Ok(())
    } else {
        Err(MediaError::argument("playlist items", items))
    }
}

pub fn validate_subtitle_language(lang: &str) -> Result<(), MediaError> {
    if lang.len() <= 64
        && lang
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && lang
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(MediaError::argument("subtitle language", lang))
    }
}

/// Validates a literal output filename. Template substitutions are generated inside this module.
pub fn validate_output_template(template: &str) -> Result<(), MediaError> {
    validate_filename(template)?;
    if template.starts_with('-')
        || template.contains("..")
        || template.contains("$(")
        || template.contains(['%', '\u{0060}'])
    {
        return Err(MediaError::argument("yt-dlp output template", template));
    }
    Ok(())
}

pub(crate) fn validate_filename(name: &str) -> Result<(), MediaError> {
    validate_safe_filename_component(name)?;
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CONIN$" | "CONOUT$")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|rest| matches!(rest, "¹" | "²" | "³"))
        });
    if reserved {
        return Err(slipstream_protocol::PathSecurityError::ReservedDeviceName.into());
    }
    Ok(())
}

/// Validates a local path before handing it to FFmpeg's URL-based argument parser.
pub fn validate_local_path(path: &Path) -> Result<(), MediaError> {
    let raw = path
        .to_str()
        .ok_or_else(|| MediaError::argument("local path", path.to_string_lossy()))?;
    let invalid = || MediaError::argument("local path", raw);

    // Windows canonicalization returns drive paths with the verbatim prefix.
    // Device paths and UNC paths are excluded, including verbatim UNC paths.
    let raw_path = if cfg!(windows) {
        raw.strip_prefix(r"\\?\").unwrap_or(raw)
    } else {
        raw
    };
    let bytes = raw_path.as_bytes();
    let has_drive = cfg!(windows)
        && bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':');
    let path_body = if has_drive {
        let tail = &raw_path[2..];
        tail.strip_prefix(['/', '\\']).ok_or_else(invalid)?
    } else {
        if raw.starts_with(r"\\?\")
            || raw_path.starts_with(['\\', '-'])
            || raw_path.starts_with("//")
        {
            return Err(invalid());
        }
        raw_path.strip_prefix('/').unwrap_or(raw_path)
    };
    // Validate components lexically, before Path can normalize away any traversal.
    for component in path_body.split(['/', '\\']) {
        validate_filename(component)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct BuildYtDlpArgs {
    args: Vec<OsString>,
}

impl BuildYtDlpArgs {
    pub fn probe(playlist: bool) -> Self {
        let mut builder = Self::base();
        builder.flag("--dump-single-json");
        builder.flag(if playlist {
            "--flat-playlist"
        } else {
            "--no-playlist"
        });
        builder
    }

    pub fn download(playlist: bool) -> Self {
        let mut builder = Self::base();
        builder.flag(if playlist {
            "--yes-playlist"
        } else {
            "--no-playlist"
        });
        builder.flag("--newline");
        builder.flag("--progress");
        builder.option(
            "--progress-template",
            concat!(
                "download:SLIPSTREAM|",
                "%(progress.downloaded_bytes)s|%(progress.total_bytes)s|",
                "%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress.eta)s",
            ),
        );
        builder.option("-o", DOWNLOAD_TEMPLATE);
        builder
    }

    pub fn format(mut self, format: &str) -> Result<Self, MediaError> {
        validate_format_id(format)?;
        self.option("-f", format);
        Ok(self)
    }

    pub fn playlist_items(mut self, items: &str) -> Result<Self, MediaError> {
        validate_playlist_items(items)?;
        self.option("--playlist-items", items);
        Ok(self)
    }

    pub fn output_template(mut self, filename: &str) -> Result<Self, MediaError> {
        validate_output_template(filename)?;
        if let Some(index) = self.args.iter().position(|arg| arg == "-o") {
            self.args[index + 1] = filename.into();
        } else {
            self.option("-o", filename);
        }
        Ok(self)
    }

    pub fn ffmpeg_location(mut self, binary: &Path) -> Result<Self, MediaError> {
        validate_local_path(binary)?;
        if !binary.is_absolute() {
            return Err(MediaError::argument(
                "FFmpeg executable path",
                binary.to_string_lossy(),
            ));
        }
        self.option("--ffmpeg-location", binary);
        Ok(self)
    }

    pub fn subtitles(mut self, languages: &[String]) -> Result<Self, MediaError> {
        if !languages.is_empty() {
            for lang in languages {
                validate_subtitle_language(lang)?;
            }
            // yt-dlp treats each language setting as a regular expression.
            let patterns: Vec<_> = languages.iter().map(|lang| format!("^{lang}$")).collect();
            self.flag("--write-subs");
            self.flag("--write-auto-subs");
            self.option("--sub-langs", patterns.join(","));
            self.option("--sub-format", "srt/vtt");
        }
        Ok(self)
    }

    pub fn write_thumbnail(mut self) -> Self {
        self.flag("--write-thumbnail");
        self
    }

    pub fn convert_thumbnail_to_png(mut self) -> Self {
        self.option("--convert-thumbnails", "png");
        self
    }

    pub fn add_metadata(mut self) -> Self {
        self.flag("--add-metadata");
        self
    }

    pub fn build(mut self, url: &str) -> Result<Vec<OsString>, MediaError> {
        let url = validate_http_url(url)?;
        self.flag("--");
        self.args.push(url.as_str().into());
        Ok(self.args)
    }

    fn base() -> Self {
        let mut builder = Self { args: Vec::new() };
        for flag in [
            "--ignore-config",
            "--no-config-locations",
            "--no-cache-dir",
            "--no-exec",
            "--no-warnings",
        ] {
            builder.flag(flag);
        }
        builder
    }

    fn flag(&mut self, flag: &str) {
        debug_assert!(validate_ytdlp_flag(flag).is_ok());
        self.args.push(flag.into());
    }

    fn option(&mut self, flag: &str, value: impl Into<OsString>) {
        self.flag(flag);
        self.args.push(value.into());
    }
}

#[derive(Debug, Clone)]
pub struct BuildFfmpegArgs {
    args: Vec<OsString>,
}

impl Default for BuildFfmpegArgs {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildFfmpegArgs {
    pub fn new() -> Self {
        Self {
            args: [
                "-hide_banner",
                "-nostdin",
                "-y",
                "-progress",
                "pipe:1",
                "-nostats",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        }
    }

    pub fn local_input(mut self, path: &Path) -> Result<Self, MediaError> {
        self.input(path)?;
        Ok(self)
    }

    pub fn normalize_audio(mut self) -> Self {
        self.option("-af", LOUDNORM_FILTER);
        self
    }

    pub fn build(
        self,
        path: &Path,
        container: MediaContainer,
    ) -> Result<Vec<OsString>, MediaError> {
        self.output(path, container_muxer(container))
    }

    pub(crate) fn pcm_intermediate(self, path: &Path) -> Result<Vec<OsString>, MediaError> {
        self.output(path, "w64")
    }

    fn output(mut self, path: &Path, muxer: &'static str) -> Result<Vec<OsString>, MediaError> {
        validate_local_path(path)?;
        self.option("-f", muxer);
        self.flag("--");
        self.args.push(path.into());
        Ok(self.args)
    }

    pub(crate) fn input(&mut self, path: &Path) -> Result<(), MediaError> {
        // Force a standalone demuxer. Disguised playlists cannot select a
        // demuxer that opens additional URLs.
        self.option("-f", local_demuxer(path)?);
        self.option("-i", path);
        Ok(())
    }

    pub(crate) fn flag(&mut self, flag: &str) {
        assert!(
            validate_ffmpeg_flag(flag).is_ok(),
            "internal FFmpeg flag was not allowlisted"
        );
        self.args.push(flag.into());
    }

    pub(crate) fn option(&mut self, flag: &str, value: impl Into<OsString>) {
        self.flag(flag);
        self.args.push(value.into());
    }
}

pub(crate) fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub(crate) fn local_demuxer(path: &Path) -> Result<&'static str, MediaError> {
    validate_local_path(path)?;
    let demuxer = match extension(path).as_str() {
        "mp4" | "m4a" | "m4v" | "mov" | "3gp" => "mov",
        "mkv" | "webm" | "mka" => "matroska",
        "ogg" | "oga" | "ogv" | "opus" => "ogg",
        "mp3" => "mp3",
        "aac" => "aac",
        "wav" => "wav",
        "w64" => "w64",
        "flac" => "flac",
        "avi" => "avi",
        "flv" => "flv",
        "ts" | "m2ts" => "mpegts",
        "mpg" | "mpeg" => "mpeg",
        "srt" => "srt",
        "vtt" => "webvtt",
        "ass" | "ssa" => "ass",
        "jpg" | "jpeg" => "jpeg_pipe",
        "png" => "png_pipe",
        "webp" => "webp_pipe",
        _ => {
            return Err(MediaError::argument(
                "local media format",
                path.to_string_lossy(),
            ))
        }
    };
    Ok(demuxer)
}

pub(crate) fn container_extension(container: MediaContainer) -> &'static str {
    match container {
        MediaContainer::Mp4 => "mp4",
        MediaContainer::Mkv => "mkv",
        MediaContainer::Webm => "webm",
        MediaContainer::Mp3 => "mp3",
        MediaContainer::M4a => "m4a",
        MediaContainer::Flac => "flac",
        MediaContainer::Opus => "opus",
        MediaContainer::Wav => "wav",
    }
}

fn container_muxer(container: MediaContainer) -> &'static str {
    match container {
        MediaContainer::Mkv => "matroska",
        MediaContainer::M4a => "ipod",
        _ => container_extension(container),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_ytdlp_arguments() {
        for flag in [
            "--exec=calc",
            "--exec",
            "--exec-before-download",
            "--batch-file",
            "-a",
            "--config-locations",
            "--config-location",
            "--alias",
            "--plugin-dirs",
            "--postprocessor-args",
            "--ppa",
            "--downloader-args",
            "--print-to-file",
        ] {
            assert!(validate_ytdlp_flag(flag).is_err(), "{flag}");
        }
        for template in [
            "$(calc).mp4",
            "-o $(calc)",
            "../outside.mp4",
            "%(title)s/../../evil.mp4",
            "clip-%(ext)s",
            "test\u{0060}name.mp4",
            "CON.mp4",
            "evil.mp4:stream",
            "bad\0.mp4",
            "\u{202E}cod.mp4",
            "--exec",
        ] {
            assert!(
                BuildYtDlpArgs::download(false)
                    .output_template(template)
                    .is_err(),
                "{template:?}"
            );
        }
        for url in [
            "file:///C:/clip.mp4",
            "smb://server/clip.mp4",
            "javascript:alert(1)",
            "-x",
        ] {
            assert!(BuildYtDlpArgs::probe(false).build(url).is_err(), "{url}");
        }
        for items in [
            "1-2 --exec calc",
            "1, 2",
            "1;2",
            "1-2-",
            "0",
            "1,,5",
            "1-3,$(calc)",
            "-1",
        ] {
            assert!(
                BuildYtDlpArgs::download(true)
                    .playlist_items(items)
                    .is_err(),
                "{items}"
            );
        }
        for format in [
            "-137",
            "137/-140",
            "best audio",
            "x\n",
            "137;140",
            "137++140",
            "a|b",
            "best[height=720]",
        ] {
            assert!(
                BuildYtDlpArgs::download(false).format(format).is_err(),
                "{format:?}"
            );
        }
        assert!(BuildYtDlpArgs::download(false)
            .subtitles(&["en.*".to_owned()])
            .is_err());
    }

    #[test]
    fn keeps_safe_ytdlp_values_discrete() {
        let url = "https://example.com/a%20b?items=[1]&lng=en%2DUS";
        for title in ["東京建築_4K_HDR", "ملف_تجريبي", "Café & Crème [4K 60fps]"] {
            let filename = format!("{title}.mp4");
            let args = BuildYtDlpArgs::download(true)
                .format("137+140")
                .unwrap()
                .playlist_items("1-3,5")
                .unwrap()
                .output_template(&filename)
                .unwrap()
                .subtitles(&["en-US".to_owned(), "ar".to_owned()])
                .unwrap()
                .build(url)
                .unwrap();
            for (flag, expected) in [
                ("-o", filename.as_str()),
                ("-f", "137+140"),
                ("--", url),
                ("--playlist-items", "1-3,5"),
                ("--sub-langs", "^en-US$,^ar$"),
            ] {
                assert!(args
                    .windows(2)
                    .any(|pair| pair[0] == flag && pair[1] == expected));
            }
            for flag in [
                "--ignore-config",
                "--no-config-locations",
                "--no-cache-dir",
                "--no-exec",
            ] {
                assert!(args.iter().any(|arg| arg == flag), "{flag}");
            }
        }
        assert!(validate_format_id("bestvideo+bestaudio/best").is_ok());
        let probe = BuildYtDlpArgs::probe(true).build(url).unwrap();
        assert!(probe.iter().any(|arg| arg == "--flat-playlist"));
        assert!(probe.iter().any(|arg| arg == "--no-cache-dir"));
        assert!(probe.iter().any(|arg| arg == "--no-exec"));
        assert!(!probe.iter().any(|arg| arg == "--no-playlist"));
    }

    #[test]
    fn rejects_unsafe_ffmpeg_inputs() {
        assert!(validate_ffmpeg_audio_filter("amovie=other.mp3").is_err());
        assert!(validate_ffmpeg_audio_filter("loudnorm=I=-16:TP=-1.5:LRA=11").is_ok());
        for flag in [
            "-filter_complex",
            "-lavfi",
            "-filter_complex_script",
            "-pre",
            "-fpre",
            "-vf",
            "-af=other",
            "-f lavfi",
            "-metadata:s:s:1-",
        ] {
            assert!(validate_ffmpeg_flag(flag).is_err(), "{flag}");
        }
        for path in [
            "concat:clip.mp4|other.mp4",
            "file:clip.mp4",
            "file:///C:/clip.mp4",
            "subfile:clip.mp4",
            "http://example.com/clip.mp4",
            "tcp:127.0.0.1:2",
            r"\\server\media\clip.mp4",
            r"\\.\C:\clip.mp4",
            "smb://server/clip.mp4",
            "../clip.mp4",
            r"clips\..\clip.mp4",
            "clips/./clip.mp4",
            "CON.mp4",
            "COM¹.mp4",
            "evil.mp4:stream",
            "clip\0.mp4",
            "\u{202E}cod.mp4",
            "-.mp4",
            "playlist.m3u8",
            "clip.ffconcat",
        ] {
            assert!(
                BuildFfmpegArgs::new().local_input(Path::new(path)).is_err(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn ffmpeg_uses_fixed_demuxer_filter_and_muxer() {
        let filename = "Café & Crème [4K 60fps].mp4";
        let args = BuildFfmpegArgs::new()
            .local_input(Path::new(filename))
            .unwrap()
            .normalize_audio()
            .build(Path::new("東京建築_4K_HDR.mkv"), MediaContainer::Mkv)
            .unwrap();
        assert!(args
            .windows(4)
            .any(|pair| pair == ["-f", "mov", "-i", filename]));
        assert!(args.windows(2).any(|pair| pair == ["-af", LOUDNORM_FILTER]));
        assert_eq!(
            args[args.len() - 4..],
            ["-f", "matroska", "--", "東京建築_4K_HDR.mkv"]
        );
        #[cfg(windows)]
        assert!(validate_local_path(Path::new(r"\\?\C:\Downloads\ملف_تجريبي.mp4")).is_ok());
    }
}
