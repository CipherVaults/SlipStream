use crate::{shell::DesktopUi, tools, AppState, ToolchainStatus};
use slipstream_protocol::{
    AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings, BrowserKind,
    BrowserSetupResult, BrowserStatus, MediaProbeResult, RefreshDownloadUrlRequest, TelemetryBatch,
    TorrentFileEntry, UpdateProgress, UpdateProgressStage, UpdateStatus,
};
use std::{
    fmt::Display,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;

type Daemon<'a> = State<'a, Arc<AppState>>;
type Result<T> = std::result::Result<T, String>;

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn get_snapshot(state: Daemon<'_>) -> Result<TelemetryBatch> {
    Ok(state.snapshot().await)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn add_http_download(
    state: Daemon<'_>,
    request: AddHttpDownloadRequest,
) -> Result<String> {
    state.add_http_download(request).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn expand_wildcard_urls(pattern: String) -> Result<Vec<String>> {
    crate::expand_wildcard_urls(&pattern)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn probe_media(state: Daemon<'_>, url: String) -> Result<MediaProbeResult> {
    state.probe_media(&url).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn add_media_download(
    state: Daemon<'_>,
    request: AddMediaDownloadRequest,
) -> Result<String> {
    state.add_media_download(request).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn add_torrent(state: Daemon<'_>, request: AddTorrentRequest) -> Result<String> {
    state.add_torrent(request).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn list_torrent_files(
    state: Daemon<'_>,
    transfer_id: String,
) -> Result<Vec<TorrentFileEntry>> {
    state.list_torrent_files(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn set_torrent_files(
    state: Daemon<'_>,
    transfer_id: String,
    selected_indices: Vec<usize>,
) -> Result<()> {
    state
        .set_torrent_files(&transfer_id, selected_indices)
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn pause_transfer(state: Daemon<'_>, transfer_id: String) -> Result<()> {
    state.pause_transfer(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn resume_transfer(state: Daemon<'_>, transfer_id: String) -> Result<()> {
    state.resume_transfer(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn cancel_transfer(
    state: Daemon<'_>,
    transfer_id: String,
    delete_files: bool,
) -> Result<()> {
    state.cancel_transfer(&transfer_id, delete_files).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn refresh_download_url(
    state: Daemon<'_>,
    request: RefreshDownloadUrlRequest,
) -> Result<()> {
    state.refresh_download_url(request).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn set_speed_limit(state: Daemon<'_>, limit_bps: Option<u64>) -> Result<()> {
    state.set_speed_limit(limit_bps).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn verify_transfer_checksum(
    state: Daemon<'_>,
    transfer_id: String,
    expected_sha256: String,
) -> Result<bool> {
    state
        .verify_transfer_checksum(&transfer_id, &expected_sha256)
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn reveal_in_explorer(state: Daemon<'_>, transfer_id: String) -> Result<()> {
    state.reveal_in_explorer(&transfer_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn get_settings(state: Daemon<'_>) -> Result<AppSettings> {
    Ok(state.settings())
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn save_settings(state: Daemon<'_>, settings: AppSettings) -> Result<()> {
    state.save_settings(settings).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn get_toolchain_status(state: Daemon<'_>) -> Result<ToolchainStatus> {
    Ok(state.get_toolchain_status().await)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn update_ytdlp(state: Daemon<'_>) -> Result<ToolchainStatus> {
    tools::update_ytdlp_managed().await?;
    Ok(state.get_toolchain_status().await)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn list_browsers(state: Daemon<'_>) -> Result<Vec<BrowserStatus>> {
    let base_key = state.registry_base_key().to_owned();
    let resource_dir = state.resource_dir().map(std::path::Path::to_owned);
    tokio::task::spawn_blocking(move || tools::list_browsers(&base_key, resource_dir.as_deref()))
        .await
        .map_err(|_| "Could not check for installed browsers.".to_owned())
}

#[tauri::command]
pub(crate) async fn open_browser_extension_setup(
    state: Daemon<'_>,
    browser_id: BrowserKind,
    custom_extension_id: Option<String>,
) -> Result<BrowserSetupResult> {
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
    app: AppHandle,
    state: Daemon<'_>,
    chrome_id: Option<String>,
    edge_id: Option<String>,
    firefox_id: Option<String>,
) -> Result<Vec<String>> {
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
pub(crate) async fn check_for_updates(app: AppHandle) -> Result<UpdateStatus> {
    const ACTION: &str = "check for updates";
    let current_version = app.package_info().version.to_string();
    let update = app
        .updater()
        .map_err(|error| updater_error(ACTION, error))?
        .check()
        .await
        .map_err(|error| updater_error(ACTION, error))?;

    Ok(match update {
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
    })
}

#[tauri::command]
pub(crate) async fn install_update(app: AppHandle, state: Daemon<'_>) -> Result<bool> {
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
    state.shutdown().await?;
    // On Windows, the updater exits SlipStream and NSIS relaunches it after installing.
    update.install(bytes).map_err(|error| {
        format!(
            "{} Restart SlipStream to resume downloads.",
            updater_error("install the update", error)
        )
    })?;
    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn hibernate_to_tray(ui: State<'_, Arc<DesktopUi>>) -> Result<()> {
    ui.hibernate().await
}
