#![cfg(windows)]

use slipstream_desktop_lib::{AppState, DaemonOptions, OpenPickerEvent, UiBridge};
use slipstream_native_host::{pipe::PipeClient, token::SessionToken};
use slipstream_protocol::{
    AccentMode, AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings,
    NativeBridgeRequest as Request, NativeBridgeResponse as Response, RefreshDownloadUrlRequest,
    TelemetryBatch, ThemeMode, TransferKind, TransferSnapshot, TransferStatus,
};
use slipstream_torrent::TorrentManagerConfig;
use std::{
    env, fs,
    future::Future,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    pin::Pin,
    process::Command,
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
    time::{sleep, timeout},
};
use uuid::Uuid;
use windows_sys::Win32::{
    Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    },
    System::{
        Ioctl::FSCTL_SET_REPARSE_POINT,
        Registry::{RegDeleteTreeW, HKEY_CURRENT_USER},
        SystemServices::IO_REPARSE_TAG_MOUNT_POINT,
        IO::DeviceIoControl,
    },
};

const TEST_HASH: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = env::temp_dir().join(format!("slipstream-desktop-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FakeUi {
    visible: AtomicBool,
    quitting: AtomicBool,
    presence_checks: AtomicUsize,
    snapshots: Mutex<Vec<TelemetryBatch>>,
    events: Mutex<Vec<OpenPickerEvent>>,
}

impl UiBridge for FakeUi {
    fn show_main_window(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.visible.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn emit_telemetry(&self, batch: &TelemetryBatch) -> Result<(), String> {
        self.snapshots.lock().unwrap().push(batch.clone());
        Ok(())
    }

    fn emit_open_picker(&self, event: &OpenPickerEvent) -> Result<(), String> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    fn has_main_window(&self) -> bool {
        self.presence_checks.fetch_add(1, Ordering::SeqCst);
        self.visible.load(Ordering::SeqCst)
    }

    fn request_quit(&self) {
        self.quitting.store(true, Ordering::SeqCst);
    }
}

struct HttpServer {
    base_url: String,
    task: JoinHandle<()>,
}

impl HttpServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, _) = result.unwrap();
                        connections.spawn(serve(stream));
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self { base_url, task }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        request.push(stream.read_u8().await?);
    }
    let request = String::from_utf8_lossy(&request);
    let header = request.lines().next().unwrap();
    let path = header.split_ascii_whitespace().nth(1).unwrap();
    if path == "/404" {
        return stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    }
    if header.starts_with("HEAD ") {
        return stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\nETag: \"test\"\r\nConnection: close\r\n\r\n").await;
    }
    let range = request.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case("range").then_some(value.trim())
    });
    let data = b"test";
    if let Some(range) = range {
        let (first, last) = range
            .strip_prefix("bytes=")
            .unwrap()
            .split_once('-')
            .unwrap();
        let first: usize = first.parse().unwrap();
        let last: usize = last.parse().unwrap();
        let response = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {first}-{last}/4\r\nContent-Length: {}\r\nETag: \"test\"\r\nConnection: close\r\n\r\n", last - first + 1);
        stream.write_all(response.as_bytes()).await?;
        if path.starts_with("/hold") {
            stream.write_all(&data[first..=first]).await?;
            sleep(Duration::from_secs(30)).await;
        }
        return stream.write_all(&data[first..=last]).await;
    }
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntest")
        .await
}

fn http_request(server: &HttpServer, path: &str, filename: &str) -> AddHttpDownloadRequest {
    AddHttpDownloadRequest {
        url: server.url(path),
        filename: Some(filename.into()),
        save_dir: None,
        max_connections: Some(1),
        queue_id: None,
        headers: Default::default(),
        referrer: None,
        expected_sha256: None,
    }
}

async fn wait_for(state: &AppState, id: &str, status: TransferStatus) -> TransferSnapshot {
    timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = state
                .snapshot()
                .await
                .transfers
                .into_iter()
                .find(|snapshot| snapshot.id == id)
                .unwrap();
            if snapshot.status == status {
                return snapshot;
            }
            if snapshot.status == TransferStatus::Failed {
                panic!("unexpected failure for {id}: {:?}", snapshot.error_message);
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{id} did not reach {status:?}"))
}

struct RegistryKey(String);

impl RegistryKey {
    fn isolated() -> Self {
        Self(format!(
            r"Software\SlipStreamDesktopDaemonTest-{}",
            Uuid::new_v4()
        ))
    }
}

impl Drop for RegistryKey {
    fn drop(&mut self) {
        let wide: Vec<_> = self.0.encode_utf16().chain([0]).collect();
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide.as_ptr()) };
    }
}

fn options(pipe: &str, registry_key: &str) -> DaemonOptions {
    DaemonOptions {
        pipe_name: Some(pipe.into()),
        allow_loopback: true,
        registry_base_key: Some(registry_key.into()),
        resource_dir: None,
        torrent: TorrentManagerConfig {
            enable_dht: false,
            enable_trackers: false,
            listen_port_range: None,
            ..TorrentManagerConfig::default()
        },
    }
}

fn installed_settings(data: &Path, downloads: &Path) -> AppSettings {
    let settings = AppSettings {
        default_download_dir: downloads.to_string_lossy().into_owned(),
        auto_defender_scan: false,
        ..AppSettings::default()
    };
    fs::write(
        data.join("settings.json"),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    settings
}

async fn media_tool_fixture(directory: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/media_tool.rs");
    let exe = directory.join("media-tool.exe");
    let status = tokio::process::Command::from(Command::new(
        env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()),
    ))
    .arg("--edition=2021")
    .arg(source)
    .arg("-o")
    .arg(&exe)
    .status()
    .await
    .unwrap();
    assert!(status.success());
    exe
}

fn torrent_file(directory: &Path) -> PathBuf {
    // A single four-byte file with one SHA-1 piece, SHA-1("test").
    let mut bytes =
        b"d4:infod6:lengthi4e4:name11:torrent.bin12:piece lengthi16384e6:pieces20:".to_vec();
    bytes.extend([
        0xa9, 0x4a, 0x8f, 0xe5, 0xcc, 0xb1, 0x9b, 0xa6, 0x1c, 0x4c, 0x08, 0x73, 0xd3, 0x91, 0xe9,
        0x87, 0x98, 0x2f, 0xbb, 0xd3,
    ]);
    bytes.extend(b"ee");
    let path = directory.join("fixture.torrent");
    fs::write(&path, bytes).unwrap();
    path
}

struct Bridge(Option<PipeClient>);

impl Bridge {
    async fn request(&mut self, request: Request) -> Response {
        let (client, response) = self.0.take().unwrap().request(&request).await.unwrap();
        self.0 = Some(client);
        response
    }

    async fn ack(&mut self, request: Request) {
        let response = self.request(request).await;
        assert!(
            matches!(
                response,
                Response::Ack {
                    ok: true,
                    message: None
                }
            ),
            "{response:?}"
        );
    }

    async fn created(&mut self, request: Request) -> String {
        match self.request(request).await {
            Response::TransferCreated { transfer_id } => transfer_id,
            other => panic!("expected a created transfer, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pipe_routes_transfers_and_reopens_a_hibernated_window() {
    let temp = TempDir::new();
    let dir = &temp.0;
    let downloads = dir.join("Downloads");
    let settings = installed_settings(dir, &downloads);
    let pipe = format!(r"\\.\pipe\SlipStream.DesktopTest.{}", Uuid::new_v4());
    let registry_key = RegistryKey::isolated();
    let ui = Arc::new(FakeUi::default());
    let state = AppState::start(dir.clone(), ui.clone(), options(&pipe, &registry_key.0))
        .await
        .unwrap();
    sleep(Duration::from_millis(80)).await;
    let checks = ui.presence_checks.load(Ordering::SeqCst);
    sleep(Duration::from_millis(140)).await;
    assert_eq!(checks, ui.presence_checks.load(Ordering::SeqCst));
    assert!(ui.snapshots.lock().unwrap().is_empty());

    let token = SessionToken::read(&dir.join("session.token")).unwrap();
    assert!(
        PipeClient::connect(&pipe, &SessionToken::generate().unwrap(), "wrong-token")
            .await
            .is_err()
    );
    let mut bridge = Bridge(Some(
        PipeClient::connect(&pipe, &token, "desktop-integration")
            .await
            .unwrap(),
    ));
    bridge.ack(Request::Ping).await;
    assert!(matches!(
        bridge.request(Request::GetStatus).await,
        Response::Status { theme: ThemeMode::Dark, accent: AccentMode::Blue, batch } if batch.transfers.is_empty()
    ));

    let server = HttpServer::start().await;
    let http = bridge
        .created(Request::AddHttpDownload {
            request: http_request(&server, "hold", "transfer.bin"),
        })
        .await;
    wait_for(&state, &http, TransferStatus::Active).await;
    bridge
        .ack(Request::PauseTransfer {
            transfer_id: http.clone(),
        })
        .await;
    wait_for(&state, &http, TransferStatus::Paused).await;
    bridge
        .ack(Request::RefreshDownloadUrl {
            request: RefreshDownloadUrlRequest {
                transfer_id: http.clone(),
                new_url: server.url("fresh"),
                headers: Default::default(),
                referrer: None,
            },
        })
        .await;
    bridge
        .ack(Request::ResumeTransfer {
            transfer_id: http.clone(),
        })
        .await;
    wait_for(&state, &http, TransferStatus::Completed).await;
    assert_eq!(fs::read(downloads.join("transfer.bin")).unwrap(), b"test");
    assert!(state
        .verify_transfer_checksum(&http, "invalid")
        .await
        .is_err());
    assert!(!state
        .verify_transfer_checksum(&http, &"0".repeat(64))
        .await
        .unwrap());
    assert_eq!(
        state.snapshot().await.transfers[0].sha256_verified,
        Some(false)
    );
    assert!(state
        .verify_transfer_checksum(&http, &TEST_HASH.to_uppercase())
        .await
        .unwrap());
    assert_eq!(
        state.snapshot().await.transfers[0].sha256_verified,
        Some(true)
    );

    let executable = media_tool_fixture(dir).await;
    let executable = executable.to_str().unwrap().to_owned();
    let missing = dir.join("missing.exe").to_string_lossy().into_owned();
    state
        .save_settings(AppSettings {
            ytdlp_path: Some(executable.clone()),
            ffmpeg_path: Some(missing.clone()),
            ..settings.clone()
        })
        .await
        .unwrap();
    let tools = state.get_toolchain_status().await;
    assert_eq!(tools.ytdlp_version.as_deref(), Some("yt-dlp fixture 1.0"));
    assert!(tools.ffmpeg_path.is_none());
    assert!(!tools.native_hosts_installed);
    state
        .save_settings(AppSettings {
            ytdlp_path: Some(missing),
            ffmpeg_path: Some(executable.clone()),
            ..settings.clone()
        })
        .await
        .unwrap();
    let tools = state.get_toolchain_status().await;
    assert_eq!(tools.ffmpeg_version.as_deref(), Some("ffmpeg fixture 1.0"));
    assert!(tools.ytdlp_path.is_none());
    state
        .save_settings(AppSettings {
            ytdlp_path: Some(executable.clone()),
            ffmpeg_path: Some(executable),
            ..settings
        })
        .await
        .unwrap();

    let media_url = "https://example.com/video";
    assert!(matches!(
        bridge.request(Request::ProbeMedia { url: media_url.into() }).await,
        Response::MediaProbed { probe } if probe.title == "Fixture video"
    ));
    let media = bridge
        .created(Request::AddMediaDownload {
            request: AddMediaDownloadRequest {
                url: media_url.into(),
                format_id: Some("18".into()),
                audio_only_preset: None,
                target_container: None,
                embed_subtitles_langs: Vec::new(),
                lossless_remux: false,
                normalize_audio: false,
                save_dir: None,
                filename: Some("video.mp4".into()),
                playlist_items: None,
            },
        })
        .await;
    wait_for(&state, &media, TransferStatus::Active).await;
    bridge
        .ack(Request::PauseTransfer {
            transfer_id: media.clone(),
        })
        .await;
    wait_for(&state, &media, TransferStatus::Paused).await;
    bridge
        .ack(Request::ResumeTransfer {
            transfer_id: media.clone(),
        })
        .await;
    let media_snap = wait_for(&state, &media, TransferStatus::Completed).await;
    assert!(!media_snap.resumable);
    assert!(media_snap.format_tags.contains(&"18".to_owned()));
    assert!(state.pause_transfer(&media).await.is_err());
    assert_eq!(fs::read(downloads.join("video.mp4")).unwrap(), b"test");
    assert!(state
        .verify_transfer_checksum(&media, TEST_HASH)
        .await
        .unwrap());

    // Browser Named Pipe must reject non-magnet AddTorrent sources and any save_dir overrides.
    let local_torrent = torrent_file(dir);
    assert!(matches!(
        bridge
            .request(Request::AddTorrent {
                request: AddTorrentRequest {
                    source: local_torrent.to_string_lossy().into_owned(),
                    save_dir: None,
                    selected_file_indices: None,
                    sequential: false,
                },
            })
            .await,
        Response::Error { .. }
    ));
    assert!(matches!(
        bridge
            .request(Request::AddTorrent {
                request: AddTorrentRequest {
                    source: r"\\server\share\evil.torrent".into(),
                    save_dir: None,
                    selected_file_indices: None,
                    sequential: false,
                },
            })
            .await,
        Response::Error { .. }
    ));
    assert!(matches!(
        bridge
            .request(Request::AddHttpDownload {
                request: AddHttpDownloadRequest {
                    save_dir: Some(dir.to_string_lossy().into_owned()),
                    ..http_request(&server, "fresh", "override.bin")
                },
            })
            .await,
        Response::Error { .. }
    ));
    assert!(matches!(
        bridge
            .request(Request::AddMediaDownload {
                request: AddMediaDownloadRequest {
                    url: media_url.into(),
                    format_id: Some("18".into()),
                    audio_only_preset: None,
                    target_container: None,
                    embed_subtitles_langs: Vec::new(),
                    lossless_remux: false,
                    normalize_audio: false,
                    save_dir: Some(dir.to_string_lossy().into_owned()),
                    filename: Some("override.mp4".into()),
                    playlist_items: None,
                },
            })
            .await,
        Response::Error { .. }
    ));

    let torrent = state
        .add_torrent(AddTorrentRequest {
            source: local_torrent.to_string_lossy().into_owned(),
            save_dir: Some(downloads.to_string_lossy().into_owned()),
            selected_file_indices: Some(vec![0]),
            sequential: false,
        })
        .await
        .unwrap();
    let magnet_from_pipe = bridge
        .created(Request::AddTorrent {
            request: AddTorrentRequest {
                source: "magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&dn=PipeTest"
                    .into(),
                save_dir: None,
                selected_file_indices: None,
                sequential: false,
            },
        })
        .await;
    state
        .cancel_transfer(&magnet_from_pipe, false)
        .await
        .unwrap();
    assert_eq!(
        state.list_torrent_files(&torrent).await.unwrap()[0].relative_path,
        "torrent.bin"
    );
    state.set_torrent_files(&torrent, vec![0]).await.unwrap();
    state.set_speed_limit(Some(7)).await.unwrap();
    let batch = state.snapshot().await;
    assert_eq!(batch.speed_limit_bps, Some(7));
    assert_eq!(
        batch
            .transfers
            .iter()
            .map(|item| item.kind)
            .collect::<Vec<_>>(),
        [
            TransferKind::Http,
            TransferKind::Media,
            TransferKind::Torrent
        ]
    );
    state.set_speed_limit(None).await.unwrap();

    assert_eq!(state.registry_base_key(), registry_key.0);
    assert!(matches!(
        bridge
            .request(Request::OpenDesktopWindow {
                open_picker_url: Some("file:///C:/private".into())
            })
            .await,
        Response::Error { .. }
    ));
    assert!(!ui.visible.load(Ordering::SeqCst));
    bridge
        .ack(Request::OpenDesktopWindow {
            open_picker_url: Some(media_url.into()),
        })
        .await;
    assert!(ui.events.lock().unwrap().is_empty());
    let generation = state.window_generation();
    state.window_ready(generation.wrapping_add(1)).unwrap();
    assert!(ui.events.lock().unwrap().is_empty());
    state.window_ready(generation).unwrap();
    assert_eq!(
        *ui.events.lock().unwrap(),
        vec![OpenPickerEvent {
            url: Some(media_url.into())
        }]
    );
    bridge
        .ack(Request::OpenDesktopWindow {
            open_picker_url: None,
        })
        .await;
    assert_eq!(ui.events.lock().unwrap().last().unwrap().url, None);
    timeout(Duration::from_secs(1), async {
        while ui.snapshots.lock().unwrap().len() < 2 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    ui.visible.store(false, Ordering::SeqCst);
    let stale_generation = state.window_generation();
    let current_generation = state.window_loading();
    sleep(Duration::from_millis(80)).await;
    let checks = ui.presence_checks.load(Ordering::SeqCst);
    let emissions = ui.snapshots.lock().unwrap().len();
    sleep(Duration::from_millis(140)).await;
    assert_eq!(ui.presence_checks.load(Ordering::SeqCst), checks);
    assert_eq!(ui.snapshots.lock().unwrap().len(), emissions);
    let magnet = format!("magnet:?xt=urn:btih:{torrent}");
    bridge
        .ack(Request::OpenDesktopWindow {
            open_picker_url: Some(magnet.clone()),
        })
        .await;
    assert_eq!(ui.events.lock().unwrap().len(), 2);
    state.window_ready(stale_generation).unwrap();
    assert_eq!(ui.events.lock().unwrap().len(), 2);
    state.window_ready(current_generation).unwrap();
    assert_eq!(
        ui.events.lock().unwrap().last().unwrap().url.as_deref(),
        Some(magnet.as_str())
    );

    let count = state.snapshot().await.transfers.len();
    assert!(matches!(
        bridge
            .request(Request::GrabBatchUrls {
                urls: vec![server.url("one.bin"), "file:///C:/private".into()],
                referrer: None
            })
            .await,
        Response::Error { .. }
    ));
    assert_eq!(state.snapshot().await.transfers.len(), count);
    bridge
        .ack(Request::GrabBatchUrls {
            urls: vec![server.url("one.bin"), server.url("two.bin")],
            referrer: Some(server.url("index")),
        })
        .await;
    assert!(
        matches!(bridge.request(Request::GetStatus).await, Response::Status { batch, .. } if batch.transfers.len() == count + 2)
    );

    let active = state
        .add_http_download(http_request(&server, "hold-cancel", "active.bin"))
        .await
        .unwrap();
    let active_snapshot = wait_for(&state, &active, TransferStatus::Active).await;
    let part = PathBuf::from(active_snapshot.save_dir).join(format!(".{active}.slipstream-part"));
    assert!(part.is_file());
    state.cancel_transfer(&active, true).await.unwrap();
    assert!(!part.exists());

    fs::remove_file(downloads.join("transfer.bin")).unwrap();
    fs::create_dir(downloads.join("transfer.bin")).unwrap();
    assert!(state
        .verify_transfer_checksum(&http, TEST_HASH)
        .await
        .is_err());
    assert!(state.cancel_transfer(&http, true).await.is_err());
    assert!(downloads.join("transfer.bin").is_dir());
    fs::remove_dir(downloads.join("transfer.bin")).unwrap();
    fs::write(downloads.join("transfer.bin"), b"test").unwrap();
    state.cancel_transfer(&http, true).await.unwrap();
    state.cancel_transfer(&media, true).await.unwrap();
    state.cancel_transfer(&torrent, true).await.unwrap();
    assert!(!downloads.join("transfer.bin").exists());
    assert!(!downloads.join("video.mp4").exists());
    assert!(!downloads.join("torrent.bin").exists());

    fs::write(downloads.join("existing.bin"), b"owned by the user").unwrap();
    let failed = state
        .add_http_download(http_request(&server, "404", "existing.bin"))
        .await
        .unwrap();
    wait_for(&state, &failed, TransferStatus::Failed).await;
    state.cancel_transfer(&failed, true).await.unwrap();
    assert_eq!(
        fs::read(downloads.join("existing.bin")).unwrap(),
        b"owned by the user"
    );
    assert!(state
        .snapshot()
        .await
        .transfers
        .iter()
        .all(|item| ![&http, &media, &torrent, &active, &failed].contains(&&item.id)));
    state.quit().await;
    assert!(ui.quitting.load(Ordering::SeqCst));
    assert!(PipeClient::connect(&pipe, &token, "after-shutdown")
        .await
        .is_err());

    let restarted = AppState::start(
        dir.clone(),
        Arc::new(FakeUi::default()),
        options(&pipe, &registry_key.0),
    )
    .await
    .unwrap();
    assert!(restarted
        .snapshot()
        .await
        .transfers
        .iter()
        .all(|item| ![&http, &active, &failed].contains(&&item.id)));
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn pipe_status_redacts_desktop_transfer_details() {
    let temp = TempDir::new();
    let downloads = temp.0.join("Downloads");
    installed_settings(&temp.0, &downloads);
    let pipe = format!(r"\\.\pipe\SlipStream.DesktopStatus.{}", Uuid::new_v4());
    let registry_key = RegistryKey::isolated();
    let state = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&pipe, &registry_key.0),
    )
    .await
    .unwrap();
    let token = SessionToken::read(&temp.0.join("session.token")).unwrap();
    let mut bridge = Bridge(Some(
        PipeClient::connect(&pipe, &token, "desktop-integration")
            .await
            .unwrap(),
    ));

    let server = HttpServer::start().await;
    let signed_path = "signed?token=private#fragment";
    let id = state
        .add_http_download(http_request(&server, signed_path, "signed.bin"))
        .await
        .unwrap();
    let Response::Status { batch, .. } = bridge.request(Request::GetStatus).await else {
        panic!("expected browser status");
    };
    assert_eq!(batch.transfers[0].id, id);
    assert_eq!(batch.transfers[0].source_url, server.url("signed"));
    assert!(batch.transfers[0].save_dir.is_empty());

    let desktop = state.snapshot().await;
    assert_eq!(desktop.transfers[0].id, id);
    assert_eq!(desktop.transfers[0].source_url, server.url(signed_path));
    assert_eq!(
        fs::canonicalize(&desktop.transfers[0].save_dir).unwrap(),
        fs::canonicalize(downloads).unwrap()
    );
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_reload_and_rebase_an_empty_torrent_manager() {
    let temp = TempDir::new();
    let old_dir = temp.0.join("old");
    let new_dir = temp.0.join("new");
    let mut settings = installed_settings(&temp.0, &old_dir);
    let pipe = format!(r"\\.\pipe\SlipStream.DesktopSettings.{}", Uuid::new_v4());
    let registry_key = RegistryKey::isolated();
    let state = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&pipe, &registry_key.0),
    )
    .await
    .unwrap();
    settings.default_download_dir = new_dir.to_string_lossy().into_owned();
    settings.default_connections = u8::MAX;
    settings.max_concurrent_downloads = 0;
    settings.global_speed_limit_bps = Some(0);
    state.save_settings(settings).await.unwrap();
    assert_eq!(state.settings().default_connections, 32);
    assert_eq!(state.settings().max_concurrent_downloads, 1);
    assert_eq!(state.snapshot().await.speed_limit_bps, None);
    state.set_speed_limit(Some(1024)).await.unwrap();
    let persisted: AppSettings =
        serde_json::from_slice(&fs::read(temp.0.join("settings.json")).unwrap()).unwrap();
    assert_eq!(persisted.global_speed_limit_bps, Some(1024));
    assert!(new_dir.is_dir());
    assert!(fs::read_dir(&temp.0).unwrap().all(|entry| entry
        .unwrap()
        .path()
        .extension()
        .is_none_or(|ext| ext != "tmp")));
    let torrent = state
        .add_torrent(AddTorrentRequest {
            source: torrent_file(&temp.0).to_string_lossy().into_owned(),
            save_dir: None,
            selected_file_indices: None,
            sequential: true,
        })
        .await
        .unwrap();
    let batch = state.snapshot().await;
    assert_eq!(batch.speed_limit_bps, Some(1024));
    assert_eq!(batch.transfers[0].id, torrent);
    assert_eq!(
        fs::canonicalize(&batch.transfers[0].save_dir).unwrap(),
        fs::canonicalize(new_dir).unwrap()
    );
    state.shutdown().await.unwrap();
    let state = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&pipe, &registry_key.0),
    )
    .await
    .unwrap();
    assert_eq!(state.settings().default_connections, 32);
    assert_eq!(state.settings().max_concurrent_downloads, 1);
    assert_eq!(state.snapshot().await.speed_limit_bps, Some(1024));
    state.shutdown().await.unwrap();
}

fn directory_junction(link: &Path, destination: &Path) {
    fs::create_dir(link).unwrap();
    let destination = fs::canonicalize(destination).unwrap();
    let raw = destination.to_string_lossy();
    let drive = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let substitute: Vec<u16> = format!(r"\??\{drive}").encode_utf16().collect();
    let print: Vec<u16> = drive.encode_utf16().collect();
    let substitute_len = (substitute.len() * 2) as u16;
    let print_len = (print.len() * 2) as u16;
    let mut buffer = Vec::new();
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    for value in [
        8 + substitute_len + print_len + 4,
        0,
        0,
        substitute_len,
        substitute_len + 2,
        print_len,
    ] {
        buffer.extend(value.to_le_bytes());
    }
    for unit in substitute.into_iter().chain([0]).chain(print).chain([0]) {
        buffer.extend(unit.to_le_bytes());
    }
    let handle = fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(link)
        .unwrap();
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    assert_ne!(result, 0, "{}", std::io::Error::last_os_error());
    assert_ne!(
        fs::symlink_metadata(link).unwrap().file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT,
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_media_shutdown_ssrf_guard_and_reparse_tool_rejection() {
    let temp = TempDir::new();
    let downloads = temp.0.join("Downloads");
    let safe_bin = temp.0.join("safe-bin");
    let mutable_bin = temp.0.join("mutable-bin");
    fs::create_dir_all(&safe_bin).unwrap();
    fs::create_dir_all(&mutable_bin).unwrap();

    let built_exe = media_tool_fixture(&safe_bin).await;
    let mutable_exe = mutable_bin.join("media-tool.exe");
    fs::copy(&built_exe, &mutable_exe).unwrap();

    let settings = installed_settings(&temp.0, &downloads);
    let pipe = format!(r"\\.\pipe\SlipStream.DesktopMediaGuards.{}", Uuid::new_v4());
    let registry_key = RegistryKey::isolated();
    let mut strict_options = options(&pipe, &registry_key.0);
    strict_options.allow_loopback = false;

    let state = AppState::start(temp.0.clone(), Arc::new(FakeUi::default()), strict_options)
        .await
        .unwrap();

    state
        .save_settings(AppSettings {
            ytdlp_path: Some(mutable_exe.to_string_lossy().into_owned()),
            ffmpeg_path: Some(mutable_exe.to_string_lossy().into_owned()),
            ..settings.clone()
        })
        .await
        .unwrap();

    for blocked in [
        "http://127.0.0.1:8080/video",
        "http://[::1]:8080/video",
        "http://localhost:8080/video",
        "http://169.254.169.254/latest/meta-data",
        "http://100.64.0.1/video",
        "http://[::ffff:127.0.0.1]/video",
    ] {
        assert!(state.probe_media(blocked).await.is_err(), "{blocked}");
        assert!(
            state
                .add_media_download(AddMediaDownloadRequest {
                    url: blocked.into(),
                    format_id: None,
                    audio_only_preset: None,
                    target_container: None,
                    embed_subtitles_langs: Vec::new(),
                    lossless_remux: false,
                    normalize_audio: false,
                    save_dir: None,
                    filename: None,
                    playlist_items: None,
                })
                .await
                .is_err(),
            "{blocked}"
        );
    }

    // Swap a previously validated tool directory into a directory junction (TOCTOU).
    fs::remove_dir_all(&mutable_bin).unwrap();
    directory_junction(&mutable_bin, &safe_bin);

    let status = state.get_toolchain_status().await;
    assert_eq!(status.ytdlp_path, None);
    assert_eq!(status.ffmpeg_path, None);
    assert!(state
        .probe_media("https://example.com/video")
        .await
        .is_err());
    let rejected_media = state
        .add_media_download(AddMediaDownloadRequest {
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
        })
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            let snap = state
                .snapshot()
                .await
                .transfers
                .into_iter()
                .find(|item| item.id == rejected_media)
                .unwrap();
            assert_ne!(snap.status, TransferStatus::Completed);
            if snap.status == TransferStatus::Failed {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    state.cancel_transfer(&rejected_media, true).await.unwrap();

    // Restore clean canonical binaries, complete a media download, and shut down
    // while the completed media job remains in AppState.
    state
        .save_settings(AppSettings {
            ytdlp_path: Some(built_exe.to_string_lossy().into_owned()),
            ffmpeg_path: Some(built_exe.to_string_lossy().into_owned()),
            ..settings
        })
        .await
        .unwrap();
    let media_id = state
        .add_media_download(AddMediaDownloadRequest {
            url: "https://example.com/video".into(),
            format_id: Some("18".into()),
            audio_only_preset: None,
            target_container: None,
            embed_subtitles_langs: Vec::new(),
            lossless_remux: false,
            normalize_audio: false,
            save_dir: None,
            filename: Some("done.mp4".into()),
            playlist_items: None,
        })
        .await
        .unwrap();
    wait_for(&state, &media_id, TransferStatus::Completed).await;
    state.shutdown().await.unwrap();
}
