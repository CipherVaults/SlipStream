use crate::{
    expand_wildcard_urls, install_browser_hosts_into, next_numbered_filename, resolve_category_dir,
    tools::{
        list_browsers, native_host_executable, open_browser_extension_setup, update_ytdlp_managed,
    },
    updater::{self, friendly_updater_error},
    view_model::{
        self, classify_icon_kind, compute_nav_counts, demo_telemetry_batch, format_bytes,
        format_duration, format_eta, format_speed, format_transfer_percent, is_torrent_input,
        looks_like_direct_file_url, looks_like_http_url, matches_filter,
        pair_video_with_best_audio, parse_speed_limit_bps, parse_speed_limit_slider_mbps,
        should_clear_seeded_headers, url_origin, video_format_options, NavFilter,
    },
    win32, AppState, BrowserCardModel, CompletedRowModel, ConnectionRowModel, DaemonOptions,
    DownloadCompleteWindow, DownloadConfirmWindow, DownloadProgressWindow, FormatOptionModel,
    InspectorModel, MainWindow, OpenPickerEvent, PaletteActionModel, PlaylistGroupModel,
    PlaylistItemModel, SegmentTileModel, SubtitleItemModel, Theme, ToolchainStatus,
    TorrentFileRowModel, TorrentPeerRowModel, TorrentPreviewFileModel, TransferRowModel, UiBridge,
};
use i_slint_backend_winit::{winit::window::ResizeDirection, WinitWindowAccessor};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use slipstream_protocol::{
    AccentMode, AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings,
    AudioPreset, BrowserKind, ConfirmPendingDownloadRequest, MediaContainer, MediaFormatOption,
    MediaProbeResult, PendingDownloadPreview, RefreshDownloadUrlRequest, SegmentState,
    SelectedPlaylistEntry, TelemetryBatch, ThemeMode, ToolSource, TorrentFileEntry,
    TorrentPeerSummary, TorrentPreviewResult, TransferCompletionOptions, TransferKind,
    TransferSnapshot, TransferStatus, UpdateMode, UpdateProgress, UpdateProgressStage,
};
use slipstream_torrent::{parse_magnet_uri, CancellationToken};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const MAX_SPEED_SAMPLES: usize = 28;
const APP_ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

pub fn default_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("SlipStream"))
        .unwrap_or_else(|| std::env::temp_dir().join("SlipStream"))
}

pub fn default_resource_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

#[derive(Default)]
struct SharedBridgeSlots {
    pending_batch: Mutex<Option<TelemetryBatch>>,
    telemetry_scheduled: AtomicBool,
    app: Mutex<Option<Arc<AppState>>>,
}

pub struct NativeUi {
    slots: Arc<SharedBridgeSlots>,
    quitting: AtomicBool,
}

impl NativeUi {
    fn new(slots: Arc<SharedBridgeSlots>) -> Self {
        Self {
            slots,
            quitting: AtomicBool::new(false),
        }
    }
}

impl UiBridge for NativeUi {
    fn show_main_window(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async {
            let _ = slint::invoke_from_event_loop(|| {
                with_controller(|ctrl| ctrl.show_main_window());
            });
            Ok(())
        })
    }

    fn emit_telemetry(&self, batch: &TelemetryBatch) -> Result<(), String> {
        *self.slots.pending_batch.lock().unwrap() = Some(batch.clone());
        if !self.slots.telemetry_scheduled.swap(true, Ordering::AcqRel) {
            let slots = self.slots.clone();
            let _ = slint::invoke_from_event_loop(move || {
                slots.telemetry_scheduled.store(false, Ordering::Release);
                if let Some(latest) = slots.pending_batch.lock().unwrap().take() {
                    with_controller(|ctrl| ctrl.apply_telemetry(latest));
                }
            });
        }
        Ok(())
    }

    fn emit_open_picker(&self, event: &OpenPickerEvent) -> Result<(), String> {
        let event = event.clone();
        let _ = slint::invoke_from_event_loop(move || {
            with_controller(|ctrl| ctrl.open_add_dialog_with_seed(event));
        });
        Ok(())
    }

    fn emit_settings_changed(&self) {
        let _ = slint::invoke_from_event_loop(|| {
            with_controller(|ctrl| ctrl.refresh_theme_from_settings());
        });
    }

    fn has_main_window(&self) -> bool {
        true
    }

    fn supports_dialogs(&self) -> bool {
        true
    }

    fn open_confirm_dialog(&self, id: &str) -> Result<(), String> {
        let id = id.to_owned();
        let _ = slint::invoke_from_event_loop(move || {
            with_controller(|ctrl| ctrl.open_confirm_dialog(&id));
        });
        Ok(())
    }

    fn close_confirm_dialog(&self, id: &str) {
        let id = id.to_owned();
        let _ = slint::invoke_from_event_loop(move || {
            with_controller(|ctrl| ctrl.close_confirm_dialog(&id));
        });
    }

    fn open_progress_dialog(&self, id: &str) -> Result<(), String> {
        let id = id.to_owned();
        let _ = slint::invoke_from_event_loop(move || {
            with_controller(|ctrl| ctrl.open_progress_dialog(&id));
        });
        Ok(())
    }

    fn close_progress_dialog(&self, id: &str) {
        let id = id.to_owned();
        let _ = slint::invoke_from_event_loop(move || {
            with_controller(|ctrl| ctrl.close_progress_dialog(&id));
        });
    }

    fn open_complete_dialog(&self, id: &str) -> Result<(), String> {
        let id = id.to_owned();
        let _ = slint::invoke_from_event_loop(move || {
            with_controller(|ctrl| ctrl.open_complete_dialog(&id));
        });
        Ok(())
    }

    fn trigger_ready_check(&self) {
        if let Some(app) = self.slots.app.lock().unwrap().clone() {
            let gen = app.window_generation();
            let _ = app.window_ready(gen);
        }
    }

    fn request_quit(&self) {
        self.quitting.store(true, Ordering::Release);
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    }
}

struct ConfirmWindowState {
    window: DownloadConfirmWindow,
    cascade: i32,
    selected_category_id: String,
    overwrite_existing: bool,
    submitting: bool,
}

struct ProgressWindowState {
    window: DownloadProgressWindow,
    cascade: i32,
    models: ProgressModels,
    snapshot: TransferSnapshot,
    speed_edit_revision: u64,
    speed_dirty: bool,
}

#[derive(Default)]
pub struct ProgressModels {
    segments_model: Rc<VecModel<f32>>,
    connections_model: Rc<VecModel<ConnectionRowModel>>,
    peers_model: Rc<VecModel<TorrentPeerRowModel>>,
}

impl ProgressModels {
    pub fn attach(&self, window: &DownloadProgressWindow) {
        window.set_segments(self.segments_model.clone().into());
        window.set_connections(self.connections_model.clone().into());
        window.set_peers(self.peers_model.clone().into());
    }

    pub fn update_peers(&self, window: &DownloadProgressWindow, summary: &TorrentPeerSummary) {
        sync_vec_model_by(&self.peers_model, peer_rows(summary), PartialEq::eq);
        window.set_peers_summary_text(peer_summary_text(summary).into());
    }
}

#[derive(Default)]
pub struct DesktopModels {
    playlist_groups_model: Rc<VecModel<PlaylistGroupModel>>,
    active_rows_model: Rc<VecModel<TransferRowModel>>,
    completed_rows_model: Rc<VecModel<CompletedRowModel>>,
    inspector_segments_model: Rc<VecModel<SegmentTileModel>>,
    inspector_files_model: Rc<VecModel<TorrentFileRowModel>>,
    inspector_peers_model: Rc<VecModel<TorrentPeerRowModel>>,
    row_segment_models: HashMap<String, Rc<VecModel<f32>>>,
    playlist_item_models: HashMap<String, Rc<VecModel<TransferRowModel>>>,
}

impl DesktopModels {
    pub fn attach(&self, main: &MainWindow) {
        main.set_playlist_groups(self.playlist_groups_model.clone().into());
        main.set_active_rows(self.active_rows_model.clone().into());
        main.set_completed_rows(self.completed_rows_model.clone().into());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TransferDialog {
    Progress,
    Complete,
}

struct CompleteWindowState {
    window: DownloadCompleteWindow,
    cascade: i32,
    snapshot: TransferSnapshot,
}

#[derive(Default)]
struct AddDialogRuntime {
    generation: u64,
    source: String,
    probe: Option<MediaProbeResult>,
    seeded_headers: HashMap<String, String>,
    seeded_origin: Option<String>,
    selected_format_id: Option<String>,
    checked_playlist: HashSet<u32>,
    checked_subtitles: HashSet<String>,
    torrent_preview: Option<TorrentPreviewResult>,
    torrent_preview_token: Option<String>,
    torrent_preview_cancel: Option<CancellationToken>,
    checked_preview_files: HashSet<usize>,
    preview_disk_free: Option<u64>,
}

struct ControllerState {
    main: MainWindow,
    runtime: tokio::runtime::Handle,
    app: Option<Arc<AppState>>,
    demo_mode: bool,
    is_light: bool,
    is_mono: bool,
    is_maximized: bool,
    last_batch: Option<TelemetryBatch>,
    speed_history: VecDeque<u64>,
    expanded_playlists: HashSet<String>,
    selected_id: Option<String>,
    models: DesktopModels,
    last_disk_check: Option<Instant>,
    disk_space: Option<(u64, u64)>,
    torrent_files: HashMap<String, Vec<TorrentFileEntry>>,
    torrent_file_selections: HashMap<String, HashSet<usize>>,
    torrent_padding_files: HashMap<String, HashSet<usize>>,
    torrent_files_pending: HashMap<String, Instant>,
    torrent_file_apply_pending: HashSet<String>,
    torrent_peers: HashMap<String, TorrentPeerSummary>,
    torrent_peers_pending: HashSet<String>,
    pending_transfer_dialogs: HashSet<(String, TransferDialog)>,
    torrent_preview_files_model: Rc<VecModel<TorrentPreviewFileModel>>,
    toast_timer: Timer,
    confirm_windows: HashMap<String, ConfirmWindowState>,
    progress_windows: HashMap<String, ProgressWindowState>,
    complete_windows: HashMap<String, CompleteWindowState>,
    add_state: AddDialogRuntime,
    pending_picker_requests: VecDeque<OpenPickerEvent>,
    refresh_target_id: Option<String>,
    verify_target_id: Option<String>,
    last_clipboard_seq: u32,
    clipboard_in_flight: Arc<AtomicBool>,
    theme_poll_counter: u8,
    _tray_icon: Option<TrayIcon>,
    tray_menu_ids: TrayMenuIds,
}

#[derive(Default, Clone)]
struct TrayMenuIds {
    show: Option<muda::MenuId>,
    add: Option<muda::MenuId>,
    pause_all: Option<muda::MenuId>,
    resume_all: Option<muda::MenuId>,
    quit: Option<muda::MenuId>,
}

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<RefCell<ControllerState>>>> = const { RefCell::new(None) };
}

fn with_controller<F: FnOnce(&mut ControllerState)>(f: F) {
    CONTROLLER.with(|cell| {
        if let Some(rc) = cell.borrow().as_ref() {
            let mut guard = rc.borrow_mut();
            f(&mut guard);
        }
    });
}

fn hwnd_of(window: &slint::Window) -> isize {
    let mut result = 0_isize;
    window.with_winit_window(|w| {
        if let Ok(handle) = w.window_handle() {
            if let RawWindowHandle::Win32(win32_handle) = handle.as_raw() {
                result = win32_handle.hwnd.get();
            }
        }
    });
    result
}

fn apply_native_window_chrome(window: &slint::Window) {
    window.with_winit_window(|w| {
        if let Ok(handle) = w.window_handle() {
            if let RawWindowHandle::Win32(win32_handle) = handle.as_raw() {
                win32::round_corners(win32_handle.hwnd.get());
            }
        }
    });
}

fn foreground_dialog(window: &slint::Window, cascade: i32) {
    apply_native_window_chrome(window);
    let hwnd = hwnd_of(window);
    win32::bring_window_to_front(hwnd);
    win32::center_window_on_active_monitor(hwnd, cascade);
}

fn drag_slint_window(window: &slint::Window) {
    window.with_winit_window(|w| {
        let _ = w.drag_window();
    });
}

fn toggle_maximize_slint_window(window: &slint::Window, currently_max: bool) -> bool {
    let mut next_max = currently_max;
    window.with_winit_window(|w| {
        next_max = !w.is_maximized();
        w.set_maximized(next_max);
    });
    next_max
}

fn resolve_theme_flags(settings: &AppSettings) -> (bool, bool) {
    let light = match settings.theme {
        ThemeMode::Light => true,
        ThemeMode::Dark => false,
        ThemeMode::System => win32::is_system_light_theme(),
    };
    let mono = settings.accent == AccentMode::Mono;
    (light, mono)
}

macro_rules! set_win_theme {
    ($win:expr, $light:expr, $mono:expr) => {{
        let global = $win.global::<Theme<'_>>();
        global.set_is_light($light);
        global.set_is_mono($mono);
    }};
}

fn format_option_detail(f: &MediaFormatOption) -> String {
    let parts: Vec<String> = [
        Some(f.container.to_ascii_uppercase()),
        f.filesize_bytes.map(format_bytes),
        f.bitrate_kbps.map(|b| format!("{b:.0} kbps")),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.join(" • ")
}

fn build_tray_icon() -> Option<(TrayIcon, TrayMenuIds)> {
    let img = image::load_from_memory(APP_ICON_PNG).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let icon = tray_icon::Icon::from_rgba(img.into_raw(), w, h).ok()?;

    let show_item = MenuItem::new("Show SlipStream", true, None);
    let add_item = MenuItem::new("Add Download...", true, None);
    let pause_item = MenuItem::new("Pause All", true, None);
    let resume_item = MenuItem::new("Resume All", true, None);
    let quit_item = MenuItem::new("Quit", true, None);

    let ids = TrayMenuIds {
        show: Some(show_item.id().clone()),
        add: Some(add_item.id().clone()),
        pause_all: Some(pause_item.id().clone()),
        resume_all: Some(resume_item.id().clone()),
        quit: Some(quit_item.id().clone()),
    };

    let menu = Menu::new();
    let _ = menu.append(&show_item);
    let _ = menu.append(&add_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&pause_item);
    let _ = menu.append(&resume_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit_item);

    let tray = TrayIconBuilder::new()
        .with_tooltip("SlipStream")
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .build()
        .ok()?;
    Some((tray, ids))
}

fn sync_vec_model_by<T: Clone + 'static, F: Fn(&T, &T) -> bool>(
    model: &VecModel<T>,
    next: Vec<T>,
    same: F,
) {
    while model.row_count() > next.len() {
        model.remove(model.row_count() - 1);
    }
    for (index, item) in next.into_iter().enumerate() {
        match model.row_data(index) {
            Some(current) if !same(&current, &item) => model.set_row_data(index, item),
            Some(_) => {}
            None => model.push(item),
        }
    }
}

fn reconcile_vec_model_by_key<T: Clone + 'static>(
    model: &VecModel<T>,
    next: Vec<T>,
    key: impl Fn(&T) -> &str,
    same: impl Fn(&T, &T) -> bool,
) {
    let next_keys: HashSet<_> = next.iter().map(&key).collect();
    for index in (0..model.row_count()).rev() {
        if model
            .row_data(index)
            .is_some_and(|row| !next_keys.contains(key(&row)))
        {
            model.remove(index);
        }
    }

    for (index, item) in next.into_iter().enumerate() {
        if let Some(current) = model.row_data(index) {
            if key(&current) == key(&item) {
                if !same(&current, &item) {
                    model.set_row_data(index, item);
                }
                continue;
            }
            if let Some(from) = (index + 1..model.row_count()).find(|&from| {
                model
                    .row_data(from)
                    .is_some_and(|row| key(&row) == key(&item))
            }) {
                let current = model.remove(from);
                model.insert(index, if same(&current, &item) { current } else { item });
                continue;
            }
        }
        model.insert(index, item);
    }
}

fn sync_row_segments(
    models: &mut HashMap<String, Rc<VecModel<f32>>>,
    transfer: &TransferSnapshot,
) -> ModelRc<f32> {
    let model = models.entry(transfer.id.clone()).or_default();
    sync_vec_model_by(model, transfer.ui_segments.to_vec(), PartialEq::eq);
    model.clone().into()
}

fn same_transfer_row(a: &TransferRowModel, b: &TransferRowModel) -> bool {
    a.id == b.id
        && a.filename == b.filename
        && a.kind == b.kind
        && a.status == b.status
        && a.is_active == b.is_active
        && a.is_paused == b.is_paused
        && a.is_completed == b.is_completed
        && a.is_torrent == b.is_torrent
        && a.is_error == b.is_error
        && a.percent == b.percent
        && a.progress_label == b.progress_label
        && a.meta_line == b.meta_line
        && a.source_domain == b.source_domain
        && a.protocol_label == b.protocol_label
        && a.tag_text == b.tag_text
        && a.sha_verified == b.sha_verified
        && a.error_text == b.error_text
        && a.segment_tone == b.segment_tone
        && a.icon_kind == b.icon_kind
        && a.can_pause_resume == b.can_pause_resume
        && a.playlist_index_text == b.playlist_index_text
        && a.format_summary == b.format_summary
}

fn peer_rows(summary: &TorrentPeerSummary) -> Vec<TorrentPeerRowModel> {
    summary
        .peers
        .iter()
        .map(|peer| TorrentPeerRowModel {
            address: peer.address.clone().into(),
            state_label: peer.state.clone().into(),
            state_tone: match peer.state.as_str() {
                "Live" | "live" => 2,
                "Connecting" | "connecting" => 3,
                _ => 0,
            },
            downloaded_text: format_bytes(peer.downloaded_bytes).into(),
            speed_text: format_speed(peer.download_speed_bps).into(),
            pieces_text: peer.pieces_downloaded.to_string().into(),
            direction_text: if peer.incoming { "In" } else { "Out" }.into(),
        })
        .collect()
}

fn peer_summary_text(summary: &TorrentPeerSummary) -> String {
    format!(
        "Live: {} • Connecting: {} • Queued: {} • Seen: {}",
        summary.live, summary.connecting, summary.queued, summary.seen
    )
}

fn wildcard_batch(source: &str) -> Option<Vec<String>> {
    let source = source.trim();
    if !source.contains('[') || is_torrent_input(source) {
        return None;
    }
    // The engine accepts [first-last]. An IPv6 host can leave the input unchanged.
    expand_wildcard_urls(source)
        .ok()
        .filter(|urls| urls.first().is_some_and(|url| url != source))
}

fn should_auto_probe(source: &str) -> bool {
    !is_torrent_input(source)
        && looks_like_http_url(source)
        && !looks_like_direct_file_url(source)
        && wildcard_batch(source).is_none()
}

fn should_auto_preview(source: &str) -> bool {
    let source = source.trim();
    if source.to_ascii_lowercase().starts_with("magnet:") {
        parse_magnet_uri(source).is_ok()
    } else {
        is_torrent_input(source)
    }
}

fn rejected_padding_file(error: &str) -> Option<usize> {
    error
        .strip_prefix("invalid torrent state: padding file ")?
        .strip_suffix(" is not selectable")?
        .parse()
        .ok()
}

fn can_stop(transfer: &TransferSnapshot) -> bool {
    matches!(
        transfer.status,
        TransferStatus::Active
            | TransferStatus::Queued
            | TransferStatus::Probing
            | TransferStatus::Remuxing
            | TransferStatus::Seeding
    )
}

fn transfer_to_row(
    t: &TransferSnapshot,
    compact: bool,
    segments: &mut HashMap<String, Rc<VecModel<f32>>>,
) -> TransferRowModel {
    let (percent, progress_label) = format_transfer_percent(t);
    let is_paused = t.status == TransferStatus::Paused;
    let is_completed = t.status == TransferStatus::Completed;
    let is_torrent = t.kind == TransferKind::Torrent;
    let is_active = matches!(
        t.status,
        TransferStatus::Active
            | TransferStatus::Probing
            | TransferStatus::Remuxing
            | TransferStatus::Seeding
    );
    let is_error = t.status == TransferStatus::Failed || t.error_message.is_some();
    let tag_text = t
        .format_tags
        .first()
        .cloned()
        .unwrap_or_else(|| match t.kind {
            TransferKind::Torrent => format!("{} peers", t.active_peers),
            TransferKind::Media => "MEDIA".into(),
            TransferKind::Http => format!("{} conn", t.active_connections.max(1)),
        });

    let progress_bytes = match t.total_bytes {
        Some(total) => format!(
            "{} / {}",
            format_bytes(t.downloaded_bytes),
            format_bytes(total)
        ),
        None => format_bytes(t.downloaded_bytes),
    };
    let mut parts = vec![progress_bytes];
    if t.download_speed_bps > 0 {
        parts.push(format_speed(t.download_speed_bps));
    }
    if let Some(eta) = t.eta_seconds {
        parts.push(format!("{} left", format_eta(Some(eta))));
    } else if is_paused {
        parts.push("Paused".into());
    } else if t.status == TransferStatus::Queued {
        parts.push("Queued".into());
    } else if t.status == TransferStatus::Probing {
        parts.push("Probing...".into());
    } else if t.status == TransferStatus::Remuxing {
        parts.push("Remuxing...".into());
    }
    if compact {
        parts.truncate(2);
    }
    if is_torrent {
        parts.push(format!("{} peers", t.active_peers));
        parts.push(format!("↑ {}", format_speed(t.upload_speed_bps)));
    }

    let segment_tone = if is_paused {
        2
    } else if is_torrent {
        1
    } else {
        0
    };

    TransferRowModel {
        id: t.id.clone().into(),
        filename: t.filename.clone().into(),
        kind: match t.kind {
            TransferKind::Http => "http",
            TransferKind::Media => "media",
            TransferKind::Torrent => "torrent",
        }
        .into(),
        status: format!("{:?}", t.status).to_ascii_lowercase().into(),
        is_active,
        is_paused,
        is_completed,
        is_torrent,
        is_error,
        percent,
        progress_label: progress_label.into(),
        meta_line: parts.join(" • ").into(),
        source_domain: t.source_domain.clone().into(),
        protocol_label: t.protocol_label.clone().into(),
        tag_text: tag_text.into(),
        sha_verified: match t.sha256_verified {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        error_text: t.error_message.clone().unwrap_or_default().into(),
        segments: sync_row_segments(segments, t),
        segment_tone,
        icon_kind: classify_icon_kind(&t.filename, t.kind),
        can_pause_resume: !is_completed && t.status != TransferStatus::Canceled,
        playlist_index_text: t
            .playlist_index
            .map(|i| i.to_string())
            .unwrap_or_default()
            .into(),
        format_summary: t.format_tags.first().cloned().unwrap_or_default().into(),
    }
}

impl DesktopModels {
    pub fn update_inspector(
        &self,
        main: &MainWindow,
        selected: Option<&TransferSnapshot>,
        peers: Option<&TorrentPeerSummary>,
    ) {
        let previous = main.get_inspector();
        let inspector = self.build_inspector(selected, peers);
        if previous.id != inspector.id {
            main.set_context_menu_open(false);
        }
        if !inspector.is_torrent && main.get_inspector_tab() == 2 {
            main.set_inspector_tab(0);
        }
        if previous != inspector {
            main.set_inspector(inspector);
        }
    }

    fn build_inspector(
        &self,
        selected: Option<&TransferSnapshot>,
        peers: Option<&TorrentPeerSummary>,
    ) -> InspectorModel {
        let inspector = InspectorModel {
            segments: self.inspector_segments_model.clone().into(),
            torrent_files: self.inspector_files_model.clone().into(),
            peers: self.inspector_peers_model.clone().into(),
            ..InspectorModel::default()
        };
        let Some(t) = selected else {
            sync_vec_model_by(&self.inspector_segments_model, Vec::new(), PartialEq::eq);
            return inspector;
        };

        let (status_label, status_tone) = match t.status {
            TransferStatus::Active => ("Active", 1),
            TransferStatus::Probing => ("Probing", 1),
            TransferStatus::Remuxing => ("Remuxing", 1),
            TransferStatus::Seeding => ("Seeding", 2),
            TransferStatus::Completed => ("Completed", 2),
            TransferStatus::Paused => ("Paused", 0),
            TransferStatus::Queued => ("Queued", 0),
            TransferStatus::Failed => ("Failed", 3),
            TransferStatus::Canceled => ("Canceled", 0),
        };

        let seg_models: Vec<SegmentTileModel> = t
            .segments
            .iter()
            .take(16)
            .map(|s| {
                let (code, lbl) = match s.state {
                    SegmentState::Active => (0, "Active"),
                    SegmentState::Completed => (1, "Done"),
                    SegmentState::Paused => (2, "Paused"),
                    SegmentState::Split => (3, "Split"),
                    SegmentState::Error => (4, "Error"),
                    SegmentState::Pending => (5, "Pending"),
                };
                SegmentTileModel {
                    index_text: format!("{:02}", s.index).into(),
                    bytes_text: format_bytes(s.downloaded_bytes).into(),
                    state_code: code,
                    state_label: lbl.into(),
                }
            })
            .collect();

        let size_text = match t.total_bytes {
            Some(total) => format!(
                "{} / {}",
                format_bytes(t.downloaded_bytes),
                format_bytes(total)
            ),
            None => format_bytes(t.downloaded_bytes),
        };
        let (sha_text, sha_tone) = match t.sha256_verified {
            Some(true) => ("SHA-256 Verified".to_owned(), 2),
            Some(false) => ("SHA-256 Verification Failed".to_owned(), 3),
            None => (String::new(), 0),
        };
        let is_completed = t.status == TransferStatus::Completed;

        sync_vec_model_by(&self.inspector_segments_model, seg_models, PartialEq::eq);

        let is_torrent = t.kind == TransferKind::Torrent;
        let ratio = peers.map_or_else(
            || {
                if t.downloaded_bytes == 0 {
                    0.0
                } else {
                    t.uploaded_bytes as f64 / t.downloaded_bytes as f64
                }
            },
            |p| p.ratio,
        );
        InspectorModel {
            has_selection: true,
            id: t.id.clone().into(),
            filename: t.filename.clone().into(),
            source_url: t.source_url.clone().into(),
            source_domain: t.source_domain.clone().into(),
            save_dir: t.save_dir.clone().into(),
            protocol_label: t.protocol_label.clone().into(),
            status_label: status_label.into(),
            status_tone,
            kind: match t.kind {
                TransferKind::Http => "http",
                TransferKind::Media => "media",
                TransferKind::Torrent => "torrent",
            }
            .into(),
            is_media: t.kind == TransferKind::Media,
            is_torrent,
            is_paused: t.status == TransferStatus::Paused,
            is_completed,
            can_pause_resume: !is_completed && t.status != TransferStatus::Canceled,
            can_stop: can_stop(t),
            can_verify: t.kind == TransferKind::Http && is_completed,
            can_refresh_url: t.kind == TransferKind::Http && !is_completed,
            speed_text: if t.download_speed_bps > 0 {
                format_speed(t.download_speed_bps)
            } else {
                "--".into()
            }
            .into(),
            eta_text: format_eta(t.eta_seconds).into(),
            size_text: size_text.into(),
            connections_text: if t.kind == TransferKind::Torrent {
                format!("{} peers / {} conn", t.active_peers, t.active_connections)
            } else {
                format!("{} conn", t.active_connections)
            }
            .into(),
            upload_speed_text: format_speed(t.upload_speed_bps).into(),
            ratio_text: format!("{ratio:.2}").into(),
            pieces_text: peers
                .map(|p| format!("{}/{}", p.completed_pieces, p.total_pieces))
                .unwrap_or_else(|| "—".into())
                .into(),
            peers_summary_text: peers
                .map(peer_summary_text)
                .unwrap_or_else(|| format!("Live: {}", t.active_peers))
                .into(),
            trackers_text: peers
                .map(|p| p.trackers.join("\n"))
                .unwrap_or_default()
                .into(),
            sha_text: sha_text.into(),
            sha_tone,
            error_message: t.error_message.clone().unwrap_or_default().into(),
            completion_notice: t.completion_notice.clone().unwrap_or_default().into(),
            duration_text: String::new().into(),
            ..inspector
        }
    }

    pub fn update_inspector_files(
        &self,
        files: &[TorrentFileEntry],
        file_selection: Option<&HashSet<usize>>,
    ) {
        let torrent_rows: Vec<TorrentFileRowModel> = files
            .iter()
            .map(|f| TorrentFileRowModel {
                index: f.index as i32,
                relative_path: f.relative_path.clone().into(),
                size_text: format_bytes(f.size_bytes).into(),
                downloaded_text: format_bytes(f.downloaded_bytes).into(),
                selected: file_selection.map_or(f.selected, |indices| indices.contains(&f.index)),
            })
            .collect();

        sync_vec_model_by(&self.inspector_files_model, torrent_rows, PartialEq::eq);
    }

    pub fn update_inspector_peers(&self, peers: Option<&TorrentPeerSummary>) {
        sync_vec_model_by(
            &self.inspector_peers_model,
            peers.map(peer_rows).unwrap_or_default(),
            PartialEq::eq,
        );
    }

    pub fn apply_telemetry_snapshot(
        &mut self,
        main: &MainWindow,
        batch: &TelemetryBatch,
        speed_history: &[u64],
        expanded_playlists: &HashSet<String>,
        selected_id: &mut Option<String>,
    ) {
        let counts = compute_nav_counts(&batch.transfers);
        main.set_count_all(counts.all);
        main.set_count_active(counts.active);
        main.set_count_completed(counts.completed);
        main.set_count_queued(counts.queued);
        main.set_count_torrents(counts.torrents);
        main.set_count_paused(counts.paused);

        main.set_speed_down_text(format_speed(batch.aggregate_down_bps).into());
        main.set_speed_up_text(format_speed(batch.aggregate_up_bps).into());
        let summary = if batch.active_count > 0 {
            format!(
                "{} active • {} threads",
                batch.active_count, batch.active_threads
            )
        } else {
            "Idle".into()
        };
        main.set_active_summary_text(summary.into());

        let (line_cmd, fill_cmd) = view_model::build_sparkline_paths(speed_history, 180.0, 28.0);
        main.set_spark_line_cmd(line_cmd.into());
        main.set_spark_fill_cmd(fill_cmd.into());

        if let Some(limit) = batch.speed_limit_bps {
            main.set_speed_limit_label(format!("{}/s", format_bytes(limit)).into());
            main.set_speed_limit_active(true);
        } else {
            main.set_speed_limit_label("Unlimited".into());
            main.set_speed_limit_active(false);
        }

        let any_active_or_queued = batch.transfers.iter().any(|t| {
            matches!(
                t.status,
                TransferStatus::Active
                    | TransferStatus::Queued
                    | TransferStatus::Probing
                    | TransferStatus::Remuxing
            )
        });
        main.set_all_paused(!any_active_or_queued && counts.paused > 0);

        let nav = NavFilter::from_nav_id(&main.get_active_nav());
        let query = main.get_search_query().to_string().to_ascii_lowercase();
        let visible: Vec<&TransferSnapshot> = batch
            .transfers
            .iter()
            .filter(|t| matches_filter(t, nav, &query))
            .collect();

        let transfer_ids: HashSet<_> = batch.transfers.iter().map(|t| t.id.as_str()).collect();
        self.row_segment_models
            .retain(|id, _| transfer_ids.contains(id.as_str()));
        let playlist_ids: HashSet<_> = batch
            .transfers
            .iter()
            .filter_map(|t| t.playlist_id.as_deref())
            .collect();
        self.playlist_item_models
            .retain(|id, _| playlist_ids.contains(id.as_str()));

        // Group playlists vs standalone transfers
        let mut playlist_map: HashMap<String, Vec<&TransferSnapshot>> = HashMap::new();
        let mut active_rows = Vec::new();
        let mut completed_rows = Vec::new();
        for &t in &visible {
            if let Some(pid) = &t.playlist_id {
                playlist_map.entry(pid.clone()).or_default().push(t);
            } else if t.status == TransferStatus::Completed {
                completed_rows.push(CompletedRowModel {
                    id: t.id.clone().into(),
                    filename: t.filename.clone().into(),
                    source_domain: t.source_domain.clone().into(),
                    size_text: format_bytes(t.total_bytes.unwrap_or(t.downloaded_bytes)).into(),
                    tag_text: t.format_tags.first().cloned().unwrap_or_default().into(),
                    sha_verified: match t.sha256_verified {
                        Some(true) => 1,
                        Some(false) => 2,
                        None => 0,
                    },
                    icon_kind: classify_icon_kind(&t.filename, t.kind),
                });
            } else {
                active_rows.push(transfer_to_row(t, false, &mut self.row_segment_models));
            }
        }

        let mut playlist_groups: Vec<PlaylistGroupModel> = playlist_map
            .into_iter()
            .map(|(pid, mut items)| {
                items.sort_by_key(|t| t.playlist_index.unwrap_or(0));
                let title = items
                    .iter()
                    .find_map(|t| t.playlist_title.clone())
                    .unwrap_or_else(|| "Playlist Queue".into());
                let total = items
                    .first()
                    .and_then(|t| t.playlist_total)
                    .unwrap_or(items.len() as u32);
                let done_cnt = items
                    .iter()
                    .filter(|t| t.status == TransferStatus::Completed)
                    .count();
                let active_cnt = items
                    .iter()
                    .filter(|t| {
                        matches!(
                            t.status,
                            TransferStatus::Active
                                | TransferStatus::Probing
                                | TransferStatus::Remuxing
                        )
                    })
                    .count();
                let speed_sum: u64 = items.iter().map(|t| t.download_speed_bps).sum();
                let any_paused = items.iter().any(|t| t.status == TransferStatus::Paused);
                let has_pause_items = items.iter().any(|t| {
                    matches!(
                        t.status,
                        TransferStatus::Active
                            | TransferStatus::Queued
                            | TransferStatus::Probing
                            | TransferStatus::Remuxing
                    )
                });
                let mut summary_parts = vec![format!("{done_cnt}/{total} completed")];
                if active_cnt > 0 {
                    summary_parts.push(format!("{active_cnt} active"));
                }
                if speed_sum > 0 {
                    summary_parts.push(format_speed(speed_sum));
                }
                let row_models = items
                    .iter()
                    .map(|t| transfer_to_row(t, true, &mut self.row_segment_models))
                    .collect();
                let item_model = self.playlist_item_models.entry(pid.clone()).or_default();
                sync_vec_model_by(item_model, row_models, same_transfer_row);
                let pct_avg = if items.is_empty() {
                    0.0
                } else {
                    items
                        .iter()
                        .map(|t| format_transfer_percent(t).0)
                        .sum::<f32>()
                        / items.len() as f32
                };
                PlaylistGroupModel {
                    playlist_id: pid.clone().into(),
                    title: title.into(),
                    expanded: expanded_playlists.contains(&pid),
                    summary_line: summary_parts.join(" • ").into(),
                    overall_percent: pct_avg,
                    is_paused: any_paused && !has_pause_items,
                    has_pause_items,
                    has_resume_items: any_paused,
                    first_transfer_id: items
                        .first()
                        .map(|t| t.id.clone())
                        .unwrap_or_default()
                        .into(),
                    items: item_model.clone().into(),
                }
            })
            .collect();
        playlist_groups.sort_by(|a, b| {
            a.title
                .cmp(&b.title)
                .then(a.playlist_id.cmp(&b.playlist_id))
        });

        sync_vec_model_by(&self.playlist_groups_model, playlist_groups, PartialEq::eq);
        reconcile_vec_model_by_key(
            &self.active_rows_model,
            active_rows,
            |row| row.id.as_str(),
            same_transfer_row,
        );
        reconcile_vec_model_by_key(
            &self.completed_rows_model,
            completed_rows,
            |row| row.id.as_str(),
            PartialEq::eq,
        );

        let valid_selection = selected_id
            .as_deref()
            .and_then(|id| batch.transfers.iter().find(|t| t.id == id));
        let selected_transfer = valid_selection
            .or_else(|| visible.first().copied())
            .or_else(|| batch.transfers.first());
        *selected_id = selected_transfer.map(|t| t.id.clone());
        main.set_selected_id(selected_id.clone().unwrap_or_default().into());
    }
}

pub fn populate_confirm_preview(
    win: &DownloadConfirmWindow,
    preview: &PendingDownloadPreview,
    settings: &AppSettings,
    save_dir: &str,
    filename: &str,
    cat_idx: usize,
) {
    let cat_names: Vec<SharedString> = settings
        .categories
        .iter()
        .map(|c| c.name.clone().into())
        .collect();
    let cat_label = settings
        .categories
        .get(cat_idx)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "General".into());

    win.set_has_duplicate(preview.duplicate.is_some());
    win.set_is_duplicate_view(preview.duplicate.is_some());
    win.set_duplicate_filename(
        preview
            .duplicate
            .as_ref()
            .map(|d| d.filename.clone())
            .unwrap_or_else(|| preview.filename.clone())
            .into(),
    );
    win.set_url(preview.url.clone().into());
    win.set_source_domain(preview.source_domain.clone().into());
    win.set_size_text(
        preview
            .expected_size_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "Unknown size".into())
            .into(),
    );
    win.set_icon_kind(classify_icon_kind(filename, TransferKind::Http));
    win.set_category_names(ModelRc::from(Rc::new(VecModel::from(cat_names))));
    win.set_category_index(cat_idx as i32);
    win.set_category_label(cat_label.into());
    win.set_save_dir(save_dir.into());
    win.set_resolved_dir_hint(save_dir.into());
    win.set_filename(filename.into());
}

pub fn populate_progress_window(
    models: &ProgressModels,
    win: &DownloadProgressWindow,
    t: &TransferSnapshot,
    update_speed_fields: bool,
) {
    let (pct, _) = format_transfer_percent(t);
    win.set_title_bar_text(format!("{pct:.0}% {}", t.filename).into());
    win.set_url(t.source_url.clone().into());
    let status_text = match t.status {
        TransferStatus::Active => "Receiving data...",
        TransferStatus::Probing => "Probing stream...",
        TransferStatus::Remuxing => "Muxing media tracks...",
        TransferStatus::Paused => "Paused",
        TransferStatus::Queued => "Queued",
        TransferStatus::Completed => "Completed",
        TransferStatus::Failed => "Failed",
        TransferStatus::Seeding => "Seeding",
        TransferStatus::Canceled => "Canceled",
    };
    win.set_status_text(status_text.into());
    win.set_size_text(
        t.total_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "Unknown".into())
            .into(),
    );
    let downloaded_str = if let Some(total) = t.total_bytes.filter(|&v| v > 0) {
        format!(
            "{} ({:.2} %)",
            format_bytes(t.downloaded_bytes),
            (t.downloaded_bytes as f64 / total as f64) * 100.0
        )
    } else {
        format_bytes(t.downloaded_bytes)
    };
    win.set_downloaded_text(downloaded_str.into());
    win.set_speed_text(format_speed(t.download_speed_bps).into());
    win.set_eta_text(format_eta(t.eta_seconds).into());
    win.set_resume_text(if t.resumable { "Yes" } else { "No" }.into());
    let server_line = match t.connect_rtt_ms {
        Some(ms) => format!(
            "{} • {} • about {} ms",
            t.source_domain, t.protocol_label, ms
        ),
        None => format!("{} • {}", t.source_domain, t.protocol_label),
    };
    win.set_server_text(server_line.into());
    win.set_error_text(t.error_message.clone().unwrap_or_default().into());
    win.set_is_paused(t.status == TransferStatus::Paused);
    win.set_is_completed(t.status == TransferStatus::Completed);
    win.set_can_pause_resume(
        t.status != TransferStatus::Completed && t.status != TransferStatus::Canceled,
    );
    win.set_can_stop(can_stop(t));
    win.set_is_torrent(t.kind == TransferKind::Torrent);
    win.set_upload_speed_text(format_speed(t.upload_speed_bps).into());
    sync_vec_model_by(
        &models.segments_model,
        t.ui_segments.to_vec(),
        PartialEq::eq,
    );
    win.set_segment_tone(if t.status == TransferStatus::Paused {
        2
    } else if t.kind == TransferKind::Torrent {
        1
    } else {
        0
    });

    let conn_rows: Vec<ConnectionRowModel> = t
        .segments
        .iter()
        .map(|s| {
            let info = match s.state {
                SegmentState::Active => {
                    if s.speed_bps > 0 {
                        format!("Receiving data ({})", format_speed(s.speed_bps))
                    } else {
                        "Receiving data...".into()
                    }
                }
                SegmentState::Completed => "Done".into(),
                SegmentState::Paused => "Paused".into(),
                SegmentState::Split => "Split segment".into(),
                SegmentState::Error => "Error".into(),
                SegmentState::Pending => "Pending".into(),
            };
            ConnectionRowModel {
                num_text: (s.index as usize + 1).to_string().into(),
                downloaded_text: format_bytes(s.downloaded_bytes).into(),
                info_text: info.into(),
            }
        })
        .collect();
    sync_vec_model_by(&models.connections_model, conn_rows, PartialEq::eq);

    if update_speed_fields {
        if let Some(limit) = t.speed_limit_bps {
            win.set_speed_limiter_enabled(true);
            if limit >= 1_048_576 && limit % 1_048_576 == 0 {
                win.set_speed_limit_value((limit / 1_048_576).to_string().into());
                win.set_speed_limit_unit(1);
            } else {
                win.set_speed_limit_value((limit / 1024).max(1).to_string().into());
                win.set_speed_limit_unit(0);
            }
        } else {
            win.set_speed_limiter_enabled(false);
        }
    }
}

pub fn populate_complete_window(win: &DownloadCompleteWindow, t: &TransferSnapshot) -> PathBuf {
    let full_path = Path::new(&t.save_dir).join(&t.filename);
    win.set_filename(t.filename.clone().into());
    win.set_url(t.source_url.clone().into());
    win.set_size_text(format_bytes(t.total_bytes.unwrap_or(t.downloaded_bytes)).into());
    win.set_save_path(full_path.to_string_lossy().to_string().into());
    win.set_sha_verified(match t.sha256_verified {
        Some(true) => 1,
        Some(false) => 2,
        None => 0,
    });
    full_path
}

impl ControllerState {
    fn show_main_window(&mut self) {
        let _ = self.main.show();
        apply_native_window_chrome(self.main.window());
        win32::bring_window_to_front(hwnd_of(self.main.window()));
    }

    fn refresh_theme_from_settings(&mut self) {
        let (light, mono) = self
            .app
            .as_ref()
            .map(|a| resolve_theme_flags(&a.settings()))
            .unwrap_or((self.is_light, self.is_mono));
        self.apply_theme(light, mono);
    }

    fn apply_theme(&mut self, is_light: bool, is_mono: bool) {
        self.is_light = is_light;
        self.is_mono = is_mono;
        self.main.set_theme_light(is_light);
        self.main.set_theme_mono(is_mono);
        set_win_theme!(&self.main, is_light, is_mono);
        for entry in self.confirm_windows.values() {
            set_win_theme!(&entry.window, is_light, is_mono);
        }
        for entry in self.progress_windows.values() {
            set_win_theme!(&entry.window, is_light, is_mono);
        }
        for entry in self.complete_windows.values() {
            set_win_theme!(&entry.window, is_light, is_mono);
        }
    }

    fn show_toast(&mut self, message: impl Into<SharedString>, error: bool) {
        self.main.set_toast_text(message.into());
        self.main.set_toast_error(error);
        self.main.set_toast_visible(true);
        self.toast_timer
            .start(TimerMode::SingleShot, Duration::from_secs(4), || {
                with_controller(|ctrl| ctrl.main.set_toast_visible(false));
            });
    }

    fn transfer(&self, id: &str) -> Option<&TransferSnapshot> {
        self.last_batch
            .as_ref()
            .and_then(|b| b.transfers.iter().find(|t| t.id == id))
            .or_else(|| self.progress_windows.get(id).map(|entry| &entry.snapshot))
            .or_else(|| self.complete_windows.get(id).map(|entry| &entry.snapshot))
    }

    fn refresh_views(&mut self) {
        let Some(batch) = &self.last_batch else {
            return;
        };
        self.models.apply_telemetry_snapshot(
            &self.main,
            batch,
            self.speed_history.make_contiguous(),
            &self.expanded_playlists,
            &mut self.selected_id,
        );
        self.refresh_inspector();
        if let Some(id) = self.selected_id.clone() {
            if self
                .transfer(&id)
                .is_some_and(|t| t.kind == TransferKind::Torrent)
                && !self.torrent_files.contains_key(&id)
            {
                self.poll_torrent_files(id);
            }
        }
    }

    fn refresh_inspector(&self) {
        let id = self.selected_id.as_deref().unwrap_or_default();
        let peers = self.torrent_peers.get(id);
        if self.main.get_inspector().id != id {
            self.refresh_inspector_files();
            self.models.update_inspector_peers(peers);
        }
        self.models
            .update_inspector(&self.main, self.transfer(id), peers);
    }

    fn refresh_inspector_files(&self) {
        let id = self.selected_id.as_deref().unwrap_or_default();
        let files = self.torrent_files.get(id).map_or(&[][..], Vec::as_slice);
        self.models
            .update_inspector_files(files, self.torrent_file_selections.get(id));
        self.main
            .set_torrent_files_applying(self.torrent_file_apply_pending.contains(id));
    }

    fn apply_telemetry(&mut self, batch: TelemetryBatch) {
        if self
            .last_batch
            .as_ref()
            .is_some_and(|last| batch.timestamp < last.timestamp)
        {
            return;
        }
        if self.speed_history.len() >= MAX_SPEED_SAMPLES {
            self.speed_history.pop_front();
        }
        self.speed_history.push_back(batch.aggregate_down_bps);

        if self
            .last_disk_check
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
        {
            let dir = self
                .app
                .as_ref()
                .map(|a| PathBuf::from(a.settings().default_download_dir))
                .unwrap_or_else(default_data_dir);
            self.disk_space = win32::disk_space_info(&dir);
            self.last_disk_check = Some(Instant::now());
            if let Some((free, total)) = self.disk_space.filter(|&(_, total)| total > 0) {
                self.main
                    .set_disk_free_text(format!("{} free", format_bytes(free)).into());
                self.main.set_disk_used_ratio(
                    (total.saturating_sub(free) as f32 / total as f32).clamp(0.05, 1.0),
                );
            } else {
                self.main
                    .set_disk_free_text("Disk space unavailable".into());
                self.main.set_disk_used_ratio(0.0);
            }
        }

        let mut reveal_folders = Vec::new();
        let mut auto_close = Vec::new();
        for transfer in &batch.transfers {
            if let Some(entry) = self.progress_windows.get_mut(&transfer.id) {
                let was_completed = entry.window.get_is_completed();
                populate_progress_window(
                    &entry.models,
                    &entry.window,
                    transfer,
                    !entry.speed_dirty,
                );
                if transfer.kind == TransferKind::Torrent && !entry.window.get_show_details() {
                    entry
                        .window
                        .set_peers_summary_text(format!("Live: {}", transfer.active_peers).into());
                }
                entry.snapshot = transfer.clone();
                if !was_completed && entry.window.get_is_completed() {
                    if entry.window.get_opt_open_folder() {
                        reveal_folders.push(transfer.id.clone());
                    }
                    if entry.window.get_opt_auto_close() {
                        auto_close.push(transfer.id.clone());
                    }
                }
            }
        }

        let visible_ids: HashSet<_> = batch
            .transfers
            .iter()
            .map(|transfer| transfer.id.as_str())
            .collect();
        self.torrent_files
            .retain(|id, _| visible_ids.contains(id.as_str()));
        self.torrent_file_selections
            .retain(|id, _| visible_ids.contains(id.as_str()));
        self.torrent_padding_files
            .retain(|id, _| visible_ids.contains(id.as_str()));
        self.torrent_peers.retain(|id, _| {
            visible_ids.contains(id.as_str()) || self.progress_windows.contains_key(id)
        });
        let remove_id = self.main.get_remove_confirm_id();
        if self.main.get_remove_confirm_open()
            && !visible_ids.contains(remove_id.as_str())
            && !self.progress_windows.contains_key(remove_id.as_str())
            && !self.complete_windows.contains_key(remove_id.as_str())
        {
            self.main.set_remove_confirm_open(false);
        }
        self.last_batch = Some(batch);
        self.refresh_views();
        for id in reveal_folders {
            self.reveal_in_explorer(&id);
        }
        for id in auto_close {
            self.close_progress_dialog(&id);
        }
    }

    fn poll_torrent_files(&mut self, id: String) {
        let Some(app) = self.app.clone() else { return };
        if self.torrent_file_apply_pending.contains(&id)
            || self.torrent_files_pending.contains_key(&id)
        {
            return;
        }
        let requested_at = Instant::now();
        self.torrent_files_pending.insert(id.clone(), requested_at);
        self.runtime.spawn(async move {
            let result = app.list_torrent_files(&id).await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if ctrl.torrent_files_pending.get(&id) != Some(&requested_at) {
                        return;
                    }
                    ctrl.torrent_files_pending.remove(&id);
                    if ctrl.transfer(&id).is_none() {
                        return;
                    }
                    // A failed poll while metadata is resolving preserves the last successful list.
                    match result {
                        Ok(mut files) => {
                            if let Some(padding) = ctrl.torrent_padding_files.get(&id) {
                                files.retain(|file| !padding.contains(&file.index));
                            }
                            ctrl.torrent_files.insert(id.clone(), files);
                            if ctrl.selected_id.as_deref() == Some(&id) {
                                ctrl.refresh_inspector_files();
                            }
                        }
                        Err(_) => {
                            ctrl.torrent_files.entry(id).or_default();
                        }
                    }
                });
            });
        });
    }

    fn poll_torrent_details(&mut self) {
        let Some(app) = self.app.clone() else { return };
        let mut ids = HashSet::new();
        if let Some(id) = self.selected_id.clone() {
            if self
                .transfer(&id)
                .is_some_and(|t| t.kind == TransferKind::Torrent)
            {
                self.poll_torrent_files(id.clone());
                ids.insert(id);
            }
        }
        ids.extend(
            self.progress_windows
                .iter()
                .filter(|(_, entry)| {
                    entry.window.get_is_torrent() && entry.window.get_show_details()
                })
                .map(|(id, _)| id.clone()),
        );
        for id in ids {
            if !self.torrent_peers_pending.insert(id.clone()) {
                continue;
            }
            let app = app.clone();
            self.runtime.spawn(async move {
                let result = app.get_torrent_peers(&id).await;
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.torrent_peers_pending.remove(&id);
                        if let Ok(summary) = result {
                            if let Some(entry) = ctrl.progress_windows.get(&id) {
                                entry.models.update_peers(&entry.window, &summary);
                            }
                            if ctrl.transfer(&id).is_some() {
                                ctrl.torrent_peers.insert(id.clone(), summary);
                                if ctrl.selected_id.as_deref() == Some(&id) {
                                    ctrl.models
                                        .update_inspector_peers(ctrl.torrent_peers.get(&id));
                                    ctrl.refresh_inspector();
                                }
                            }
                        }
                    });
                });
            });
        }
    }

    fn close_add_dialog(&mut self) {
        self.clear_add_probe_ui();
        self.add_state = AddDialogRuntime {
            generation: self.add_state.generation.wrapping_add(1),
            ..AddDialogRuntime::default()
        };
        self.main.set_add_open(false);
        self.main.set_add_submitting(false);
        if let Some(event) = self.pending_picker_requests.pop_front() {
            self.open_add_dialog_with_seed(event);
        }
    }

    fn open_add_dialog_with_seed(&mut self, event: OpenPickerEvent) {
        const MAX_PENDING_PICKER_REQUESTS: usize = 16;

        if self.main.get_add_open() {
            if event.url.as_deref().unwrap_or_default() != self.main.get_add_source_text().as_str()
            {
                if self.pending_picker_requests.len() < MAX_PENDING_PICKER_REQUESTS {
                    self.pending_picker_requests.push_back(event);
                    let waiting = self.pending_picker_requests.len();
                    self.show_toast(
                        format!("Queued download dialog request ({waiting} waiting)"),
                        false,
                    );
                } else {
                    self.show_toast("The download dialog queue is full.", true);
                }
            }
            self.show_main_window();
            return;
        }
        self.main.set_settings_open(false);
        self.main.set_palette_open(false);
        self.main.set_refresh_open(false);
        self.main.set_verify_open(false);
        self.main.set_remove_confirm_open(false);
        self.main.set_context_menu_open(false);
        self.main.set_speed_popover_open(false);
        self.main.set_add_tab(0);
        self.main.set_add_torrent_start_paused(false);
        self.main.set_add_filename_override("".into());
        self.main.set_add_expected_sha256("".into());

        if let Some(app) = &self.app {
            let s = app.settings();
            self.main.set_add_save_dir(s.default_download_dir.into());
            let idx = match s.default_connections {
                24..=32 => 0,
                12..=23 => 1,
                6..=11 => 2,
                _ => 3,
            };
            self.main.set_add_threads_index(idx);
        }

        if let Some(url) = event.url {
            if let Some(origin) = url_origin(&url) {
                self.add_state.seeded_origin = Some(origin);
                self.add_state.seeded_headers = event.headers;
            }
            self.main.set_add_source_text(url.clone().into());
            self.update_add_source_classification(&url);
        } else {
            self.main.set_add_source_text("".into());
            self.update_add_source_classification("");
        }
        self.add_state.source = self.main.get_add_source_text().to_string();
        self.main.set_add_open(true);
        self.show_main_window();
        let source = self.main.get_add_source_text();
        if is_torrent_input(&source) {
            self.trigger_torrent_preview();
        } else if should_auto_probe(&source) {
            self.trigger_probe();
        }
    }

    fn clear_add_probe_ui(&mut self) {
        self.cancel_torrent_preview();
        self.add_state.torrent_preview = None;
        self.add_state.torrent_preview_token = None;
        self.add_state.checked_preview_files.clear();
        self.main.set_has_torrent_preview(false);
        self.main.set_add_torrent_preview_filter("".into());
        sync_vec_model_by(&self.torrent_preview_files_model, Vec::new(), PartialEq::eq);
        self.add_state.probe = None;
        self.add_state.selected_format_id = None;
        self.add_state.checked_playlist.clear();
        self.add_state.checked_subtitles.clear();
        self.main.set_add_probing(false);
        self.main.set_add_error_text("".into());
        self.main.set_add_has_probe(false);
        self.main.set_add_is_playlist(false);
        self.main.set_add_format_options(ModelRc::default());
        self.main.set_add_subtitles(ModelRc::default());
        self.main.set_add_playlist_items(ModelRc::default());
    }

    fn handle_add_source_changed(&mut self, source: &str) {
        if self.add_state.source == source {
            return;
        }
        self.add_state.source = source.to_owned();
        self.add_state.generation = self.add_state.generation.wrapping_add(1);
        self.clear_add_probe_ui();
        if should_clear_seeded_headers(self.add_state.seeded_origin.as_deref(), source) {
            self.add_state.seeded_headers.clear();
            self.add_state.seeded_origin = None;
        }
        self.update_add_source_classification(source);
        if should_auto_preview(source) {
            self.trigger_torrent_preview();
        }
    }

    fn update_add_source_classification(&mut self, source: &str) {
        let torrent = is_torrent_input(source);
        if torrent {
            self.main.set_add_tab(3);
        } else if self.main.get_add_is_torrent_input() {
            self.main.set_add_tab(0);
        }
        self.main.set_add_is_torrent_input(torrent);
        let batch_n = wildcard_batch(source).map_or(0, |urls| urls.len());
        self.main.set_add_batch_preview_count(batch_n as i32);
        let direct = !torrent && batch_n == 0 && looks_like_direct_file_url(source);
        self.main.set_add_is_direct_http(direct);
        let hint = if torrent {
            "BitTorrent engine"
        } else if batch_n > 0 {
            "Wildcard parallel batch"
        } else {
            "16 HTTP threads"
        };
        self.main.set_add_footer_hint(hint.into());
    }

    fn open_confirm_dialog(&mut self, pending_id: &str) {
        let Some(app) = &self.app else { return };
        let Ok(preview) = app.get_pending_download(pending_id) else {
            return;
        };
        let settings = app.settings();
        if let Some(existing) = self.confirm_windows.get(pending_id) {
            let _ = existing.window.show();
            foreground_dialog(existing.window.window(), existing.cascade);
            return;
        }
        let Ok(win) = DownloadConfirmWindow::new() else {
            return;
        };
        set_win_theme!(&win, self.is_light, self.is_mono);

        let cat_idx = settings
            .categories
            .iter()
            .position(|c| c.id == preview.suggested_category_id)
            .unwrap_or(0);
        let cat_id = settings
            .categories
            .get(cat_idx)
            .map(|c| c.id.clone())
            .unwrap_or_else(|| "general".into());
        let save_dir = preview.suggested_save_dir.clone();
        let filename = preview.filename.clone();
        populate_confirm_preview(&win, &preview, &settings, &save_dir, &filename, cat_idx);

        let pid = pending_id.to_owned();
        let w_weak = win.as_weak();
        win.on_start_drag(move || {
            if let Some(w) = w_weak.upgrade() {
                drag_slint_window(w.window());
            }
        });

        let w_weak = win.as_weak();
        win.on_minimize_window(move || {
            if let Some(w) = w_weak.upgrade() {
                w.window().with_winit_window(|ww| ww.set_minimized(true));
            }
        });

        let pid_cancel = pid.clone();
        win.on_cancel_clicked(move || {
            with_controller(|ctrl| {
                if let Some(app) = &ctrl.app {
                    app.cancel_pending_download(&pid_cancel);
                } else {
                    ctrl.close_confirm_dialog(&pid_cancel);
                }
            });
        });

        let pid_close = pid.clone();
        win.window().on_close_requested(move || {
            let mut keep_open = false;
            with_controller(|ctrl| {
                if ctrl
                    .confirm_windows
                    .get(&pid_close)
                    .is_some_and(|e| e.submitting)
                {
                    keep_open = true;
                    return;
                }
                if let Some(app) = &ctrl.app {
                    app.cancel_pending_download(&pid_close);
                }
                ctrl.confirm_windows.remove(&pid_close);
            });
            if keep_open {
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                slint::CloseRequestResponse::HideWindow
            }
        });

        let pid_dup = pid.clone();
        win.on_duplicate_ok_clicked(move || {
            with_controller(|ctrl| ctrl.handle_duplicate_ok(&pid_dup));
        });

        let pid_cat = pid.clone();
        win.on_category_selected(move |idx| {
            with_controller(|ctrl| ctrl.handle_confirm_category_changed(&pid_cat, idx as usize));
        });

        let pid_add = pid.clone();
        win.on_add_category_toggled(move || {
            with_controller(|ctrl| {
                if let Some(entry) = ctrl.confirm_windows.get(&pid_add) {
                    let cur = entry.window.get_creating_category();
                    entry.window.set_creating_category(!cur);
                }
            });
        });

        let pid_new_cat = pid.clone();
        win.on_save_new_category_clicked(move |name, exts| {
            with_controller(|ctrl| {
                ctrl.handle_create_category(&pid_new_cat, name.into(), exts.into())
            });
        });

        let pid_browse = pid.clone();
        win.on_browse_dir_clicked(move || {
            let mut owner_opt = None;
            let mut cur_opt = None;
            with_controller(|ctrl| {
                if let Some(entry) = ctrl.confirm_windows.get(&pid_browse) {
                    owner_opt = Some(hwnd_of(entry.window.window()));
                    cur_opt = Some(entry.window.get_save_dir().to_string());
                }
            });
            if let (Some(owner), Some(cur)) = (owner_opt, cur_opt) {
                let pid_back = pid_browse.clone();
                std::thread::spawn(move || {
                    if let Ok(Some(chosen)) = win32::pick_download_folder(owner, Some(cur)) {
                        let _ = slint::invoke_from_event_loop(move || {
                            with_controller(|ctrl| {
                                if let Some(entry) = ctrl.confirm_windows.get(&pid_back) {
                                    entry.window.set_save_dir(chosen.clone().into());
                                    entry.window.set_resolved_dir_hint(chosen.into());
                                }
                            });
                        });
                    }
                });
            }
        });

        let pid_later = pid.clone();
        win.on_download_later_clicked(move || {
            with_controller(|ctrl| ctrl.submit_confirm(&pid_later, false));
        });

        let pid_start = pid.clone();
        win.on_start_download_clicked(move || {
            with_controller(|ctrl| ctrl.submit_confirm(&pid_start, true));
        });

        let _ = win.show();
        let cascade = self.confirm_windows.len() as i32;
        foreground_dialog(win.window(), cascade);

        self.confirm_windows.insert(
            pending_id.to_owned(),
            ConfirmWindowState {
                window: win,
                cascade,
                selected_category_id: cat_id,
                overwrite_existing: false,
                submitting: false,
            },
        );
    }

    fn close_confirm_dialog(&mut self, pending_id: &str) {
        if let Some(entry) = self.confirm_windows.remove(pending_id) {
            let _ = entry.window.hide();
        }
    }

    fn handle_duplicate_ok(&mut self, pending_id: &str) {
        let Some(entry) = self.confirm_windows.get_mut(pending_id) else {
            return;
        };
        let choice = entry.window.get_duplicate_choice();
        match choice {
            0 => {
                // Numbered filename
                let dir = PathBuf::from(entry.window.get_save_dir().to_string());
                let cur_name = entry.window.get_filename().to_string();
                let next_name = next_numbered_filename(&dir, &cur_name);
                entry.overwrite_existing = false;
                entry.window.set_filename(next_name.into());
                entry.window.set_is_duplicate_view(false);
            }
            1 => {
                // Overwrite
                entry.overwrite_existing = true;
                entry.window.set_is_duplicate_view(false);
            }
            _ => {
                // Resume or show complete dialog
                let Some(app) = &self.app else { return };
                entry.window.set_submitting(true);
                entry.window.set_error_text("".into());
                let app_c = app.clone();
                let pid = pending_id.to_owned();
                self.runtime.spawn(async move {
                    let res = app_c.resolve_duplicate_existing(&pid).await;
                    let _ = slint::invoke_from_event_loop(move || {
                        with_controller(|ctrl| match res {
                            Ok(_) => ctrl.close_confirm_dialog(&pid),
                            Err(err) => {
                                if let Some(e) = ctrl.confirm_windows.get(&pid) {
                                    e.window.set_submitting(false);
                                    e.window.set_error_text(err.into());
                                }
                            }
                        });
                    });
                });
            }
        }
    }

    fn handle_confirm_category_changed(&mut self, pending_id: &str, idx: usize) {
        let Some(app) = &self.app else { return };
        let settings = app.settings();
        if let Some(cat) = settings.categories.get(idx) {
            let resolved = resolve_category_dir(&settings, &cat.id);
            let dir_str = resolved.to_string_lossy().to_string();
            if let Some(entry) = self.confirm_windows.get_mut(pending_id) {
                entry.selected_category_id = cat.id.clone();
                entry.window.set_category_index(idx as i32);
                entry.window.set_category_label(cat.name.clone().into());
                entry.window.set_save_dir(dir_str.clone().into());
                entry.window.set_resolved_dir_hint(dir_str.into());
            }
        }
    }

    fn handle_create_category(&mut self, pending_id: &str, name: String, exts_raw: String) {
        let Some(app) = &self.app else { return };
        let trimmed = name.trim().to_owned();
        if trimmed.is_empty() {
            if let Some(e) = self.confirm_windows.get(pending_id) {
                e.window.set_error_text("Enter a category name.".into());
            }
            return;
        }
        let exts: Vec<String> = exts_raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let save_dir = self
            .confirm_windows
            .get(pending_id)
            .map(|e| e.window.get_save_dir().to_string())
            .filter(|s| !s.trim().is_empty());

        let app_c = app.clone();
        let pid = pending_id.to_owned();
        self.runtime.spawn(async move {
            let res = app_c.add_download_category(trimmed, exts, save_dir).await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| match res {
                    Ok(cat) => {
                        let settings = ctrl.app.as_ref().map(|a| a.settings()).unwrap_or_default();
                        let cat_idx = settings
                            .categories
                            .iter()
                            .position(|c| c.id == cat.id)
                            .unwrap_or(0);
                        let cat_names: Vec<SharedString> = settings
                            .categories
                            .iter()
                            .map(|c| c.name.clone().into())
                            .collect();
                        if let Some(entry) = ctrl.confirm_windows.get_mut(&pid) {
                            entry.selected_category_id = cat.id;
                            entry.window.set_category_names(ModelRc::from(Rc::new(
                                VecModel::from(cat_names),
                            )));
                            entry.window.set_category_index(cat_idx as i32);
                            entry.window.set_category_label(cat.name.into());
                            entry.window.set_creating_category(false);
                            entry.window.set_new_category_name("".into());
                            entry.window.set_new_category_extensions("".into());
                            entry.window.set_error_text("".into());
                        }
                    }
                    Err(err) => {
                        if let Some(entry) = ctrl.confirm_windows.get(&pid) {
                            entry.window.set_error_text(err.into());
                        }
                    }
                });
            });
        });
    }

    fn submit_confirm(&mut self, pending_id: &str, start_immediately: bool) {
        let Some(app) = &self.app else { return };
        let Some(entry) = self.confirm_windows.get_mut(pending_id) else {
            return;
        };
        let req = ConfirmPendingDownloadRequest {
            pending_id: pending_id.to_owned(),
            filename: entry.window.get_filename().to_string(),
            save_dir: entry.window.get_save_dir().to_string(),
            category_id: entry.selected_category_id.clone(),
            remember_category_path: entry.window.get_remember_category(),
            start_immediately,
            overwrite_existing: entry.overwrite_existing,
            description: {
                let d = entry.window.get_description().to_string();
                let trimmed = d.trim().to_owned();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            },
        };
        entry.submitting = true;
        entry.window.set_submitting(true);
        entry.window.set_error_text("".into());
        let app_c = app.clone();
        let pid = pending_id.to_owned();
        self.runtime.spawn(async move {
            let res = app_c.confirm_pending_download(req).await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| match res {
                    Ok(_) => {
                        if let Some(entry) = ctrl.confirm_windows.get_mut(&pid) {
                            entry.submitting = false;
                        }
                        ctrl.close_confirm_dialog(&pid);
                    }
                    Err(err) => {
                        if let Some(entry) = ctrl.confirm_windows.get_mut(&pid) {
                            entry.submitting = false;
                            entry.window.set_submitting(false);
                            entry.window.set_error_text(err.into());
                        }
                    }
                });
            });
        });
    }

    fn open_transfer_dialog(&mut self, transfer_id: &str, dialog: TransferDialog) {
        match dialog {
            TransferDialog::Progress => {
                if let Some(entry) = self.progress_windows.get(transfer_id) {
                    let _ = entry.window.show();
                    foreground_dialog(entry.window.window(), entry.cascade);
                    return;
                }
            }
            TransferDialog::Complete => {
                if let Some(entry) = self.complete_windows.get(transfer_id) {
                    let _ = entry.window.show();
                    foreground_dialog(entry.window.window(), entry.cascade);
                    return;
                }
            }
        }
        if let Some(transfer) = self
            .last_batch
            .as_ref()
            .and_then(|batch| batch.transfers.iter().find(|t| t.id == transfer_id))
            .cloned()
        {
            self.show_transfer_dialog(dialog, transfer);
            return;
        }
        let Some(app) = self.app.clone() else { return };
        let id = transfer_id.to_owned();
        if !self.pending_transfer_dialogs.insert((id.clone(), dialog)) {
            return;
        }
        self.runtime.spawn(async move {
            let result = app.get_transfer_snapshot(&id).await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if !ctrl.pending_transfer_dialogs.remove(&(id.clone(), dialog)) {
                        return;
                    }
                    match result {
                        Ok(snapshot) => {
                            let snapshot = ctrl
                                .last_batch
                                .as_ref()
                                .and_then(|batch| batch.transfers.iter().find(|t| t.id == id))
                                .cloned()
                                .unwrap_or(snapshot);
                            ctrl.show_transfer_dialog(dialog, snapshot);
                        }
                        Err(error) => {
                            ctrl.show_toast(format!("Could not load transfer: {error}"), true)
                        }
                    }
                });
            });
        });
    }

    fn show_transfer_dialog(&mut self, dialog: TransferDialog, transfer: TransferSnapshot) {
        match dialog {
            TransferDialog::Progress => self.show_progress_dialog(transfer),
            TransferDialog::Complete => self.show_complete_dialog(transfer),
        }
    }

    fn open_progress_dialog(&mut self, transfer_id: &str) {
        self.open_transfer_dialog(transfer_id, TransferDialog::Progress);
    }

    fn show_progress_dialog(&mut self, transfer: TransferSnapshot) {
        let transfer_id = transfer.id.as_str();
        if let Some(entry) = self.progress_windows.get_mut(transfer_id) {
            populate_progress_window(&entry.models, &entry.window, &transfer, !entry.speed_dirty);
            entry.snapshot = transfer;
            let _ = entry.window.show();
            foreground_dialog(entry.window.window(), entry.cascade);
            return;
        }
        let Ok(win) = DownloadProgressWindow::new() else {
            return;
        };
        set_win_theme!(&win, self.is_light, self.is_mono);

        let models = ProgressModels::default();
        models.attach(&win);
        populate_progress_window(&models, &win, &transfer, true);
        win.set_peers_summary_text(format!("Live: {}", transfer.active_peers).into());
        if let Some(summary) = self.torrent_peers.get(transfer_id) {
            models.update_peers(&win, summary);
        }

        let tid = transfer_id.to_owned();
        let w_weak = win.as_weak();
        win.on_start_drag(move || {
            if let Some(w) = w_weak.upgrade() {
                drag_slint_window(w.window());
            }
        });
        let w_weak = win.as_weak();
        win.on_minimize_window(move || {
            if let Some(w) = w_weak.upgrade() {
                w.window().with_winit_window(|ww| ww.set_minimized(true));
            }
        });
        let tid_close = tid.clone();
        win.on_close_clicked(move || {
            with_controller(|ctrl| ctrl.close_progress_dialog(&tid_close));
        });
        let tid_close2 = tid.clone();
        win.window().on_close_requested(move || {
            with_controller(|ctrl| {
                ctrl.progress_windows.remove(&tid_close2);
            });
            slint::CloseRequestResponse::HideWindow
        });
        let tid_toggle = tid.clone();
        win.on_toggle_pause_clicked(move || {
            with_controller(|ctrl| {
                let paused = ctrl
                    .progress_windows
                    .get(&tid_toggle)
                    .is_some_and(|e| e.window.get_is_paused());
                ctrl.toggle_transfer_pause(&tid_toggle, paused);
            });
        });
        let tid_stop = tid.clone();
        win.on_stop_transfer_clicked(move || {
            with_controller(|ctrl| ctrl.toggle_transfer_pause(&tid_stop, false));
        });
        let tid_cancel = tid.clone();
        win.on_cancel_transfer_clicked(move || {
            with_controller(|ctrl| ctrl.request_remove_transfer(&tid_cancel));
        });
        let tid_det = tid.clone();
        win.on_toggle_details_clicked(move || {
            with_controller(|ctrl| {
                if let Some(e) = ctrl.progress_windows.get(&tid_det) {
                    let cur = e.window.get_show_details();
                    e.window.set_show_details(!cur);
                }
            });
        });
        let tid_dirty = tid.clone();
        win.on_speed_input_dirty(move || {
            with_controller(|ctrl| {
                if let Some(e) = ctrl.progress_windows.get_mut(&tid_dirty) {
                    e.speed_dirty = true;
                    e.speed_edit_revision = e.speed_edit_revision.wrapping_add(1);
                }
            });
        });
        let tid_spd = tid.clone();
        win.on_apply_speed_limit(move |enabled, val, unit| {
            with_controller(|ctrl| {
                let Some(app) = ctrl.app.clone() else { return };
                let limit = if !enabled {
                    Ok(None)
                } else {
                    parse_speed_limit_bps(&val, unit == 1)
                };
                let bps = match limit {
                    Ok(bps) => bps,
                    Err(error) => {
                        ctrl.show_main_window();
                        ctrl.show_toast(error, true);
                        return;
                    }
                };
                let Some(entry) = ctrl.progress_windows.get(&tid_spd) else {
                    return;
                };
                let revision = entry.speed_edit_revision;
                let id = tid_spd.clone();
                ctrl.runtime.spawn(async move {
                    let result = app.set_transfer_speed_limit(&id, bps).await;
                    let batch = if result.is_ok() {
                        Some(app.snapshot().await)
                    } else {
                        None
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        with_controller(|ctrl| match result {
                            Ok(()) => {
                                if let Some(entry) = ctrl.progress_windows.get_mut(&id) {
                                    if entry.speed_edit_revision == revision {
                                        entry.speed_dirty = false;
                                    }
                                }
                                if let Some(batch) = batch {
                                    ctrl.apply_telemetry(batch);
                                }
                            }
                            Err(error) => {
                                ctrl.show_main_window();
                                ctrl.show_toast(
                                    format!("Could not set speed limit: {error}"),
                                    true,
                                );
                            }
                        });
                    });
                });
            });
        });
        let tid_opt = tid.clone();
        win.on_completion_options_changed(move |show_c, auto_c, open_f| {
            with_controller(|ctrl| {
                let Some(app) = ctrl.app.clone() else { return };
                let app_c = app.clone();
                let id_c = tid_opt.clone();
                let opts = TransferCompletionOptions {
                    show_complete_dialog: show_c,
                    auto_close_progress: auto_c,
                    open_folder_on_complete: open_f,
                };
                ctrl.runtime.spawn(async move {
                    let _ = app_c.set_transfer_completion_options(&id_c, opts).await;
                });
            });
        });
        let tid_open = tid.clone();
        win.on_open_file_clicked(move || {
            with_controller(|ctrl| ctrl.open_completed_file(&tid_open));
        });
        let tid_folder = tid.clone();
        win.on_open_folder_clicked(move || {
            with_controller(|ctrl| ctrl.reveal_in_explorer(&tid_folder));
        });

        let _ = win.show();
        let cascade = self.progress_windows.len() as i32;
        foreground_dialog(win.window(), cascade);

        if let Some(app) = &self.app {
            let app_c = app.clone();
            let id_c = transfer_id.to_owned();
            self.runtime.spawn(async move {
                if let Ok(opts) = app_c.get_transfer_completion_options(&id_c).await {
                    let _ = slint::invoke_from_event_loop(move || {
                        with_controller(|ctrl| {
                            if let Some(e) = ctrl.progress_windows.get(&id_c) {
                                e.window.set_opt_show_complete(opts.show_complete_dialog);
                                e.window.set_opt_auto_close(opts.auto_close_progress);
                                e.window.set_opt_open_folder(opts.open_folder_on_complete);
                                e.window.set_options_loading(false);
                            }
                        });
                    });
                }
            });
        }

        self.progress_windows.insert(
            transfer_id.to_owned(),
            ProgressWindowState {
                window: win,
                cascade,
                models,
                snapshot: transfer,
                speed_edit_revision: 0,
                speed_dirty: false,
            },
        );
    }

    fn close_progress_dialog(&mut self, transfer_id: &str) {
        self.pending_transfer_dialogs
            .remove(&(transfer_id.to_owned(), TransferDialog::Progress));
        if let Some(entry) = self.progress_windows.remove(transfer_id) {
            let _ = entry.window.hide();
        }
    }

    fn open_complete_dialog(&mut self, transfer_id: &str) {
        self.open_transfer_dialog(transfer_id, TransferDialog::Complete);
    }

    fn show_complete_dialog(&mut self, transfer: TransferSnapshot) {
        let transfer_id = transfer.id.as_str();
        if let Some(entry) = self.complete_windows.get_mut(transfer_id) {
            populate_complete_window(&entry.window, &transfer);
            entry.snapshot = transfer;
            let _ = entry.window.show();
            foreground_dialog(entry.window.window(), entry.cascade);
            return;
        }
        let Ok(win) = DownloadCompleteWindow::new() else {
            return;
        };
        set_win_theme!(&win, self.is_light, self.is_mono);
        populate_complete_window(&win, &transfer);

        let tid = transfer_id.to_owned();
        let w_weak = win.as_weak();
        win.on_start_drag(move || {
            if let Some(w) = w_weak.upgrade() {
                drag_slint_window(w.window());
            }
        });
        let tid_open = tid.clone();
        win.on_open_clicked(move || {
            with_controller(|ctrl| ctrl.open_completed_file(&tid_open));
        });
        let tid_with = tid.clone();
        win.on_open_with_clicked(move || {
            with_controller(|ctrl| ctrl.open_completed_file_with(&tid_with));
        });
        let tid_dir = tid.clone();
        win.on_open_folder_clicked(move || {
            with_controller(|ctrl| ctrl.reveal_in_explorer(&tid_dir));
        });
        let tid_close = tid.clone();
        win.on_close_clicked(move || {
            with_controller(|ctrl| {
                if let Some(entry) = ctrl.complete_windows.remove(&tid_close) {
                    let _ = entry.window.hide();
                }
            });
        });
        let tid_x = tid.clone();
        win.window().on_close_requested(move || {
            with_controller(|ctrl| {
                ctrl.complete_windows.remove(&tid_x);
            });
            slint::CloseRequestResponse::HideWindow
        });

        let _ = win.show();
        let cascade = self.complete_windows.len() as i32;
        foreground_dialog(win.window(), cascade);
        self.complete_windows.insert(
            transfer_id.to_owned(),
            CompleteWindowState {
                window: win,
                cascade,
                snapshot: transfer,
            },
        );
    }

    fn dispatch_transfer_action(
        &self,
        action: &'static str,
        refresh: bool,
        work: impl Future<Output = Result<(), String>> + Send + 'static,
    ) {
        let app = self.app.clone();
        self.runtime.spawn(async move {
            let result = work.await;
            let batch = match app {
                Some(app) if refresh && result.is_ok() => Some(app.snapshot().await),
                _ => None,
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| match result {
                    Ok(()) => {
                        if let Some(batch) = batch {
                            ctrl.apply_telemetry(batch);
                        }
                    }
                    Err(error) => {
                        ctrl.show_main_window();
                        ctrl.show_toast(format!("Could not {action}: {error}"), true);
                    }
                });
            });
        });
    }

    fn set_all_paused(&self, pause: bool) {
        let Some(app) = self.app.clone() else { return };
        self.dispatch_transfer_action("change transfer state", true, async move {
            app.change_all(pause).await;
            Ok(())
        });
    }

    fn toggle_transfer_pause(&mut self, id: &str, currently_paused: bool) {
        let Some(app) = self.app.clone() else { return };
        let id = id.to_owned();
        let action = if currently_paused {
            "resume transfer"
        } else {
            "pause transfer"
        };
        self.dispatch_transfer_action(action, true, async move {
            if currently_paused {
                app.resume_transfer(&id).await
            } else {
                app.pause_transfer(&id).await
            }
        });
    }

    fn request_remove_transfer(&mut self, id: &str) {
        let Some(transfer) = self.transfer(id) else {
            return;
        };
        self.main
            .set_remove_confirm_filename(transfer.filename.clone().into());
        self.main.set_remove_confirm_id(id.into());
        self.main.set_context_menu_open(false);
        self.main.set_remove_confirm_open(true);
        self.show_main_window();
        self.main.invoke_focus_remove_confirm();
    }

    fn confirm_remove_transfer(&mut self, id: String, delete_files: bool) {
        if !self.main.get_remove_confirm_open() || self.main.get_remove_confirm_id() != id.as_str()
        {
            return;
        }
        self.main.set_remove_confirm_open(false);
        let Some(app) = self.app.clone() else { return };
        self.runtime.spawn(async move {
            let result = match app.cancel_transfer(&id, delete_files).await {
                Ok(()) => Ok(app.snapshot().await),
                Err(error) => Err(error),
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| match result {
                    Ok(batch) => {
                        ctrl.close_progress_dialog(&id);
                        ctrl.pending_transfer_dialogs
                            .remove(&(id.clone(), TransferDialog::Complete));
                        if let Some(entry) = ctrl.complete_windows.remove(&id) {
                            let _ = entry.window.hide();
                        }
                        ctrl.apply_telemetry(batch);
                        ctrl.show_toast("Transfer removed", false);
                    }
                    Err(error) => {
                        ctrl.show_toast(format!("Could not remove transfer: {error}"), true)
                    }
                });
            });
        });
    }

    fn copy_transfer_url(&mut self, id: &str) {
        let Some(transfer) = self.transfer(id) else {
            return;
        };
        match win32::copy_to_clipboard(&transfer.source_url) {
            Ok(()) => {
                self.last_clipboard_seq = win32::clipboard_sequence_number();
                self.show_toast("Link copied to clipboard", false);
            }
            Err(error) => self.show_toast(format!("Could not copy link: {error}"), true),
        }
    }

    fn open_completed_file(&mut self, id: &str) {
        let Some(app) = self.app.clone() else { return };
        let id = id.to_owned();
        self.dispatch_transfer_action("open file", false, async move {
            app.open_completed_file(&id).await
        });
    }

    fn open_completed_file_with(&mut self, id: &str) {
        let Some(app) = self.app.clone() else { return };
        let id = id.to_owned();
        self.dispatch_transfer_action("open file with another app", false, async move {
            app.open_completed_file_with(&id).await
        });
    }

    fn reveal_in_explorer(&mut self, id: &str) {
        let Some(app) = self.app.clone() else { return };
        let id = id.to_owned();
        self.dispatch_transfer_action("open folder", false, async move {
            app.reveal_in_explorer(&id).await
        });
    }

    fn cancel_torrent_preview(&mut self) {
        if let Some(token) = self.add_state.torrent_preview_cancel.take() {
            token.cancel();
        }
        self.main.set_add_torrent_probing(false);
    }

    fn trigger_torrent_preview(&mut self) {
        let Some(app) = self.app.clone() else { return };
        let source = self.main.get_add_source_text().trim().to_owned();
        if !is_torrent_input(&source)
            || self.main.get_add_torrent_probing()
            || self.main.get_add_submitting()
        {
            return;
        }
        let save_dir = self.main.get_add_save_dir().trim().to_owned();
        let expected_gen = self.add_state.generation;
        let cancel = CancellationToken::new();
        self.add_state.torrent_preview_cancel = Some(cancel.clone());
        self.main.set_add_torrent_probing(true);
        self.main.set_add_error_text("".into());
        self.runtime.spawn(async move {
            let dir = (!save_dir.is_empty()).then_some(save_dir.as_str());
            let result = app.preview_torrent(&source, dir, cancel.clone()).await;
            let magnet_selection = result
                .as_ref()
                .ok()
                .filter(|preview| preview.is_magnet)
                .and_then(|_| parse_magnet_uri(&source).ok())
                .and_then(|magnet| magnet.select_only)
                .map(|indices| indices.into_iter().collect::<HashSet<_>>());
            let disk_free = if result.is_ok() {
                let path = dir
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(app.settings().default_download_dir));
                win32::disk_space_info(&path).map(|(free, _)| free)
            } else {
                None
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if cancel.is_cancelled() || expected_gen != ctrl.add_state.generation {
                        return;
                    }
                    ctrl.add_state.torrent_preview_cancel = None;
                    ctrl.main.set_add_torrent_probing(false);
                    match result {
                        Ok(mut preview) => {
                            // Preview metadata marks selectable files before any user selection.
                            preview.files.retain(|file| file.selected);
                            let same_torrent = ctrl
                                .add_state
                                .torrent_preview
                                .as_ref()
                                .is_some_and(|previous| previous.info_hash == preview.info_hash);
                            ctrl.add_state.checked_preview_files = preview
                                .files
                                .iter()
                                .filter(|file| {
                                    if same_torrent {
                                        return ctrl
                                            .add_state
                                            .checked_preview_files
                                            .contains(&file.index);
                                    }
                                    magnet_selection
                                        .as_ref()
                                        .is_none_or(|indices| indices.contains(&file.index))
                                })
                                .map(|file| file.index)
                                .collect();
                            ctrl.add_state.torrent_preview_token = preview.preview_token.clone();
                            ctrl.add_state.preview_disk_free = disk_free;
                            ctrl.main
                                .set_add_torrent_preview_name(preview.name.clone().into());
                            ctrl.main
                                .set_add_torrent_preview_trackers(preview.trackers.len() as i32);
                            ctrl.add_state.torrent_preview = Some(preview);
                            ctrl.refresh_torrent_preview();
                            ctrl.main.set_has_torrent_preview(true);
                        }
                        Err(error) => ctrl.main.set_add_error_text(
                            format!("Could not inspect torrent: {error}").into(),
                        ),
                    }
                });
            });
        });
    }

    fn refresh_torrent_preview(&self) {
        let Some(preview) = &self.add_state.torrent_preview else {
            return;
        };
        let filter = self
            .main
            .get_add_torrent_preview_filter()
            .trim()
            .to_lowercase();
        let checked = &self.add_state.checked_preview_files;
        let selected: u64 = preview
            .files
            .iter()
            .filter(|file| checked.contains(&file.index))
            .map(|file| file.size_bytes)
            .sum();
        let free = self
            .add_state
            .preview_disk_free
            .map(|free| format!(" • {} free", format_bytes(free)))
            .unwrap_or_default();
        self.main.set_add_torrent_preview_meta(
            format!(
                "{} • {} files • {} / {} selected{free}",
                preview.version,
                preview.files.len(),
                format_bytes(selected),
                format_bytes(preview.total_bytes),
            )
            .into(),
        );
        let rows = preview
            .files
            .iter()
            .filter(|file| filter.is_empty() || file.relative_path.to_lowercase().contains(&filter))
            .map(|file| TorrentPreviewFileModel {
                index: file.index as i32,
                relative_path: file.relative_path.clone().into(),
                size_text: format_bytes(file.size_bytes).into(),
                checked: checked.contains(&file.index),
            })
            .collect();
        sync_vec_model_by(&self.torrent_preview_files_model, rows, PartialEq::eq);
    }

    fn select_preview_files(&mut self, selected: bool) {
        self.add_state.checked_preview_files.clear();
        if selected {
            if let Some(preview) = &self.add_state.torrent_preview {
                self.add_state
                    .checked_preview_files
                    .extend(preview.files.iter().map(|file| file.index));
            }
        }
        self.refresh_torrent_preview();
    }

    fn trigger_probe(&mut self) {
        let Some(app) = &self.app else { return };
        let source = self.main.get_add_source_text().to_string();
        let trimmed = source.trim().to_owned();
        if trimmed.is_empty()
            || is_torrent_input(&trimmed)
            || self.main.get_add_probing()
            || self.main.get_add_submitting()
        {
            return;
        }
        if !looks_like_http_url(&trimmed) {
            self.main
                .set_add_error_text("Enter a valid http:// or https:// URL.".into());
            return;
        }
        if should_clear_seeded_headers(self.add_state.seeded_origin.as_deref(), &trimmed) {
            self.add_state.seeded_headers.clear();
            self.add_state.seeded_origin = None;
        }
        let expected_gen = self.add_state.generation;
        self.main.set_add_probing(true);
        self.main.set_add_error_text("".into());
        let headers = self.add_state.seeded_headers.clone();
        let app_c = app.clone();
        self.runtime.spawn(async move {
            let res = app_c
                .probe_media_with_headers(&trimmed, None, &headers)
                .await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| ctrl.apply_probe_result(expected_gen, res));
            });
        });
    }

    fn apply_probe_result(&mut self, expected_gen: u64, res: Result<MediaProbeResult, String>) {
        if expected_gen != self.add_state.generation || !self.main.get_add_open() {
            return;
        }
        self.main.set_add_probing(false);
        match res {
            Ok(probe) => {
                let video_opts = video_format_options(&probe);
                let default_fmt = video_opts.first().map(|f| f.format_id.clone());
                self.add_state.selected_format_id = default_fmt.clone();
                let fmt_models: Vec<FormatOptionModel> = video_opts
                    .iter()
                    .map(|f| FormatOptionModel {
                        format_id: f.format_id.clone().into(),
                        title: f.label.clone().into(),
                        detail: format_option_detail(f).into(),
                        selected: default_fmt.as_deref() == Some(f.format_id.as_str()),
                    })
                    .collect();

                self.add_state.checked_subtitles.clear();
                let sub_models: Vec<SubtitleItemModel> = probe
                    .subtitles
                    .iter()
                    .map(|s| SubtitleItemModel {
                        lang: s.lang.clone().into(),
                        label: format!("{} ({})", s.name, s.lang).into(),
                        is_auto: s.is_auto,
                        checked: false,
                    })
                    .collect();

                self.add_state.checked_playlist =
                    probe.playlist_items.iter().map(|i| i.index).collect();
                let pl_models: Vec<PlaylistItemModel> = probe
                    .playlist_items
                    .iter()
                    .map(|i| PlaylistItemModel {
                        index: i.index as i32,
                        title: i.title.clone().into(),
                        duration_text: format_duration(i.duration_seconds).into(),
                        checked: true,
                    })
                    .collect();

                let dur = format_duration(probe.duration_seconds);
                let meta = if dur.is_empty() {
                    probe.extractor.clone()
                } else {
                    format!("{} • {}", probe.extractor, dur)
                };
                self.main.set_add_has_probe(true);
                self.main.set_add_is_direct_http(false);
                self.main.set_add_probe_title(probe.title.clone().into());
                self.main.set_add_probe_meta(meta.into());
                self.main.set_add_is_playlist(probe.is_playlist);
                self.main.set_add_playlist_count(
                    probe.playlist_count.unwrap_or(pl_models.len() as u32) as i32,
                );
                self.main
                    .set_add_playlist_selected_count(self.add_state.checked_playlist.len() as i32);
                self.main
                    .set_add_playlist_folder_name(probe.title.clone().into());
                self.main
                    .set_add_playlist_items(ModelRc::from(Rc::new(VecModel::from(pl_models))));
                self.main
                    .set_add_format_options(ModelRc::from(Rc::new(VecModel::from(fmt_models))));
                self.main
                    .set_add_subtitles(ModelRc::from(Rc::new(VecModel::from(sub_models))));
                self.add_state.probe = Some(probe);
            }
            Err(err) => {
                self.main.set_add_error_text(err.into());
            }
        }
    }

    fn refresh_playlist_items_ui(&mut self) {
        let Some(probe) = &self.add_state.probe else {
            return;
        };
        let filter = self
            .main
            .get_add_playlist_filter()
            .to_string()
            .to_ascii_lowercase();
        let models: Vec<PlaylistItemModel> = probe
            .playlist_items
            .iter()
            .filter(|item| filter.is_empty() || item.title.to_ascii_lowercase().contains(&filter))
            .map(|item| PlaylistItemModel {
                index: item.index as i32,
                title: item.title.clone().into(),
                duration_text: format_duration(item.duration_seconds).into(),
                checked: self.add_state.checked_playlist.contains(&item.index),
            })
            .collect();
        self.main
            .set_add_playlist_selected_count(self.add_state.checked_playlist.len() as i32);
        self.main
            .set_add_playlist_items(ModelRc::from(Rc::new(VecModel::from(models))));
    }

    fn submit_add_download(&mut self) {
        let Some(app) = &self.app else { return };
        if self.main.get_add_submitting() {
            return;
        }
        let source = self
            .main
            .get_add_source_text()
            .to_string()
            .trim()
            .to_owned();
        if source.is_empty() {
            self.main
                .set_add_error_text("Enter a URL, magnet link, or .torrent path.".into());
            return;
        }
        let tab = self.main.get_add_tab();
        let torrent = tab == 3 || is_torrent_input(&source);
        if torrent && self.main.get_add_torrent_probing() {
            self.main
                .set_add_error_text("Wait for torrent inspection to finish or cancel it.".into());
            return;
        }
        let save_dir = {
            let s = self.main.get_add_save_dir().to_string().trim().to_owned();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let max_conn = match self.main.get_add_threads_index() {
            0 => 32,
            2 => 8,
            3 => 4,
            _ => 16,
        };
        let filename = {
            let f = self
                .main
                .get_add_filename_override()
                .to_string()
                .trim()
                .to_owned();
            if f.is_empty() {
                None
            } else {
                Some(f)
            }
        };
        let sha = {
            let s = self
                .main
                .get_add_expected_sha256()
                .to_string()
                .trim()
                .to_owned();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        if should_clear_seeded_headers(self.add_state.seeded_origin.as_deref(), &source) {
            self.add_state.seeded_headers.clear();
            self.add_state.seeded_origin = None;
        }
        let headers = self.add_state.seeded_headers.clone();
        let expected_gen = self.add_state.generation;

        self.main.set_add_submitting(true);
        self.main.set_add_error_text("".into());
        let app_c = app.clone();

        if torrent {
            let selected_file_indices = if self.add_state.torrent_preview.is_none() {
                None
            } else {
                let mut indices: Vec<_> = self
                    .add_state
                    .checked_preview_files
                    .iter()
                    .copied()
                    .collect();
                if indices.is_empty() {
                    self.main.set_add_submitting(false);
                    self.main
                        .set_add_error_text("Select at least one file to download.".into());
                    return;
                }
                indices.sort_unstable();
                Some(indices)
            };
            let token = self.add_state.torrent_preview_token.clone();
            let request = AddTorrentRequest {
                source,
                save_dir,
                selected_file_indices,
                sequential: self.main.get_add_torrent_sequential(),
                start_paused: self.main.get_add_torrent_start_paused(),
            };
            self.runtime.spawn(async move {
                let res = app_c.add_torrent_with_preview(request, token).await;
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.finish_add_submission(expected_gen, res.map(|_| ()))
                    });
                });
            });
            return;
        }

        if let Some(urls) = wildcard_batch(&source) {
            self.runtime.spawn(async move {
                let mut err_msg = None;
                for url in urls {
                    if let Err(e) = app_c
                        .add_http_download(AddHttpDownloadRequest {
                            url,
                            filename: None,
                            save_dir: save_dir.clone(),
                            max_connections: Some(max_conn),
                            queue_id: None,
                            headers: headers.clone(),
                            referrer: None,
                            expected_sha256: None,
                            expected_size_bytes: None,
                            start_paused: false,
                            overwrite_existing: false,
                        })
                        .await
                    {
                        err_msg = Some(e);
                        break;
                    }
                }
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.finish_add_submission(expected_gen, err_msg.map_or(Ok(()), Err));
                    });
                });
            });
            return;
        }

        let use_media = self.add_state.probe.is_some()
            || tab == 1
            || tab == 2
            || !looks_like_direct_file_url(&source);

        if use_media && !self.main.get_add_is_direct_http() {
            let format_id = if tab == 0 {
                self.add_state.probe.as_ref().and_then(|probe| {
                    let sel = self.add_state.selected_format_id.as_deref()?;
                    let opt = probe.formats.iter().find(|f| f.format_id == sel)?;
                    Some(pair_video_with_best_audio(opt, &probe.formats))
                })
            } else {
                None
            };
            let audio_only_preset = if tab == 1 {
                Some(match self.main.get_add_audio_preset_index() {
                    1 => AudioPreset::M4aAac,
                    2 => AudioPreset::Opus,
                    3 => AudioPreset::Flac,
                    4 => AudioPreset::Wav,
                    _ => AudioPreset::Mp3_320,
                })
            } else {
                None
            };
            let target_container = if tab == 0 && self.add_state.probe.is_some() {
                Some(match self.main.get_add_container_index() {
                    1 => MediaContainer::Mkv,
                    2 => MediaContainer::Webm,
                    _ => MediaContainer::Mp4,
                })
            } else {
                None
            };
            let embed_subtitles_langs: Vec<String> = if self.main.get_add_embed_subtitles() {
                self.add_state.checked_subtitles.iter().cloned().collect()
            } else {
                Vec::new()
            };
            let (download_playlist, selected_entries, playlist_folder_name) =
                if let Some(probe) = &self.add_state.probe {
                    if probe.is_playlist {
                        let sel: Vec<SelectedPlaylistEntry> = probe
                            .playlist_items
                            .iter()
                            .filter(|i| self.add_state.checked_playlist.contains(&i.index))
                            .map(|i| SelectedPlaylistEntry {
                                index: i.index,
                                title: i.title.clone(),
                                url: i.url.clone(),
                            })
                            .collect();
                        let folder = self.main.get_add_playlist_folder_name().to_string();
                        (
                            true,
                            Some(sel),
                            if folder.trim().is_empty() {
                                None
                            } else {
                                Some(folder)
                            },
                        )
                    } else {
                        (false, None, None)
                    }
                } else {
                    (false, None, None)
                };

            let req = AddMediaDownloadRequest {
                url: source,
                format_id,
                audio_only_preset,
                target_container,
                embed_subtitles_langs,
                lossless_remux: self.main.get_add_lossless_remux(),
                normalize_audio: self.main.get_add_normalize_audio(),
                save_dir,
                filename,
                playlist_items: None,
                download_playlist,
                max_height: None,
                selected_entries,
                playlist_folder_name,
                headers,
            };
            self.runtime.spawn(async move {
                let res = app_c.add_media_download(req).await;
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.finish_add_submission(expected_gen, res.map(|_| ()))
                    });
                });
            });
            return;
        }

        let req = AddHttpDownloadRequest {
            url: source,
            filename,
            save_dir,
            max_connections: Some(max_conn),
            queue_id: None,
            headers,
            referrer: None,
            expected_sha256: sha,
            expected_size_bytes: None,
            start_paused: false,
            overwrite_existing: false,
        };
        self.runtime.spawn(async move {
            let res = app_c.add_http_download(req).await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| ctrl.finish_add_submission(expected_gen, res.map(|_| ())));
            });
        });
    }

    fn finish_add_submission(&mut self, expected_gen: u64, res: Result<(), String>) {
        if expected_gen != self.add_state.generation {
            return;
        }
        self.main.set_add_submitting(false);
        match res {
            Ok(()) => self.close_add_dialog(),
            Err(err) => {
                self.main.set_add_error_text(err.into());
            }
        }
    }

    fn open_settings_dialog(&mut self) {
        if let Some(app) = &self.app {
            let s = app.settings();
            self.main.set_set_theme_index(match s.theme {
                ThemeMode::System => 0,
                ThemeMode::Dark => 1,
                ThemeMode::Light => 2,
            });
            self.main.set_set_accent_index(match s.accent {
                AccentMode::Blue => 0,
                AccentMode::Mono => 1,
            });
            self.main
                .set_set_default_download_dir(s.default_download_dir.into());
            self.main
                .set_set_default_connections(s.default_connections.to_string().into());
            self.main
                .set_set_max_concurrent(s.max_concurrent_downloads.to_string().into());
            self.main.set_set_speed_limit_mbps(
                s.global_speed_limit_bps
                    .map(|bps| {
                        format!("{:.2}", bps as f64 / 1_048_576.0)
                            .trim_end_matches('0')
                            .trim_end_matches('.')
                            .to_owned()
                    })
                    .unwrap_or_default()
                    .into(),
            );
            self.main.set_set_close_to_tray(s.close_to_tray_hibernate);
            self.main.set_set_auto_defender_scan(s.auto_defender_scan);
            self.main.set_set_monitor_clipboard(s.monitor_clipboard);
            self.main
                .set_set_show_download_dialog(s.show_download_dialog);
            self.main
                .set_set_show_progress_dialog(s.show_progress_dialog);
            self.main
                .set_set_show_complete_dialog(s.show_complete_dialog);
            self.main.set_set_update_mode_index(match s.update_mode {
                UpdateMode::Auto => 0,
                UpdateMode::Manual => 1,
            });
            self.main
                .set_set_ytdlp_override(s.ytdlp_path.unwrap_or_default().into());
            self.main
                .set_set_ffmpeg_override(s.ffmpeg_path.unwrap_or_default().into());
            self.main.set_set_error_text("".into());
            self.refresh_toolchain_and_browsers();
        }
        self.main.set_settings_open(true);
    }

    fn refresh_toolchain_and_browsers(&mut self) {
        let Some(app) = &self.app else { return };
        let browsers = list_browsers(app.registry_base_key(), app.resource_dir());
        let b_models: Vec<BrowserCardModel> = browsers
            .into_iter()
            .map(|b| BrowserCardModel {
                id: match b.id {
                    BrowserKind::Chrome => "chrome",
                    BrowserKind::Edge => "edge",
                    BrowserKind::Brave => "brave",
                    BrowserKind::Firefox => "firefox",
                }
                .into(),
                name: b.name.into(),
                installed: b.installed,
                native_host_registered: b.native_host_registered,
                extension_id: b.extension_id.into(),
                bundled_extension_dir: b.bundled_extension_dir.unwrap_or_default().into(),
            })
            .collect();
        self.main
            .set_set_browsers(ModelRc::from(Rc::new(VecModel::from(b_models))));

        let app_c = app.clone();
        self.runtime.spawn(async move {
            let status = app_c.get_toolchain_status().await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| ctrl.apply_toolchain_status(status));
            });
        });
    }

    fn apply_toolchain_status(&mut self, st: ToolchainStatus) {
        let map_src = |src: ToolSource| -> (&'static str, i32) {
            match src {
                ToolSource::Missing => ("Missing", 3),
                ToolSource::Custom => ("Custom", 1),
                ToolSource::Managed => ("Managed", 2),
                ToolSource::Bundled => ("Bundled", 2),
                ToolSource::SystemPath => ("System", 0),
            }
        };
        let (y_l, y_t) = map_src(st.ytdlp_source);
        self.main.set_set_ytdlp_status_label(y_l.into());
        self.main.set_set_ytdlp_status_tone(y_t);
        self.main
            .set_set_ytdlp_version(st.ytdlp_version.unwrap_or_default().into());
        self.main
            .set_set_ytdlp_path(st.ytdlp_path.unwrap_or_default().into());

        let (f_l, f_t) = map_src(st.ffmpeg_source);
        self.main.set_set_ffmpeg_status_label(f_l.into());
        self.main.set_set_ffmpeg_status_tone(f_t);
        self.main
            .set_set_ffmpeg_version(st.ffmpeg_version.unwrap_or_default().into());
        self.main
            .set_set_ffmpeg_path(st.ffmpeg_path.unwrap_or_default().into());

        let (p_l, p_t) = map_src(st.ffprobe_source);
        self.main.set_set_ffprobe_status_label(p_l.into());
        self.main.set_set_ffprobe_status_tone(p_t);
        self.main
            .set_set_ffprobe_path(st.ffprobe_path.unwrap_or_default().into());
    }

    fn save_settings(&mut self) {
        let Some(app) = &self.app else { return };
        let mut s = app.settings();
        s.theme = match self.main.get_set_theme_index() {
            0 => ThemeMode::System,
            2 => ThemeMode::Light,
            _ => ThemeMode::Dark,
        };
        s.accent = if self.main.get_set_accent_index() == 1 {
            AccentMode::Mono
        } else {
            AccentMode::Blue
        };
        s.default_download_dir = self.main.get_set_default_download_dir().to_string();
        s.default_connections = self
            .main
            .get_set_default_connections()
            .to_string()
            .trim()
            .parse::<u8>()
            .unwrap_or(16)
            .clamp(1, 32);
        s.max_concurrent_downloads = self
            .main
            .get_set_max_concurrent()
            .to_string()
            .trim()
            .parse::<u8>()
            .unwrap_or(4)
            .clamp(1, 32);
        s.global_speed_limit_bps =
            match parse_speed_limit_slider_mbps(&self.main.get_set_speed_limit_mbps()) {
                Ok(bps) => bps,
                Err(msg) => {
                    self.main.set_set_error_text(msg.into());
                    return;
                }
            };
        s.close_to_tray_hibernate = self.main.get_set_close_to_tray();
        s.auto_defender_scan = self.main.get_set_auto_defender_scan();
        s.monitor_clipboard = self.main.get_set_monitor_clipboard();
        s.show_download_dialog = self.main.get_set_show_download_dialog();
        s.show_progress_dialog = self.main.get_set_show_progress_dialog();
        s.show_complete_dialog = self.main.get_set_show_complete_dialog();
        s.update_mode = if self.main.get_set_update_mode_index() == 1 {
            UpdateMode::Manual
        } else {
            UpdateMode::Auto
        };
        let y_ov = self.main.get_set_ytdlp_override().to_string();
        s.ytdlp_path = if y_ov.trim().is_empty() {
            None
        } else {
            Some(y_ov)
        };
        let f_ov = self.main.get_set_ffmpeg_override().to_string();
        s.ffmpeg_path = if f_ov.trim().is_empty() {
            None
        } else {
            Some(f_ov)
        };

        if s.monitor_clipboard {
            self.last_clipboard_seq = win32::clipboard_sequence_number();
        }

        self.main.set_set_saving(true);
        self.main.set_set_error_text("".into());
        let app_c = app.clone();
        self.runtime.spawn(async move {
            let res = app_c.save_settings(s).await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    ctrl.main.set_set_saving(false);
                    match res {
                        Ok(()) => {
                            ctrl.main.set_settings_open(false);
                            ctrl.refresh_theme_from_settings();
                        }
                        Err(err) => ctrl.main.set_set_error_text(err.into()),
                    }
                });
            });
        });
    }

    fn check_updates(&mut self, startup: bool) {
        let Some(app) = &self.app else { return };
        self.main.set_set_update_checking(true);
        self.main.set_set_update_error("".into());
        if !startup {
            self.main
                .set_set_update_status_text("Checking GitHub releases...".into());
        }
        let app_c = app.clone();
        self.runtime.spawn(async move {
            let res = updater::check_for_updates(crate::desktop_version(), startup, &app_c).await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    ctrl.main.set_set_update_checking(false);
                    match res {
                        Ok(Some(status)) => {
                            ctrl.main.set_set_update_available(status.available);
                            if status.available {
                                let v = status.version.unwrap_or_default();
                                ctrl.main
                                    .set_set_update_version_label(format!("v{v}").into());
                                ctrl.main
                                    .set_set_update_notes(status.notes.unwrap_or_default().into());
                                ctrl.main.set_set_update_status_text(
                                    format!("SlipStream v{v} is ready to install.").into(),
                                );
                            } else if !startup {
                                ctrl.main.set_set_update_status_text(
                                    format!(
                                        "SlipStream v{} is up to date.",
                                        status.current_version
                                    )
                                    .into(),
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            if !startup {
                                ctrl.main.set_set_update_error(
                                    friendly_updater_error("check for updates", err).into(),
                                );
                            }
                        }
                    }
                });
            });
        });
    }

    fn install_update(&mut self) {
        let Some(app) = &self.app else { return };
        self.main.set_set_update_installing(true);
        self.main.set_set_update_error("".into());
        let app_c = app.clone();
        self.runtime.spawn(async move {
            let res = updater::install_update(
                app_c.clone(),
                crate::desktop_version(),
                |prog: UpdateProgress| {
                    let _ = slint::invoke_from_event_loop(move || {
                        with_controller(|ctrl| {
                            let pct = match prog.total_bytes {
                                Some(tot) if tot > 0 => {
                                    (prog.downloaded_bytes as f32 / tot as f32).clamp(0.0, 1.0)
                                }
                                _ => 0.0,
                            };
                            let stage_label = match prog.stage {
                                UpdateProgressStage::Downloading => "Downloading installer",
                                UpdateProgressStage::Verifying => "Verifying Minisign signature",
                                UpdateProgressStage::Installing => "Launching silent installer",
                            };
                            ctrl.main.set_set_update_progress_percent(pct);
                            ctrl.main.set_set_update_progress_text(
                                format!("{stage_label}: {}", format_bytes(prog.downloaded_bytes))
                                    .into(),
                            );
                        });
                    });
                },
            )
            .await;
            match res {
                Ok(true) => {
                    app_c.quit().await;
                }
                Ok(false) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        with_controller(|ctrl| ctrl.main.set_set_update_installing(false));
                    });
                }
                Err(err) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        with_controller(|ctrl| {
                            ctrl.main.set_set_update_installing(false);
                            ctrl.main.set_set_update_error(err.into());
                        });
                    });
                }
            }
        });
    }

    fn update_palette_actions(&mut self) {
        let q = self
            .main
            .get_palette_query()
            .to_string()
            .to_ascii_lowercase();
        let all = [
            ("add", "Add Download", "Ctrl+N"),
            ("pause_all", "Pause / Resume All Transfers", ""),
            ("toggle_theme", "Toggle Light / Dark Theme", ""),
            ("settings", "Open Settings", ""),
            ("check_updates", "Check for Updates", ""),
            ("hibernate", "Hibernate to System Tray", ""),
        ];
        let models: Vec<PaletteActionModel> = all
            .into_iter()
            .filter(|(_, lbl, _)| q.is_empty() || lbl.to_ascii_lowercase().contains(&q))
            .map(|(id, lbl, hint)| PaletteActionModel {
                id: id.into(),
                label: lbl.into(),
                hint: hint.into(),
            })
            .collect();
        self.main
            .set_palette_actions(ModelRc::from(Rc::new(VecModel::from(models))));
        self.main.set_palette_selected_index(0);
    }
}

fn wire_main_callbacks(main: &MainWindow) {
    let m = main.as_weak();
    main.on_start_drag(move || {
        if let Some(win) = m.upgrade() {
            drag_slint_window(win.window());
        }
    });

    let m = main.as_weak();
    main.on_start_resize(move |direction| {
        let direction = match direction {
            0 => ResizeDirection::North,
            1 => ResizeDirection::South,
            2 => ResizeDirection::West,
            3 => ResizeDirection::East,
            4 => ResizeDirection::NorthWest,
            5 => ResizeDirection::NorthEast,
            6 => ResizeDirection::SouthWest,
            7 => ResizeDirection::SouthEast,
            _ => return,
        };
        if let Some(win) = m.upgrade() {
            win.window().with_winit_window(|w| {
                if let Err(error) = w.drag_resize_window(direction) {
                    tracing::debug!(%error, "window resize could not start");
                }
            });
        }
    });

    let m = main.as_weak();
    main.on_minimize_window(move || {
        if let Some(win) = m.upgrade() {
            win.window().with_winit_window(|w| w.set_minimized(true));
        }
    });

    let m = main.as_weak();
    main.on_toggle_maximize_window(move || {
        with_controller(|ctrl| {
            if let Some(win) = m.upgrade() {
                ctrl.is_maximized = toggle_maximize_slint_window(win.window(), ctrl.is_maximized);
                ctrl.main.set_is_maximized(ctrl.is_maximized);
            }
        });
    });

    main.on_close_window(|| {
        with_controller(|ctrl| {
            let hibernate = ctrl
                .app
                .as_ref()
                .is_some_and(|a| a.settings().close_to_tray_hibernate);
            if hibernate && !ctrl.demo_mode {
                let _ = ctrl.main.hide();
            } else if let Some(app) = &ctrl.app {
                let app_c = app.clone();
                ctrl.runtime.spawn(async move {
                    app_c.quit().await;
                });
            } else {
                let _ = slint::quit_event_loop();
            }
        });
    });

    main.window().on_close_requested(|| {
        let mut hide = false;
        let mut demo_quit = false;
        with_controller(|ctrl| {
            if let Some(app) = &ctrl.app {
                if app.settings().close_to_tray_hibernate && !app.is_quitting() {
                    hide = true;
                } else {
                    let app_c = app.clone();
                    ctrl.runtime.spawn(async move {
                        app_c.quit().await;
                    });
                }
            } else {
                demo_quit = true;
            }
        });
        if demo_quit {
            let _ = slint::quit_event_loop();
        }
        let _ = hide;
        slint::CloseRequestResponse::HideWindow
    });

    main.on_nav_selected(|nav| {
        with_controller(|ctrl| {
            ctrl.main.set_active_nav(nav);
            ctrl.refresh_views();
        });
    });

    main.on_search_changed(|_| {
        with_controller(|ctrl| {
            ctrl.refresh_views();
        });
    });

    main.on_toggle_theme_clicked(|| {
        with_controller(|ctrl| {
            let next_light = !ctrl.is_light;
            ctrl.apply_theme(next_light, ctrl.is_mono);
            if let Some(app) = &ctrl.app {
                let mut s = app.settings();
                s.theme = if next_light {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                };
                let app_c = app.clone();
                ctrl.runtime.spawn(async move {
                    let _ = app_c.save_settings(s).await;
                });
            }
        });
    });

    main.on_toggle_pause_all_clicked(|| {
        with_controller(|ctrl| ctrl.set_all_paused(!ctrl.main.get_all_paused()));
    });

    main.on_open_add_dialog_clicked(|| {
        with_controller(|ctrl| {
            ctrl.open_add_dialog_with_seed(OpenPickerEvent {
                url: None,
                headers: HashMap::new(),
            });
        });
    });

    main.on_open_settings_clicked(|| {
        with_controller(|ctrl| ctrl.open_settings_dialog());
    });

    main.on_open_palette_clicked(|| {
        with_controller(|ctrl| {
            ctrl.main.set_palette_query("".into());
            ctrl.update_palette_actions();
            ctrl.main.set_palette_open(true);
        });
    });

    main.on_apply_speed_limit_clicked(|raw| {
        with_controller(|ctrl| {
            let Some(app) = ctrl.app.clone() else { return };
            match parse_speed_limit_slider_mbps(&raw) {
                Ok(bps) => {
                    ctrl.dispatch_transfer_action("set global speed limit", true, async move {
                        app.set_speed_limit(bps).await
                    })
                }
                Err(error) => ctrl.show_toast(error, true),
            }
        });
    });

    main.on_select_transfer(|id| {
        with_controller(|ctrl| {
            ctrl.selected_id = Some(id.into());
            ctrl.refresh_views();
        });
    });

    main.on_toggle_transfer_pause(|id, paused| {
        with_controller(|ctrl| ctrl.toggle_transfer_pause(&id, paused));
    });

    main.on_open_floating_progress(|id| {
        with_controller(|ctrl| ctrl.open_progress_dialog(&id));
    });

    main.on_open_context_menu(|id, x, y| {
        with_controller(|ctrl| {
            ctrl.selected_id = Some(id.clone().into());
            ctrl.refresh_views();
            if ctrl.main.get_selected_id() == id {
                ctrl.main.set_context_menu_x(x);
                ctrl.main.set_context_menu_y(y);
                ctrl.main.set_context_menu_open(true);
            }
        });
    });

    main.on_stop_transfer(|id| {
        with_controller(|ctrl| ctrl.toggle_transfer_pause(&id, false));
    });

    main.on_request_remove_transfer(|id| {
        with_controller(|ctrl| ctrl.request_remove_transfer(&id));
    });

    main.on_confirm_remove_transfer(|id, delete_files| {
        with_controller(|ctrl| ctrl.confirm_remove_transfer(id.into(), delete_files));
    });

    main.on_copy_transfer_url(|id| {
        with_controller(|ctrl| ctrl.copy_transfer_url(&id));
    });

    main.on_open_completed_file(|id| {
        with_controller(|ctrl| ctrl.open_completed_file(&id));
    });

    main.on_reveal_in_explorer(|id| {
        with_controller(|ctrl| ctrl.reveal_in_explorer(&id));
    });

    main.on_open_refresh_dialog(|id| {
        with_controller(|ctrl| {
            let tid = id.to_string();
            let filename = ctrl
                .last_batch
                .as_ref()
                .and_then(|b| b.transfers.iter().find(|t| t.id == tid))
                .map(|t| t.filename.clone())
                .unwrap_or_default();
            ctrl.refresh_target_id = Some(tid);
            ctrl.main.set_refresh_filename(filename.into());
            ctrl.main.set_refresh_url("".into());
            ctrl.main.set_refresh_error("".into());
            ctrl.main.set_refresh_open(true);
        });
    });

    main.on_open_verify_dialog(|id| {
        with_controller(|ctrl| {
            let tid = id.to_string();
            let filename = ctrl
                .last_batch
                .as_ref()
                .and_then(|b| b.transfers.iter().find(|t| t.id == tid))
                .map(|t| t.filename.clone())
                .unwrap_or_default();
            ctrl.verify_target_id = Some(tid);
            ctrl.main.set_verify_filename(filename.into());
            ctrl.main.set_verify_sha("".into());
            ctrl.main.set_verify_error("".into());
            ctrl.main.set_verify_open(true);
        });
    });

    main.on_toggle_playlist_expand(|pid| {
        with_controller(|ctrl| {
            let p = pid.to_string();
            if !ctrl.expanded_playlists.remove(&p) {
                ctrl.expanded_playlists.insert(p);
            }
            ctrl.refresh_views();
        });
    });

    main.on_toggle_playlist_pause(|pid, paused| {
        with_controller(|ctrl| {
            let Some(app) = ctrl.app.clone() else { return };
            let id = pid.to_string();
            let action = if paused {
                "resume playlist"
            } else {
                "pause playlist"
            };
            ctrl.dispatch_transfer_action(action, true, async move {
                if paused {
                    app.resume_playlist(&id).await
                } else {
                    app.pause_playlist(&id).await
                }
            });
        });
    });

    main.on_reveal_playlist_folder(|pid| {
        with_controller(|ctrl| {
            if let Some(app) = &ctrl.app {
                if let Err(error) = app.reveal_playlist_folder(&pid) {
                    ctrl.show_toast(format!("Could not open playlist folder: {error}"), true);
                }
            }
        });
    });

    main.on_toggle_torrent_file(|idx| {
        with_controller(|ctrl| {
            let Some(id) = ctrl.selected_id.clone() else {
                return;
            };
            let Some(files) = ctrl.torrent_files.get(&id) else {
                return;
            };
            if idx < 0 || ctrl.torrent_file_apply_pending.contains(&id) {
                return;
            }
            let indices = ctrl.torrent_file_selections.entry(id).or_insert_with(|| {
                files
                    .iter()
                    .filter(|file| file.selected)
                    .map(|file| file.index)
                    .collect()
            });
            let index = idx as usize;
            if !indices.remove(&index) {
                indices.insert(index);
            }
            ctrl.refresh_inspector_files();
        });
    });

    main.on_select_torrent_files(|selected| {
        with_controller(|ctrl| {
            let Some(id) = ctrl.selected_id.clone() else {
                return;
            };
            let Some(files) = ctrl.torrent_files.get(&id) else {
                return;
            };
            if !ctrl.torrent_file_apply_pending.contains(&id) {
                ctrl.torrent_file_selections.insert(
                    id,
                    files
                        .iter()
                        .filter(|_| selected)
                        .map(|file| file.index)
                        .collect(),
                );
                ctrl.refresh_inspector_files();
            }
        });
    });

    main.on_apply_torrent_files(|id| {
        with_controller(|ctrl| {
            let Some(app) = ctrl.app.clone() else { return };
            let Some(selection) = ctrl.torrent_file_selections.get(id.as_str()) else {
                return;
            };
            if selection.is_empty() {
                ctrl.show_toast("Select at least one torrent file to download.", true);
                return;
            }
            let mut indices: Vec<_> = selection.iter().copied().collect();
            indices.sort_unstable();
            let id = id.to_string();
            if !ctrl.torrent_file_apply_pending.insert(id.clone()) {
                return;
            }
            ctrl.torrent_files_pending.remove(&id);
            ctrl.refresh_inspector_files();
            ctrl.runtime.spawn(async move {
                let mut padding = HashSet::new();
                let result = loop {
                    if indices.is_empty() {
                        break Err("Select at least one downloadable torrent file.".into());
                    }
                    match app.set_torrent_files(&id, indices.clone()).await {
                        Ok(()) => break Ok(()),
                        Err(error) => {
                            // The file-list API reports selection without a padding flag.
                            // The daemon validates the entire selection before changing it.
                            let Some(position) = rejected_padding_file(&error)
                                .and_then(|index| indices.binary_search(&index).ok())
                            else {
                                break Err(error);
                            };
                            padding.insert(indices.remove(position));
                        }
                    }
                };
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.torrent_file_apply_pending.remove(&id);
                        if !padding.is_empty() {
                            if let Some(files) = ctrl.torrent_files.get_mut(&id) {
                                files.retain(|file| !padding.contains(&file.index));
                                ctrl.torrent_padding_files
                                    .entry(id.clone())
                                    .or_default()
                                    .extend(&padding);
                            }
                            if let Some(selection) = ctrl.torrent_file_selections.get_mut(&id) {
                                selection.retain(|index| !padding.contains(index));
                            }
                        }
                        match result {
                            Ok(()) => {
                                if let Some(files) = ctrl.torrent_files.get_mut(&id) {
                                    for file in files {
                                        file.selected = indices.binary_search(&file.index).is_ok();
                                    }
                                }
                                ctrl.torrent_file_selections.remove(&id);
                                ctrl.show_toast("Torrent file selection updated", false);
                            }
                            Err(error) => ctrl.show_toast(
                                format!("Could not apply torrent files: {error}"),
                                true,
                            ),
                        }
                        ctrl.refresh_inspector_files();
                    });
                });
            });
        });
    });

    main.on_add_close_requested(|| with_controller(ControllerState::close_add_dialog));

    main.on_add_source_edited(|txt| {
        with_controller(|ctrl| {
            let s = txt.to_string();
            ctrl.handle_add_source_changed(&s);
        });
    });

    main.on_add_probe_clicked(|| {
        with_controller(|ctrl| {
            if ctrl.main.get_add_is_torrent_input() {
                ctrl.trigger_torrent_preview();
            } else {
                ctrl.trigger_probe();
            }
        });
    });

    main.on_add_cancel_torrent_preview_clicked(|| {
        with_controller(|ctrl| {
            ctrl.cancel_torrent_preview();
            ctrl.main.set_add_error_text("".into());
        });
    });

    main.on_torrent_preview_file_toggled(|index| {
        with_controller(|ctrl| {
            if index >= 0 {
                let index = index as usize;
                let checked = &mut ctrl.add_state.checked_preview_files;
                if !checked.remove(&index) {
                    checked.insert(index);
                }
                ctrl.refresh_torrent_preview();
            }
        });
    });
    main.on_torrent_preview_select_all(|| with_controller(|ctrl| ctrl.select_preview_files(true)));
    main.on_torrent_preview_select_none(|| {
        with_controller(|ctrl| ctrl.select_preview_files(false))
    });
    main.on_torrent_preview_filter_edited(|_| {
        with_controller(|ctrl| ctrl.refresh_torrent_preview())
    });

    main.on_add_browse_torrent_clicked(|| {
        let mut owner = 0_isize;
        let mut initial = None;
        let mut generation = 0;
        with_controller(|ctrl| {
            owner = hwnd_of(ctrl.main.window());
            initial = ctrl.app.as_ref().map(|a| a.settings().default_download_dir);
            generation = ctrl.add_state.generation;
        });
        std::thread::spawn(move || {
            let picked = win32::pick_torrent_file(owner, initial);
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if !ctrl.main.get_add_open() || ctrl.add_state.generation != generation {
                        return;
                    }
                    match picked {
                        Ok(Some(path)) => {
                            ctrl.main.set_add_source_text(path.clone().into());
                            ctrl.handle_add_source_changed(&path);
                        }
                        Ok(None) => {}
                        Err(err) => {
                            ctrl.main.set_add_error_text(err.to_string().into());
                        }
                    }
                });
            });
        });
    });

    main.on_add_browse_save_dir_clicked(|| {
        let mut owner = 0_isize;
        let mut initial = String::new();
        let mut generation = 0;
        with_controller(|ctrl| {
            owner = hwnd_of(ctrl.main.window());
            initial = ctrl.main.get_add_save_dir().to_string();
            generation = ctrl.add_state.generation;
        });
        std::thread::spawn(move || {
            if let Ok(Some(path)) = win32::pick_download_folder(owner, Some(initial)) {
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        if ctrl.main.get_add_open() && ctrl.add_state.generation == generation {
                            ctrl.main.set_add_save_dir(path.into());
                        }
                    });
                });
            }
        });
    });

    main.on_add_format_selected(|fmt_id| {
        with_controller(|ctrl| {
            let sel = fmt_id.to_string();
            ctrl.add_state.selected_format_id = Some(sel.clone());
            if let Some(probe) = &ctrl.add_state.probe {
                let models: Vec<FormatOptionModel> = video_format_options(probe)
                    .iter()
                    .map(|f| FormatOptionModel {
                        format_id: f.format_id.clone().into(),
                        title: f.label.clone().into(),
                        detail: format_option_detail(f).into(),
                        selected: f.format_id == sel,
                    })
                    .collect();
                ctrl.main
                    .set_add_format_options(ModelRc::from(Rc::new(VecModel::from(models))));
            }
        });
    });

    main.on_add_playlist_item_toggled(|idx| {
        with_controller(|ctrl| {
            let key = idx as u32;
            if !ctrl.add_state.checked_playlist.remove(&key) {
                ctrl.add_state.checked_playlist.insert(key);
            }
            ctrl.refresh_playlist_items_ui();
        });
    });

    main.on_add_playlist_select_all(|| {
        with_controller(|ctrl| {
            if let Some(probe) = &ctrl.add_state.probe {
                ctrl.add_state.checked_playlist =
                    probe.playlist_items.iter().map(|i| i.index).collect();
            }
            ctrl.refresh_playlist_items_ui();
        });
    });

    main.on_add_playlist_clear(|| {
        with_controller(|ctrl| {
            ctrl.add_state.checked_playlist.clear();
            ctrl.refresh_playlist_items_ui();
        });
    });

    main.on_add_subtitle_toggled(|lang| {
        with_controller(|ctrl| {
            let l = lang.to_string();
            if !ctrl.add_state.checked_subtitles.remove(&l) {
                ctrl.add_state.checked_subtitles.insert(l);
            }
            if let Some(probe) = &ctrl.add_state.probe {
                let sub_models: Vec<SubtitleItemModel> = probe
                    .subtitles
                    .iter()
                    .map(|s| SubtitleItemModel {
                        lang: s.lang.clone().into(),
                        label: format!("{} ({})", s.name, s.lang).into(),
                        is_auto: s.is_auto,
                        checked: ctrl.add_state.checked_subtitles.contains(&s.lang),
                    })
                    .collect();
                ctrl.main
                    .set_add_subtitles(ModelRc::from(Rc::new(VecModel::from(sub_models))));
            }
        });
    });

    main.on_add_submit_clicked(|| {
        with_controller(|ctrl| ctrl.submit_add_download());
    });

    // Settings Callbacks
    main.on_settings_close_requested(|| {
        with_controller(|ctrl| ctrl.main.set_settings_open(false));
    });

    main.on_settings_save_clicked(|| {
        with_controller(|ctrl| ctrl.save_settings());
    });

    main.on_settings_browse_download_dir(|| {
        let mut owner = 0_isize;
        let mut cur = String::new();
        with_controller(|ctrl| {
            owner = hwnd_of(ctrl.main.window());
            cur = ctrl.main.get_set_default_download_dir().to_string();
        });
        std::thread::spawn(move || {
            if let Ok(Some(chosen)) = win32::pick_download_folder(owner, Some(cur)) {
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| ctrl.main.set_set_default_download_dir(chosen.into()));
                });
            }
        });
    });

    main.on_settings_hibernate_now(|| {
        with_controller(|ctrl| {
            ctrl.main.set_settings_open(false);
            let _ = ctrl.main.hide();
        });
    });

    main.on_settings_refresh_toolchain(|| {
        with_controller(|ctrl| ctrl.refresh_toolchain_and_browsers());
    });

    main.on_settings_update_ytdlp(|| {
        with_controller(|ctrl| {
            let Some(app) = &ctrl.app else { return };
            ctrl.main.set_set_ytdlp_updating(true);
            ctrl.main.set_set_ytdlp_notice("".into());
            ctrl.main.set_set_ytdlp_error("".into());
            let app_c = app.clone();
            ctrl.runtime.spawn(async move {
                let res = update_ytdlp_managed().await;
                if res.is_ok() {
                    app_c.invalidate_toolchain_cache().await;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.main.set_set_ytdlp_updating(false);
                        match res {
                            Ok((_path, version)) => {
                                ctrl.main.set_set_ytdlp_notice(
                                    format!("Updated yt-dlp to {version}.").into(),
                                );
                                ctrl.refresh_toolchain_and_browsers();
                            }
                            Err(err) => ctrl.main.set_set_ytdlp_error(err.into()),
                        }
                    });
                });
            });
        });
    });

    main.on_settings_setup_browser(|id| {
        with_controller(|ctrl| {
            let Some(app) = &ctrl.app else { return };
            let kind = match id.as_str() {
                "edge" => BrowserKind::Edge,
                "brave" => BrowserKind::Brave,
                "firefox" => BrowserKind::Firefox,
                _ => BrowserKind::Chrome,
            };
            let manifests = app.data_dir().join("native-host");
            match open_browser_extension_setup(
                app.registry_base_key(),
                &manifests,
                app.resource_dir(),
                kind,
                None,
            ) {
                Ok(res) => {
                    ctrl.main.set_set_browser_notice(res.instructions.into());
                    ctrl.main.set_set_browser_error("".into());
                    ctrl.refresh_toolchain_and_browsers();
                }
                Err(err) => {
                    ctrl.main.set_set_browser_error(err.to_string().into());
                }
            }
        });
    });

    main.on_settings_copy_text(|txt| {
        let _ = win32::copy_to_clipboard(&txt);
    });

    main.on_settings_check_updates(|| {
        with_controller(|ctrl| ctrl.check_updates(false));
    });

    main.on_settings_install_update(|| {
        with_controller(|ctrl| ctrl.install_update());
    });

    // Palette, Refresh, Verify callbacks
    main.on_palette_query_edited(|_| {
        with_controller(|ctrl| ctrl.update_palette_actions());
    });

    main.on_palette_action_triggered(|id| {
        with_controller(|ctrl| {
            ctrl.main.set_palette_open(false);
            match id.as_str() {
                "add" => ctrl.open_add_dialog_with_seed(OpenPickerEvent {
                    url: None,
                    headers: HashMap::new(),
                }),
                "pause_all" => ctrl.set_all_paused(!ctrl.main.get_all_paused()),
                "toggle_theme" => {
                    let next_light = !ctrl.is_light;
                    ctrl.apply_theme(next_light, ctrl.is_mono);
                }
                "settings" => ctrl.open_settings_dialog(),
                "check_updates" => {
                    ctrl.open_settings_dialog();
                    ctrl.check_updates(false);
                }
                "hibernate" => {
                    let _ = ctrl.main.hide();
                }
                _ => {}
            }
        });
    });

    main.on_refresh_submit_clicked(|| {
        with_controller(|ctrl| {
            let (Some(app), Some(tid)) = (&ctrl.app, ctrl.refresh_target_id.clone()) else {
                return;
            };
            let new_url = ctrl.main.get_refresh_url().to_string().trim().to_owned();
            if !looks_like_http_url(&new_url) {
                ctrl.main
                    .set_refresh_error("Enter a valid http:// or https:// URL.".into());
                return;
            }
            ctrl.main.set_refresh_submitting(true);
            ctrl.main.set_refresh_error("".into());
            let app_c = app.clone();
            ctrl.runtime.spawn(async move {
                let res = app_c
                    .refresh_download_url(RefreshDownloadUrlRequest {
                        transfer_id: tid,
                        new_url,
                        headers: HashMap::new(),
                        referrer: None,
                    })
                    .await;
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.main.set_refresh_submitting(false);
                        match res {
                            Ok(()) => ctrl.main.set_refresh_open(false),
                            Err(err) => ctrl.main.set_refresh_error(err.into()),
                        }
                    });
                });
            });
        });
    });

    main.on_verify_submit_clicked(|| {
        with_controller(|ctrl| {
            let (Some(app), Some(tid)) = (&ctrl.app, ctrl.verify_target_id.clone()) else {
                return;
            };
            let expected = ctrl.main.get_verify_sha().to_string().trim().to_owned();
            ctrl.main.set_verify_submitting(true);
            ctrl.main.set_verify_error("".into());
            let app_c = app.clone();
            ctrl.runtime.spawn(async move {
                let res = app_c.verify_transfer_checksum(&tid, &expected).await;
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.main.set_verify_submitting(false);
                        match res {
                            Ok(true) => ctrl.main.set_verify_open(false),
                            Ok(false) => ctrl
                                .main
                                .set_verify_error("SHA-256 digest mismatch!".into()),
                            Err(err) => ctrl.main.set_verify_error(err.into()),
                        }
                    });
                });
            });
        });
    });
}

pub fn run_app(demo_mode: bool) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to initialize Tokio runtime: {e}"))?;

    let main = MainWindow::new().map_err(|e| format!("Failed to create Slint MainWindow: {e}"))?;
    main.set_version_label(crate::desktop_version().into());
    main.set_demo_mode(demo_mode);
    wire_main_callbacks(&main);

    let (app, is_light, is_mono) = if demo_mode {
        (None, false, false)
    } else {
        let slots = Arc::new(SharedBridgeSlots::default());
        let ui: Arc<dyn UiBridge> = Arc::new(NativeUi::new(slots.clone()));
        let options = DaemonOptions {
            resource_dir: default_resource_dir(),
            ..DaemonOptions::default()
        };
        let app_state = runtime.block_on(AppState::start(default_data_dir(), ui, options))?;
        *slots.app.lock().unwrap() = Some(app_state.clone());

        let settings = app_state.settings();
        let (light, mono) = resolve_theme_flags(&settings);
        let gen = app_state.window_loading();
        let _ = app_state.window_ready(gen);
        (Some(app_state), light, mono)
    };

    let (tray_icon, tray_menu_ids) = if demo_mode {
        (None, TrayMenuIds::default())
    } else {
        build_tray_icon()
            .map(|(t, ids)| (Some(t), ids))
            .unwrap_or_default()
    };

    let controller = Rc::new(RefCell::new(ControllerState {
        main: main.clone_strong(),
        runtime: runtime.handle().clone(),
        app: app.clone(),
        demo_mode,
        is_light,
        is_mono,
        is_maximized: false,
        last_batch: None,
        speed_history: VecDeque::with_capacity(MAX_SPEED_SAMPLES),
        expanded_playlists: HashSet::new(),
        selected_id: None,
        models: DesktopModels::default(),
        last_disk_check: None,
        disk_space: None,
        torrent_files: HashMap::new(),
        torrent_file_selections: HashMap::new(),
        torrent_padding_files: HashMap::new(),
        torrent_files_pending: HashMap::new(),
        torrent_file_apply_pending: HashSet::new(),
        torrent_peers: HashMap::new(),
        torrent_peers_pending: HashSet::new(),
        pending_transfer_dialogs: HashSet::new(),
        torrent_preview_files_model: Rc::default(),
        toast_timer: Timer::default(),
        confirm_windows: HashMap::new(),
        progress_windows: HashMap::new(),
        complete_windows: HashMap::new(),
        add_state: AddDialogRuntime::default(),
        pending_picker_requests: VecDeque::new(),
        refresh_target_id: None,
        verify_target_id: None,
        last_clipboard_seq: win32::clipboard_sequence_number(),
        clipboard_in_flight: Arc::new(AtomicBool::new(false)),
        theme_poll_counter: 0,
        _tray_icon: tray_icon,
        tray_menu_ids,
    }));

    CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(controller.clone());
    });

    {
        let mut ctrl = controller.borrow_mut();
        ctrl.models.attach(&ctrl.main);
        ctrl.main
            .set_torrent_preview_files(ctrl.torrent_preview_files_model.clone().into());
        ctrl.apply_theme(is_light, is_mono);
        ctrl.show_main_window();
        if demo_mode {
            let demo = demo_telemetry_batch();
            ctrl.apply_telemetry(demo.clone());
            ctrl.apply_telemetry(demo);
        } else if let Some(app_state) = &app {
            let initial = runtime.block_on(app_state.snapshot());
            ctrl.apply_telemetry(initial);
            ctrl.check_updates(true);

            // Clean up any prior staged updater installers & auto-register native messaging hosts
            let app_bg = app_state.clone();
            runtime.spawn_blocking(move || {
                updater::cleanup_stale_update_dirs(app_bg.data_dir());
                if let Ok(exe) = native_host_executable(app_bg.resource_dir()) {
                    let manifests = app_bg.data_dir().join("native-host");
                    if let Err(err) = install_browser_hosts_into(
                        app_bg.registry_base_key(),
                        &manifests,
                        &exe,
                        None,
                        None,
                        None,
                    ) {
                        tracing::warn!(%err, "startup browser native host sync failed");
                    }
                }
            });
        }
    }

    if let Some(source) = crate::take_initial_picker_source() {
        with_controller(|ctrl| {
            ctrl.open_add_dialog_with_seed(OpenPickerEvent {
                url: Some(source),
                headers: HashMap::new(),
            });
        });
    }

    let peer_timer = Timer::default();
    if !demo_mode {
        peer_timer.start(TimerMode::Repeated, Duration::from_millis(1000), || {
            with_controller(ControllerState::poll_torrent_details);
        });
    }

    // 80ms Tray + Menu event pump (only when tray active)
    let tray_timer = Timer::default();
    if !demo_mode {
        tray_timer.start(TimerMode::Repeated, Duration::from_millis(80), || {
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = ev
                {
                    with_controller(|ctrl| ctrl.show_main_window());
                }
            }
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                let mut action = None;
                with_controller(|ctrl| {
                    let ids = &ctrl.tray_menu_ids;
                    if ids.show.as_ref() == Some(&ev.id) {
                        action = Some("show");
                    } else if ids.add.as_ref() == Some(&ev.id) {
                        action = Some("add");
                    } else if ids.pause_all.as_ref() == Some(&ev.id) {
                        action = Some("pause_all");
                    } else if ids.resume_all.as_ref() == Some(&ev.id) {
                        action = Some("resume_all");
                    } else if ids.quit.as_ref() == Some(&ev.id) {
                        action = Some("quit");
                    }
                });
                match action {
                    Some("show") => with_controller(|ctrl| ctrl.show_main_window()),
                    Some("add") => with_controller(|ctrl| {
                        ctrl.open_add_dialog_with_seed(OpenPickerEvent {
                            url: None,
                            headers: HashMap::new(),
                        });
                    }),
                    Some("pause_all") => with_controller(|ctrl| ctrl.set_all_paused(true)),
                    Some("resume_all") => with_controller(|ctrl| ctrl.set_all_paused(false)),
                    Some("quit") => with_controller(|ctrl| {
                        if let Some(a) = &ctrl.app {
                            let ac = a.clone();
                            ctrl.runtime.spawn(async move { ac.quit().await });
                        } else {
                            let _ = slint::quit_event_loop();
                        }
                    }),
                    _ => {}
                }
            }
        });
    }

    // 900ms sequence-tracked clipboard monitor + live Windows System theme watcher
    let clip_timer = Timer::default();
    if !demo_mode {
        clip_timer.start(TimerMode::Repeated, Duration::from_millis(900), || {
            let mut dispatch_check: Option<(Arc<AppState>, Arc<AtomicBool>)> = None;
            with_controller(|ctrl| {
                let Some(app) = ctrl.app.clone() else { return };
                let settings = app.settings();

                // Poll Windows Personalize theme when System theme mode is active (~1.8s cadence)
                ctrl.theme_poll_counter = ctrl.theme_poll_counter.wrapping_add(1);
                if settings.theme == ThemeMode::System && ctrl.theme_poll_counter % 2 == 0 {
                    let sys_light = win32::is_system_light_theme();
                    if sys_light != ctrl.is_light {
                        ctrl.apply_theme(sys_light, ctrl.is_mono);
                    }
                }

                let seq = win32::clipboard_sequence_number();
                if seq != ctrl.last_clipboard_seq {
                    ctrl.last_clipboard_seq = seq;
                    if settings.monitor_clipboard
                        && !ctrl.clipboard_in_flight.swap(true, Ordering::AcqRel)
                    {
                        dispatch_check = Some((app.clone(), ctrl.clipboard_in_flight.clone()));
                    }
                }
            });

            if let Some((app_c, flag)) = dispatch_check {
                let handle = with_runtime();
                std::thread::spawn(move || {
                    let text = win32::read_clipboard_text();
                    flag.store(false, Ordering::Release);
                    if let Some(raw) = text {
                        let trimmed = raw.trim().to_owned();
                        if looks_like_http_url(&trimmed) || is_torrent_input(&trimmed) {
                            if let Some(h) = handle {
                                h.spawn(async move {
                                    let _ = app_c.open_picker(Some(trimmed)).await;
                                });
                            }
                        }
                    }
                });
            }
        });
    }

    let _res = slint::run_event_loop_until_quit();

    tray_timer.stop();
    clip_timer.stop();
    peer_timer.stop();
    with_controller(ControllerState::cancel_torrent_preview);
    if let Some(app_state) = app {
        if !app_state.is_quitting() {
            let _ = runtime.block_on(app_state.shutdown());
        }
    }
    Ok(())
}

fn with_runtime() -> Option<tokio::runtime::Handle> {
    CONTROLLER.with(|cell| cell.borrow().as_ref().map(|rc| rc.borrow().runtime.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_vec_model_by_preserves_unchanged_rows_and_updates_the_tail() {
        let first = Rc::new(10);
        let changed = Rc::new(30);
        let model = VecModel::from(vec![first.clone(), Rc::new(20), Rc::new(40)]);
        sync_vec_model_by(&model, vec![Rc::new(10), changed.clone()], PartialEq::eq);

        assert_eq!(model.row_count(), 2);
        assert!(Rc::ptr_eq(&model.row_data(0).unwrap(), &first));
        assert!(Rc::ptr_eq(&model.row_data(1).unwrap(), &changed));

        sync_vec_model_by(
            &model,
            vec![Rc::new(10), Rc::new(30), Rc::new(50)],
            PartialEq::eq,
        );
        assert_eq!(model.row_count(), 3);
        assert!(Rc::ptr_eq(&model.row_data(0).unwrap(), &first));
        assert!(Rc::ptr_eq(&model.row_data(1).unwrap(), &changed));
        assert_eq!(*model.row_data(2).unwrap(), 50);
    }

    #[test]
    fn reconcile_vec_model_removes_the_vanished_transfer_at_its_index() {
        use slint::private_unstable_api::re_exports::{
            ModelChangeListener, ModelChangeListenerContainer,
        };

        #[derive(Default)]
        struct RowChanges(RefCell<Vec<(&'static str, usize, usize)>>);

        impl ModelChangeListener for RowChanges {
            fn row_changed(self: Pin<&Self>, index: usize) {
                self.0.borrow_mut().push(("changed", index, 1));
            }

            fn row_added(self: Pin<&Self>, index: usize, count: usize) {
                self.0.borrow_mut().push(("added", index, count));
            }

            fn row_removed(self: Pin<&Self>, index: usize, count: usize) {
                self.0.borrow_mut().push(("removed", index, count));
            }

            fn reset(self: Pin<&Self>) {
                self.0.borrow_mut().push(("reset", 0, 0));
            }
        }

        let changes = Box::pin(ModelChangeListenerContainer::<RowChanges>::default());
        let surviving = Rc::new(String::from("B"));
        let rows = VecModel::from(vec![Rc::new(String::from("A")), surviving.clone()]);
        rows.model_tracker()
            .attach_peer(changes.as_ref().model_peer());

        reconcile_vec_model_by_key(
            &rows,
            vec![Rc::new(String::from("B"))],
            |row| row.as_str(),
            PartialEq::eq,
        );

        assert_eq!(*changes.0.borrow(), [("removed", 0, 1)]);
        assert_eq!(rows.row_count(), 1);
        assert!(Rc::ptr_eq(&rows.row_data(0).unwrap(), &surviving));
    }

    #[test]
    fn transfer_rows_keep_the_same_segment_model_when_only_a_segment_changes() {
        let mut transfer = demo_telemetry_batch().transfers.remove(0);
        let mut segment_models = HashMap::new();
        let before = transfer_to_row(&transfer, false, &mut segment_models);
        let rows = VecModel::from(vec![before.clone()]);
        transfer.ui_segments[15] = 0.5;
        let after = transfer_to_row(&transfer, false, &mut segment_models);

        assert!(same_transfer_row(&before, &after));
        assert_eq!(before.segments, after.segments);
        reconcile_vec_model_by_key(&rows, vec![after], |row| row.id.as_str(), same_transfer_row);
        assert_eq!(rows.row_data(0).unwrap().segments.row_count(), 16);
        assert_eq!(rows.row_data(0).unwrap().segments.row_data(15), Some(0.5));
    }

    #[test]
    fn seeded_urls_auto_probe_only_when_media_detection_is_needed() {
        assert!(should_auto_probe(" https://www.youtube.com/watch?v=demo "));
        assert!(!should_auto_probe("https://example.org/installer.exe"));
        assert!(!should_auto_probe("https://example.org/watch[07-09]"));
        assert!(!should_auto_probe("https://example.org/content.torrent"));
        assert!(!should_auto_probe("file:///C:/downloads/movie.mp4"));
        assert!(!should_auto_probe(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"
        ));
    }

    #[test]
    fn torrent_edits_auto_inspect_paths_and_complete_magnets() {
        assert!(should_auto_preview(r" C:DownloadsLinux.TORRENT "));
        assert!(should_auto_preview(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(!should_auto_preview("magnet:?xt=urn:btih:01234567"));
        assert!(!should_auto_preview("https://example.org/movie.mp4"));
    }
}
