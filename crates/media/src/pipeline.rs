use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

use slipstream_protocol::{
    safe_join_under, sanitize_filename, sanitize_title_to_filename, validate_http_url,
    validate_safe_filename_component, AddMediaDownloadRequest, AudioPreset, MediaContainer,
    MediaProbeResult, TransferStatus,
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt},
    sync::mpsc::Sender,
};
use uuid::Uuid;

use crate::{
    args::{
        container_extension, extension, local_demuxer, sidecar_path, validate_filename,
        validate_local_path, validate_subtitle_language, DOWNLOAD_PROGRESS_PREFIX,
    },
    motw::{is_regular_file, is_reparse_point, write_mark_of_the_web},
    toolchain::{command, spawn_sidecar, ResolvedBinary},
    BuildFfmpegArgs, BuildYtDlpArgs, MediaError, MediaToolchain, ProbeOptions,
};

#[derive(Debug, Clone, PartialEq)]
pub enum MediaProgressEvent {
    Probing,
    Probed {
        title: String,
        is_playlist: bool,
        playlist_count: Option<u32>,
    },
    Downloading(DownloadProgress),
    PlaylistDownloading {
        progress: DownloadProgress,
        /// Download ordinal across the whole yt-dlp invocation.
        playlist_index: u32,
        playlist_count: Option<u32>,
        format_id: Option<String>,
    },
    Remuxing(FfmpegProgress),
    Completed {
        paths: Vec<PathBuf>,
    },
    Failed {
        message: String,
    },
}

impl MediaProgressEvent {
    pub fn transfer_status(&self) -> TransferStatus {
        match self {
            Self::Probing | Self::Probed { .. } => TransferStatus::Probing,
            Self::Downloading(_) | Self::PlaylistDownloading { .. } => TransferStatus::Active,
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
    match parse_ytdlp_progress_event(line)? {
        MediaProgressEvent::Downloading(progress)
        | MediaProgressEvent::PlaylistDownloading { progress, .. } => Some(progress),
        _ => None,
    }
}

fn parse_ytdlp_progress_event(line: &str) -> Option<MediaProgressEvent> {
    let parts: Vec<_> = line
        .trim()
        .strip_prefix(DOWNLOAD_PROGRESS_PREFIX)?
        .split('|')
        .collect();
    let [downloaded, total, estimated_total, speed, eta, playlist @ ..] = parts.as_slice() else {
        return None;
    };
    let progress = DownloadProgress {
        downloaded_bytes: downloaded.parse().ok()?,
        total_bytes: total.parse().ok().or_else(|| estimated_total.parse().ok()),
        speed_bps: nonnegative_number(speed),
        eta_seconds: nonnegative_number(eta),
    };
    match playlist {
        [] => Some(MediaProgressEvent::Downloading(progress)),
        [index, count] | [index, count, _] => {
            let Some(playlist_index) = index.parse::<u32>().ok().filter(|&index| index > 0) else {
                return Some(MediaProgressEvent::Downloading(progress));
            };
            Some(MediaProgressEvent::PlaylistDownloading {
                progress,
                playlist_index,
                playlist_count: count.parse::<u32>().ok().filter(|&count| count > 0),
                format_id: playlist
                    .get(2)
                    .filter(|&&id| id != "NA")
                    .map(|&id| id.to_owned()),
            })
        }
        _ => None,
    }
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
    pub pre_probe: Option<MediaProbeResult>,
    pub cache_dir: Option<PathBuf>,
}

#[derive(Debug)]
pub struct DownloadOutcome {
    pub paths: Vec<PathBuf>,
    pub completion_notice: Option<String>,
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
        self.download_with_summary(request, options, progress)
            .await
            .map(|outcome| outcome.paths)
    }

    pub async fn download_with_summary(
        &self,
        request: &AddMediaDownloadRequest,
        options: &DownloadOptions,
        progress: Option<&Sender<MediaProgressEvent>>,
    ) -> Result<DownloadOutcome, MediaError> {
        let result = self.download_inner(request, options, progress).await;
        emit_result(
            progress,
            result.as_ref().map(|outcome| outcome.paths.as_slice()),
        )
        .await;
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
        emit_result(progress, result.as_ref().map(Vec::as_slice)).await;
        result.map(|mut files| files.remove(0))
    }

    async fn download_inner(
        &self,
        request: &AddMediaDownloadRequest,
        options: &DownloadOptions,
        progress: Option<&Sender<MediaProgressEvent>>,
    ) -> Result<DownloadOutcome, MediaError> {
        let url = validate_http_url(&request.url)?;
        let playlist =
            options.playlist || request.download_playlist || request.playlist_items.is_some();
        let named_single_item = request.filename.is_some()
            && request
                .playlist_items
                .as_deref()
                .is_some_and(|items| items.parse::<u32>().is_ok());
        let binary = self.tools.require_ytdlp()?;
        let preset = download_audio_preset(request)?;
        let filename = prevalidate_download_filename(
            request.filename.as_deref(),
            preset,
            request.target_container,
            playlist && !named_single_item,
        )?;
        if request.max_height.is_some() && (preset.is_some() || request.format_id.is_some()) {
            return Err(MediaError::argument(
                "maximum video height",
                "cannot be combined with an audio preset or explicit format ID",
            ));
        }
        let mut builder = BuildYtDlpArgs::download(playlist)
            .ffmpeg_location(&self.tools.ffmpeg().path)?
            .merge_output_format(request.target_container)
            .headers(&request.headers)?;
        if let Some(dir) = &options.cache_dir {
            builder = builder.cache_dir(dir)?;
        }
        if let Some(format) = request
            .format_id
            .as_deref()
            .or(preset.map(|_| "bestaudio/best"))
        {
            builder = builder.format(format)?;
        } else if playlist || request.max_height.is_some() {
            builder = builder.playlist_video(request.max_height)?;
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
        let probe = match options.pre_probe.as_ref().filter(|probe| {
            validate_http_url(&probe.source_url).is_ok_and(|source| source == url)
                && validate_probe_matches_mode(playlist, probe, &request.url).is_ok()
        }) {
            Some(probe) => probe.clone(),
            None => {
                crate::probe_url(
                    &self.tools,
                    &request.url,
                    ProbeOptions {
                        playlist,
                        cache_dir: options.cache_dir.clone(),
                        headers: request.headers.clone(),
                    },
                )
                .await?
            }
        };
        validate_probe_matches_mode(playlist, &probe, &request.url)?;
        let selected_count =
            playlist_selection_count(probe.playlist_count, request.playlist_items.as_deref());
        emit(
            progress,
            MediaProgressEvent::Probed {
                title: probe.title.clone(),
                is_playlist: probe.is_playlist,
                playlist_count: selected_count.and_then(|count| u32::try_from(count).ok()),
            },
        )
        .await;
        emit(
            progress,
            MediaProgressEvent::Downloading(DownloadProgress::default()),
        )
        .await;
        let download = run_streaming(binary, "yt-dlp", &args, Some(&stage.path), progress).await;
        let download = match download {
            Err(MediaError::ToolFailed { status, .. })
                if playlist && matches!(status.code(), Some(1 | 101)) =>
            {
                download
            }
            result => {
                result?;
                Ok(())
            }
        };
        let media = match stage.downloaded_items().await {
            Err(MediaError::NoMedia) => {
                download?;
                return Err(MediaError::NoMedia);
            }
            items => items?,
        };
        let download_failed = download.is_err();
        if let Err(error) = download {
            tracing::warn!(%error, items = media.len(), "continuing with downloaded playlist items");
        }
        let numbered = (playlist && !named_single_item) || media.len() > 1;
        let media_count = media.len();
        // Some extractors omit unavailable entries from the flat probe. Only
        // use its titles when every downloaded index aligns with the full list.
        let titles_aligned = playlist
            && request.playlist_items.is_none()
            && selected_count == Some(media_count as u64)
            && media
                .keys()
                .map(|&(_, index)| index)
                .eq(1..=media.len() as u64);
        let mut ready = Vec::with_capacity(media.len());
        let mut indices = HashSet::new();
        let mut first_processing_error = None;
        let mut missing_subtitles = 0;
        for ((ordinal, index), mut item) in media {
            let duplicate = (!indices.insert(index)).then_some(ordinal);
            let input = item.media.take().ok_or(MediaError::NoMedia)?;
            let mut local_options = LocalMediaOptions {
                normalize_audio: request.normalize_audio,
                metadata: options.metadata.clone(),
                ..LocalMediaOptions::default()
            };
            let mut missing_requested_subtitle = false;
            for lang in &request.embed_subtitles_langs {
                if let Some(path) = item.subtitles.get(lang) {
                    local_options.subtitles.push(SubtitleInput {
                        path: path.clone(),
                        language: lang.clone(),
                    });
                } else if !playlist {
                    return Err(MediaError::argument("missing downloaded subtitle", lang));
                } else {
                    missing_requested_subtitle = true;
                }
            }
            if options.embed_cover_art {
                local_options.cover_art = item.cover;
                if !playlist && local_options.cover_art.is_none() {
                    return Err(MediaError::InvalidToolOutput {
                        tool: "yt-dlp",
                        kind: "downloaded cover art",
                    });
                }
            }
            let native_container = default_container(&input);
            let clean_matroska = request.lossless_remux
                && request.target_container.is_none()
                && preset.is_none()
                && !local_options.requires_conversion()
                && matches!(extension(&input).as_str(), "mkv" | "webm")
                && self.detects_demuxer(&input, "matroska", "matroska").await;
            let mut conversion = preset.map(Conversion::Audio).or_else(|| {
                let needs_conversion = (request.lossless_remux && !clean_matroska)
                    || local_options.requires_conversion();
                request
                    .target_container
                    .or_else(|| needs_conversion.then_some(native_container))
                    .map(Conversion::Remux)
            });
            if playlist && conversion.is_none() {
                match self.playlist_raw_conversion(&input).await {
                    Ok(required) => conversion = required,
                    Err(error) => {
                        tracing::warn!(%error, index, "skipping unreadable playlist item");
                        first_processing_error.get_or_insert(error);
                        continue;
                    }
                }
            }
            let staged = if let Some(conversion) = conversion {
                let mut output = stage.path.join(format!(
                    "output-{ordinal}-{index}.{}",
                    container_extension(conversion.container())
                ));
                let mut result = self
                    .convert(&input, &output, conversion, &local_options, progress)
                    .await;
                if matches!(result, Err(MediaError::ToolFailed { .. }))
                    && request.target_container.is_none()
                    && local_options.cover_art.is_none()
                    && matches!(
                        conversion,
                        Conversion::Remux(MediaContainer::Mp4 | MediaContainer::Webm)
                    )
                {
                    tracing::warn!(error = %result.unwrap_err(), index, "retrying implicit remux as MKV");
                    output.set_extension("mkv");
                    result = self
                        .convert(
                            &input,
                            &output,
                            Conversion::Remux(MediaContainer::Mkv),
                            &local_options,
                            progress,
                        )
                        .await;
                }
                if let Err(error) = result {
                    if playlist && matches!(error, MediaError::ToolFailed { .. }) {
                        tracing::warn!(%error, index, "skipping playlist item after conversion failure");
                        first_processing_error.get_or_insert(error);
                        continue;
                    }
                    return Err(error);
                }
                output
            } else {
                input
            };
            let title = titles_aligned
                .then(|| probe.playlist_entries.get(index as usize - 1))
                .flatten()
                .filter(|title| !title.is_empty())
                .unwrap_or(&probe.title);
            let name = download_filename(
                filename.as_deref(),
                title,
                &extension(&staged),
                numbered.then_some((index, duplicate)),
            )?;
            let destination = destination(&save_dir, &name).await?;
            ready.push((staged, destination));
            missing_subtitles += usize::from(missing_requested_subtitle);
        }
        if ready.is_empty() {
            return Err(first_processing_error.unwrap_or(MediaError::NoMedia));
        }
        let paths = stage.publish(&ready, Some(&request.url)).await?;
        let completion_notice = if playlist {
            let expected = if media_count == indices.len() {
                selected_count.filter(|&count| count >= media_count as u64)
            } else {
                // The flat probe counts top-level entries, while these are leaf downloads.
                None
            };
            playlist_completion_notice(
                paths.len(),
                expected,
                download_failed || first_processing_error.is_some(),
                missing_subtitles,
            )
        } else {
            None
        };
        Ok(DownloadOutcome {
            paths,
            completion_notice,
        })
    }

    async fn playlist_raw_conversion(
        &self,
        input: &Path,
    ) -> Result<Option<Conversion>, MediaError> {
        let ext = extension(input);
        if local_demuxer(input)? != "mov" {
            return Ok(None);
        }
        match self.actual_input_demuxer(input, "mov,mpegts").await {
            Some("mov") => Ok(None),
            Some("mpegts") => Ok(Some(Conversion::Remux(if ext == "m4a" {
                MediaContainer::M4a
            } else {
                MediaContainer::Mp4
            }))),
            _ => Err(MediaError::InvalidToolOutput {
                tool: if self.tools.ffprobe().is_some() {
                    "ffprobe"
                } else {
                    "ffmpeg"
                },
                kind: "unreadable staged media",
            }),
        }
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
        let by_extension = local_demuxer(&input)?;
        // yt-dlp can write HLS transport streams under a MOV or Matroska extension.
        let is_mpegts = match by_extension {
            "mpegts" => true,
            "mov" => self.detects_demuxer(&input, "mov,mpegts", "mpegts").await,
            "matroska" => {
                self.detects_demuxer(&input, "matroska,mpegts", "mpegts")
                    .await
            }
            _ => false,
        };
        let demuxer = if is_mpegts { "mpegts" } else { by_extension };
        let mut args = BuildFfmpegArgs::new();
        if is_mpegts {
            args.flag("-xerror");
            args.option("-abort_on", "empty_output");
        }
        args.input_with_demuxer(&input, is_mpegts)?;
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
                    Some(ffprobe) => count_video_streams(ffprobe, &input, demuxer)
                        .await
                        .unwrap_or(1),
                    None => 1,
                }
            } else {
                0
            };
        let copy_all_subtitles = demuxer == "matroska"
            && match container {
                MediaContainer::Mkv => extension(&input) != "webm",
                MediaContainer::Webm => extension(&input) == "webm",
                _ => false,
            };
        let subtitle_conversion = subtitle_codec(container).filter(|_| !copy_all_subtitles);
        let selected_subtitles = match (subtitle_conversion, self.tools.ffprobe()) {
            (Some(codec), Some(ffprobe)) => {
                text_subtitle_maps(ffprobe, &input, demuxer, codec).await
            }
            _ => None,
        };
        let try_all_subtitles = subtitle_conversion.is_some() && selected_subtitles.is_none();
        let embedded_subtitles = selected_subtitles.unwrap_or_else(|| {
            if copy_all_subtitles || (container == MediaContainer::Mp4 && demuxer == "mov") {
                vec![(None, "copy")]
            } else if let Some(codec) = subtitle_conversion {
                vec![(None, codec)]
            } else {
                Vec::new()
            }
        });
        let build_args = |mut args: BuildFfmpegArgs, embedded_subtitles: &[_]| {
            conversion.configure(&mut args, options, cover_video_index, embedded_subtitles);
            options.metadata.apply(&mut args)?;
            if options.normalize_audio {
                args = args.normalize_audio();
            }
            if matches!(conversion, Conversion::PcmIntermediate) {
                args.pcm_intermediate(output)
            } else {
                args.build(output, container)
            }
        };
        let ffmpeg_args = build_args(args.clone(), &embedded_subtitles)?;
        emit(
            progress,
            MediaProgressEvent::Remuxing(FfmpegProgress::default()),
        )
        .await;
        let ffmpeg = self.tools.ffmpeg();
        let result = run_streaming(ffmpeg, "ffmpeg", &ffmpeg_args, output.parent(), progress).await;
        if try_all_subtitles && matches!(result, Err(MediaError::ToolFailed { .. })) {
            // Unknown subtitle codecs can fail conversion. Keep external
            // subtitles when retrying without the embedded streams.
            let fallback = build_args(args, &[])?;
            return run_streaming(ffmpeg, "ffmpeg", &fallback, output.parent(), progress).await;
        }
        result
    }

    async fn detects_demuxer(&self, input: &Path, whitelist: &'static str, name: &str) -> bool {
        self.actual_input_demuxer(input, whitelist).await == Some(name)
    }

    async fn actual_input_demuxer(
        &self,
        input: &Path,
        whitelist: &'static str,
    ) -> Option<&'static str> {
        let path = sidecar_path(input).ok()?;
        let Some(ffprobe) = self.tools.ffprobe() else {
            let ffmpeg = self.tools.ffmpeg();
            for demuxer in whitelist.split(',') {
                let mut cmd = command(&ffmpeg.path, "ffmpeg");
                cmd.args([
                    "-hide_banner",
                    "-nostdin",
                    "-v",
                    "error",
                    "-xerror",
                    "-protocol_whitelist",
                    "file",
                    "-format_whitelist",
                    demuxer,
                    "-i",
                ])
                .arg(&path)
                .args([
                    "-map",
                    "0:V?",
                    "-map",
                    "0:a?",
                    "-c",
                    "copy",
                    "-t",
                    "1",
                    "-abort_on",
                    "empty_output",
                    "-f",
                    "null",
                    "--",
                    "-",
                ]);
                if crate::toolchain::capture(
                    cmd,
                    &ffmpeg.path,
                    "ffmpeg",
                    std::time::Duration::from_secs(10),
                    64 * 1024,
                )
                .await
                .is_ok()
                {
                    return Some(demuxer);
                }
            }
            return None;
        };
        let mut cmd = command(&ffprobe.path, "ffprobe");
        // The whitelist is checked before reading the format header. In
        // particular, mislabeled HLS/concat inputs cannot open other files.
        cmd.args([
            "-v",
            "error",
            "-format_whitelist",
            whitelist,
            "-protocol_whitelist",
            "file",
            "-show_entries",
            "format=format_name:stream=codec_type:stream_disposition=attached_pic",
            "-of",
            "json",
            "--",
        ])
        .arg(path);
        let output = capture_ffprobe(cmd, ffprobe).await?;
        let data: serde_json::Value = serde_json::from_slice(&output).ok()?;
        if !data["streams"].as_array()?.iter().any(|stream| {
            stream["codec_type"] == "audio"
                || (stream["codec_type"] == "video" && stream["disposition"]["attached_pic"] != 1)
        }) {
            return None;
        }
        let demuxer = data["format"]["format_name"].as_str()?.split(',').next()?;
        whitelist.split(',').find(|name| *name == demuxer)
    }
}

impl LocalMediaOptions {
    fn requires_conversion(&self) -> bool {
        self.normalize_audio
            || !self.subtitles.is_empty()
            || self.cover_art.is_some()
            || !self.metadata.is_empty()
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
        embedded_subtitles: &[(Option<u64>, &'static str)],
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
        for (stream, _) in embedded_subtitles {
            args.option(
                "-map",
                stream.map_or_else(|| "0:s?".to_owned(), |index| format!("0:{index}")),
            );
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
        for (index, subtitle) in options.subtitles.iter().enumerate() {
            if let Some(codec) = subtitles {
                args.option(&format!("-c:s:{index}"), codec);
            }
            args.option(
                &format!("-metadata:s:s:{index}"),
                format!(
                    "language={}",
                    container_language(&subtitle.language, container)
                ),
            );
        }
        for (index, (stream, codec)) in embedded_subtitles.iter().enumerate() {
            if *codec != "copy" {
                if stream.is_some() {
                    args.option(&format!("-c:s:{}", options.subtitles.len() + index), *codec);
                } else {
                    args.option("-c:s", *codec);
                }
            }
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

async fn text_subtitle_maps(
    ffprobe: &ResolvedBinary,
    input: &Path,
    demuxer: &str,
    target_codec: &'static str,
) -> Option<Vec<(Option<u64>, &'static str)>> {
    let mut cmd = command(&ffprobe.path, "ffprobe");
    cmd.args([
        "-v",
        "error",
        "-protocol_whitelist",
        "file",
        "-f",
        demuxer,
        "-select_streams",
        "s",
        "-show_entries",
        "stream=index,codec_name",
        "-of",
        "json",
        "--",
    ])
    .arg(sidecar_path(input).ok()?);
    let output = capture_ffprobe(cmd, ffprobe).await?;
    let data: serde_json::Value = serde_json::from_slice(&output).ok()?;
    Some(
        data["streams"]
            .as_array()?
            .iter()
            .filter_map(|stream| {
                let codec = stream["codec_name"].as_str()?;
                let target = match codec {
                    "subrip" => "srt",
                    "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text" | "eia_608"
                    | "jacosub" | "microdvd" | "mpl2" | "pjs" | "realtext" | "sami" | "stl"
                    | "subviewer" | "subviewer1" | "vplayer" => codec,
                    _ => return None,
                };
                let codec = if target == target_codec {
                    "copy"
                } else {
                    target_codec
                };
                Some((Some(stream["index"].as_u64()?), codec))
            })
            .collect(),
    )
}

async fn count_video_streams(
    ffprobe: &ResolvedBinary,
    input: &Path,
    demuxer: &str,
) -> Option<usize> {
    let mut cmd = command(&ffprobe.path, "ffprobe");
    cmd.args([
        "-v",
        "error",
        "-protocol_whitelist",
        "file",
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
    .arg(sidecar_path(input).ok()?);
    let output = capture_ffprobe(cmd, ffprobe).await?;
    let count = String::from_utf8_lossy(&output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    Some(count)
}

async fn capture_ffprobe(
    cmd: tokio::process::Command,
    ffprobe: &ResolvedBinary,
) -> Option<Vec<u8>> {
    crate::toolchain::capture(
        cmd,
        &ffprobe.path,
        "ffprobe",
        std::time::Duration::from_secs(10),
        64 * 1024,
    )
    .await
    .ok()
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
    result: Result<&[PathBuf], &MediaError>,
) {
    if progress.is_some() {
        let event = match result {
            Ok(paths) => MediaProgressEvent::Completed {
                paths: paths.to_vec(),
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
        cmd.current_dir(sidecar_path(cwd)?);
    }
    let mut child = spawn_sidecar(&mut cmd)
        .map_err(|err| MediaError::io("start sidecar", &binary.path, err))?;
    let stdout = child.child.stdout.take().expect("stdout is piped");
    let stderr = child.child.stderr.take().expect("stderr is piped");
    let mut parser = FfmpegProgressParser::default();
    let (_, stderr, status) = tokio::try_join!(
        read_progress(stdout, false, |line| {
            let event = if tool == "yt-dlp" {
                parse_ytdlp_progress_event(line)
            } else {
                parser.push_line(line).map(MediaProgressEvent::Remuxing)
            };
            send_sample(progress, event);
        }),
        read_progress(stderr, true, |line| {
            if tool == "yt-dlp" {
                send_sample(progress, parse_ytdlp_progress_event(line));
            }
        }),
        child.wait(),
    )
    .map_err(|err| MediaError::io("stream sidecar output", &binary.path, err))?;
    if status.success() {
        Ok(())
    } else {
        Err(MediaError::tool_failed(tool, status, stderr))
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
        playlist.then_some((1, None)),
    )?;
    Ok(Some(sanitized))
}

fn validate_probe_matches_mode(
    playlist: bool,
    probe: &MediaProbeResult,
    url: &str,
) -> Result<(), MediaError> {
    if playlist != probe.is_playlist {
        return Err(MediaError::argument(
            if playlist {
                "playlist mode requires a playlist URL"
            } else {
                "playlist URL requires playlist mode"
            },
            url,
        ));
    }
    Ok(())
}

fn playlist_selection_count(count: Option<u32>, selection: Option<&str>) -> Option<u64> {
    let count = u64::from(count?);
    let Some(selection) = selection else {
        return Some(count);
    };
    // The selector was validated before launching yt-dlp. Merge overlapping
    // ranges and clip them to the probed playlist, without expanding them.
    let mut ranges: Vec<_> = selection
        .split(',')
        .filter_map(|part| {
            let (first, last) = part.split_once('-').unwrap_or((part, part));
            let (first, last): (u64, u64) = (first.parse().ok()?, last.parse().ok()?);
            Some((first, last.min(count)))
        })
        .collect();
    ranges.sort_unstable();
    let mut end = 0;
    let mut selected = 0;
    for (first, last) in ranges {
        let first = first.max(end + 1);
        if first <= last {
            selected += last - first + 1;
            end = last;
        }
    }
    Some(selected)
}

fn playlist_completion_notice(
    downloaded: usize,
    expected: Option<u64>,
    skipped_items: bool,
    missing_subtitles: usize,
) -> Option<String> {
    let mut notices = Vec::new();
    if let Some(expected) = expected.filter(|&expected| expected > downloaded as u64) {
        let skipped = expected - downloaded as u64;
        notices.push(format!(
            "{downloaded} of {expected} videos downloaded ({skipped} skipped)."
        ));
    } else if skipped_items {
        let noun = if downloaded == 1 { "video" } else { "videos" };
        notices.push(format!(
            "{downloaded} {noun} downloaded. Some playlist items were skipped or reported errors."
        ));
    }
    if missing_subtitles > 0 {
        let noun = if missing_subtitles == 1 {
            "video"
        } else {
            "videos"
        };
        notices.push(format!(
            "Requested subtitles were unavailable for {missing_subtitles} downloaded {noun}."
        ));
    }
    (!notices.is_empty()).then(|| notices.join(" "))
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
    numbering: Option<(u64, Option<u64>)>,
) -> Result<String, MediaError> {
    if let Some((index, duplicate)) = numbering {
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
        let tail = match duplicate {
            Some(ordinal) => format!("{index:06}-{ordinal:06}.{ext}"),
            None => format!("{index:06}.{ext}"),
        };
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

fn stage_number(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|&number| number > 0 && value.bytes().all(|byte| byte.is_ascii_digit()))
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

    // Keys are (download ordinal, original playlist index). Single-video
    // stages use one number for both.
    async fn downloaded_items(&self) -> Result<BTreeMap<(u64, u64), DownloadedItem>, MediaError> {
        self.check().await?;
        let mut entries = fs::read_dir(&self.path)
            .await
            .map_err(|err| MediaError::io("read downloaded media", &self.path, err))?;
        let mut items = BTreeMap::<_, DownloadedItem>::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| MediaError::io("read staged entry", &self.path, err))?
        {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((stem, suffix)) = name
                .strip_prefix("media-")
                .and_then(|rest| rest.split_once('.'))
            else {
                continue;
            };
            let (index, ordinal) = stem.split_once('-').unwrap_or((stem, stem));
            let (Some(index), Some(ordinal)) = (stage_number(index), stage_number(ordinal)) else {
                continue;
            };
            if suffix.rsplit_once('.').is_some_and(|(stem, _)| {
                stem.split('.').any(|part| {
                    part == "temp"
                        || part.strip_prefix('f').is_some_and(|digits| {
                            !digits.is_empty() && digits.bytes().all(|digit| digit.is_ascii_digit())
                        })
                })
            }) {
                continue;
            }
            validate_safe_filename_component(name)?;
            let key = (ordinal, index);
            let path = entry.path();
            match suffix {
                "jpg" | "jpeg" | "png" | "webp" => {
                    set_unique(&mut items.entry(key).or_default().cover, path)?;
                }
                _ if suffix.ends_with(".srt") || suffix.ends_with(".vtt") => {
                    let (lang, _) = suffix
                        .rsplit_once('.')
                        .expect("subtitle suffix includes a dot");
                    validate_subtitle_language(lang)?;
                    let subtitles = &mut items.entry(key).or_default().subtitles;
                    if subtitles.insert(lang.to_owned(), path).is_some() {
                        return Err(MediaError::InvalidToolOutput {
                            tool: "yt-dlp",
                            kind: "duplicate subtitle",
                        });
                    }
                }
                _ if !suffix.contains('.')
                    && local_demuxer(&path).is_ok_and(|demuxer| {
                        !matches!(
                            demuxer,
                            "srt" | "webvtt" | "ass" | "jpeg_pipe" | "png_pipe" | "webp_pipe"
                        )
                    }) =>
                {
                    let metadata = fs::symlink_metadata(&path)
                        .await
                        .map_err(|err| MediaError::io("inspect downloaded media", &path, err))?;
                    if !is_regular_file(&metadata) {
                        return Err(MediaError::argument(
                            "regular downloaded media file",
                            path.to_string_lossy(),
                        ));
                    }
                    if metadata.len() == 0 {
                        continue;
                    }
                    set_unique(&mut items.entry(key).or_default().media, path)?;
                }
                _ => {}
            }
        }
        items.retain(|_, item| item.media.is_some());
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

    #[tokio::test]
    async fn nested_playlist_indices_keep_separate_media_and_sidecars() {
        let root = fs::canonicalize(std::env::temp_dir()).await.unwrap();
        let stage = Stage::create(&root).await.unwrap();
        for (name, bytes) in [
            ("media-000001-000001.mp4", "first"),
            ("media-000001-000002.mp4", "second"),
            ("media-000001-000001.en.srt", "first subtitle"),
            ("media-000001-000002.en.srt", "second subtitle"),
            ("media-000001-000002.png", "second cover"),
            ("media-000001-000003.mp4.part", "unfinished"),
            ("media-000001-000003.f137.mp4", "unmerged video"),
            (
                "media-000001-000003.temp.mp4",
                "failed postprocessing output",
            ),
            ("media-000001-000001.f137.vtt", "intermediate subtitle"),
            ("media-000001-000002.temp.srt", "intermediate subtitle"),
            ("media-000001-+00004.mp4", "invalid ordinal"),
        ] {
            fs::write(stage.path.join(name), bytes).await.unwrap();
        }
        let items = stage.downloaded_items().await.unwrap();
        assert_eq!(items.keys().copied().collect::<Vec<_>>(), [(1, 1), (2, 1)]);
        for ((ordinal, index), expected) in [((1, 1), "first"), ((2, 1), "second")] {
            let item = &items[&(ordinal, index)];
            assert_eq!(
                fs::read_to_string(item.media.as_ref().unwrap())
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(
                fs::read_to_string(&item.subtitles["en"]).await.unwrap(),
                format!("{expected} subtitle")
            );
            assert_eq!(item.subtitles.len(), 1);
        }
        assert!(items[&(1, 1)].cover.is_none());
        assert!(items[&(2, 1)].cover.is_some());
        assert_eq!(
            download_filename(None, "Playlist", "mp4", Some((1, Some(2)))).unwrap(),
            "Playlist-000001-000002.mp4"
        );
    }

    #[test]
    fn playlist_summary_counts_only_requested_items() {
        assert_eq!(
            playlist_selection_count(Some(8), Some("1-4,2,7-10")),
            Some(6)
        );
        assert_eq!(
            playlist_selection_count(Some(8), Some("1-4294967295")),
            Some(8)
        );
        assert_eq!(playlist_selection_count(Some(8), Some("7-3,1")), Some(1));
        assert_eq!(playlist_completion_notice(6, Some(6), false, 0), None);
        assert_eq!(
            playlist_completion_notice(6, Some(6), false, 2).as_deref(),
            Some("Requested subtitles were unavailable for 2 downloaded videos.")
        );
    }

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
    fn reads_playlist_progress_and_keeps_legacy_progress_api() {
        let line = "SLIPSTREAM|2048|NA|4096|1024.7|2|8|6|140";
        let progress = parse_ytdlp_progress_line(line).unwrap();
        assert_eq!(progress.downloaded_bytes, 2048);
        assert_eq!(progress.total_bytes, Some(4096));
        assert_eq!(
            parse_ytdlp_progress_event(line),
            Some(MediaProgressEvent::PlaylistDownloading {
                progress,
                playlist_index: 8,
                playlist_count: Some(6),
                format_id: Some("140".into()),
            })
        );
        assert!(matches!(
            parse_ytdlp_progress_event("SLIPSTREAM|4|5|NA|NA|NA|3|NA"),
            Some(MediaProgressEvent::PlaylistDownloading {
                playlist_index: 3,
                playlist_count: None,
                ..
            })
        ));
        assert!(matches!(
            parse_ytdlp_progress_event("SLIPSTREAM|4|5|NA|NA|NA|NA|NA|18"),
            Some(MediaProgressEvent::Downloading(_))
        ));
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
                download_filename(None, title, "mp4", Some((5, None))).unwrap(),
                format!("{title}-000005.mp4")
            );
        }
        let name = download_filename(None, &"東".repeat(100), "mkv", Some((20, None))).unwrap();
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

        let playlist_probe = MediaProbeResult {
            title: "Playlist".into(),
            source_url: "https://example.com/playlist?list=PL1".into(),
            extractor: "youtube:tab".into(),
            duration_seconds: None,
            thumbnail_url: None,
            is_playlist: true,
            playlist_count: Some(2),
            playlist_entries: Vec::new(),
            playlist_items: Vec::new(),
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
