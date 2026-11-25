use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use slipstream_media::{
    discover_ytdlp_only, probe_url, probe_url_with_binary, DownloadOptions, LocalMediaOptions,
    MediaError, MediaMetadata, MediaPipeline, MediaProgressEvent, MediaToolchain, ProbeOptions,
    ResolvedBinary, SubtitleInput, ToolchainConfig,
};
use slipstream_protocol::{AddMediaDownloadRequest, AudioPreset, MediaContainer, TransferStatus};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::Command,
    sync::mpsc,
};
use uuid::Uuid;

const SOURCE: &str = "https://example.com/video?clip=[1]&language=en%2DUS";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("slipstream-media-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn sidecar(binary: &ResolvedBinary, args: &[&OsStr]) -> Vec<u8> {
    let mut cmd = Command::new(&binary.path);
    cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .expect("fixture sidecar timed out")
        .unwrap();
    assert!(
        output.status.success(),
        "{:?} {:?}: {}",
        binary.path,
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

async fn fixture(binary: &ResolvedBinary, flags: &[&str], output: &Path) {
    let mut args: Vec<_> = flags.iter().copied().map(OsStr::new).collect();
    args.push(output.as_os_str());
    sidecar(binary, &args).await;
}

async fn probe(binary: &ResolvedBinary, flags: &[&str], input: &Path) -> Value {
    let mut args: Vec<_> = ["-v", "error", "-of", "json"]
        .into_iter()
        .chain(flags.iter().copied())
        .map(OsStr::new)
        .collect();
    args.push(input.as_os_str());
    serde_json::from_slice(&sidecar(binary, &args).await).unwrap()
}

fn stream<'a>(probe: &'a Value, kind: &str) -> &'a Value {
    probe["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stream| stream["codec_type"] == kind)
        .unwrap_or_else(|| panic!("missing {kind} stream in {probe}"))
}

async fn packet_hashes(binary: &ResolvedBinary, path: &Path, selector: &str) -> Vec<String> {
    probe(
        binary,
        &[
            "-select_streams",
            selector,
            "-show_packets",
            "-show_data_hash",
            "sha256",
        ],
        path,
    )
    .await["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|packet| packet["data_hash"].as_str().unwrap().to_owned())
        .collect()
}

async fn build_playlist_tool(root: &Path) -> PathBuf {
    let executable = root.join("playlist-tool.exe");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/playlist_tool.rs");
    let status = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--edition=2021")
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    executable
}

async fn check_playlist_recovery(root: &Path, ffmpeg: &ResolvedBinary, ffprobe: &ResolvedBinary) {
    let executable = build_playlist_tool(root).await;
    let tools = MediaToolchain::discover(ToolchainConfig {
        ytdlp_path: Some(executable),
        ffmpeg_path: Some(ffmpeg.path.clone()),
        ffprobe_path: Some(ffprobe.path.clone()),
        ..ToolchainConfig::default()
    })
    .await
    .unwrap();

    for status in [1, 101] {
        let save_dir = root.join(format!("playlist-{status}"));
        let pipeline = MediaPipeline::new(tools.clone(), &save_dir);
        let request = AddMediaDownloadRequest {
            url: format!("https://example.com/playlist/{status}"),
            format_id: None,
            audio_only_preset: None,
            target_container: None,
            embed_subtitles_langs: Vec::new(),
            lossless_remux: false,
            normalize_audio: false,
            save_dir: None,
            filename: None,
            playlist_items: None,
            download_playlist: true,
            max_height: None,
            selected_entries: None,
            playlist_folder_name: None,
            headers: Default::default(),
        };
        let outcome = pipeline
            .download_with_summary(&request, &DownloadOptions::default(), None)
            .await
            .unwrap();
        assert_eq!(outcome.paths.len(), 2);
        assert_eq!(fs::read_dir(&save_dir).unwrap().count(), 2);
        assert_eq!(
            outcome.completion_notice.as_deref(),
            Some("2 of 4 videos downloaded (2 skipped).")
        );
        assert_eq!(
            fs::read(&outcome.paths[0]).unwrap(),
            fs::read(root.join("synthetic.mp4")).unwrap()
        );
        assert_eq!(outcome.paths[1].file_name().unwrap(), "Recovery-000002.mp4");
        let details = probe(
            ffprobe,
            &["-show_format", "-show_streams"],
            &outcome.paths[1],
        )
        .await;
        assert!(details["format"]["format_name"]
            .as_str()
            .unwrap()
            .split(',')
            .any(|name| name == "mp4"));
        assert_eq!(stream(&details, "video")["codec_name"], "h264");
        assert_eq!(stream(&details, "audio")["codec_name"], "aac");
    }
}

#[tokio::test]
async fn fast_probe_and_single_video_download_reuse_matching_probe() {
    let temp = TempDir::new();
    let executable = build_playlist_tool(&temp.0).await;
    let binary = discover_ytdlp_only(ToolchainConfig {
        ytdlp_path: Some(executable.clone()),
        ffmpeg_path: Some(temp.0.join("missing-ffmpeg.exe")),
        ffprobe_path: Some(temp.0.join("missing-ffprobe.exe")),
        ..ToolchainConfig::default()
    })
    .await
    .unwrap();
    let cache_dir = temp.0.join("probe-cache");
    let request = AddMediaDownloadRequest {
        url: "https://example.com/video".into(),
        format_id: None,
        audio_only_preset: None,
        target_container: None,
        embed_subtitles_langs: Vec::new(),
        lossless_remux: false,
        normalize_audio: false,
        save_dir: None,
        filename: None,
        playlist_items: None,
        download_playlist: false,
        max_height: Some(720),
        selected_entries: None,
        playlist_folder_name: None,
        headers: [("user-agent".into(), "SlipStream Test".into())].into(),
    };
    let mut cached = probe_url_with_binary(
        &binary,
        &request.url,
        ProbeOptions {
            cache_dir: Some(cache_dir.clone()),
            headers: request.headers.clone(),
            ..ProbeOptions::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(cached.title, "Fresh video");
    let marker = temp.0.join("probed");
    fs::remove_file(&marker).unwrap();
    cached.title = "Cached video".into();
    cached.source_url = "https://EXAMPLE.com:443/video".into();
    let tools = MediaToolchain::discover(ToolchainConfig {
        ytdlp_path: Some(executable.clone()),
        ffmpeg_path: Some(executable.clone()),
        ffprobe_path: Some(executable),
        ..ToolchainConfig::default()
    })
    .await
    .unwrap();

    for (name, source_url, is_playlist, expected_file, expected_probe) in [
        (
            "match",
            cached.source_url.as_str(),
            false,
            "Cached video.mp4",
            false,
        ),
        (
            "url-mismatch",
            "https://example.com/another",
            false,
            "Fresh video.mp4",
            true,
        ),
        (
            "mode-mismatch",
            cached.source_url.as_str(),
            true,
            "Fresh video.mp4",
            true,
        ),
    ] {
        let mut pre_probe = cached.clone();
        pre_probe.source_url = source_url.into();
        pre_probe.is_playlist = is_playlist;
        let pipeline = MediaPipeline::new(tools.clone(), temp.0.join(name));
        let paths = pipeline
            .download(
                &request,
                &DownloadOptions {
                    cache_dir: Some(cache_dir.clone()),
                    pre_probe: Some(pre_probe),
                    ..DownloadOptions::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(paths[0].file_name().unwrap(), expected_file);
        assert_eq!(marker.exists(), expected_probe, "{name}");
        if expected_probe {
            fs::remove_file(&marker).unwrap();
        }
    }

    let request = AddMediaDownloadRequest {
        filename: Some("02 - Transport Stream".into()),
        download_playlist: true,
        playlist_items: Some("2".into()),
        ..request
    };
    let directory = temp.0.join("Numbered playlist");
    let result = MediaPipeline::new(tools, &directory)
        .download(
            &request,
            &DownloadOptions {
                pre_probe: Some(cached),
                cache_dir: Some(cache_dir),
                ..DownloadOptions::default()
            },
            None,
        )
        .await
        .unwrap();
    assert!(temp.0.join("playlist-probed").is_file());
    assert_eq!(
        result,
        [fs::canonicalize(directory)
            .unwrap()
            .join("02 - Transport Stream.webm")]
    );
    assert_eq!(fs::read(&result[0]).unwrap(), b"single playlist item");
}

fn sha256(path: &Path) -> String {
    let mut hash = Sha256::new();
    std::io::copy(&mut fs::File::open(path).unwrap(), &mut hash).unwrap();
    hex::encode(hash.finalize())
}

#[tokio::test]
async fn real_ffmpeg_pipeline() {
    let tools = match MediaToolchain::discover_ffmpeg_only(ToolchainConfig::default()).await {
        Ok(tools) => tools,
        Err(MediaError::BinaryNotFound { tool: "ffmpeg" }) => {
            eprintln!("FFmpeg integration skipped: ffmpeg is missing from PATH");
            return;
        }
        Err(err) => panic!("FFmpeg discovery failed: {err}"),
    };
    let Some(ffprobe) = tools.ffprobe().cloned() else {
        eprintln!("FFmpeg integration skipped: ffprobe is missing from PATH");
        return;
    };
    assert!(tools.ytdlp().is_none());
    assert!(tools.ffmpeg().sha256.is_empty());
    assert!(tools.ffmpeg().version.starts_with("ffmpeg version"));
    assert!(ffprobe.version.starts_with("ffprobe version"));
    let ffmpeg_bin = tools.ffmpeg().clone();

    let temp = TempDir::new();
    let input = temp.0.join("synthetic.mp4");
    let cover = temp.0.join("cover.png");
    let srt = temp.0.join("captions.srt");
    let save_dir = temp.0.join("outputs");
    // These fixture sources are fixed in the test. The production builder only
    // accepts standalone local file demuxers.
    fixture(
        tools.ffmpeg(),
        &[
            "-hide_banner",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=blue:size=96x64:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "1.2",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-f",
            "mp4",
        ],
        &input,
    )
    .await;
    fixture(
        tools.ffmpeg(),
        &[
            "-hide_banner",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=red:size=32x32",
            "-frames:v",
            "1",
            "-update",
            "1",
            "-c:v",
            "png",
            "-f",
            "image2",
        ],
        &cover,
    )
    .await;
    fs::write(&srt, "1\n00:00:00,100 --> 00:00:00,750\nHello SlipStream\n").unwrap();

    let ts_in_mp4 = temp.0.join("hls-segments.mp4");
    fixture(
        &ffmpeg_bin,
        &[
            "-hide_banner",
            "-nostdin",
            "-y",
            "-i",
            input.to_str().unwrap(),
            "-c",
            "copy",
            "-f",
            "mpegts",
        ],
        &ts_in_mp4,
    )
    .await;
    let pipeline = MediaPipeline::new(tools, &save_dir);
    let converted = pipeline
        .remux(
            &ts_in_mp4,
            "hls-faststart.mp4",
            MediaContainer::Mp4,
            &LocalMediaOptions::default(),
            None,
        )
        .await
        .unwrap();
    let detected = probe(&ffprobe, &["-show_format", "-show_streams"], &converted).await;
    assert!(detected["format"]["format_name"]
        .as_str()
        .unwrap()
        .split(',')
        .any(|name| name == "mp4"));
    assert_eq!(stream(&detected, "video")["codec_name"], "h264");
    assert_eq!(stream(&detected, "audio")["codec_name"], "aac");
    let bytes = fs::read(converted).unwrap();
    assert!(
        bytes.windows(4).position(|part| part == b"moov").unwrap()
            < bytes.windows(4).position(|part| part == b"mdat").unwrap()
    );
    let disguised_playlist = temp.0.join("disguised.mp4");
    fs::write(
        &disguised_playlist,
        "#EXTM3U\n#EXTINF:1,\nhls-segments.mp4\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    assert!(pipeline
        .remux(
            &disguised_playlist,
            "bad.mp4",
            MediaContainer::Mp4,
            &LocalMediaOptions::default(),
            None
        )
        .await
        .is_err());
    assert!(!save_dir.join("bad.mp4").exists());
    check_playlist_recovery(&temp.0, &ffmpeg_bin, &ffprobe).await;

    let options = LocalMediaOptions {
        subtitles: vec![SubtitleInput {
            path: srt,
            language: "en".into(),
        }],
        metadata: MediaMetadata {
            title: Some("Café & Crème [4K 60fps]".into()),
            ..MediaMetadata::default()
        },
        source_url: Some(SOURCE.into()),
        ..LocalMediaOptions::default()
    };
    let (tx, mut rx) = mpsc::channel(32);
    let (remux, events) = tokio::join!(
        async {
            let result = pipeline
                .remux(
                    &input,
                    "東京建築_4K_HDR.mkv",
                    MediaContainer::Mkv,
                    &options,
                    Some(&tx),
                )
                .await;
            drop(tx);
            result
        },
        async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        }
    );
    let mkv = remux.unwrap();
    assert_eq!(mkv.parent().unwrap(), fs::canonicalize(&save_dir).unwrap());
    assert_eq!(
        events.first().unwrap().transfer_status(),
        TransferStatus::Remuxing
    );
    assert!(events.iter().any(|event| matches!(event, MediaProgressEvent::Remuxing(progress) if progress.finished && progress.out_time_micros.is_some())));
    assert_eq!(
        events.last(),
        Some(&MediaProgressEvent::Completed {
            paths: vec![mkv.clone()]
        })
    );
    let details = probe(&ffprobe, &["-show_streams", "-show_format"], &mkv).await;
    assert_eq!(stream(&details, "video")["codec_name"], "h264");
    assert_eq!(stream(&details, "audio")["codec_name"], "aac");
    assert_eq!(stream(&details, "subtitle")["codec_name"], "subrip");
    assert_eq!(stream(&details, "subtitle")["tags"]["language"], "eng");
    assert_eq!(
        details["format"]["tags"]["title"],
        "Café & Crème [4K 60fps]"
    );
    let from_mkv = pipeline
        .remux(
            &mkv,
            "from-mkv.mp4",
            MediaContainer::Mp4,
            &LocalMediaOptions {
                subtitles: vec![SubtitleInput {
                    path: options.subtitles[0].path.clone(),
                    language: "es".into(),
                }],
                ..LocalMediaOptions::default()
            },
            None,
        )
        .await
        .unwrap();
    let details = probe(&ffprobe, &["-show_streams"], &from_mkv).await;
    let languages: Vec<_> = details["streams"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|stream| stream["codec_type"] == "subtitle")
        .map(|stream| {
            assert_eq!(stream["codec_name"], "mov_text");
            stream["tags"]["language"].as_str().unwrap()
        })
        .collect();
    assert_eq!(languages, ["spa", "eng"]);
    for stream in ["v:0", "a:0"] {
        assert_eq!(
            packet_hashes(&ffprobe, &input, stream).await,
            packet_hashes(&ffprobe, &mkv, stream).await
        );
    }
    #[cfg(windows)]
    {
        let mut ads = mkv.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        let marker = fs::read_to_string(PathBuf::from(ads)).unwrap();
        assert_eq!(
            marker,
            format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={SOURCE}\r\n")
        );
    }

    let mp4 = pipeline
        .remux(
            &input,
            "with-cover.mp4",
            MediaContainer::Mp4,
            &LocalMediaOptions {
                cover_art: Some(cover.clone()),
                ..options
            },
            None,
        )
        .await
        .unwrap();
    let details = probe(&ffprobe, &["-show_streams"], &mp4).await;
    assert_eq!(stream(&details, "subtitle")["codec_name"], "mov_text");
    assert_eq!(stream(&details, "subtitle")["tags"]["language"], "eng");
    let video_streams: Vec<_> = details["streams"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|stream| stream["codec_type"] == "video")
        .collect();
    assert_eq!(video_streams.len(), 2);
    assert_eq!(video_streams[0]["codec_name"], "h264");
    assert_eq!(video_streams[0]["disposition"]["attached_pic"], 0);
    assert_eq!(video_streams[1]["disposition"]["attached_pic"], 1);

    for (ext, muxer) in [("mov", "mov"), ("m4v", "mp4")] {
        let input = temp.0.join(format!("with-text.{ext}"));
        fixture(
            &ffmpeg_bin,
            &[
                "-hide_banner",
                "-nostdin",
                "-y",
                "-i",
                mp4.to_str().unwrap(),
                "-map",
                "0:V",
                "-map",
                "0:a",
                "-map",
                "0:s",
                "-c",
                "copy",
                "-f",
                muxer,
            ],
            &input,
        )
        .await;
        let remuxed = pipeline
            .remux(
                &input,
                &format!("from-{ext}.mp4"),
                MediaContainer::Mp4,
                &LocalMediaOptions::default(),
                None,
            )
            .await
            .unwrap();
        let details = probe(&ffprobe, &["-show_streams"], &remuxed).await;
        assert_eq!(stream(&details, "subtitle")["codec_name"], "mov_text");
        assert_eq!(stream(&details, "subtitle")["tags"]["language"], "eng");
        assert_eq!(
            packet_hashes(&ffprobe, &input, "s:0").await,
            packet_hashes(&ffprobe, &remuxed, "s:0").await,
        );
    }

    // Re-remux an MP4 that already contains an attached_pic stream; -select_streams V
    // must ignore the prior cover art so the replacement cover stays at v:1.
    let reembedded = pipeline
        .remux(
            &mp4,
            "reembedded.mp4",
            MediaContainer::Mp4,
            &LocalMediaOptions {
                cover_art: Some(cover.clone()),
                subtitles: Vec::new(),
                source_url: Some("https://user:secret@example.com/watch?v=1".into()),
                ..LocalMediaOptions::default()
            },
            None,
        )
        .await
        .unwrap();
    let details = probe(&ffprobe, &["-show_streams"], &reembedded).await;
    assert_eq!(stream(&details, "subtitle")["codec_name"], "mov_text");
    let re_video_streams: Vec<_> = details["streams"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|stream| stream["codec_type"] == "video")
        .collect();
    assert_eq!(re_video_streams.len(), 2);
    assert_eq!(re_video_streams[0]["codec_name"], "h264");
    assert_eq!(re_video_streams[0]["disposition"]["attached_pic"], 0);
    assert_eq!(re_video_streams[1]["disposition"]["attached_pic"], 1);
    #[cfg(windows)]
    {
        let mut ads = reembedded.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        let marker = fs::read_to_string(PathBuf::from(ads)).unwrap();
        assert_eq!(
            marker,
            "[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.com/watch?v=1\r\n"
        );
    }

    // M4a remux has subtitle_codec == None and maps only 0:a:0 + cover art (v:0).
    let m4a_cover = pipeline
        .remux(
            &input,
            "cover-only.m4a",
            MediaContainer::M4a,
            &LocalMediaOptions {
                cover_art: Some(cover.clone()),
                ..LocalMediaOptions::default()
            },
            None,
        )
        .await
        .unwrap();
    let details = probe(&ffprobe, &["-show_streams"], &m4a_cover).await;
    assert_eq!(stream(&details, "audio")["codec_name"], "aac");
    assert_eq!(stream(&details, "video")["disposition"]["attached_pic"], 1);

    let ffmpeg_pin = sha256(&ffmpeg_bin.path);
    let ffprobe_pin = sha256(&ffprobe.path);
    let pinned_tools = MediaToolchain::discover_ffmpeg_only(ToolchainConfig {
        ffmpeg_path: Some(ffmpeg_bin.path.clone()),
        expected_ffmpeg_sha256: Some(ffmpeg_pin.clone()),
        expected_ffprobe_sha256: Some(ffprobe_pin.clone()),
        ..ToolchainConfig::default()
    })
    .await
    .unwrap();
    assert_eq!(pinned_tools.ffmpeg().sha256, ffmpeg_pin);
    assert_eq!(pinned_tools.ffprobe().unwrap().sha256, ffprobe_pin);

    for (preset, extension, codec) in [
        (AudioPreset::Mp3_320, "mp3", "mp3"),
        (AudioPreset::M4aAac, "m4a", "aac"),
        (AudioPreset::Opus, "opus", "opus"),
        (AudioPreset::Flac, "flac", "flac"),
        (AudioPreset::Wav, "wav", "pcm_s16le"),
    ] {
        let audio = pipeline
            .extract_audio(
                &input,
                &format!("ملف_تجريبي.{extension}"),
                preset,
                &LocalMediaOptions {
                    cover_art: (preset == AudioPreset::Mp3_320).then(|| cover.clone()),
                    metadata: MediaMetadata {
                        artist: Some("SlipStream Test".into()),
                        ..MediaMetadata::default()
                    },
                    ..LocalMediaOptions::default()
                },
                None,
            )
            .await
            .unwrap();
        let details = probe(&ffprobe, &["-show_streams", "-show_format"], &audio).await;
        assert_eq!(stream(&details, "audio")["codec_name"], codec);
        if preset == AudioPreset::Mp3_320 {
            assert_eq!(stream(&details, "audio")["bit_rate"], "320000");
            assert_eq!(stream(&details, "video")["disposition"]["attached_pic"], 1);
            assert_eq!(details["format"]["tags"]["artist"], "SlipStream Test");
        }
        if preset == AudioPreset::Flac {
            let normalized = pipeline
                .extract_audio(
                    &input,
                    "normalized.flac",
                    AudioPreset::Flac,
                    &LocalMediaOptions {
                        normalize_audio: true,
                        ..LocalMediaOptions::default()
                    },
                    None,
                )
                .await
                .unwrap();
            let details = probe(&ffprobe, &["-show_streams"], &normalized).await;
            assert_eq!(stream(&details, "audio")["codec_name"], "flac");
            assert_ne!(
                packet_hashes(&ffprobe, &audio, "a:0").await,
                packet_hashes(&ffprobe, &normalized, "a:0").await
            );
        }
    }

    let (tx, mut rx) = mpsc::channel(4);
    let duplicate = pipeline
        .remux(
            &input,
            "with-cover.mp4",
            MediaContainer::Mp4,
            &LocalMediaOptions::default(),
            Some(&tx),
        )
        .await;
    assert!(matches!(duplicate, Err(MediaError::DestinationExists(_))));
    assert_eq!(
        rx.recv().await.unwrap().transfer_status(),
        TransferStatus::Failed
    );
    assert!(fs::read_dir(save_dir).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".slipstream-")));
}

async fn spawn_media_server(
    files: BTreeMap<&'static str, (&'static str, Vec<u8>)>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let files = Arc::new(files);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let files = files.clone();
            tokio::spawn(async move {
                let mut req_buf = Vec::new();
                let mut chunk = [0_u8; 2048];
                while !req_buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            req_buf.extend_from_slice(&chunk[..n]);
                            if req_buf.len() > 32 * 1024 {
                                return;
                            }
                        }
                    }
                }
                let req = String::from_utf8_lossy(&req_buf);
                let is_head = req.starts_with("HEAD ");
                let path = req.split_ascii_whitespace().nth(1).unwrap_or_default();
                let Some((mime, data)) = files.get(path) else {
                    let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    return;
                };
                let total = data.len();
                let (status, start, end) = req
                    .lines()
                    .find_map(|line| {
                        let (name, val) = line.split_once(':')?;
                        if !name.eq_ignore_ascii_case("Range") {
                            return None;
                        }
                        let spec = val.trim().strip_prefix("bytes=")?;
                        let (s, e) = spec.split_once('-')?;
                        let start = s.parse::<usize>().ok()?;
                        let end = if e.is_empty() {
                            total.saturating_sub(1)
                        } else {
                            e.parse::<usize>().ok()?.min(total.saturating_sub(1))
                        };
                        (start <= end && end < total).then_some((206, start, end))
                    })
                    .unwrap_or((200, 0, total.saturating_sub(1)));
                let slice = &data[start..=end];
                let header = if status == 206 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: {mime}\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        slice.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nAccept-Ranges: bytes\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                    )
                };
                if socket.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                if !is_head {
                    let _ = socket.write_all(slice).await;
                }
            });
        }
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn real_ytdlp_probe_and_download_loopback() {
    let tools = match MediaToolchain::discover(ToolchainConfig::default()).await {
        Ok(tools) => tools,
        Err(MediaError::BinaryNotFound { .. }) => {
            eprintln!("yt-dlp integration skipped: yt-dlp or ffmpeg missing from PATH");
            return;
        }
        Err(err) => panic!("toolchain discovery failed: {err}"),
    };
    let Some(ffprobe) = tools.ffprobe().cloned() else {
        eprintln!("yt-dlp integration skipped: ffprobe missing from PATH");
        return;
    };

    let temp = TempDir::new();
    let input = temp.0.join("source.mp4");
    fixture(
        tools.ffmpeg(),
        &[
            "-hide_banner",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=green:size=64x48:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=520:sample_rate=48000",
            "-t",
            "0.8",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-f",
            "mp4",
        ],
        &input,
    )
    .await;
    let video = temp.0.join("video.ts");
    let audio = temp.0.join("audio.aac");
    for (path, only, muxer) in [(&video, "-an", "mpegts"), (&audio, "-vn", "adts")] {
        fixture(
            tools.ffmpeg(),
            &[
                "-hide_banner",
                "-nostdin",
                "-y",
                "-i",
                input.to_str().unwrap(),
                only,
                "-c",
                "copy",
                "-f",
                muxer,
            ],
            path,
        )
        .await;
    }
    let mime = "application/vnd.apple.mpegurl";
    let media_playlist = |file| {
        format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:0.8,\n{file}\n#EXT-X-ENDLIST\n")
            .into_bytes()
    };
    let master = concat!(
        "#EXTM3U\n",
        "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,URI=\"audio.m3u8\"\n",
        "#EXT-X-STREAM-INF:BANDWIDTH=450000,RESOLUTION=64x48,FRAME-RATE=10.0,CODECS=\"avc1.64000a,mp4a.40.2\",AUDIO=\"aud\"\n",
        "video.m3u8\n",
    );
    let (base, server) = spawn_media_server(BTreeMap::from([
        (
            "/synthetic_clip.mp4",
            ("video/mp4", fs::read(&input).unwrap()),
        ),
        ("/master.m3u8", (mime, master.as_bytes().to_vec())),
        ("/audio.m3u8", (mime, media_playlist("audio.aac"))),
        ("/video.m3u8", (mime, media_playlist("video.ts"))),
        ("/audio.aac", ("audio/aac", fs::read(audio).unwrap())),
        ("/video.ts", ("video/mp2t", fs::read(video).unwrap())),
    ]))
    .await;
    let url = format!("{base}/synthetic_clip.mp4");

    let probe_res = probe_url(&tools, &url, ProbeOptions::default())
        .await
        .unwrap();
    assert!(!probe_res.is_playlist);
    assert!(!probe_res.formats.is_empty());

    let save_dir = temp.0.join("downloads");
    let pipeline = MediaPipeline::new(tools.clone(), &save_dir);
    let (tx, mut rx) = mpsc::channel(32);
    let request = AddMediaDownloadRequest {
        url: url.clone(),
        format_id: None,
        audio_only_preset: Some(AudioPreset::Mp3_320),
        target_container: None,
        embed_subtitles_langs: Vec::new(),
        lossless_remux: false,
        normalize_audio: false,
        save_dir: None,
        filename: Some("My Clip.v2.mp4".to_owned()),
        playlist_items: None,
        download_playlist: false,
        max_height: None,
        selected_entries: None,
        playlist_folder_name: None,
        headers: Default::default(),
    };
    let (download, events) = tokio::join!(
        async {
            let out = pipeline
                .download(&request, &DownloadOptions::default(), Some(&tx))
                .await;
            drop(tx);
            out
        },
        async {
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            events
        }
    );
    let paths = download.unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(
        paths[0].file_name().and_then(|n| n.to_str()),
        Some("My Clip.v2.mp3")
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, MediaProgressEvent::Probing)));
    assert!(events
        .iter()
        .any(|e| matches!(e, MediaProgressEvent::Downloading(_))));
    assert!(events
        .iter()
        .any(|e| matches!(e, MediaProgressEvent::Remuxing(_))));
    assert_eq!(
        events.last(),
        Some(&MediaProgressEvent::Completed {
            paths: paths.clone()
        })
    );

    let details = probe(&ffprobe, &["-show_streams"], &paths[0]).await;
    assert_eq!(stream(&details, "audio")["codec_name"], "mp3");
    assert_eq!(stream(&details, "audio")["bit_rate"], "320000");

    #[cfg(windows)]
    {
        let mut ads = paths[0].as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        let marker = fs::read_to_string(PathBuf::from(ads)).unwrap();
        assert_eq!(
            marker,
            format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={url}\r\n")
        );
    }

    let hls_url = format!("{base}/master.m3u8");
    let hls = probe_url(&tools, &hls_url, ProbeOptions::default())
        .await
        .unwrap();
    let video = hls
        .formats
        .iter()
        .find(|f| f.has_video && !f.has_audio)
        .unwrap();
    let audio = hls.formats.iter().find(|f| f.is_audio_only).unwrap();
    let merged = pipeline
        .download(
            &AddMediaDownloadRequest {
                url: hls_url,
                format_id: Some(format!("{}+{}", video.format_id, audio.format_id)),
                audio_only_preset: None,
                filename: Some("Merged HLS".into()),
                lossless_remux: true,
                ..request
            },
            &DownloadOptions::default(),
            None,
        )
        .await
        .unwrap();
    server.abort();
    let expected_format = match merged[0].extension().and_then(OsStr::to_str) {
        Some("mp4") => "mp4",
        Some("mkv") => "matroska",
        extension => panic!("unexpected output extension {extension:?}"),
    };
    let details = probe(&ffprobe, &["-show_format", "-show_streams"], &merged[0]).await;
    assert!(details["format"]["format_name"]
        .as_str()
        .unwrap()
        .split(',')
        .any(|name| name == expected_format));
    assert_eq!(stream(&details, "video")["codec_name"], "h264");
    assert_eq!(stream(&details, "audio")["codec_name"], "aac");
}
