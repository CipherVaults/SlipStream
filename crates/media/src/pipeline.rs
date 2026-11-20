use std::{
    collections::BTreeMap,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

use slipstream_protocol::{
    safe_join_under, sanitize_filename, sanitize_title_to_filename, validate_http_url,
    AddMediaDownloadRequest, AudioPreset, MediaContainer, TransferStatus,
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt},
    sync::mpsc::Sender,
};
use uuid::Uuid;

use crate::{
    args::{
        container_extension, extension, local_demuxer, validate_filename, validate_local_path,
        validate_subtitle_language, DOWNLOAD_PROGRESS_PREFIX,
    },
    motw::{is_regular_file, is_reparse_point, write_mark_of_the_web},
    toolchain::{command, ResolvedBinary},
    BuildFfmpegArgs, BuildYtDlpArgs, MediaError, MediaToolchain, ProbeOptions,
};

#[derive(Debug, Clone, PartialEq)]
pub enum MediaProgressEvent {
    Probing,
    Downloading(DownloadProgress),
    Remuxing(FfmpegProgress),
    Completed { paths: Vec<PathBuf> },
    Failed { message: String },
}

impl MediaProgressEvent {
    pub fn transfer_status(&self) -> TransferStatus {
        match self {
            Self::Probing => TransferStatus::Probing,
            Self::Downloading(_) => TransferStatus::Active,
            Self::Remuxing(_) => TransferStatus::Remuxing,
            Self::Completed { .. } => TransferStatus::Completed,
            Self::Failed { .. } => TransferStatus::Failed,
        }
    }
}

impl From<&MediaProgressEvent> for TransferStatus {
    fn from(event: &MediaProgressEvent) -> Self {
        event.transfer_status()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: Option<u64>,
    pub eta_seconds: Option<u64>,
}

impl DownloadProgress {
    pub fn percent(&self) -> Option<f32> {
        let total = self.total_bytes.filter(|&bytes| bytes > 0)?;
        let pct = (self.downloaded_bytes as f64 / total as f64) * 100.0;
        Some(pct.clamp(0.0, 100.0) as f32)
    }
}

pub fn parse_ytdlp_progress_line(line: &str) -> Option<DownloadProgress> {
    let parts: Vec<_> = line
        .trim()
        .strip_prefix(DOWNLOAD_PROGRESS_PREFIX)?
        .split('|')
        .collect();
    let [downloaded, total, estimated_total, speed, eta] = parts[..] else {
        return None;
    };
    Some(DownloadProgress {
        downloaded_bytes: downloaded.parse().ok()?,
        total_bytes: total.parse().ok().or_else(|| estimated_total.parse().ok()),
        speed_bps: nonnegative_number(speed),
        eta_seconds: nonnegative_number(eta),
    })
}

fn nonnegative_number(value: &str) -> Option<u64> {
    value.parse().ok().or_else(|| {
        let value: f64 = value.parse().ok()?;
        (value >= 0.0 && value < u64::MAX as f64).then_some(value as u64)
    })
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct FfmpegProgress {
    pub out_time_micros: Option<u64>,
    pub total_bytes: Option<u64>,
    pub speed: Option<f32>,
    pub finished: bool,
}

impl FfmpegProgress {
    pub fn percent(&self, total_duration_micros: u64) -> Option<f32> {
        if self.finished {
            return Some(100.0);
        }
        if total_duration_micros == 0 {
            return None;
        }
        let elapsed = self.out_time_micros?;
        let pct = (elapsed as f64 / total_duration_micros as f64) * 100.0;
        Some(pct.clamp(0.0, 100.0) as f32)
    }
}

#[derive(Debug, Default)]
pub struct FfmpegProgressParser(FfmpegProgress);

impl FfmpegProgressParser {
    pub fn push_line(&mut self, line: &str) -> Option<FfmpegProgress> {
        let (key, value) = line.trim().split_once('=')?;
        match key {
            "out_time_us" => self.0.out_time_micros = nonnegative_number(value),
            "total_size" => self.0.total_bytes = value.parse().ok(),
            "speed" => {
                self.0.speed = value
                    .trim()
                    .strip_suffix('x')
                    .and_then(|speed| speed.parse::<f32>().ok())
                    .filter(|speed| speed.is_finite() && *speed >= 0.0)
            }
            "progress" if matches!(value, "continue" | "end") => {
                self.0.finished = value == "end";
                return Some(std::mem::take(&mut self.0));
            }
            _ => {}
        }
        None
    }
}

#[derive(Debug, Default, Clone)]
pub struct MediaMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SubtitleInput {
    pub path: PathBuf,
    pub language: String,
}

#[derive(Debug, Default, Clone)]
pub struct LocalMediaOptions {
    pub normalize_audio: bool,
    pub subtitles: Vec<SubtitleInput>,
    pub cover_art: Option<PathBuf>,
    pub metadata: MediaMetadata,
    /// When converting a downloaded file, supply its source to preserve the Internet zone marker.
    pub source_url: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct DownloadOptions {
    pub playlist: bool,
    pub embed_cover_art: bool,
    pub metadata: MediaMetadata,
}

#[derive(Debug, Clone)]
pub struct MediaPipeline {
    tools: MediaToolchain,
    default_save_dir: PathBuf,
    create_save_dir: bool,
}

impl MediaPipeline {
    pub fn new(tools: MediaToolchain, default_save_dir: impl Into<PathBuf>) -> Self {
        Self {
            tools,
            default_save_dir: default_save_dir.into(),
            create_save_dir: true,
        }
    }

    pub fn create_save_dir(mut self, create: bool) -> Self {
        self.create_save_dir = create;
        self
    }

    /// A receiver can consume events concurrently with the operation. Phase and
    /// terminal events honor channel backpressure. Intermediate samples may be coalesced.
    pub async fn download(
        &self,
        request: &AddMediaDownloadRequest,
        options: &DownloadOptions,
        progress: Option<&Sender<MediaProgressEvent>>,
    ) -> Result<Vec<PathBuf>, MediaError> {
        let result = self.download_inner(request, options, progress).await;
        emit_result(progress, &result).await;
        result
    }

    pub async fn remux(
        &self,
        input: &Path,
        filename: &str,
        container: MediaContainer,
        options: &LocalMediaOptions,
        progress: Option<&Sender<MediaProgressEvent>>,
    ) -> Result<PathBuf, MediaError> {
        self.process_local(
            LocalJob {
                input,
                filename,
                conversion: Conversion::Remux(container),
                options,
            },
            progress,
        )
        .await
    }

    pub async fn extract_audio(
        &self,
        input: &Path,
        filename: &str,
        preset: AudioPreset,
        options: &LocalMediaOptions,
        progress: Option<&Sender<MediaProgressEvent>>,
    ) -> Result<PathBuf, MediaError> {
        self.process_local(
            LocalJob {
                input,
                filename,
                conversion: Conversion::Audio(preset),
                options,
            },
            progress,
        )
        .await
    }

    async fn process_local(
        &self,
        job: LocalJob<'_>,
        progress: Option<&Sender<MediaProgressEvent>>,
    ) -> Result<PathBuf, MediaError> {
        let result = async {
            if let Some(url) = &job.options.source_url {
                validate_http_url(url)?;
            }
            let save_dir = canonical_save_dir(&self.default_save_dir, self.create_save_dir).await?;
            let destination = destination(&save_dir, job.filename).await?;
            let stage = Stage::create(&save_dir).await?;
            let output = stage.path.join(format!(
                "output.{}",
                container_extension(job.conversion.container())
            ));
            self.convert(job.input, &output, job.conversion, job.options, progress)
                .await?;
            stage
                .publish(&[(output, destination)], job.options.source_url.as_deref())
                .await
        }
        .await;
        emit_result(progress, &result).await;
        result.map(|mut files| files.remove(0))
    }

    async fn download_inner(
        &self,
        request: &AddMediaDownloadRequest,
        options: &DownloadOptions,
        progress: Option<&Sender<MediaProgressEvent>>,
    ) -> Result<Vec<PathBuf>, MediaError> {
        validate_http_url(&request.url)?;
        let playlist = options.playlist || request.playlist_items.is_some();
        let binary = self.tools.require_ytdlp()?;
        let preset = download_audio_preset(request)?;
        let filename = prevalidate_download_filename(
            request.filename.as_deref(),
            preset,
            request.target_container,
            playlist,
        )?;
        let mut builder =
            BuildYtDlpArgs::download(playlist).ffmpeg_location(&self.tools.ffmpeg().path)?;
        if let Some(format) = request
            .format_id
            .as_deref()
            .or(preset.map(|_| "bestaudio/best"))
        {
            builder = builder.format(format)?;
        }
        if let Some(items) = &request.playlist_items {
            builder = builder.playlist_items(items)?;
        }
        builder = builder.subtitles(&request.embed_subtitles_langs)?;
        if options.embed_cover_art {
            builder = builder.write_thumbnail().convert_thumbnail_to_png();
        }
        let args = builder.build(&request.url)?;

        let save_dir = request
            .save_dir
            .as_deref()
            .map(Path::new)
            .unwrap_or(&self.default_save_dir);
        let save_dir = canonical_save_dir(save_dir, self.create_save_dir).await?;
        let stage = Stage::create(&save_dir).await?;
        emit(progress, MediaProgressEvent::Probing).await;
        let probe = crate::probe_url(&self.tools, &request.url, ProbeOptions { playlist }).await?;
        validate_probe_matches_mode(playlist, &probe, &request.url)?;
        emit(
            progress,
            MediaProgressEvent::Downloading(DownloadProgress::default()),
        )
        .await;
        run_streaming(binary, "yt-dlp", &args, Some(&stage.path), progress).await?;

        let media = stage.downloaded_items().await?;
        let numbered = playlist || media.len() > 1;
        let mut ready = Vec::with_capacity(media.len());
        for (index, mut item) in media {
            let input = item.media.take().ok_or(MediaError::NoMedia)?;
            let mut local_options = LocalMediaOptions {
                normalize_audio: request.normalize_audio,
                metadata: options.metadata.clone(),
                ..LocalMediaOptions::default()
            };
            for lang in &request.embed_subtitles_langs {
                let path = item
                    .subtitles
                    .get(lang)
                    .ok_or_else(|| MediaError::argument("missing downloaded subtitle", lang))?;
                local_options.subtitles.push(SubtitleInput {
                    path: path.clone(),
                    language: lang.clone(),
                });
            }
            if options.embed_cover_art {
                local_options.cover_art =
                    Some(item.cover.ok_or(MediaError::InvalidToolOutput {
                        tool: "yt-dlp",
                        kind: "downloaded cover art",
                    })?);
            }
            let conversion = preset.map(Conversion::Audio).or_else(|| {
                let needs_conversion = request.lossless_remux
                    || request.normalize_audio
                    || !local_options.subtitles.is_empty()
                    || local_options.cover_art.is_some()
                    || !local_options.metadata.is_empty();
                request
                    .target_container
                    .or_else(|| needs_conversion.then(|| default_container(&input)))
                    .map(Conversion::Remux)
            });
            let ext = conversion
                .map(|job| container_extension(job.container()).to_owned())
                .unwrap_or_else(|| extension(&input));
            let name = download_filename(
                filename.as_deref(),
                &probe.title,
                &ext,
                numbered.then_some(index),
            )?;
            let destination = destination(&save_dir, &name).await?;
            let staged = if let Some(conversion) = conversion {
                let output = stage.path.join(format!("output-{index}.{ext}"));
                self.convert(&input, &output, conversion, &local_options, progress)
                    .await?;
                output
            } else {
                input
            };
            ready.push((staged, destination));
        }
        stage.publish(&ready, Some(&request.url)).await
    }

    async fn convert(
        &self,
        input: &Path,
        output: &Path,
        conversion: Conversion,
        options: &LocalMediaOptions,
        progress: Option<&Sender<MediaProgressEvent>>,
    ) -> Result<(), MediaError> {
        if options.normalize_audio && conversion.container() == MediaContainer::Flac {
            // loudnorm can produce audio blocks larger than FLAC accepts.
            // Wave64 preserves the floating-point audio and reads it back in
            // bounded blocks, with room for PCM intermediates beyond 4 GiB.
            let pcm = output.with_extension("normalized.w64");
            let normalize = LocalMediaOptions {
                normalize_audio: true,
                ..LocalMediaOptions::default()
            };
            self.convert_once(
                input,
                &pcm,
                Conversion::PcmIntermediate,
                &normalize,
                progress,
            )
            .await?;
            let encode = LocalMediaOptions {
                normalize_audio: false,
                ..options.clone()
            };
            return self
                .convert_once(
                    &pcm,
                    output,
                    Conversion::Audio(AudioPreset::Flac),
                    &encode,
                    progress,
                )
                .await;
        }
        self.convert_once(input, output, conversion, options, progress)
            .await
    }

    async fn convert_once(
        &self,
        input: &Path,
        output: &Path,
        conversion: Conversion,
        options: &LocalMediaOptions,
        progress: Option<&Sender<MediaProgressEvent>>,
    ) -> Result<(), MediaError> {
        let input = canonical_input(input).await?;
        let mut args = BuildFfmpegArgs::new().local_input(&input)?;
        let container = conversion.container();
        if !options.subtitles.is_empty() && subtitle_codec(container).is_none() {
            return Err(MediaError::argument(
                "subtitle output container",
                container_extension(container),
            ));
        }
        for subtitle in &options.subtitles {
            validate_subtitle_language(&subtitle.language)?;
            let path = canonical_input(&subtitle.path).await?;
            if !matches!(extension(&path).as_str(), "srt" | "vtt" | "ass" | "ssa") {
                return Err(MediaError::argument(
                    "subtitle input",
                    path.to_string_lossy(),
                ));
            }
            args.input(&path)?;
        }
        if let Some(cover) = &options.cover_art {
            if !matches!(
                container,
                MediaContainer::Mp4
                    | MediaContainer::M4a
                    | MediaContainer::Mp3
                    | MediaContainer::Flac
            ) {
                return Err(MediaError::argument(
                    "cover art output container",
                    container_extension(container),
                ));
            }
            let cover = canonical_input(cover).await?;
            if !matches!(extension(&cover).as_str(), "png" | "jpg" | "jpeg") {
                return Err(MediaError::argument(
                    "cover art input",
                    cover.to_string_lossy(),
                ));
            }
            args.input(&cover)?;
        }
        let cover_video_index =
            if options.cover_art.is_some() && subtitle_codec(container).is_some() {
                match self.tools.ffprobe() {
                    Some(ffprobe) => count_video_streams(ffprobe, &input).await.unwrap_or(1),
                    None => 1,
                }
            } else {
                0
            };
        conversion.configure(&mut args, options, cover_video_index);
        options.metadata.apply(&mut args)?;
        if options.normalize_audio {
            args = args.normalize_audio();
        }
        let args = if matches!(conversion, Conversion::PcmIntermediate) {
            args.pcm_intermediate(output)?
        } else {
            args.build(output, container)?
        };
        emit(
            progress,
            MediaProgressEvent::Remuxing(FfmpegProgress::default()),
        )
        .await;
        run_streaming(
            self.tools.ffmpeg(),
            "ffmpeg",
            &args,
            output.parent(),
            progress,
        )
        .await
    }
}

struct LocalJob<'a> {
    input: &'a Path,
    filename: &'a str,
    conversion: Conversion,
    options: &'a LocalMediaOptions,
}

#[derive(Clone, Copy)]
enum Conversion {
    Remux(MediaContainer),
    Audio(AudioPreset),
    PcmIntermediate,
}

impl Conversion {
    fn container(self) -> MediaContainer {
        match self {
            Self::Remux(container) => container,
            Self::Audio(preset) => audio_container(preset),
            Self::PcmIntermediate => MediaContainer::Wav,
        }
    }

    fn configure(
        self,
        args: &mut BuildFfmpegArgs,
        options: &LocalMediaOptions,
        cover_video_index: usize,
    ) {
        let container = self.container();
        let cover = options.cover_art.is_some();
        let subtitles = subtitle_codec(container);
        let video = subtitles.is_some();
        if video {
            args.option("-map", "0:V?");
            args.option("-map", "0:a?");
        } else {
            args.option("-map", "0:a:0");
            if !cover {
                args.flag("-vn");
            }
        }
        if cover {
            args.option("-map", format!("{}:v:0", options.subtitles.len() + 1));
        }
        for i in 1..=options.subtitles.len() {
            args.option("-map", format!("{i}:s:0"));
        }
        if video {
            args.option("-map", "0:s?");
        }
        args.option("-c", "copy");
        match self {
            Self::Audio(preset) => configure_audio(args, preset),
            Self::Remux(_) if options.normalize_audio => {
                configure_normalized_remux_audio(args, container)
            }
            Self::Remux(_) => {}
            Self::PcmIntermediate => args.option("-c:a", "pcm_f64le"),
        }
        if let Some(codec) = subtitles {
            args.option("-c:s", codec);
        }
        for (index, subtitle) in options.subtitles.iter().enumerate() {
            args.option(
                &format!("-metadata:s:s:{index}"),
                format!(
                    "language={}",
                    container_language(&subtitle.language, container)
                ),
            );
        }
        if cover {
            args.option(
                &format!("-disposition:v:{cover_video_index}"),
                "attached_pic",
            );
        }
        if matches!(container, MediaContainer::Mp4 | MediaContainer::M4a) {
            args.option("-movflags", "+faststart");
        }
    }
}

async fn count_video_streams(ffprobe: &ResolvedBinary, input: &Path) -> Option<usize> {
    let demuxer = local_demuxer(input).ok()?;
    let mut cmd = command(&ffprobe.path, "ffprobe");
    cmd.args([
        "-v",
        "error",
        "-f",
        demuxer,
        "-select_streams",
        "V",
        "-show_entries",
        "stream=index",
        "-of",
        "csv=p=0",
        "--",
    ])
    .arg(input);
    let output = crate::toolchain::capture(
        cmd,
        &ffprobe.path,
        "ffprobe",
        std::time::Duration::from_secs(10),
        64 * 1024,
    )
    .await
    .ok()?;
    let count = String::from_utf8_lossy(&output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    Some(count)
}

fn configure_normalized_remux_audio(args: &mut BuildFfmpegArgs, container: MediaContainer) {
    if let Some(preset) = preset_for_container(container) {
        configure_audio(args, preset);
    } else {
        args.option(
            "-c:a",
            if container == MediaContainer::Webm {
                "libopus"
            } else {
                "aac"
            },
        );
    }
}

fn configure_audio(args: &mut BuildFfmpegArgs, preset: AudioPreset) {
    match preset {
        AudioPreset::Mp3_320 => {
            args.option("-c:a", "libmp3lame");
            args.option("-b:a", "320k");
            args.option("-id3v2_version", "3");
        }
        AudioPreset::M4aAac => {
            args.option("-c:a", "aac");
            args.option("-b:a", "256k");
        }
        AudioPreset::Opus => {
            args.option("-c:a", "libopus");
            args.option("-b:a", "160k");
        }
        AudioPreset::Flac => {
            args.option("-c:a", "flac");
            args.option("-compression_level", "5");
        }
        AudioPreset::Wav => args.option("-c:a", "pcm_s16le"),
    }
}

fn audio_container(preset: AudioPreset) -> MediaContainer {
    match preset {
        AudioPreset::Mp3_320 => MediaContainer::Mp3,
        AudioPreset::M4aAac => MediaContainer::M4a,
        AudioPreset::Opus => MediaContainer::Opus,
        AudioPreset::Flac => MediaContainer::Flac,
        AudioPreset::Wav => MediaContainer::Wav,
    }
}

fn preset_for_container(container: MediaContainer) -> Option<AudioPreset> {
    Some(match container {
        MediaContainer::Mp3 => AudioPreset::Mp3_320,
        MediaContainer::M4a => AudioPreset::M4aAac,
        MediaContainer::Opus => AudioPreset::Opus,
        MediaContainer::Flac => AudioPreset::Flac,
        MediaContainer::Wav => AudioPreset::Wav,
        _ => return None,
    })
}

fn download_audio_preset(
    request: &AddMediaDownloadRequest,
) -> Result<Option<AudioPreset>, MediaError> {
    if let (Some(preset), Some(container)) = (request.audio_only_preset, request.target_container) {
        if audio_container(preset) != container {
            return Err(MediaError::argument(
                "audio preset output container",
                container_extension(container),
            ));
        }
    }
    Ok(request
        .audio_only_preset
        .or_else(|| request.target_container.and_then(preset_for_container)))
}

fn default_container(path: &Path) -> MediaContainer {
    match extension(path).as_str() {
        "mp4" => MediaContainer::Mp4,
        "m4a" => MediaContainer::M4a,
        "mp3" => MediaContainer::Mp3,
        "flac" => MediaContainer::Flac,
        "opus" => MediaContainer::Opus,
        "wav" => MediaContainer::Wav,
        "webm" => MediaContainer::Webm,
        _ => MediaContainer::Mkv,
    }
}

fn subtitle_codec(container: MediaContainer) -> Option<&'static str> {
    match container {
        MediaContainer::Mp4 => Some("mov_text"),
        MediaContainer::Mkv => Some("srt"),
        MediaContainer::Webm => Some("webvtt"),
        _ => None,
    }
}

fn container_language(language: &str, container: MediaContainer) -> String {
    if container == MediaContainer::Webm {
        return language.to_owned();
    }
    // ISO 639-2 identifiers for the common two-letter language codes returned by yt-dlp.
    let base = language
        .split(['-', '_'])
        .next()
        .unwrap_or(language)
        .to_ascii_lowercase();
    match base.as_str() {
        "ar" => "ara".to_owned(),
        "de" => "deu".to_owned(),
        "en" => "eng".to_owned(),
        "es" => "spa".to_owned(),
        "fr" => "fra".to_owned(),
        "hi" => "hin".to_owned(),
        "it" => "ita".to_owned(),
        "ja" => "jpn".to_owned(),
        "ko" => "kor".to_owned(),
        "pt" => "por".to_owned(),
        "ru" => "rus".to_owned(),
        "tr" => "tur".to_owned(),
        "uk" => "ukr".to_owned(),
        "zh" => "zho".to_owned(),
        other => other.to_owned(),
    }
}

impl MediaMetadata {
    fn values(&self) -> [(&'static str, Option<&str>); 4] {
        [
            ("title", self.title.as_deref()),
            ("artist", self.artist.as_deref()),
            ("album", self.album.as_deref()),
            ("comment", self.comment.as_deref()),
        ]
    }

    fn is_empty(&self) -> bool {
        self.values().iter().all(|(_, value)| value.is_none())
    }

    fn apply(&self, args: &mut BuildFfmpegArgs) -> Result<(), MediaError> {
        for (key, value) in self.values() {
            if let Some(value) = value {
                if value.len() > 4096 || value.chars().any(|ch| {
                    ch.is_control() || matches!(ch, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
                }) {
                    return Err(MediaError::argument("media metadata", key));
                }
                args.option("-metadata", format!("{key}={value}"));
            }
        }
        Ok(())
    }
}

async fn emit(progress: Option<&Sender<MediaProgressEvent>>, event: MediaProgressEvent) {
    if let Some(progress) = progress {
        let _ = progress.send(event).await;
    }
}

async fn emit_result(
    progress: Option<&Sender<MediaProgressEvent>>,
    result: &Result<Vec<PathBuf>, MediaError>,
) {
    if progress.is_some() {
        let event = match result {
            Ok(paths) => MediaProgressEvent::Completed {
                paths: paths.clone(),
            },
            Err(err) => MediaProgressEvent::Failed {
                message: err.to_string(),
            },
        };
        emit(progress, event).await;
    }
}

async fn run_streaming(
    binary: &ResolvedBinary,
    tool: &'static str,
    args: &[OsString],
    cwd: Option<&Path>,
    progress: Option<&Sender<MediaProgressEvent>>,
) -> Result<(), MediaError> {
    let mut cmd = command(&binary.path, tool);
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd
        .spawn()
        .map_err(|err| MediaError::io("start sidecar", &binary.path, err))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut parser = FfmpegProgressParser::default();
    let (_, stderr, status) = tokio::try_join!(
        read_progress(stdout, false, |line| {
            let event = if tool == "yt-dlp" {
                parse_ytdlp_progress_line(line).map(MediaProgressEvent::Downloading)
            } else {
                parser.push_line(line).map(MediaProgressEvent::Remuxing)
            };
            send_sample(progress, event);
        }),
        read_progress(stderr, true, |line| {
            if tool == "yt-dlp" {
                send_sample(
                    progress,
                    parse_ytdlp_progress_line(line).map(MediaProgressEvent::Downloading),
                );
            }
        }),
        child.wait(),
    )
    .map_err(|err| MediaError::io("stream sidecar output", &binary.path, err))?;
    if status.success() {
        Ok(())
    } else {
        Err(MediaError::ToolFailed {
            tool,
            status,
            stderr,
        })
    }
}

fn send_sample(progress: Option<&Sender<MediaProgressEvent>>, event: Option<MediaProgressEvent>) {
    if let (Some(progress), Some(event)) = (progress, event) {
        let _ = progress.try_send(event);
    }
}

async fn read_progress(
    mut pipe: impl AsyncRead + Unpin,
    keep_tail: bool,
    mut process_line: impl FnMut(&str),
) -> io::Result<String> {
    const LIMIT: usize = 8192;
    let mut buffer = [0_u8; LIMIT];
    let mut line = Vec::new();
    let mut tail = Vec::new();
    let mut oversized = false;
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        if keep_tail {
            let overflow = (tail.len() + count).saturating_sub(LIMIT);
            tail.drain(..overflow);
            tail.extend_from_slice(&buffer[..count]);
        }
        for chunk in buffer[..count].split_inclusive(|byte| *byte == b'\n') {
            oversized |= line.len() + chunk.len() > LIMIT;
            if !oversized {
                line.extend_from_slice(chunk);
            }
            if chunk.ends_with(b"\n") {
                if !oversized {
                    process_line(&String::from_utf8_lossy(&line));
                }
                line.clear();
                oversized = false;
            }
        }
    }
    if !oversized && !line.is_empty() {
        process_line(&String::from_utf8_lossy(&line));
    }
    Ok(String::from_utf8_lossy(&tail).trim().to_owned())
}

async fn canonical_save_dir(path: &Path, create: bool) -> Result<PathBuf, MediaError> {
    if path.as_os_str().is_empty() {
        return Err(slipstream_protocol::PathSecurityError::Empty.into());
    }
    validate_local_path(&path.join(".slipstream-save-dir"))?;
    if create {
        fs::create_dir_all(path)
            .await
            .map_err(|err| MediaError::io("create save directory", path, err))?;
    }
    let directory = fs::canonicalize(path)
        .await
        .map_err(|err| MediaError::io("resolve save directory", path, err))?;
    validate_local_path(&directory.join(".slipstream-save-dir"))?;
    if !directory.is_dir() {
        return Err(MediaError::argument(
            "save directory",
            path.to_string_lossy(),
        ));
    }
    Ok(directory)
}

async fn canonical_input(path: &Path) -> Result<PathBuf, MediaError> {
    validate_local_path(path)?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|err| MediaError::io("inspect media input", path, err))?;
    if !is_regular_file(&metadata) {
        return Err(MediaError::argument(
            "regular local media file",
            path.to_string_lossy(),
        ));
    }
    let canonical = fs::canonicalize(path)
        .await
        .map_err(|err| MediaError::io("resolve media input", path, err))?;
    validate_local_path(&canonical)?;
    Ok(canonical)
}

async fn destination(save_dir: &Path, name: &str) -> Result<PathBuf, MediaError> {
    let name = sanitize_filename(name)?;
    validate_filename(&name)?;
    let path = safe_join_under(save_dir, &name)?;
    check_destination(save_dir, &path).await?;
    Ok(path)
}

async fn check_destination(save_dir: &Path, path: &Path) -> Result<(), MediaError> {
    let parent = path
        .parent()
        .ok_or(slipstream_protocol::PathSecurityError::EscapesBaseDir)?;
    let resolved = fs::canonicalize(parent)
        .await
        .map_err(|err| MediaError::io("resolve output directory", parent, err))?;
    if resolved != save_dir || !path.starts_with(save_dir) || path == save_dir {
        return Err(slipstream_protocol::PathSecurityError::EscapesBaseDir.into());
    }
    match fs::symlink_metadata(path).await {
        Ok(_) => Err(MediaError::DestinationExists(path.to_owned())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(MediaError::io("inspect destination", path, err)),
    }
}

fn prevalidate_download_filename(
    raw_filename: Option<&str>,
    preset: Option<AudioPreset>,
    target_container: Option<MediaContainer>,
    playlist: bool,
) -> Result<Option<String>, MediaError> {
    let Some(raw) = raw_filename else {
        return Ok(None);
    };
    let sanitized = sanitize_filename(raw)?;
    validate_filename(&sanitized)?;
    let precheck_ext = preset
        .map(|p| container_extension(audio_container(p)))
        .or_else(|| target_container.map(container_extension))
        .unwrap_or("webm");
    download_filename(
        Some(&sanitized),
        "download",
        precheck_ext,
        playlist.then_some(1),
    )?;
    Ok(Some(sanitized))
}

fn validate_probe_matches_mode(
    playlist: bool,
    probe: &slipstream_protocol::MediaProbeResult,
    url: &str,
) -> Result<(), MediaError> {
    if !playlist && probe.is_playlist {
        return Err(MediaError::argument(
            "playlist URL requires playlist mode",
            url,
        ));
    }
    Ok(())
}

fn is_known_media_extension(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp4"
            | "mkv"
            | "webm"
            | "mp3"
            | "m4a"
            | "flac"
            | "opus"
            | "wav"
            | "mov"
            | "avi"
            | "ogg"
            | "ts"
    )
}

fn download_filename(
    filename: Option<&str>,
    title: &str,
    ext: &str,
    index: Option<u64>,
) -> Result<String, MediaError> {
    if let Some(index) = index {
        let base = filename
            .map(|filename| {
                let path = Path::new(filename);
                match path.extension().and_then(|e| e.to_str()) {
                    Some(existing) if is_known_media_extension(existing) => path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or(filename),
                    _ => filename,
                }
            })
            .unwrap_or(title);
        // Reserve room for the per-item suffix even when the title reaches the filename length cap.
        let tail = format!("{index:06}.{ext}");
        let mut safe = sanitize_title_to_filename(base, &tail)?;
        let insert = safe.len() - tail.len() - 1;
        safe.replace_range(insert..=insert, "-");
        return Ok(safe);
    }
    match filename {
        Some(name) => {
            let path = Path::new(name);
            match path.extension().and_then(|existing| existing.to_str()) {
                Some(existing) if existing.eq_ignore_ascii_case(ext) => Ok(name.to_owned()),
                Some(existing) if is_known_media_extension(existing) => {
                    let replaced = path.with_extension(ext);
                    let candidate = replaced
                        .to_str()
                        .ok_or_else(|| MediaError::argument("download filename", name))?;
                    sanitize_filename(candidate).map_err(Into::into)
                }
                _ => sanitize_filename(&format!("{name}.{ext}")).map_err(Into::into),
            }
        }
        None => sanitize_title_to_filename(title, ext).map_err(Into::into),
    }
}

struct Stage {
    root: PathBuf,
    path: PathBuf,
}

#[derive(Default)]
struct DownloadedItem {
    media: Option<PathBuf>,
    cover: Option<PathBuf>,
    subtitles: BTreeMap<String, PathBuf>,
}

impl Stage {
    async fn create(root: &Path) -> Result<Self, MediaError> {
        let path = safe_join_under(root, &format!(".slipstream-{}", Uuid::new_v4()))?;
        // The guard owns the directory as soon as it exists, including on cancellation.
        std::fs::create_dir(&path)
            .map_err(|err| MediaError::io("create media staging directory", &path, err))?;
        let stage = Self {
            root: root.to_owned(),
            path,
        };
        stage.check().await?;
        Ok(stage)
    }

    async fn check(&self) -> Result<(), MediaError> {
        let metadata = fs::symlink_metadata(&self.path)
            .await
            .map_err(|err| MediaError::io("inspect media staging directory", &self.path, err))?;
        let resolved = fs::canonicalize(&self.path)
            .await
            .map_err(|err| MediaError::io("resolve media staging directory", &self.path, err))?;
        if !metadata.is_dir()
            || is_reparse_point(&metadata)
            || resolved != self.path
            || resolved.parent() != Some(self.root.as_path())
        {
            return Err(slipstream_protocol::PathSecurityError::EscapesBaseDir.into());
        }
        Ok(())
    }

    async fn downloaded_items(&self) -> Result<BTreeMap<u64, DownloadedItem>, MediaError> {
        self.check().await?;
        let mut entries = fs::read_dir(&self.path)
            .await
            .map_err(|err| MediaError::io("read downloaded media", &self.path, err))?;
        let mut items = BTreeMap::<u64, DownloadedItem>::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| MediaError::io("read staged entry", &self.path, err))?
        {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((index, suffix)) = name
                .strip_prefix("media-")
                .and_then(|rest| rest.split_once('.'))
            else {
                continue;
            };
            let Ok(index) = index.parse::<u64>() else {
                continue;
            };
            let path = entry.path();
            match suffix {
                "jpg" | "jpeg" | "png" | "webp" => {
                    set_unique(&mut items.entry(index).or_default().cover, path)?;
                }
                _ if suffix.ends_with(".srt") || suffix.ends_with(".vtt") => {
                    let (lang, _) = suffix
                        .rsplit_once('.')
                        .expect("subtitle suffix includes a dot");
                    validate_subtitle_language(lang)?;
                    let subtitles = &mut items.entry(index).or_default().subtitles;
                    if subtitles.insert(lang.to_owned(), path).is_some() {
                        return Err(MediaError::InvalidToolOutput {
                            tool: "yt-dlp",
                            kind: "duplicate subtitle",
                        });
                    }
                }
                _ if !suffix.contains('.') && local_demuxer(&path).is_ok() => {
                    set_unique(&mut items.entry(index).or_default().media, path)?;
                }
                _ => {}
            }
        }
        if items.is_empty() {
            return Err(MediaError::NoMedia);
        }
        Ok(items)
    }

    async fn publish(
        &self,
        files: &[(PathBuf, PathBuf)],
        source_url: Option<&str>,
    ) -> Result<Vec<PathBuf>, MediaError> {
        self.check().await?;
        for (source, destination) in files {
            let metadata = fs::symlink_metadata(source)
                .await
                .map_err(|err| MediaError::io("inspect staged output", source, err))?;
            if !is_regular_file(&metadata)
                || metadata.len() == 0
                || source.parent() != Some(self.path.as_path())
            {
                return Err(MediaError::argument(
                    "staged output file",
                    source.to_string_lossy(),
                ));
            }
            check_destination(&self.root, destination).await?;
            if let Some(url) = source_url {
                write_mark_of_the_web(source, url)?;
            }
        }

        let mut published = Vec::with_capacity(files.len());
        for (source, destination) in files {
            // A same-volume hard link publishes the complete file and its ADS in
            // one operation, and fails atomically if the destination exists.
            if let Err(err) = fs::hard_link(source, destination).await {
                let source = match err.kind() {
                    io::ErrorKind::AlreadyExists => {
                        MediaError::DestinationExists(destination.clone())
                    }
                    _ => MediaError::io("publish staged file", destination, err),
                };
                return Err(if published.is_empty() {
                    source
                } else {
                    MediaError::PartialPublish {
                        published,
                        source: Box::new(source),
                    }
                });
            }
            published.push(destination.clone());
        }
        Ok(published)
    }
}

fn set_unique(slot: &mut Option<PathBuf>, path: PathBuf) -> Result<(), MediaError> {
    if slot.replace(path).is_some() {
        return Err(MediaError::InvalidToolOutput {
            tool: "yt-dlp",
            kind: "duplicate staged file",
        });
    }
    Ok(())
}

impl Drop for Stage {
    fn drop(&mut self) {
        if self.path.parent() == Some(self.root.as_path())
            && std::fs::canonicalize(&self.path).is_ok_and(|current| current == self.path)
            && std::fs::symlink_metadata(&self.path)
                .is_ok_and(|meta| meta.is_dir() && !is_reparse_point(&meta))
        {
            if let Err(err) = std::fs::remove_dir_all(&self.path) {
                tracing::warn!(path = ?self.path, error = %err, "could not remove media staging directory");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ytdlp_progress_with_optional_fields() {
        assert_eq!(
            parse_ytdlp_progress_line(" SLIPSTREAM|2048|NA|4096|1024.7|2\r\n"),
            Some(DownloadProgress {
                downloaded_bytes: 2048,
                total_bytes: Some(4096),
                speed_bps: Some(1024),
                eta_seconds: Some(2)
            })
        );
        assert_eq!(
            parse_ytdlp_progress_line("SLIPSTREAM|4|5|7|NA|NA")
                .unwrap()
                .total_bytes,
            Some(5)
        );
        for line in [
            "other|1|2|3|4|5",
            "SLIPSTREAM|NA|2|3|4|5",
            "SLIPSTREAM|1|2|3|4",
            "SLIPSTREAM|1|2|3|4|5|6",
        ] {
            assert!(parse_ytdlp_progress_line(line).is_none());
        }
    }

    #[test]
    fn parses_ffmpeg_progress_blocks() {
        let mut parser = FfmpegProgressParser::default();
        for line in [
            "total_size=200",
            "out_time_us=350000",
            "speed=1.5x",
            "ignored=1",
        ] {
            assert!(parser.push_line(line).is_none());
        }
        assert_eq!(
            parser.push_line("progress=continue"),
            Some(FfmpegProgress {
                out_time_micros: Some(350_000),
                total_bytes: Some(200),
                speed: Some(1.5),
                finished: false,
            })
        );
        parser.push_line("out_time_us=-23000");
        parser.push_line("speed=N/A");
        assert_eq!(
            parser.push_line("progress=end"),
            Some(FfmpegProgress {
                finished: true,
                ..FfmpegProgress::default()
            })
        );
    }

    #[test]
    fn uses_safe_unicode_titles_and_preserves_playlist_suffixes() {
        for title in ["東京建築_4K_HDR", "ملف_تجريبي", "Café & Crème [4K 60fps]"] {
            assert_eq!(
                download_filename(None, title, "mp4", None).unwrap(),
                format!("{title}.mp4")
            );
            assert_eq!(
                download_filename(None, title, "mp4", Some(5)).unwrap(),
                format!("{title}-000005.mp4")
            );
        }
        let name = download_filename(None, &"東".repeat(100), "mkv", Some(20)).unwrap();
        assert!(name.ends_with("-000020.mkv"));
        assert!(name.len() <= 200);
        validate_filename(&name).unwrap();
        assert_eq!(
            download_filename(Some("My Video.v2"), "ignored", "mp4", None).unwrap(),
            "My Video.v2.mp4"
        );
        assert_eq!(
            download_filename(Some("clip.mp4"), "ignored", "mp3", None).unwrap(),
            "clip.mp3"
        );
        assert_eq!(
            download_filename(Some("clip.MP3"), "ignored", "mp3", None).unwrap(),
            "clip.MP3"
        );
        assert_eq!(container_language("EN", MediaContainer::Mp4), "eng");
        assert_eq!(container_language("EN-US", MediaContainer::Mkv), "eng");
        assert_eq!(container_language("en-US", MediaContainer::Webm), "en-US");
    }

    #[test]
    fn computes_progress_percentages() {
        let dl = DownloadProgress {
            downloaded_bytes: 500,
            total_bytes: Some(1000),
            speed_bps: Some(100),
            eta_seconds: Some(5),
        };
        assert_eq!(dl.percent(), Some(50.0));
        assert_eq!(
            DownloadProgress {
                downloaded_bytes: 1500,
                total_bytes: Some(1000),
                ..DownloadProgress::default()
            }
            .percent(),
            Some(100.0)
        );
        assert_eq!(DownloadProgress::default().percent(), None);

        assert_eq!(
            FfmpegProgress {
                out_time_micros: Some(500_000),
                ..FfmpegProgress::default()
            }
            .percent(1_000_000),
            Some(50.0)
        );
        assert_eq!(
            FfmpegProgress {
                finished: true,
                ..FfmpegProgress::default()
            }
            .percent(0),
            Some(100.0)
        );
    }

    #[test]
    fn rejects_overlong_download_filename_and_unexpected_playlist() {
        let too_long = "a".repeat(200);
        assert!(matches!(
            prevalidate_download_filename(Some(&too_long), None, None, false),
            Err(MediaError::Path(
                slipstream_protocol::PathSecurityError::TooLong
            ))
        ));
        let exact = "a".repeat(196);
        assert_eq!(
            prevalidate_download_filename(Some(&exact), None, Some(MediaContainer::Mp4), false)
                .unwrap()
                .as_deref(),
            Some(exact.as_str())
        );

        let playlist_probe = slipstream_protocol::MediaProbeResult {
            title: "Playlist".into(),
            source_url: "https://example.com/playlist?list=PL1".into(),
            extractor: "youtube:tab".into(),
            duration_seconds: None,
            thumbnail_url: None,
            is_playlist: true,
            playlist_count: Some(2),
            formats: Vec::new(),
            subtitles: Vec::new(),
        };
        assert!(matches!(
            validate_probe_matches_mode(false, &playlist_probe, &playlist_probe.source_url),
            Err(MediaError::InvalidArgument {
                kind: "playlist URL requires playlist mode",
                ..
            })
        ));
        assert!(
            validate_probe_matches_mode(true, &playlist_probe, &playlist_probe.source_url).is_ok()
        );
    }
}
