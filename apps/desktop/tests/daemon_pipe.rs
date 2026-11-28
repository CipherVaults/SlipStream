#![cfg(windows)]

use slipstream_desktop_lib::{
    local_picker_pipe_name, AppState, DaemonOptions, OpenPickerEvent, UiBridge,
    LOCAL_PICKER_TOKEN_FILE,
};
use slipstream_native_host::{pipe::PipeClient, token::SessionToken};
use slipstream_protocol::{
    AccentMode, AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings,
    ConfirmPendingDownloadRequest, NativeBridgeRequest as Request,
    NativeBridgeResponse as Response, RefreshDownloadUrlRequest, SelectedPlaylistEntry,
    TelemetryBatch, ThemeMode, TransferCompletionOptions, TransferKind, TransferSnapshot,
    TransferStatus,
};
use slipstream_torrent::{CancellationToken, TorrentManagerConfig};
use std::{
    collections::HashMap,
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
        let canonical = fs::canonicalize(&path).unwrap();
        let clean = canonical
            .to_str()
            .and_then(|value| value.strip_prefix(r"\\?\"))
            .map(PathBuf::from)
            .unwrap_or(canonical);
        Self(clean)
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

#[derive(Default)]
struct DialogUi {
    main: FakeUi,
    calls: Mutex<Vec<(&'static str, String)>>,
}

impl DialogUi {
    fn record(&self, action: &'static str, id: &str) {
        self.calls.lock().unwrap().push((action, id.into()));
    }

    fn last_confirmation(&self) -> String {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(action, _)| *action == "confirm")
            .unwrap()
            .1
            .clone()
    }

    async fn wait_for(&self, action: &str, id: &str) {
        timeout(Duration::from_secs(10), async {
            loop {
                if self
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(called, transfer)| *called == action && transfer == id)
                {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing dialog event {action} for {id}"));
    }
}

impl UiBridge for DialogUi {
    fn show_main_window(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.main.show_main_window()
    }
    fn emit_telemetry(&self, batch: &TelemetryBatch) -> Result<(), String> {
        self.main.emit_telemetry(batch)
    }
    fn emit_open_picker(&self, event: &OpenPickerEvent) -> Result<(), String> {
        self.main.emit_open_picker(event)
    }
    fn has_main_window(&self) -> bool {
        self.main.has_main_window()
    }
    fn has_telemetry_windows(&self) -> bool {
        true
    }
    fn request_quit(&self) {
        self.main.request_quit();
    }
    fn supports_dialogs(&self) -> bool {
        true
    }
    fn open_confirm_dialog(&self, id: &str) -> Result<(), String> {
        self.record("confirm", id);
        Ok(())
    }
    fn close_confirm_dialog(&self, id: &str) {
        self.record("close-confirm", id);
    }
    fn open_progress_dialog(&self, id: &str) -> Result<(), String> {
        self.record("progress", id);
        Ok(())
    }
    fn close_progress_dialog(&self, id: &str) {
        self.record("close-progress", id);
    }
    fn open_complete_dialog(&self, id: &str) -> Result<(), String> {
        self.record("complete", id);
        Ok(())
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
        expected_size_bytes: None,
        start_paused: false,
        overwrite_existing: false,
        ..AddHttpDownloadRequest::default()
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
        allow_power_actions: false,
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
        torrent_network: slipstream_protocol::TorrentNetworkSettings {
            listen_port_start: 0,
            listen_port_end: 0,
            enable_dht: false,
            ..Default::default()
        },
        ..AppSettings::default()
    };
    fs::write(
        data.join("settings.json"),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    settings
}

#[tokio::test]
async fn batch_import_metadata_exports_and_native_diagnostics_round_trip() {
    let temp = TempDir::new();
    let downloads = temp.0.join("Downloads");
    installed_settings(&temp.0, &downloads);
    let pipe = format!(r"\\.\pipe\SlipStream.BatchImport.{}", Uuid::new_v4());
    let registry = RegistryKey::isolated();
    let mut config = options(&pipe, &registry.0);
    config.allow_loopback = false;
    let state = AppState::start(temp.0.clone(), Arc::new(FakeUi::default()), config)
        .await
        .unwrap();
    let zip = "https://example.com/files/item.ZIP";
    let uncategorized = "https://example.com/unknown";
    let magnet = "magnet:?xt=urn:btih:abababababababababababababababababababab";
    let text = format!("\u{feff}  {zip}\r\n{zip}#duplicate\r\n<\r\n{uncategorized}\r\nCookie: secret\r\nreferer: http://127.0.0.1/\r\n>\r\n{magnet}\r\nhttp://127.0.0.1/blocked\r\nhttp://169.254.169.254/blocked\r\n");
    let summary = state
        .add_batch_urls(&text, None, Some("video".into()), true)
        .await
        .unwrap();
    assert_eq!(
        (summary.added, summary.skipped, summary.errors.len()),
        (3, 3, 2)
    );
    assert!(summary
        .errors
        .iter()
        .all(|error| error.contains("private or loopback")));

    let batch = state.snapshot().await;
    assert!(batch
        .transfers
        .iter()
        .all(|row| row.category_id.as_deref() == Some("video")
            && row.status == TransferStatus::Paused));
    let zip_id = batch
        .transfers
        .iter()
        .find(|row| row.source_url == zip)
        .unwrap()
        .id
        .clone();
    let other_id = batch
        .transfers
        .iter()
        .find(|row| row.source_url == uncategorized)
        .unwrap()
        .id
        .clone();
    let torrent_id = batch
        .transfers
        .iter()
        .find(|row| row.kind == TransferKind::Torrent)
        .unwrap()
        .id
        .clone();
    let ids = vec![
        zip_id.clone(),
        other_id.clone(),
        zip_id.clone(),
        "missing".into(),
    ];
    assert_eq!(state.set_transfers_category(&ids, None).await.unwrap(), 2);
    assert!(state
        .set_transfers_category(&ids, Some("missing".into()))
        .await
        .is_err());
    assert_eq!(
        state
            .get_transfer_snapshot(&zip_id)
            .await
            .unwrap()
            .category_id
            .as_deref(),
        Some("compressed")
    );
    assert_eq!(
        state
            .get_transfer_snapshot(&other_id)
            .await
            .unwrap()
            .category_id
            .as_deref(),
        Some("general")
    );
    let tags: Vec<_> = ["  libr\nar\ty ", "library", "\u{7f}", &"雪".repeat(30)]
        .into_iter()
        .map(str::to_owned)
        .chain((0..10).map(|i| format!("tag-{i}")))
        .collect();
    assert_eq!(state.set_transfers_tags(&ids, tags).await.unwrap(), 2);
    let updated = state.get_transfer_snapshot(&zip_id).await.unwrap();
    assert_eq!(updated.tags.len(), 8);
    assert_eq!(updated.tags[..2], ["library", &"雪".repeat(24)]);
    let metadata_path = temp.0.join("transfer_meta.json");
    let sidecar: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    assert_eq!(sidecar[&zip_id]["tags"][0], "library");
    assert_eq!(sidecar[&torrent_id]["category_id"], "video");

    let token = SessionToken::read(&temp.0.join("session.token")).unwrap();
    let mut bridge = Bridge(Some(
        PipeClient::connect(&pipe, &token, "batch-test")
            .await
            .unwrap(),
    ));
    let Response::Status { batch, .. } = bridge.request(Request::GetStatus).await else {
        panic!("expected native telemetry");
    };
    let row = batch.transfers.iter().find(|row| row.id == zip_id).unwrap();
    assert_eq!(row.tags, updated.tags);
    assert_eq!(row.category_id, updated.category_id);
    assert!(matches!(
        bridge
            .request(Request::PauseTransfer {
                transfer_id: "missing".into()
            })
            .await,
        Response::Error { .. }
    ));
    assert!(state.get_logs().iter().any(|log| log.category == "pipe"));

    let urls = temp.0.join("urls.txt");
    fs::write(&urls, "old export").unwrap();
    let mut export_ids = ids.clone();
    export_ids.push(torrent_id.clone());
    assert_eq!(
        state
            .export_transfer_urls(&export_ids, urls.to_str().unwrap())
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        fs::read_to_string(urls).unwrap(),
        format!("{zip}\n{uncategorized}\n{magnet}\n")
    );
    let diagnostics = temp.0.join("diagnostics.log");
    let count = state.get_logs().len();
    assert_eq!(
        state
            .export_logs(diagnostics.to_str().unwrap())
            .await
            .unwrap(),
        count
    );
    let logs = fs::read_to_string(diagnostics).unwrap();
    assert!(logs.contains("Batch import: 3 added, 3 skipped"));
    assert!(logs.contains("Authenticated native request failed"));
    assert!(!logs.contains("secret"));

    let junction = temp.0.join("junction");
    directory_junction(&junction, &downloads);
    for path in [
        PathBuf::from("relative.txt"),
        PathBuf::from(r"\\host\share\out.txt"),
        junction.join("bad.txt"),
        junction.clone(),
        downloads.join("..").join("traversal.txt"),
        downloads.clone(),
        downloads.join("out.txt:stream"),
    ] {
        let path = path.to_str().unwrap();
        assert!(
            state.export_transfer_urls(&export_ids, path).await.is_err(),
            "{path}"
        );
        assert!(state.export_logs(path).await.is_err(), "{path}");
    }
    assert!(!downloads.join("bad.txt").exists());
    assert!(!temp.0.join("traversal.txt").exists());

    assert_eq!(
        state
            .bulk_set_speed_limit(&ids, Some(128_000))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        state
            .get_transfer_snapshot(&zip_id)
            .await
            .unwrap()
            .speed_limit_bps,
        Some(128_000)
    );
    assert_eq!(
        state.bulk_stop(&[other_id.clone(), "missing".into()]).await,
        1
    );
    assert_eq!(
        state.get_transfer_snapshot(&other_id).await.unwrap().status,
        TransferStatus::Canceled
    );
    assert_eq!(
        state
            .bulk_remove(&[other_id.clone(), other_id.clone()], false)
            .await,
        1
    );
    let sidecar: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    assert!(sidecar.get(&other_id).is_none());
    state.shutdown().await.unwrap();
    drop(bridge);

    let restarted = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&format!("{pipe}-restart"), &registry.0),
    )
    .await
    .unwrap();
    assert_eq!(
        restarted.get_transfer_snapshot(&zip_id).await.unwrap().tags,
        updated.tags
    );
    assert!(restarted.get_transfer_snapshot(&other_id).await.is_err());
    let sidecar: serde_json::Value =
        serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
    assert!(sidecar.get(&torrent_id).is_none());
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn bulk_resume_delivers_fresh_http_completion_alerts_without_replaying_history() {
    let temp = TempDir::new();
    let downloads = temp.0.join("Downloads");
    installed_settings(&temp.0, &downloads);
    let pipe = format!(r"\\.\pipe\SlipStream.CompletionAlert.{}", Uuid::new_v4());
    let registry = RegistryKey::isolated();
    let server = HttpServer::start().await;
    let state = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&pipe, &registry.0),
    )
    .await
    .unwrap();
    let mut alerts = state.subscribe_completion_alerts();
    let mut request = http_request(&server, "hold", "first.bin");
    let id = state.add_http_download(request.clone()).await.unwrap();
    wait_for(&state, &id, TransferStatus::Active).await;
    assert_eq!(
        state
            .bulk_pause(&[id.clone(), id.clone(), "missing".into()])
            .await,
        1
    );
    wait_for(&state, &id, TransferStatus::Paused).await;
    request.url = server.url("download");
    state
        .refresh_download_url(RefreshDownloadUrlRequest {
            transfer_id: id.clone(),
            new_url: request.url.clone(),
            headers: HashMap::new(),
            referrer: None,
        })
        .await
        .unwrap();
    assert_eq!(state.bulk_resume(std::slice::from_ref(&id)).await, 1);
    let alert = timeout(Duration::from_secs(15), alerts.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(alert.transfer_id, id);
    assert_eq!(alert.kind, TransferKind::Http);
    assert_eq!(alert.filename, "first.bin");
    state.shutdown().await.unwrap();

    let restarted = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&format!("{pipe}-restart"), &registry.0),
    )
    .await
    .unwrap();
    let mut alerts = restarted.subscribe_completion_alerts();
    request.filename = Some("second.bin".into());
    let second = restarted.add_http_download(request).await.unwrap();
    let alert = timeout(Duration::from_secs(15), alerts.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(alert.transfer_id, second);
    assert!(timeout(Duration::from_millis(300), alerts.recv())
        .await
        .is_err());
    assert_eq!(
        restarted.get_transfer_snapshot(&id).await.unwrap().status,
        TransferStatus::Completed
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn native_http_cannot_overwrite_and_superseded_rows_cannot_delete_the_output() {
    let temp = TempDir::new();
    let downloads = temp.0.join("downloads");
    installed_settings(&temp.0, &downloads);
    let registry_key = RegistryKey::isolated();
    let pipe = format!(r"\\.\pipe\slipstream-overwrite-test-{}", Uuid::new_v4());
    let state = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&pipe, &registry_key.0),
    )
    .await
    .unwrap();
    let server = HttpServer::start().await;
    let token = SessionToken::read(&temp.0.join("session.token")).unwrap();
    let mut bridge = Bridge(Some(
        PipeClient::connect(&pipe, &token, "overwrite-test")
            .await
            .unwrap(),
    ));

    let request = http_request(&server, "download", "shared.bin");
    let old = state.add_http_download(request.clone()).await.unwrap();
    wait_for(&state, &old, TransferStatus::Completed).await;
    assert!(state
        .verify_transfer_checksum(&old, TEST_HASH)
        .await
        .unwrap());

    let mut overwrite = request;
    overwrite.overwrite_existing = true;
    let response = bridge
        .request(Request::AddHttpDownload {
            request: overwrite.clone(),
        })
        .await;
    let Response::TransferCreated { transfer_id } = response else {
        panic!("expected a browser download to be queued, got {response:?}");
    };
    let browser = wait_for(&state, &transfer_id, TransferStatus::Completed).await;
    assert_eq!(browser.filename, "shared (1).bin");
    assert!(
        !wait_for(&state, &old, TransferStatus::Completed)
            .await
            .output_superseded
    );

    let replacement = state.add_http_download(overwrite).await.unwrap();
    wait_for(&state, &replacement, TransferStatus::Completed).await;
    let superseded = wait_for(&state, &old, TransferStatus::Completed).await;
    assert!(superseded.output_superseded);
    assert_eq!(superseded.sha256_verified, None);
    assert!(state.open_completed_file(&old).await.is_err());
    assert!(state
        .verify_transfer_checksum(&old, TEST_HASH)
        .await
        .is_err());
    state.cancel_transfer(&old, true).await.unwrap();
    assert_eq!(fs::read(downloads.join("shared.bin")).unwrap(), b"test");
    state.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn native_http_dialog_flow_stages_redacts_resumes_and_completes() {
    let temp = TempDir::new();
    let downloads = temp.0.join("downloads");
    installed_settings(&temp.0, &downloads);
    let registry_key = RegistryKey::isolated();
    let pipe = format!(r"\\.\pipe\slipstream-dialog-test-{}", Uuid::new_v4());
    let ui = Arc::new(DialogUi::default());
    let state = AppState::start(temp.0.clone(), ui.clone(), options(&pipe, &registry_key.0))
        .await
        .unwrap();
    let token = SessionToken::read(&temp.0.join("session.token")).unwrap();
    let mut bridge = Bridge(Some(
        PipeClient::connect(&pipe, &token, "dialogs").await.unwrap(),
    ));
    let server = HttpServer::start().await;
    let mut request = http_request(&server, "download?signature=private#old", "report.PDF");
    request
        .headers
        .insert("Cookie".into(), "session=pending-private".into());
    request.expected_size_bytes = Some(4);
    let mut invalid = request.clone();
    invalid
        .headers
        .insert("X-Invalid".into(), "bad\r\nheader".into());
    assert!(matches!(
        bridge
            .request(Request::AddHttpDownload { request: invalid })
            .await,
        Response::Error { .. }
    ));
    assert!(ui.calls.lock().unwrap().is_empty());
    bridge
        .ack(Request::AddHttpDownload {
            request: request.clone(),
        })
        .await;
    let pending_id = ui.last_confirmation();
    let preview = state.get_pending_download(&pending_id).unwrap();
    assert_eq!(preview.filename, "report.PDF");
    assert_eq!(preview.source_domain, "127.0.0.1");
    assert_eq!(preview.suggested_category_id, "documents");
    assert_eq!(
        Path::new(&preview.suggested_save_dir),
        downloads.join("Documents")
    );
    assert_eq!(preview.expected_size_bytes, Some(4));
    assert!(preview.duplicate.is_none());
    let wire = serde_json::to_string(&preview).unwrap();
    for secret in ["Cookie", "headers", "private", "signature"] {
        assert!(!wire.contains(secret));
    }
    assert!(state.snapshot().await.transfers.is_empty());
    let category = state
        .add_download_category(
            "  Reports  ".into(),
            vec![" .PDF ".into(), "pdf".into()],
            None,
        )
        .await
        .unwrap();
    assert_eq!(category.extensions, ["pdf"]);
    let destination = downloads.join("Reports");
    let id = state
        .confirm_pending_download(ConfirmPendingDownloadRequest {
            pending_id: pending_id.clone(),
            filename: "finished.pdf".into(),
            save_dir: destination.to_string_lossy().into_owned(),
            category_id: category.id.clone(),
            remember_category_path: true,
            start_immediately: false,
            overwrite_existing: false,
            description: Some("Saved for later".into()),
        })
        .await
        .unwrap();
    assert!(state.get_pending_download(&pending_id).is_err());
    ui.wait_for("close-confirm", &pending_id).await;
    assert!(!ui
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(event, _)| *event == "progress"));
    let persisted: AppSettings =
        serde_json::from_slice(&fs::read(temp.0.join("settings.json")).unwrap()).unwrap();
    let category = persisted
        .categories
        .iter()
        .find(|item| item.id == category.id)
        .unwrap();
    assert_eq!(
        Path::new(category.save_dir.as_deref().unwrap()),
        destination
    );
    let paused = wait_for(&state, &id, TransferStatus::Paused).await;
    assert_eq!(paused.description.as_deref(), Some("Saved for later"));
    assert!(state.open_completed_file(&id).await.is_err());
    assert_eq!(
        state.get_transfer_completion_options(&id).await.unwrap(),
        TransferCompletionOptions {
            show_complete_dialog: true,
            auto_close_progress: false,
            open_folder_on_complete: false,
        }
    );
    let completion = TransferCompletionOptions {
        show_complete_dialog: true,
        auto_close_progress: true,
        open_folder_on_complete: false,
    };
    state
        .set_transfer_completion_options(&id, completion.clone())
        .await
        .unwrap();
    assert_eq!(
        state.get_transfer_completion_options(&id).await.unwrap(),
        completion
    );
    state
        .set_transfer_speed_limit(&id, Some(128_000))
        .await
        .unwrap();
    assert_eq!(
        wait_for(&state, &id, TransferStatus::Paused)
            .await
            .speed_limit_bps,
        Some(128_000)
    );

    request.url = server.url("download?signature=private#new");
    bridge
        .ack(Request::AddHttpDownload {
            request: request.clone(),
        })
        .await;
    let duplicate_id = ui.last_confirmation();
    let duplicate = state
        .get_pending_download(&duplicate_id)
        .unwrap()
        .duplicate
        .unwrap();
    assert_eq!(duplicate.transfer_id, id);
    assert_eq!(duplicate.status, TransferStatus::Paused);
    assert_eq!(
        state
            .resolve_duplicate_existing(&duplicate_id)
            .await
            .unwrap(),
        id
    );
    assert!(state.get_pending_download(&duplicate_id).is_err());
    ui.wait_for("progress", &id).await;
    let finished = wait_for(&state, &id, TransferStatus::Completed).await;
    assert!(finished.connect_rtt_ms.is_some());
    ui.wait_for("close-progress", &id).await;
    ui.wait_for("complete", &id).await;
    assert_eq!(fs::read(destination.join("finished.pdf")).unwrap(), b"test");
    assert!(!ui.has_main_window());
    timeout(Duration::from_secs(3), async {
        while !ui.main.snapshots.lock().unwrap().iter().any(|batch| {
            batch
                .transfers
                .iter()
                .any(|item| item.id == id && item.status == TransferStatus::Completed)
        }) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    bridge
        .ack(Request::AddHttpDownload {
            request: request.clone(),
        })
        .await;
    let completed_id = ui.last_confirmation();
    assert_eq!(
        state
            .get_pending_download(&completed_id)
            .unwrap()
            .duplicate
            .unwrap()
            .status,
        TransferStatus::Completed
    );
    assert_eq!(
        state
            .resolve_duplicate_existing(&completed_id)
            .await
            .unwrap(),
        id
    );
    assert_eq!(
        ui.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(action, transfer)| *action == "complete" && transfer == &id)
            .count(),
        2
    );

    bridge
        .ack(Request::AddHttpDownload {
            request: request.clone(),
        })
        .await;
    let canceled = ui.last_confirmation();
    state.cancel_pending_download(&canceled);
    assert!(state.get_pending_download(&canceled).is_err());
    ui.wait_for("close-confirm", &canceled).await;

    bridge.ack(Request::AddHttpDownload { request }).await;
    let open_at_shutdown = ui.last_confirmation();
    state.shutdown().await.unwrap();
    assert!(state.get_pending_download(&open_at_shutdown).is_err());
    ui.wait_for("close-confirm", &open_at_shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn media_download_opens_progress_and_completion_dialogs() {
    let temp = TempDir::new();
    let downloads = temp.0.join("downloads");
    let mut settings = installed_settings(&temp.0, &downloads);
    let registry = RegistryKey::isolated();
    let pipe = format!(r"\\.\pipe\slipstream-media-dialog-test-{}", Uuid::new_v4());
    let ui = Arc::new(DialogUi::default());
    let state = AppState::start(temp.0.clone(), ui.clone(), options(&pipe, &registry.0))
        .await
        .unwrap();
    let executable = media_tool_fixture(&temp.0)
        .await
        .to_string_lossy()
        .into_owned();
    settings.ytdlp_path = Some(executable.clone());
    settings.ffmpeg_path = Some(executable);
    state.save_settings(settings).await.unwrap();
    let server = HttpServer::start().await;
    let mut alerts = state.subscribe_completion_alerts();
    let id = state
        .add_media_download(media_request(&server.url("video")))
        .await
        .unwrap();
    ui.wait_for("progress", &id).await;
    wait_for(&state, &id, TransferStatus::Completed).await;
    ui.wait_for("complete", &id).await;
    let alert = timeout(Duration::from_secs(5), alerts.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(alert.transfer_id, id);
    assert_eq!(alert.kind, TransferKind::Media);
    state.shutdown().await.unwrap();
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

fn media_request(url: &str) -> AddMediaDownloadRequest {
    AddMediaDownloadRequest {
        url: url.into(),
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
        max_height: None,
        selected_entries: None,
        playlist_folder_name: None,
        headers: Default::default(),
        ..AddMediaDownloadRequest::default()
    }
}

fn media_tool_log(directory: &Path) -> String {
    match fs::read_to_string(directory.join("media-tool.log")) {
        Ok(log) => log,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => panic!("could not read media fixture log: {error}"),
    }
}

async fn wait_for_media_tool_log(directory: &Path, line: &str, count: usize) {
    timeout(Duration::from_secs(10), async {
        while media_tool_log(directory)
            .lines()
            .filter(|record| *record == line)
            .count()
            < count
        {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture log did not include {line:?} {count} time(s)"));
}

async fn playlist_members(state: &AppState, first_id: &str) -> Vec<TransferSnapshot> {
    let mut transfers = state.snapshot().await.transfers;
    let id = transfers
        .iter()
        .find(|job| job.id == first_id)
        .unwrap()
        .playlist_id
        .clone()
        .unwrap();
    transfers.retain(|job| job.playlist_id.as_deref() == Some(&id));
    transfers.sort_by_key(|job| job.playlist_index);
    transfers
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
async fn local_torrent_picker_and_preview_cache_are_available_to_the_desktop() {
    let temp = TempDir::new();
    let downloads = temp.0.join("Downloads");
    installed_settings(&temp.0, &downloads);
    let pipe = format!(r"\\.\pipe\SlipStream.DesktopTest.{}", Uuid::new_v4());
    let registry_key = RegistryKey::isolated();
    let ui = Arc::new(DialogUi::default());
    let state = AppState::start(temp.0.clone(), ui.clone(), options(&pipe, &registry_key.0))
        .await
        .unwrap();
    let path = torrent_file(&temp.0);
    let source = path.to_string_lossy().into_owned();
    let picker_request = Request::OpenDesktopWindow {
        open_picker_url: Some(source.clone()),
        headers: HashMap::new(),
    };
    let native_token = SessionToken::read(&temp.0.join("session.token")).unwrap();
    let local_pipe = local_picker_pipe_name(&pipe);
    assert!(
        PipeClient::connect(&local_pipe, &native_token, "cli-handoff")
            .await
            .is_err()
    );
    let mut browser = Bridge(Some(
        PipeClient::connect(&pipe, &native_token, "browser")
            .await
            .unwrap(),
    ));
    assert!(matches!(
        browser.request(picker_request.clone()).await,
        Response::Error { .. }
    ));
    let local_token = SessionToken::read(&temp.0.join(LOCAL_PICKER_TOKEN_FILE)).unwrap();
    let mut cli = Bridge(Some(
        PipeClient::connect(&local_pipe, &local_token, "cli-handoff")
            .await
            .unwrap(),
    ));
    assert!(matches!(
        cli.request(Request::OpenDesktopWindow {
            open_picker_url: Some(r"\\server\share\blocked.torrent".into()),
            headers: HashMap::new(),
        })
        .await,
        Response::Error { .. }
    ));
    cli.ack(picker_request).await;
    let magnet = format!("magnet:?xt=urn:btih:{}", "ab".repeat(20));
    cli.ack(Request::OpenDesktopWindow {
        open_picker_url: Some(magnet.clone()),
        headers: HashMap::new(),
    })
    .await;
    state.window_ready(state.window_generation()).unwrap();
    {
        let events = ui.main.events.lock().unwrap();
        assert_eq!(events[0].url.as_deref(), Some(source.as_str()));
        assert_eq!(events[1].url.as_deref(), Some(magnet.as_str()));
    }

    let preview = state
        .preview_torrent(&source, None, CancellationToken::new())
        .await
        .unwrap();
    let mut request = AddTorrentRequest {
        source: format!("  {source}  "),
        save_dir: None,
        selected_file_indices: None,
        sequential: false,
        start_paused: false,
    };
    let mut stale_request = request.clone();
    stale_request.selected_file_indices = Some(vec![0]);
    assert_eq!(
        state
            .add_torrent_with_preview(stale_request, Some(Uuid::new_v4().to_string()))
            .await
            .unwrap_err(),
        "Torrent preview expired. Please click Inspect again before downloading."
    );
    fs::remove_file(&path).unwrap();
    let mut wrong_source = request.clone();
    wrong_source.source = temp
        .0
        .join("missing.torrent")
        .to_string_lossy()
        .into_owned();
    assert!(state
        .add_torrent_with_preview(wrong_source, preview.preview_token.clone())
        .await
        .is_err());
    request.start_paused = true;
    let id = state
        .add_torrent_with_preview(request, preview.preview_token)
        .await
        .unwrap();
    assert_eq!(id, preview.info_hash);
    assert_eq!(
        state.get_transfer_snapshot(&id).await.unwrap().status,
        TransferStatus::Paused
    );
    assert_eq!(state.get_torrent_peers(&id).await.unwrap().total_pieces, 1);
    assert!(state
        .set_transfer_speed_limit(&id, Some(1024))
        .await
        .unwrap_err()
        .contains("16 KiB"));
    state
        .set_transfer_speed_limit(&id, Some(128_000))
        .await
        .unwrap();
    assert_eq!(
        state
            .get_transfer_snapshot(&id)
            .await
            .unwrap()
            .speed_limit_bps,
        Some(128_000)
    );
    assert!(ui
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|(action, _)| *action != "progress"));
    let live = state
        .add_torrent(AddTorrentRequest {
            source: format!("magnet:?xt=urn:btih:{}", "ab".repeat(20)),
            save_dir: None,
            selected_file_indices: None,
            sequential: false,
            start_paused: false,
        })
        .await
        .unwrap();
    assert!(ui.calls.lock().unwrap().contains(&("progress", live)));
    state.shutdown().await.unwrap();
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
    sleep(Duration::from_millis(300)).await;
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
        bridge.request(Request::ProbeMedia { url: media_url.into(), playlist: None, headers: Default::default() }).await,
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
                download_playlist: false,
                max_height: None,
                selected_entries: None,
                playlist_folder_name: None,
                headers: Default::default(),
                ..AddMediaDownloadRequest::default()
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
                    start_paused: false,
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
                    start_paused: false,
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
                    download_playlist: false,
                    max_height: None,
                    selected_entries: None,
                    playlist_folder_name: None,
                    headers: Default::default(),
                    ..AddMediaDownloadRequest::default()
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
            start_paused: false,
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
                start_paused: false,
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
    assert_eq!(batch.speed_limit_bps, Some(16_384));
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
                open_picker_url: Some("file:///C:/private".into()),
                headers: HashMap::new(),
            })
            .await,
        Response::Error { .. }
    ));
    assert!(!ui.visible.load(Ordering::SeqCst));
    bridge
        .ack(Request::OpenDesktopWindow {
            open_picker_url: Some(media_url.into()),
            headers: HashMap::from([
                ("referer".into(), "https://example.com/watch".into()),
                ("Origin".into(), "https://example.com/invalid-path".into()),
                ("User-Agent".into(), "first".into()),
                ("user-agent".into(), "conflicting".into()),
                ("Cookie".into(), "session=private".into()),
            ]),
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
            url: Some(media_url.into()),
            headers: HashMap::from([("Referer".into(), "https://example.com/watch".into())]),
        }]
    );
    bridge
        .ack(Request::OpenDesktopWindow {
            open_picker_url: None,
            headers: HashMap::from([("Accept-Language".into(), "en-US".into())]),
        })
        .await;
    assert_eq!(
        ui.events.lock().unwrap().last().unwrap(),
        &OpenPickerEvent {
            url: None,
            headers: HashMap::new(),
        }
    );
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
    sleep(Duration::from_millis(300)).await;
    assert_eq!(ui.presence_checks.load(Ordering::SeqCst), checks);
    assert_eq!(ui.snapshots.lock().unwrap().len(), emissions);
    let magnet = format!("magnet:?xt=urn:btih:{torrent}");
    bridge
        .ack(Request::OpenDesktopWindow {
            open_picker_url: Some(magnet.clone()),
            headers: HashMap::new(),
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
    assert_eq!(persisted.global_speed_limit_bps, Some(16_384));
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
            start_paused: false,
        })
        .await
        .unwrap();
    let batch = state.snapshot().await;
    assert_eq!(batch.speed_limit_bps, Some(16_384));
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
    assert_eq!(state.snapshot().await.speed_limit_bps, Some(16_384));
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
async fn media_playlist_publishes_successful_items_with_original_indices_and_motw() {
    let temp = TempDir::new();
    let downloads = temp.0.join("Downloads");
    let executable = media_tool_fixture(&temp.0)
        .await
        .to_string_lossy()
        .into_owned();
    let settings = installed_settings(&temp.0, &downloads);
    let pipe = format!(r"\\.\pipe\SlipStream.Playlists.{}", Uuid::new_v4());
    let registry_key = RegistryKey::isolated();
    let state = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&pipe, &registry_key.0),
    )
    .await
    .unwrap();
    state
        .save_settings(AppSettings {
            ytdlp_path: Some(executable.clone()),
            ffmpeg_path: Some(executable),
            ..settings
        })
        .await
        .unwrap();
    let token = SessionToken::read(&temp.0.join("session.token")).unwrap();
    let mut bridge = Bridge(Some(
        PipeClient::connect(&pipe, &token, "playlist-integration")
            .await
            .unwrap(),
    ));
    let mixed = "https://example.com/watch?v=1&list=PL1";
    assert!(!state.probe_media(mixed, None).await.unwrap().is_playlist);
    let playlist = state.probe_media(mixed, Some(true)).await.unwrap();
    assert_eq!(playlist.playlist_count, Some(4));
    assert_eq!(
        playlist.playlist_entries,
        ["First_ video", "", "Broken video", "Fourth video"]
    );
    assert!(
        !state
            .probe_media(mixed, Some(false))
            .await
            .unwrap()
            .is_playlist
    );
    let pure = "https://example.com/playlist?list=PL1";
    assert!(matches!(
        bridge.request(Request::ProbeMedia { url: pure.into(), playlist: None, headers: Default::default() }).await,
        Response::MediaProbed { probe } if probe.is_playlist
    ));
    assert!(matches!(
        bridge.request(Request::ProbeMedia { url: mixed.into(), playlist: Some(true), headers: Default::default() }).await,
        Response::MediaProbed { probe } if probe.is_playlist
    ));
    let large = Request::ProbeMedia {
        url: "https://example.com/playlist?list=PL6000".into(),
        playlist: None,
        headers: Default::default(),
    };
    let large = bridge.request(large).await;
    assert!(matches!(&large, Response::MediaProbed { probe }
        if probe.playlist_count == Some(6000) && !probe.playlist_entries.is_empty()));
    assert!(
        serde_json::to_vec(&large).unwrap().len()
            < slipstream_protocol::MAX_NATIVE_MESSAGE_BYTES / 2
    );
    let id = bridge
        .created(Request::AddMediaDownload {
            request: AddMediaDownloadRequest {
                url: pure.into(),
                format_id: None,
                audio_only_preset: None,
                target_container: None,
                embed_subtitles_langs: vec!["en".into()],
                lossless_remux: true,
                normalize_audio: false,
                save_dir: None,
                filename: None,
                playlist_items: None,
                download_playlist: false,
                max_height: Some(720),
                selected_entries: None,
                playlist_folder_name: None,
                headers: Default::default(),
                ..AddMediaDownloadRequest::default()
            },
        })
        .await;
    let active = wait_for(&state, &id, TransferStatus::Active).await;
    assert_eq!(active.filename, "Fixture playlist");
    assert!(active.playlist_id.is_some());
    assert_eq!(active.playlist_title.as_deref(), Some("Fixture playlist"));
    assert_eq!(active.playlist_total, Some(4));
    let done = wait_for(&state, &id, TransferStatus::Completed).await;
    assert_eq!(done.playlist_index, Some(3));
    assert_eq!(done.filename, "Fixture playlist (2 files)");
    assert_eq!(
        done.completion_notice.as_deref(),
        Some("2 of 4 videos downloaded (2 skipped). Requested subtitles were unavailable for 1 downloaded video.")
    );
    assert!(done.error_message.is_none());
    assert_eq!(done.downloaded_bytes, 11);
    assert_eq!(done.total_bytes, Some(11));
    let downloads = downloads.join("Fixture playlist");
    for (name, payload) in [
        ("Fixture playlist-000001.mp4", &b"first"[..]),
        ("Fixture playlist-000004.mp4", &b"fourth"[..]),
    ] {
        assert_eq!(fs::read(downloads.join(name)).unwrap(), payload);
        assert_eq!(
            fs::read_to_string(downloads.join(format!("{name}:Zone.Identifier"))).unwrap(),
            format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={pure}\r\n")
        );
    }
    assert!(!downloads.join("Fixture playlist-000002.mp4").exists());
    assert!(!downloads.join("Fixture playlist-000003.mp4").exists());
    assert_eq!(fs::read_dir(&downloads).unwrap().count(), 2);
    state.cancel_transfer(&id, true).await.unwrap();
    assert_eq!(fs::read_dir(&downloads).unwrap().count(), 0);
    state.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn media_probes_coalesce_across_the_pipe_and_reuse_toolchain_and_download_metadata() {
    let temp = TempDir::new();
    let downloads = temp.0.join("Downloads");
    let executable = media_tool_fixture(&temp.0).await;
    let path = executable.to_string_lossy().into_owned();
    let settings = installed_settings(&temp.0, &downloads);
    let pipe = format!(r"\\.\pipe\SlipStream.Probes.{}", Uuid::new_v4());
    let registry_key = RegistryKey::isolated();
    let state = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&pipe, &registry_key.0),
    )
    .await
    .unwrap();
    let settings = AppSettings {
        ytdlp_path: Some(path.clone()),
        ffmpeg_path: Some(path),
        ..settings
    };
    state.save_settings(settings.clone()).await.unwrap();
    let token = SessionToken::read(&temp.0.join("session.token")).unwrap();
    let mut bridge = Bridge(Some(
        PipeClient::connect(&pipe, &token, "probe-integration")
            .await
            .unwrap(),
    ));

    let url = "https://example.com/coalesced";
    let (native, normalized, desktop) = tokio::join!(
        bridge.request(Request::ProbeMedia {
            url: url.into(),
            playlist: None,
            headers: Default::default(),
        }),
        state.probe_media("HTTPS://EXAMPLE.COM:443/coalesced", Some(false)),
        state.probe_media(url, None),
    );
    assert!(matches!(native, Response::MediaProbed { probe } if probe.source_url == url));
    assert_eq!(normalized.unwrap().source_url, url);
    assert_eq!(desktop.unwrap().title, "Fixture video");
    assert_eq!(
        state.probe_media(url, None).await.unwrap().title,
        "Fixture video"
    );
    assert_eq!(
        media_tool_log(&temp.0),
        format!("yt-dlp version\nprobe false {url}\n")
    );
    assert!(temp.0.join("yt-dlp-cache").is_dir());

    let request = media_request(url);
    let id = bridge.created(Request::AddMediaDownload { request }).await;
    wait_for(&state, &id, TransferStatus::Completed).await;
    assert_eq!(
        fs::read(downloads.join("Fixture video.mp4")).unwrap(),
        b"test"
    );
    assert_eq!(
        media_tool_log(&temp.0),
        format!("yt-dlp version\nprobe false {url}\nffmpeg version\n")
    );
    assert!(matches!(
        bridge.request(Request::ProbeMedia {
            url: url.into(),
            playlist: None,
            headers: [
                ("user-agent".into(), "Probe Fixture".into()),
                ("Cookie".into(), "session=ignored".into()),
                ("Origin".into(), "https://example.com/invalid-path".into()),
            ].into(),
        }).await,
        Response::MediaProbed { probe } if probe.title == "Header-specific video"
    ));
    let mut request = media_request(url);
    request
        .headers
        .insert("user-agent".into(), "Probe Fixture".into());
    let id = bridge.created(Request::AddMediaDownload { request }).await;
    wait_for(&state, &id, TransferStatus::Completed).await;
    assert!(downloads.join("Header-specific video.mp4").is_file());
    assert_eq!(
        media_tool_log(&temp.0)
            .lines()
            .filter(|line| *line == format!("probe false {url}"))
            .count(),
        2
    );

    let no_ffmpeg = AppSettings {
        ffmpeg_path: Some(
            temp.0
                .join("missing-ffmpeg.exe")
                .to_string_lossy()
                .into_owned(),
        ),
        ..settings
    };
    state.save_settings(no_ffmpeg).await.unwrap();
    assert!(!state.probe_media(url, None).await.unwrap().is_playlist);
    assert!(
        state
            .probe_media(url, Some(true))
            .await
            .unwrap()
            .is_playlist
    );
    let log = media_tool_log(&temp.0);
    assert_eq!(
        log.lines().filter(|line| *line == "yt-dlp version").count(),
        2
    );
    assert_eq!(
        log.lines()
            .filter(|line| *line == format!("probe false {url}"))
            .count(),
        3
    );
    assert_eq!(
        log.lines().filter(|line| *line == "ffmpeg version").count(),
        1
    );

    let binary = fs::OpenOptions::new()
        .write(true)
        .open(&executable)
        .unwrap();
    binary
        .set_modified(binary.metadata().unwrap().modified().unwrap() - Duration::from_secs(10))
        .unwrap();
    drop(binary);
    state
        .probe_media("https://example.com/after-binary-change", None)
        .await
        .unwrap();
    assert_eq!(
        media_tool_log(&temp.0)
            .lines()
            .filter(|line| *line == "yt-dlp version")
            .count(),
        3
    );
    state.shutdown().await.unwrap();
}

async fn start_playlist_fixture(temp: &TempDir) -> (Arc<AppState>, Bridge) {
    let executable = media_tool_fixture(&temp.0)
        .await
        .to_string_lossy()
        .into_owned();
    let settings = installed_settings(&temp.0, &temp.0.join("Downloads"));
    let pipe = format!(r"\\.\pipe\SlipStream.SelectivePlaylist.{}", Uuid::new_v4());
    let registry_key = RegistryKey::isolated();
    let state = AppState::start(
        temp.0.clone(),
        Arc::new(FakeUi::default()),
        options(&pipe, &registry_key.0),
    )
    .await
    .unwrap();
    state
        .save_settings(AppSettings {
            ytdlp_path: Some(executable.clone()),
            ffmpeg_path: Some(executable),
            ..settings
        })
        .await
        .unwrap();
    let token = SessionToken::read(&temp.0.join("session.token")).unwrap();
    let bridge = Bridge(Some(
        PipeClient::connect(&pipe, &token, "selective-playlist")
            .await
            .unwrap(),
    ));
    (state, bridge)
}

fn playlist_request() -> AddMediaDownloadRequest {
    let mut request = media_request("https://example.com/playlist?list=PL-links");
    request.max_height = Some(720);
    request.lossless_remux = true;
    request
        .headers
        .insert("User-Agent".into(), "Playlist Fixture".into());
    request
        .headers
        .insert("oRigin".into(), "https://example.com".into());
    request.headers.insert(
        "referer".into(),
        "https://example.com/playlist?list=PL-links&token=fixture".into(),
    );
    request
}

#[tokio::test(flavor = "multi_thread")]
async fn selected_playlist_runs_sequentially_with_pause_resume_and_middle_failure() {
    let temp = TempDir::new();
    let (state, mut bridge) = start_playlist_fixture(&temp).await;
    let mut request = playlist_request();
    request.playlist_folder_name = Some("Selected: clips".into());
    request.selected_entries = Some(
        [
            (11, "Three", "three"),
            (2, "One: video", "one"),
            (7, "Two", "two"),
        ]
        .map(|(index, title, slug)| SelectedPlaylistEntry {
            index,
            title: title.into(),
            url: Some(if slug == "three" {
                format!("https://127.0.0.1/sequence/{slug}")
            } else {
                format!("https://example.com/sequence/{slug}")
            }),
        })
        .into(),
    );
    // Validate the full selection before any job or folder is created.
    let mut invalid = request.clone();
    invalid.selected_entries.as_mut().unwrap()[2].url = Some("http://169.254.169.254/video".into());
    assert!(state.add_media_download(invalid).await.is_err());
    let mut invalid = request.clone();
    invalid.selected_entries = Some(Vec::new());
    assert!(state.add_media_download(invalid).await.is_err());
    let mut invalid = request.clone();
    invalid
        .headers
        .insert("User-Agent".into(), "line\r\nbreak".into());
    assert!(state.add_media_download(invalid).await.is_err());
    assert!(state.snapshot().await.transfers.is_empty());

    let first_id = bridge.created(Request::AddMediaDownload { request }).await;
    wait_for_media_tool_log(&temp.0, "start one", 1).await;
    let jobs = playlist_members(&state, &first_id).await;
    let playlist_id = jobs[0].playlist_id.as_deref().unwrap();
    let folder = fs::canonicalize(temp.0.join("Downloads/Selected_ clips")).unwrap();
    let names = ["02 - One_ video", "07 - Two", "11 - Three"];
    assert_eq!(jobs.len(), 3);
    assert_eq!(jobs[0].id, first_id);
    for (position, job) in jobs.iter().enumerate() {
        assert_eq!(job.playlist_id.as_deref(), Some(playlist_id));
        assert_eq!(job.playlist_title.as_deref(), Some("Selected: clips"));
        assert_eq!(job.playlist_index, Some(position as u32 + 1));
        assert_eq!(job.playlist_total, Some(3));
        assert_eq!(job.filename, names[position]);
        assert_eq!(Path::new(&job.save_dir), folder);
    }
    assert_eq!(jobs[1].status, TransferStatus::Queued);
    assert_eq!(jobs[2].status, TransferStatus::Queued);
    assert!(state.resume_transfer(&jobs[1].id).await.is_err());
    state.pause_playlist(playlist_id).await.unwrap();
    assert!(playlist_members(&state, &first_id)
        .await
        .iter()
        .all(|job| job.playlist_paused == Some(true)));
    wait_for(&state, &first_id, TransferStatus::Paused).await;
    assert_eq!(
        playlist_members(&state, &first_id).await[1].status,
        TransferStatus::Queued
    );
    assert!(!media_tool_log(&temp.0).contains("start two"));

    state.resume_playlist(playlist_id).await.unwrap();
    assert!(playlist_members(&state, &first_id)
        .await
        .iter()
        .all(|job| job.playlist_paused == Some(false)));
    wait_for_media_tool_log(&temp.0, "start one", 2).await;
    fs::write(temp.0.join("release-one"), "").unwrap();
    wait_for(&state, &first_id, TransferStatus::Completed).await;
    wait_for_media_tool_log(&temp.0, "start two", 1).await;
    assert_eq!(
        playlist_members(&state, &first_id).await[2].status,
        TransferStatus::Queued
    );
    fs::write(temp.0.join("release-two"), "").unwrap();
    let failed = wait_for(&state, &jobs[1].id, TransferStatus::Failed).await;
    assert!(failed
        .error_message
        .unwrap()
        .contains("fixture playlist entry failed"));
    wait_for_media_tool_log(&temp.0, "start three", 1).await;
    fs::write(temp.0.join("release-three"), "").unwrap();
    wait_for(&state, &jobs[2].id, TransferStatus::Completed).await;
    let log = media_tool_log(&temp.0);
    let lifecycle = log
        .lines()
        .filter(|line| line.starts_with("start ") || line.starts_with("finish "))
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle,
        [
            "start one",
            "start one",
            "finish one",
            "start two",
            "finish two",
            "start three",
            "finish three"
        ]
    );
    assert_eq!(
        fs::read(folder.join("02 - One_ video.mp4")).unwrap(),
        b"one"
    );
    assert!(!folder.join("07 - Two.mp4").exists());
    assert_eq!(fs::read(folder.join("11 - Three.mp4")).unwrap(), b"three");
    assert_eq!(
        log.lines().filter(|line| *line == "yt-dlp version").count(),
        1
    );
    assert_eq!(
        log.lines().filter(|line| *line == "ffmpeg version").count(),
        1
    );
    state.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn probed_playlist_filter_keeps_original_numbers_and_cancel_advances() {
    let temp = TempDir::new();
    let (state, mut bridge) = start_playlist_fixture(&temp).await;
    let mut request = playlist_request();
    request.playlist_items = Some("2, 11-12".into());
    let first_id = bridge.created(Request::AddMediaDownload { request }).await;
    wait_for_media_tool_log(&temp.0, "start one", 1).await;
    let jobs = playlist_members(&state, &first_id).await;
    assert_eq!(jobs.len(), 3);
    assert_eq!(jobs[1].status, TransferStatus::Queued);
    assert_eq!(jobs[1].playlist_index, Some(2));
    assert_eq!(jobs[1].playlist_total, Some(3));
    assert_eq!(jobs[1].filename, "11 - Three");
    assert_eq!(jobs[2].filename, "12 - Video 12");
    state.cancel_transfer(&first_id, true).await.unwrap();
    wait_for_media_tool_log(&temp.0, "start three", 1).await;
    fs::write(temp.0.join("release-three"), "").unwrap();
    let done = wait_for(&state, &jobs[1].id, TransferStatus::Completed).await;
    let folder = temp.0.join("Downloads/Linked_ playlist");
    assert_eq!(done.playlist_title.as_deref(), Some("Linked: playlist"));
    assert_eq!(fs::read(folder.join("11 - Three.mp4")).unwrap(), b"three");
    let unpreviewed = wait_for(&state, &jobs[2].id, TransferStatus::Completed).await;
    assert_eq!(unpreviewed.playlist_index, Some(3));
    assert_eq!(unpreviewed.playlist_total, Some(3));
    assert_eq!(
        fs::read(folder.join("12 - Video 12.mp4")).unwrap(),
        b"item 12"
    );
    assert!(!folder.join("02 - One_ video.mp4").exists());
    assert!(!media_tool_log(&temp.0).contains("start two"));
    state.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn paused_playlist_stays_paused_when_its_current_video_is_removed() {
    let temp = TempDir::new();
    let (state, mut bridge) = start_playlist_fixture(&temp).await;
    let id = bridge
        .created(Request::AddMediaDownload {
            request: playlist_request(),
        })
        .await;
    wait_for_media_tool_log(&temp.0, "start one", 1).await;
    let jobs = playlist_members(&state, &id).await;
    let playlist_id = jobs[0].playlist_id.as_deref().unwrap();
    state.pause_transfer(&id).await.unwrap();
    state.cancel_transfer(&jobs[1].id, false).await.unwrap();
    // Let the coordinator handle the cancellation while the first job is paused.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let waiting = playlist_members(&state, &id).await;
    assert_eq!(waiting[0].status, TransferStatus::Paused);
    assert_eq!(waiting[1].status, TransferStatus::Queued);
    assert!(waiting.iter().all(|job| job.playlist_paused == Some(false)));

    state.pause_playlist(playlist_id).await.unwrap();
    state.cancel_transfer(&id, false).await.unwrap();
    let waiting = playlist_members(&state, &jobs[2].id).await;
    assert_eq!(waiting[0].status, TransferStatus::Queued);
    assert_eq!(waiting[0].playlist_paused, Some(true));
    state.resume_playlist(playlist_id).await.unwrap();
    wait_for_media_tool_log(&temp.0, "start three", 1).await;
    assert_eq!(
        playlist_members(&state, &jobs[2].id).await[0].playlist_paused,
        Some(false)
    );
    fs::write(temp.0.join("release-three"), "").unwrap();
    wait_for(&state, &jobs[2].id, TransferStatus::Completed).await;
    state.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn playlist_larger_than_the_preview_downloads_as_one_unrestricted_job() {
    let temp = TempDir::new();
    let (state, mut bridge) = start_playlist_fixture(&temp).await;
    let mut request = media_request("https://example.com/playlist?list=PL6000");
    request.max_height = Some(720);
    request.lossless_remux = true;
    request.playlist_folder_name = Some("Full playlist".into());
    let id = bridge.created(Request::AddMediaDownload { request }).await;
    let done = wait_for(&state, &id, TransferStatus::Completed).await;
    assert_eq!(done.playlist_total, Some(6000));
    assert_eq!(playlist_members(&state, &id).await.len(), 1);
    let folder = temp.0.join("Downloads/Full playlist");
    assert_eq!(
        fs::read(folder.join("download-006000.mp4")).unwrap(),
        b"last"
    );
    state.shutdown().await.unwrap();
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
        assert!(state.probe_media(blocked, None).await.is_err(), "{blocked}");
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
                    download_playlist: false,
                    max_height: None,
                    selected_entries: None,
                    playlist_folder_name: None,
                    headers: Default::default(),
                    ..AddMediaDownloadRequest::default()
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
        .probe_media("https://example.com/video", None)
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
            download_playlist: false,
            max_height: None,
            selected_entries: None,
            playlist_folder_name: None,
            headers: Default::default(),
            ..AddMediaDownloadRequest::default()
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
            download_playlist: false,
            max_height: None,
            selected_entries: None,
            playlist_folder_name: None,
            headers: Default::default(),
            ..AddMediaDownloadRequest::default()
        })
        .await
        .unwrap();
    wait_for(&state, &media_id, TransferStatus::Completed).await;
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn phase2_alt_speed_backup_queues_and_auto_extract_round_trip() {
    use std::io::Write;

    let mut zip_buf = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut zip_buf);
        writer
            .start_file("hello.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"phase2 auto extract payload").unwrap();
        writer.finish().unwrap();
    }
    let zip_bytes = zip_buf.into_inner();
    let zip_for_server = zip_bytes.clone();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let body = zip_for_server.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                while !buf.ends_with(b"\r\n\r\n") {
                    let Ok(b) = stream.read_u8().await else {
                        return;
                    };
                    buf.push(b);
                }
                let req = String::from_utf8_lossy(&buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                if !req.starts_with("HEAD ") {
                    let _ = stream.write_all(&body).await;
                }
                let _ = stream.shutdown().await;
            });
        }
    });

    let temp = TempDir::new();
    let downloads = temp.0.join("Downloads");
    let mut settings = installed_settings(&temp.0, &downloads);
    settings.ytdlp_path = Some(
        temp.0
            .join("trusted-ytdlp.exe")
            .to_string_lossy()
            .into_owned(),
    );
    settings.ffmpeg_path = Some(
        temp.0
            .join("trusted-ffmpeg.exe")
            .to_string_lossy()
            .into_owned(),
    );
    let trusted_tools = (settings.ytdlp_path.clone(), settings.ffmpeg_path.clone());
    settings.alt_speed_limit_bps = Some(256 * 1024);
    settings.auto_extract = slipstream_protocol::AutoExtractSettings {
        enabled: true,
        create_subfolder: true,
        delete_archive_after_extract: false,
        overwrite_existing: true,
    };
    fs::write(
        temp.0.join("settings.json"),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();

    let pipe = format!(r"\\.\pipe\SlipStream.Phase2.{}", Uuid::new_v4());
    let registry = RegistryKey::isolated();
    let mut config = options(&pipe, &registry.0);
    config.allow_loopback = true;
    let state = AppState::start(temp.0.clone(), Arc::new(FakeUi::default()), config)
        .await
        .unwrap();

    assert!(!state.automation_status().alt_speed_active);
    let toggled = state.toggle_alt_speed().await.unwrap();
    assert!(toggled);
    let status = state.automation_status();
    assert!(status.alt_speed_active);
    assert_eq!(status.effective_speed_limit_bps, Some(256 * 1024));

    let backup_path = temp.0.join("slipstream-backup.json");
    state.export_settings_backup(&backup_path).await.unwrap();
    assert!(backup_path.is_file());
    assert!(state
        .export_settings_backup(Path::new(r"\\evil\share\b.json"))
        .await
        .is_err());
    assert!(state
        .import_settings_backup(Path::new(r"C:\Temp\..\evil.json"))
        .await
        .is_err());
    let mut bundle: slipstream_protocol::SettingsBackupBundle =
        serde_json::from_slice(&fs::read(&backup_path).unwrap()).unwrap();
    bundle.settings.ytdlp_path = Some(
        temp.0
            .join("backup-ytdlp.exe")
            .to_string_lossy()
            .into_owned(),
    );
    bundle.settings.ffmpeg_path = None;
    fs::write(&backup_path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    let imported = state.import_settings_backup(&backup_path).await.unwrap();
    assert!(imported.alt_speed_enabled);
    assert_eq!(imported.ytdlp_path, trusted_tools.0);
    assert_eq!(imported.ffmpeg_path, trusted_tools.1);
    let persisted: AppSettings =
        serde_json::from_slice(&fs::read(temp.0.join("settings.json")).unwrap()).unwrap();
    assert_eq!(persisted.ytdlp_path, trusted_tools.0);
    assert_eq!(persisted.ffmpeg_path, trusted_tools.1);

    state.pause_queue("bulk").await.unwrap();
    let tid = state
        .add_http_download(AddHttpDownloadRequest {
            url: format!("http://{addr}/bundle.zip"),
            filename: Some("bundle.zip".into()),
            save_dir: None,
            max_connections: Some(1),
            queue_id: Some("bulk".into()),
            headers: HashMap::new(),
            referrer: None,
            expected_sha256: None,
            expected_size_bytes: None,
            start_paused: false,
            overwrite_existing: true,
            ..AddHttpDownloadRequest::default()
        })
        .await
        .unwrap();
    sleep(Duration::from_millis(80)).await;
    assert_eq!(
        state.snapshot().await.transfers[0].status,
        TransferStatus::Queued
    );

    state.start_queue("bulk").await.unwrap();
    wait_for(&state, &tid, TransferStatus::Completed).await;

    let extracted = downloads.join("bundle").join("hello.txt");
    let mut zone = extracted.as_os_str().to_owned();
    zone.push(":Zone.Identifier");
    timeout(Duration::from_secs(5), async {
        while !extracted.is_file() {
            sleep(Duration::from_millis(25)).await;
        }
        // Publication is the synchronization point for both the file and its mark.
        assert_eq!(fs::read(&zone).unwrap(), b"[ZoneTransfer]\r\nZoneId=3\r\n");
    })
    .await
    .unwrap();
    assert_eq!(
        fs::read(&extracted).unwrap(),
        b"phase2 auto extract payload"
    );

    server.abort();
    state.shutdown().await.unwrap();
}
