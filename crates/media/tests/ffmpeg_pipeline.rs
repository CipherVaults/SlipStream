use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::Value;
use slipstream_media::{
    probe_url, DownloadOptions, LocalMediaOptions, MediaError, MediaMetadata, MediaPipeline,
    MediaProgressEvent, MediaToolchain, ProbeOptions, ResolvedBinary, SubtitleInput,
    ToolchainConfig,
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
    assert_eq!(tools.ffmpeg().sha256.len(), 64);
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

    let pipeline = MediaPipeline::new(tools, &save_dir);
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

    let pinned_tools = MediaToolchain::discover_ffmpeg_only(ToolchainConfig {
        ffmpeg_path: Some(ffmpeg_bin.path.clone()),
        expected_ffmpeg_sha256: Some(ffmpeg_bin.sha256.clone()),
        expected_ffprobe_sha256: Some(ffprobe.sha256.clone()),
        ..ToolchainConfig::default()
    })
    .await
    .unwrap();
    assert_eq!(pinned_tools.ffmpeg().sha256, ffmpeg_bin.sha256);

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

async fn spawn_mp4_server(payload: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let data = payload.clone();
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
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        slice.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
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
    (format!("http://{addr}/synthetic_clip.mp4"), handle)
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
    let payload = fs::read(&input).unwrap();
    let (url, server) = spawn_mp4_server(payload).await;

    let probe_res = probe_url(&tools, &url, ProbeOptions { playlist: false })
        .await
        .unwrap();
    assert!(!probe_res.is_playlist);
    assert!(!probe_res.formats.is_empty());

    let save_dir = temp.0.join("downloads");
    let pipeline = MediaPipeline::new(tools, &save_dir);
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
    server.abort();

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
}
