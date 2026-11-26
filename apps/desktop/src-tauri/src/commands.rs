use crate::{
    shell::{DesktopUi, COMPLETE_PREFIX, CONFIRM_PREFIX, MAIN, PROGRESS_PREFIX},
    tools, AppState, ToolchainStatus,
};
use slipstream_protocol::{
    AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings, BrowserKind,
    BrowserSetupResult, BrowserStatus, ConfirmPendingDownloadRequest, DownloadCategory,
    MediaProbeResult, PendingDownloadPreview, RefreshDownloadUrlRequest, TelemetryBatch,
    TorrentFileEntry, TransferCompletionOptions, UpdateProgress, UpdateProgressStage, UpdateStatus,
};
use std::{
    collections::HashMap,
    fmt::Display,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_updater::UpdaterExt;

type Daemon<'a> = State<'a, Arc<AppState>>;
type Result<T> = std::result::Result<T, String>;

fn require_main(window: &WebviewWindow) -> Result<()> {
    require_window(window.label() == MAIN)
}

fn require_confirm_for(window: &WebviewWindow, pending_id: &str) -> Result<()> {
    let label = window.label();
    require_window(label == MAIN || label.strip_prefix(CONFIRM_PREFIX) == Some(pending_id))
}

fn require_confirm_or_main(window: &WebviewWindow) -> Result<()> {
    let label = window.label();
    require_window(label == MAIN || label.starts_with(CONFIRM_PREFIX))
}

fn require_progress_or_main(window: &WebviewWindow, transfer_id: &str) -> Result<()> {
    let label = window.label();
    require_window(label == MAIN || label.strip_prefix(PROGRESS_PREFIX) == Some(transfer_id))
}

fn require_transfer_dialog_or_main(window: &WebviewWindow, transfer_id: &str) -> Result<()> {
    let label = window.label();
    require_window(
        label == MAIN
            || label.strip_prefix(PROGRESS_PREFIX) == Some(transfer_id)
            || label.strip_prefix(COMPLETE_PREFIX) == Some(transfer_id),
    )
}

fn require_app_window(window: &WebviewWindow) -> Result<()> {
    let label = window.label();
    require_window(
        label == MAIN
            || [CONFIRM_PREFIX, PROGRESS_PREFIX, COMPLETE_PREFIX]
                .into_iter()
                .any(|prefix| label.starts_with(prefix)),
    )
}

fn require_window(allowed: bool) -> Result<()> {
    if allowed {
        Ok(())
    } else {
        Err("This command is unavailable in this window.".into())
    }
}

#[tauri::command]
pub(crate) fn mark_ui_ready(state: Daemon<'_>, window: WebviewWindow) -> Result<()> {
    if window.label() != crate::shell::MAIN {
        return Err("only the main window can mark the UI ready".into());
    }
    state.window_ready(state.window_generation())
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn get_snapshot(
    window: WebviewWindow,
    state: Daemon<'_>,
) -> Result<TelemetryBatch> {
    require_app_window(&window)?;
    Ok(state.snapshot().await)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn add_http_download(
    window: WebviewWindow,
    state: Daemon<'_>,
    request: AddHttpDownloadRequest,
) -> Result<String> {
    require_main(&window)?;
    state.add_http_download(request).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn get_pending_download(
    window: WebviewWindow,
    state: Daemon<'_>,
    pending_id: String,
) -> Result<PendingDownloadPreview> {
    require_confirm_for(&window, &pending_id)?;
    state.get_pending_download(&pending_id)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn confirm_pending_download(
    window: WebviewWindow,
    state: Daemon<'_>,
    request: ConfirmPendingDownloadRequest,
) -> Result<String> {
    require_confirm_for(&window, &request.pending_id)?;
    state.confirm_pending_download(request).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn cancel_pending_download(
    window: WebviewWindow,
    state: Daemon<'_>,
    pending_id: String,
) -> Result<()> {
    require_confirm_for(&window, &pending_id)?;
    state.cancel_pending_download(&pending_id);
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn resolve_duplicate_existing(
    window: WebviewWindow,
    state: Daemon<'_>,
    pending_id: String,
) -> Result<String> {
    require_confirm_for(&window, &pending_id)?;
    state.resolve_duplicate_existing(&pending_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn add_download_category(
    window: WebviewWindow,
    state: Daemon<'_>,
    name: String,
    extensions: Vec<String>,
    save_dir: Option<String>,
) -> Result<DownloadCategory> {
    require_confirm_or_main(&window)?;
    state
        .add_download_category(name, extensions, save_dir)
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn set_transfer_speed_limit(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
    limit_bps: Option<u64>,
) -> Result<()> {
    require_progress_or_main(&window, &transfer_id)?;
    state
        .set_transfer_speed_limit(&transfer_id, limit_bps)
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn get_transfer_completion_options(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
) -> Result<TransferCompletionOptions> {
    require_progress_or_main(&window, &transfer_id)?;
    state.get_transfer_completion_options(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn set_transfer_completion_options(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
    options: TransferCompletionOptions,
) -> Result<()> {
    require_progress_or_main(&window, &transfer_id)?;
    state
        .set_transfer_completion_options(&transfer_id, options)
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn open_progress_dialog(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
) -> Result<()> {
    require_main(&window)?;
    state.open_progress_dialog(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn open_completed_file(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
) -> Result<()> {
    require_transfer_dialog_or_main(&window, &transfer_id)?;
    state.open_completed_file(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn pick_download_folder(
    window: WebviewWindow,
    initial_dir: Option<String>,
) -> Result<Option<String>> {
    require_confirm_or_main(&window)?;
    let owner = window.hwnd().map_err(|error| error.to_string())?.0 as isize;
    tokio::task::spawn_blocking(move || crate::shell::pick_download_folder(owner, initial_dir))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn expand_wildcard_urls(window: WebviewWindow, pattern: String) -> Result<Vec<String>> {
    require_main(&window)?;
    crate::expand_wildcard_urls(&pattern)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn probe_media(
    window: WebviewWindow,
    state: Daemon<'_>,
    url: String,
    playlist: Option<bool>,
    headers: Option<HashMap<String, String>>,
) -> Result<MediaProbeResult> {
    require_main(&window)?;
    state
        .probe_media_with_headers(&url, playlist, &headers.unwrap_or_default())
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn add_media_download(
    window: WebviewWindow,
    state: Daemon<'_>,
    request: AddMediaDownloadRequest,
) -> Result<String> {
    require_main(&window)?;
    state.add_media_download(request).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn add_torrent(
    window: WebviewWindow,
    state: Daemon<'_>,
    request: AddTorrentRequest,
) -> Result<String> {
    require_main(&window)?;
    state.add_torrent(request).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn list_torrent_files(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
) -> Result<Vec<TorrentFileEntry>> {
    require_main(&window)?;
    state.list_torrent_files(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn set_torrent_files(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
    selected_indices: Vec<usize>,
) -> Result<()> {
    require_main(&window)?;
    state
        .set_torrent_files(&transfer_id, selected_indices)
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn pause_transfer(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
) -> Result<()> {
    require_progress_or_main(&window, &transfer_id)?;
    state.pause_transfer(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn resume_transfer(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
) -> Result<()> {
    require_progress_or_main(&window, &transfer_id)?;
    state.resume_transfer(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn pause_playlist(
    window: WebviewWindow,
    state: Daemon<'_>,
    playlist_id: String,
) -> Result<()> {
    require_main(&window)?;
    state.pause_playlist(&playlist_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn resume_playlist(
    window: WebviewWindow,
    state: Daemon<'_>,
    playlist_id: String,
) -> Result<()> {
    require_main(&window)?;
    state.resume_playlist(&playlist_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn reveal_playlist_folder(
    window: WebviewWindow,
    state: Daemon<'_>,
    playlist_id: String,
) -> Result<()> {
    require_main(&window)?;
    let state = state.inner().clone();
    tokio::task::spawn_blocking(move || state.reveal_playlist_folder(&playlist_id))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn cancel_transfer(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
    delete_files: bool,
) -> Result<()> {
    require_window(
        window.label() == MAIN
            || (!delete_files
                && window.label().strip_prefix(PROGRESS_PREFIX) == Some(transfer_id.as_str())),
    )?;
    state.cancel_transfer(&transfer_id, delete_files).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn refresh_download_url(
    window: WebviewWindow,
    state: Daemon<'_>,
    request: RefreshDownloadUrlRequest,
) -> Result<()> {
    require_main(&window)?;
    state.refresh_download_url(request).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn set_speed_limit(
    window: WebviewWindow,
    state: Daemon<'_>,
    limit_bps: Option<u64>,
) -> Result<()> {
    require_main(&window)?;
    state.set_speed_limit(limit_bps).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn verify_transfer_checksum(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
    expected_sha256: String,
) -> Result<bool> {
    require_main(&window)?;
    state
        .verify_transfer_checksum(&transfer_id, &expected_sha256)
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn reveal_in_explorer(
    window: WebviewWindow,
    state: Daemon<'_>,
    transfer_id: String,
) -> Result<()> {
    require_transfer_dialog_or_main(&window, &transfer_id)?;
    state.reveal_in_explorer(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn get_settings(window: WebviewWindow, state: Daemon<'_>) -> Result<AppSettings> {
    require_app_window(&window)?;
    Ok(state.settings())
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn save_settings(
    window: WebviewWindow,
    state: Daemon<'_>,
    settings: AppSettings,
) -> Result<()> {
    require_main(&window)?;
    state.save_settings(settings).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn get_toolchain_status(
    window: WebviewWindow,
    state: Daemon<'_>,
) -> Result<ToolchainStatus> {
    require_main(&window)?;
    Ok(state.get_toolchain_status().await)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn update_ytdlp(
    window: WebviewWindow,
    state: Daemon<'_>,
) -> Result<ToolchainStatus> {
    require_main(&window)?;
    tools::update_ytdlp_managed().await?;
    state.invalidate_toolchain_cache().await;
    Ok(state.get_toolchain_status().await)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn list_browsers(
    window: WebviewWindow,
    state: Daemon<'_>,
) -> Result<Vec<BrowserStatus>> {
    require_main(&window)?;
    let base_key = state.registry_base_key().to_owned();
    let resource_dir = state.resource_dir().map(std::path::Path::to_owned);
    tokio::task::spawn_blocking(move || tools::list_browsers(&base_key, resource_dir.as_deref()))
        .await
        .map_err(|_| "Could not check for installed browsers.".to_owned())
}

#[tauri::command]
pub(crate) async fn open_browser_extension_setup(
    window: WebviewWindow,
    state: Daemon<'_>,
    browser_id: BrowserKind,
    custom_extension_id: Option<String>,
) -> Result<BrowserSetupResult> {
    require_main(&window)?;
    let manifests = state.data_dir().join("native-messaging");
    let base_key = state.registry_base_key().to_owned();
    let resource_dir = state.resource_dir().map(std::path::Path::to_owned);
    tokio::task::spawn_blocking(move || {
        tools::open_browser_extension_setup(
            &base_key,
            &manifests,
            resource_dir.as_deref(),
            browser_id,
            custom_extension_id.as_deref(),
        )
    })
    .await
    .map_err(|_| "Could not start browser extension setup.".to_owned())?
    .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn install_browser_hosts(
    window: WebviewWindow,
    app: AppHandle,
    state: Daemon<'_>,
    chrome_id: Option<String>,
    edge_id: Option<String>,
    firefox_id: Option<String>,
) -> Result<Vec<String>> {
    require_main(&window)?;
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|error| error.to_string())?;
    let manifests = state.data_dir().join("native-messaging");
    let base_key = state.registry_base_key().to_owned();
    tokio::task::spawn_blocking(move || {
        let executable = tools::native_host_executable(Some(&resource_dir))?;
        let paths = tools::install_browser_hosts_into(
            &base_key,
            &manifests,
            &executable,
            chrome_id.as_deref(),
            edge_id.as_deref(),
            firefox_id.as_deref(),
        )?;
        Ok::<_, std::io::Error>(
            paths
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

pub(crate) fn friendly_updater_error(action: &str, error: impl Display) -> String {
    let error = error.to_string().to_ascii_lowercase();
    let reason = if ["404", "release not found", "release json from the remote"]
        .iter()
        .any(|text| error.contains(text))
    {
        "The update release could not be found. Try again later."
    } else if [
        "signature",
        "pubkey",
        "public key",
        "verification",
        "minisign",
        "base64",
    ]
    .iter()
    .any(|text| error.contains(text))
    {
        "The update signature could not be verified."
    } else if ["json", "parse", "at line", "decode response body"]
        .iter()
        .any(|text| error.contains(text))
    {
        "The update server returned invalid release information. Try again later."
    } else if [
        "network",
        "connection",
        "connect",
        "sending request",
        "timeout",
        "timed out",
        "download request failed",
        "request or response body",
        "dns",
    ]
    .iter()
    .any(|text| error.contains(text))
    {
        "Could not reach the update server. Check your connection and try again."
    } else {
        "Try again later."
    };
    format!("Could not {action}. {reason}")
}

fn updater_error(action: &str, error: tauri_plugin_updater::Error) -> String {
    use tauri_plugin_updater::Error;

    match error {
        Error::ReleaseNotFound => friendly_updater_error(action, "release not found"),
        Error::Serialization(_) | Error::Semver(_) => {
            friendly_updater_error(action, "invalid release JSON")
        }
        Error::Minisign(_)
        | Error::Base64(_)
        | Error::SignatureUtf8(_)
        | Error::SignedVersionMismatch { .. }
        | Error::MissingSignedVersion => {
            friendly_updater_error(action, "signature verification failed")
        }
        Error::Reqwest(error) if error.is_decode() => {
            friendly_updater_error(action, "invalid release JSON")
        }
        Error::Reqwest(error) if error.status() == Some(reqwest::StatusCode::NOT_FOUND) => {
            friendly_updater_error(action, "release not found")
        }
        Error::Reqwest(_) => friendly_updater_error(action, "network error"),
        error => friendly_updater_error(action, error),
    }
}

#[tauri::command]
pub(crate) async fn check_for_updates(
    window: WebviewWindow,
    app: AppHandle,
    state: Daemon<'_>,
    startup: Option<bool>,
) -> Result<Option<UpdateStatus>> {
    require_main(&window)?;
    if startup == Some(true) && !state.claim_startup_update_check() {
        return Ok(None);
    }
    const ACTION: &str = "check for updates";
    let current_version = app.package_info().version.to_string();
    let update = app
        .updater()
        .map_err(|error| updater_error(ACTION, error))?
        .check()
        .await
        .map_err(|error| updater_error(ACTION, error))?;

    Ok(Some(match update {
        Some(update) => UpdateStatus {
            available: true,
            current_version,
            version: Some(update.version),
            notes: update.body,
            pub_date: update.date.map(|date| date.to_string()),
        },
        None => UpdateStatus {
            available: false,
            current_version,
            version: None,
            notes: None,
            pub_date: None,
        },
    }))
}

#[tauri::command]
pub(crate) async fn install_update(
    window: WebviewWindow,
    app: AppHandle,
    state: Daemon<'_>,
) -> Result<bool> {
    require_main(&window)?;
    static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _install = INSTALL
        .try_lock()
        .map_err(|_| "An update is already in progress.")?;
    const CHECK: &str = "check for updates";
    let Some(update) = app
        .updater()
        .map_err(|error| updater_error(CHECK, error))?
        .check()
        .await
        .map_err(|error| updater_error(CHECK, error))?
    else {
        return Ok(false);
    };

    let emit = |stage, downloaded_bytes, total_bytes| {
        let _ = app.emit(
            "slipstream://update-progress",
            UpdateProgress {
                stage,
                downloaded_bytes,
                total_bytes,
            },
        );
    };
    emit(UpdateProgressStage::Downloading, 0, None);
    let progress = Mutex::new((0_u64, None));
    let mut last_emit = Instant::now();
    let mut last_emitted_bytes = 0;
    let bytes = update
        .download(
            |chunk_bytes, content_length| {
                let (downloaded_bytes, total_bytes) = {
                    let mut progress = progress.lock().unwrap();
                    progress.0 += chunk_bytes as u64;
                    progress.1 = content_length.or(progress.1);
                    *progress
                };
                if last_emit.elapsed() >= Duration::from_millis(80)
                    || downloaded_bytes - last_emitted_bytes >= 131_072
                    || total_bytes == Some(downloaded_bytes)
                {
                    emit(
                        UpdateProgressStage::Downloading,
                        downloaded_bytes,
                        total_bytes,
                    );
                    last_emit = Instant::now();
                    last_emitted_bytes = downloaded_bytes;
                }
            },
            || {
                let (downloaded_bytes, total_bytes) = *progress.lock().unwrap();
                emit(
                    UpdateProgressStage::Verifying,
                    downloaded_bytes,
                    total_bytes,
                );
            },
        )
        .await
        .map_err(|error| updater_error("download the update", error))?;
    let (downloaded_bytes, total_bytes) = progress.into_inner().unwrap();
    emit(
        UpdateProgressStage::Installing,
        downloaded_bytes,
        total_bytes,
    );
    const RECOVERY: &str = "Quit and reopen SlipStream to resume downloads.";
    state.shutdown().await.map_err(|error| {
        tracing::error!(%error, "could not stop the daemon for the update");
        format!("Could not stop downloads to prepare the update. {RECOVERY}")
    })?;
    // On Windows, the updater exits SlipStream and NSIS relaunches it after installing.
    update
        .install(bytes)
        .map_err(|error| format!("{} {RECOVERY}", updater_error("install the update", error)))?;
    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn hibernate_to_tray(
    window: WebviewWindow,
    ui: State<'_, Arc<DesktopUi>>,
) -> Result<()> {
    require_main(&window)?;
    ui.hibernate().await
}
