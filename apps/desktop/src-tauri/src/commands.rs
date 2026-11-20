use crate::{shell::DesktopUi, tools, AppState, ToolchainStatus};
use slipstream_protocol::{
    AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings,
    MediaProbeResult, RefreshDownloadUrlRequest, TelemetryBatch, TorrentFileEntry,
};
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};

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
    tokio::task::spawn_blocking(move || {
        let executable = tools::native_host_executable(&resource_dir)?;
        let paths = tools::install_browser_hosts_into(
            slipstream_native_host::registry::DEFAULT_BASE_KEY,
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

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn hibernate_to_tray(ui: State<'_, Arc<DesktopUi>>) -> Result<()> {
    ui.hibernate().await
}
