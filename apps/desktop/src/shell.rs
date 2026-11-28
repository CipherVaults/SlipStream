use crate::{
    expand_wildcard_urls, install_browser_hosts_into, next_numbered_filename, resolve_category_dir,
    tools::{
        list_browsers, native_host_executable, open_browser_extension_setup, update_ytdlp_managed,
    },
    updater::{self, friendly_updater_error},
    view_model::{
        self, classify_icon_kind, compute_nav_counts, demo_telemetry_batch, format_bytes,
        format_duration, format_eta, format_minute_of_day, format_speed, format_transfer_percent,
        is_torrent_input, looks_like_direct_file_url, looks_like_http_url, matches_filter,
        pair_video_with_best_audio, parse_minute_of_day, parse_speed_limit_bps,
        parse_speed_limit_slider_mbps, post_action_from_index, post_action_label,
        should_clear_seeded_headers, url_origin, video_format_options, NavFilter,
    },
    win32, AppState, BrowserCardModel, CategoryNavModel, CompletionAlert, CompletionBannerWindow,
    ConnectionRowModel, DaemonOptions, DownloadCompleteWindow, DownloadConfirmWindow,
    DownloadProgressWindow, DropBasketWindow, FormatOptionModel, InspectorModel, LogEntryModel,
    MainWindow, OpenPickerEvent, PaletteActionModel, PlaylistGroupModel, PlaylistItemModel,
    QueueItem, RssFeedRuleItem, SegmentTileModel, SiteGrabItemRow, SiteLoginItem, SpeedGraphModel,
    SubtitleItemModel, TagChipModel, Theme, ToolchainStatus, TorrentFileRowModel,
    TorrentPeerRowModel, TorrentPreviewFileModel, TransferRowModel, UiBridge, WatchFolderItem,
};
use i_slint_backend_winit::{
    winit::{event::WindowEvent, window::ResizeDirection},
    EventResult, WinitWindowAccessor,
};
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{
    ComponentHandle, FilterModel, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
};
use slipstream_protocol::{
    default_download_queues, AccentMode, AddHttpDownloadRequest, AddMediaDownloadRequest,
    AddTorrentRequest, AppSettings, AudioPreset, BrowserKind, ConfirmPendingDownloadRequest,
    CreateTorrentRequest, DownloadCategory, DownloadQueueSpec, LogEntry, MediaContainer,
    MediaFormatOption, MediaProbeResult, PendingDownloadPreview, ProxyMode,
    RefreshDownloadUrlRequest, RssFeedRule, ScheduleMode, SegmentState, SelectedPlaylistEntry,
    SiteAuthScheme, SiteGrabRequest, SiteLoginEntry, SpeedSample, TelemetryBatch, ThemeMode,
    ToolSource, TorrentFileEntry, TorrentPeerSummary, TorrentPreviewResult,
    TransferCompletionOptions, TransferKind, TransferSnapshot, TransferStatus, UpdateMode,
    UpdateProgress, UpdateProgressStage, WatchFolderRule,
};
use slipstream_torrent::{parse_magnet_uri, CancellationToken};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    io::{self, Read},
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

const MAX_SPEED_SAMPLES: usize = 7_200;
const SPARKLINE_SAMPLES: usize = 28;
const MAX_TEXT_IMPORT_BYTES: u64 = 2 * 1024 * 1024;
const APP_ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");
const CUSTOM_VPN_ADAPTER: &str = "Custom adapter";
const NETWORK_COOKIE_BROWSERS: [&str; 4] = ["chrome", "edge", "brave", "firefox"];

fn optional_text(raw: &str) -> Option<String> {
    let raw = raw.trim();
    (!raw.is_empty()).then(|| raw.to_owned())
}

fn format_upload_kib(rate_bps: Option<u64>) -> SharedString {
    rate_bps
        .map(|rate| (rate / 1024).max(1).to_string())
        .unwrap_or_default()
        .into()
}

fn parse_upload_kib(raw: &str) -> Result<Option<u64>, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let rate = raw
        .parse::<u64>()
        .ok()
        .and_then(|kib| kib.checked_mul(1024))
        .filter(|&bps| bps <= u64::from(u32::MAX))
        .ok_or("Enter a whole upload limit in KiB/s, or leave it blank for unlimited.")?;
    Ok((rate != 0).then_some(rate))
}

#[derive(Clone, Copy)]
enum CreateTorrentPicker {
    SourceFile,
    SourceFolder,
    Output,
}

enum TorrentPowerAction {
    Recheck,
    MoveStorage,
    AddTracker(String),
    Reannounce,
    UploadLimit(Option<u64>),
}

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
    pending_telemetry: Mutex<PendingTelemetry>,
    telemetry_scheduled: AtomicBool,
    app: Mutex<Option<Arc<AppState>>>,
}

#[derive(Default)]
struct PendingTelemetry {
    batch: Option<TelemetryBatch>,
    speeds: VecDeque<SpeedSample>,
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
        {
            // Keep every speed sample even when the event loop coalesces snapshots.
            let mut pending = self.slots.pending_telemetry.lock().unwrap();
            push_speed_sample(&mut pending.speeds, speed_sample(batch));
            pending.batch = Some(batch.clone());
        }
        if !self.slots.telemetry_scheduled.swap(true, Ordering::AcqRel) {
            let slots = self.slots.clone();
            let _ = slint::invoke_from_event_loop(move || {
                slots.telemetry_scheduled.store(false, Ordering::Release);
                let pending = std::mem::take(&mut *slots.pending_telemetry.lock().unwrap());
                if let Some(latest) = pending.batch {
                    with_controller(|ctrl| {
                        for sample in pending.speeds {
                            push_speed_sample(&mut ctrl.speed_history, sample);
                        }
                        ctrl.refresh_speed_graph();
                        ctrl.apply_telemetry(latest);
                    });
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
    transfer_rows_model: Rc<VecModel<TransferRowModel>>,
    categories_model: Rc<VecModel<CategoryNavModel>>,
    tags_model: Rc<VecModel<TagChipModel>>,
    category_options_model: Rc<VecModel<SharedString>>,
    queues_model: Rc<VecModel<QueueItem>>,
    queue_options_model: Rc<VecModel<SharedString>>,
    log_entries_model: Rc<VecModel<LogEntryModel>>,
    inspector_segments_model: Rc<VecModel<SegmentTileModel>>,
    inspector_files_model: Rc<VecModel<TorrentFileRowModel>>,
    inspector_peers_model: Rc<VecModel<TorrentPeerRowModel>>,
    row_segment_models: HashMap<String, Rc<VecModel<f32>>>,
    playlist_item_models: HashMap<String, Rc<VecModel<TransferRowModel>>>,
}

impl DesktopModels {
    pub fn attach(&self, main: &MainWindow) {
        main.set_playlist_groups(self.playlist_groups_model.clone().into());
        let rows = self.transfer_rows_model.clone();
        main.set_transfer_rows(rows.clone().into());
        main.set_active_rows(ModelRc::new(FilterModel::new(rows.clone(), |row| {
            !row.in_playlist && !row.is_completed
        })));
        main.set_completed_rows(ModelRc::new(FilterModel::new(rows, |row| {
            !row.in_playlist && row.is_completed
        })));
        main.set_categories(self.categories_model.clone().into());
        main.set_tags(self.tags_model.clone().into());
        main.set_category_options(self.category_options_model.clone().into());
        main.set_queue_items(self.queues_model.clone().into());
        main.set_queue_options(self.queue_options_model.clone().into());
        main.set_log_entries(self.log_entries_model.clone().into());
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

enum PickerRequest {
    Source(OpenPickerEvent),
    BatchText(String),
}

#[derive(Default)]
pub struct TransferSelection {
    selected_id: Option<String>,
    selected_ids: HashSet<String>,
    selection_anchor: Option<String>,
    visible_ids: Vec<String>,
    initialized: bool,
}

impl TransferSelection {
    pub fn select_only(&mut self, id: &str) {
        self.selected_ids.clear();
        self.selected_ids.insert(id.to_owned());
        self.selected_id = Some(id.to_owned());
        self.selection_anchor = self.selected_id.clone();
        self.initialized = true;
    }

    pub fn select_all(&mut self) {
        self.selected_ids = self.visible_ids.iter().cloned().collect();
        self.update_primary();
        self.selection_anchor.clone_from(&self.selected_id);
        self.initialized = true;
    }

    fn clear(&mut self) {
        self.selected_ids.clear();
        self.selected_id = None;
        self.selection_anchor = None;
        self.initialized = true;
    }

    fn update_primary(&mut self) {
        if self
            .selected_id
            .as_ref()
            .is_none_or(|id| !self.selected_ids.contains(id))
        {
            self.selected_id = self
                .visible_ids
                .iter()
                .find(|id| self.selected_ids.contains(*id))
                .cloned();
        }
    }

    fn update_visible(&mut self, visible: Vec<String>) {
        let ids: HashSet<_> = visible.iter().collect();
        self.selected_ids.retain(|id| ids.contains(id));
        if self
            .selection_anchor
            .as_ref()
            .is_some_and(|id| !ids.contains(id))
        {
            self.selection_anchor = None;
        }
        self.visible_ids = visible;
        if !self.initialized {
            if let Some(first) = self.visible_ids.first().cloned() {
                self.select_only(&first);
            }
        }
        self.update_primary();
    }

    fn click(&mut self, id: &str, ctrl: bool, shift: bool) {
        if shift {
            let positions = self.selection_anchor.as_ref().and_then(|anchor| {
                let start = self
                    .visible_ids
                    .iter()
                    .position(|visible| visible == anchor)?;
                let end = self.visible_ids.iter().position(|visible| visible == id)?;
                Some((start.min(end), start.max(end)))
            });
            if let Some((start, end)) = positions {
                if !ctrl {
                    self.selected_ids.clear();
                }
                self.selected_ids
                    .extend(self.visible_ids[start..=end].iter().cloned());
                self.selected_id = Some(id.to_owned());
                return;
            }
        }
        if ctrl {
            self.selection_anchor = Some(id.to_owned());
            if self.selected_ids.remove(id) {
                self.update_primary();
            } else {
                self.selected_ids.insert(id.to_owned());
                self.selected_id = Some(id.to_owned());
            }
        } else {
            self.select_only(id);
        }
    }

    fn ordered_ids(&self) -> Vec<String> {
        self.visible_ids
            .iter()
            .filter(|id| self.selected_ids.contains(*id))
            .cloned()
            .collect()
    }
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
    speed_history: VecDeque<SpeedSample>,
    expanded_playlists: HashSet<String>,
    selection: TransferSelection,
    categories: Vec<DownloadCategory>,
    queues: Vec<DownloadQueueSpec>,
    draft_queues: Vec<DownloadQueueSpec>,
    draft_watch_folders: Vec<WatchFolderRule>,
    settings_session_id: u64,
    create_torrent_session_id: u64,
    settings_initial_alt_speed: bool,
    rss_feeds_model: Rc<VecModel<RssFeedRuleItem>>,
    site_logins_model: Rc<VecModel<SiteLoginItem>>,
    grabber_items_model: Rc<VecModel<SiteGrabItemRow>>,
    grabber_session_id: u64,
    grabber_scan: Option<tokio::task::JoinHandle<()>>,
    pending_bulk_remove: Vec<String>,
    drop_basket: Option<DropBasketWindow>,
    completion_banner: Option<CompletionBannerWindow>,
    completion_timer: Timer,
    settings_task: Option<tokio::task::JoinHandle<()>>,
    preference_revision: u64,
    saved_preference_revision: u64,
    logs: Vec<LogEntry>,
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
    torrent_actions_pending: HashSet<String>,
    pending_transfer_dialogs: HashSet<(String, TransferDialog)>,
    torrent_preview_files_model: Rc<VecModel<TorrentPreviewFileModel>>,
    toast_timer: Timer,
    confirm_windows: HashMap<String, ConfirmWindowState>,
    progress_windows: HashMap<String, ProgressWindowState>,
    complete_windows: HashMap<String, CompleteWindowState>,
    add_state: AddDialogRuntime,
    pending_picker_requests: VecDeque<PickerRequest>,
    refresh_target_id: Option<String>,
    verify_target_id: Option<String>,
    last_clipboard_seq: u32,
    clipboard_in_flight: Arc<AtomicBool>,
    theme_poll_counter: u8,
    _tray_icon: Option<TrayIcon>,
    tray_menu_ids: TrayMenuIds,
}

#[derive(Default)]
struct TrayMenuIds {
    show: Option<muda::MenuId>,
    add: Option<muda::MenuId>,
    pause_all: Option<muda::MenuId>,
    resume_all: Option<muda::MenuId>,
    drop_basket: Option<CheckMenuItem>,
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
    let basket_item = CheckMenuItem::new("Drop Basket", true, false, None);

    let ids = TrayMenuIds {
        show: Some(show_item.id().clone()),
        add: Some(add_item.id().clone()),
        pause_all: Some(pause_item.id().clone()),
        resume_all: Some(resume_item.id().clone()),
        quit: Some(quit_item.id().clone()),
        drop_basket: Some(basket_item.clone()),
    };

    let menu = Menu::new();
    let _ = menu.append(&show_item);
    let _ = menu.append(&add_item);
    let _ = menu.append(&basket_item);
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
        && a.selected == b.selected
        && a.category_name == b.category_name
        && a.tags_summary == b.tags_summary
        && a.size_text == b.size_text
        && a.speed_text == b.speed_text
        && a.eta_text == b.eta_text
        && a.added_text == b.added_text
        && a.in_playlist == b.in_playlist
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

fn transfer_category(transfer: &TransferSnapshot) -> &str {
    transfer.category_id.as_deref().unwrap_or("general")
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
        selected: false,
        category_name: transfer_category(t).into(),
        tags_summary: t.tags.join(", ").into(),
        size_text: format_bytes(t.total_bytes.unwrap_or(t.downloaded_bytes)).into(),
        speed_text: format_speed(t.download_speed_bps).into(),
        eta_text: format_eta(t.eta_seconds).into(),
        added_text: t
            .created_at
            .with_timezone(&chrono::Local)
            .format("%b %d")
            .to_string()
            .into(),
        in_playlist: t.playlist_id.is_some(),
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
            category_id: t.category_id.as_deref().unwrap_or_default().into(),
            tags_summary: t.tags.join(", ").into(),
            ..inspector
        }
    }

    pub fn update_inspector_files(
        &self,
        files: &[TorrentFileEntry],
        file_selection: Option<&HashSet<usize>>,
    ) {
        let torrent_rows = view_model::grouped_torrent_files(files)
            .into_iter()
            .map(|(f, folder, name)| TorrentFileRowModel {
                index: f.index as i32,
                relative_path: f.relative_path.clone().into(),
                folder_label: folder.into(),
                display_name: name.into(),
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
        selection: &mut TransferSelection,
        categories: &[DownloadCategory],
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
        let active_category = main.get_active_category_filter();
        let active_tag = main.get_active_tag_filter();
        let active_queue = main.get_active_queue_filter();
        self.update_filters(&batch.transfers, categories);
        let filter = |t: &&TransferSnapshot| {
            matches_filter(t, nav, &query)
                && (active_category.is_empty() || transfer_category(t) == active_category.as_str())
                && (active_tag.is_empty() || t.tags.iter().any(|tag| tag == active_tag.as_str()))
                && (active_queue.is_empty() || t.queue_id == active_queue.as_str())
        };
        let sorted;
        let visible = if main.get_compact_table_view() {
            let mut rows: Vec<_> = batch.transfers.iter().filter(filter).cloned().collect();
            view_model::sort_transfers(
                &mut rows,
                &main.get_sort_column(),
                main.get_sort_ascending(),
            );
            sorted = rows;
            sorted.iter().collect::<Vec<_>>()
        } else {
            batch.transfers.iter().filter(filter).collect()
        };

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

        let table = main.get_compact_table_view();
        let mut playlist_map: HashMap<&str, Vec<&TransferSnapshot>> = HashMap::new();
        if !table {
            for &transfer in &visible {
                if let Some(id) = transfer.playlist_id.as_deref() {
                    playlist_map.entry(id).or_default().push(transfer);
                }
            }
        }
        let mut playlists: Vec<_> = playlist_map
            .into_iter()
            .map(|(id, mut items)| {
                items.sort_by_key(|t| t.playlist_index.unwrap_or(0));
                let title = items
                    .iter()
                    .find_map(|t| t.playlist_title.as_deref())
                    .unwrap_or("Playlist Queue");
                (title, id, items)
            })
            .collect();
        playlists.sort_by_key(|(title, id, _)| (*title, *id));
        let mut ids = Vec::with_capacity(visible.len());
        if table {
            ids.extend(visible.iter().map(|t| t.id.clone()));
        } else {
            for (_, id, items) in &playlists {
                if expanded_playlists.contains(*id) {
                    ids.extend(items.iter().map(|t| t.id.clone()));
                }
            }
            for completed in [false, true] {
                ids.extend(
                    visible
                        .iter()
                        .filter(|t| {
                            t.playlist_id.is_none()
                                && (t.status == TransferStatus::Completed) == completed
                        })
                        .map(|t| t.id.clone()),
                );
            }
        }
        selection.update_visible(ids);

        let category_names: HashMap<_, _> = categories
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str()))
            .collect();
        let mut make_row = |transfer: &TransferSnapshot, compact| {
            let mut row = transfer_to_row(transfer, compact, &mut self.row_segment_models);
            row.category_name = category_names
                .get(transfer_category(transfer))
                .copied()
                .unwrap_or("General")
                .into();
            row.selected = selection.selected_ids.contains(&transfer.id);
            row
        };
        let rows = visible.iter().map(|t| make_row(t, false)).collect();
        let playlist_groups = playlists
            .into_iter()
            .map(|(title, pid, items)| {
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
                let row_models = items.iter().map(|t| make_row(t, true)).collect();
                let item_model = self.playlist_item_models.entry(pid.to_owned()).or_default();
                reconcile_vec_model_by_key(
                    item_model,
                    row_models,
                    |row| row.id.as_str(),
                    same_transfer_row,
                );
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
                    playlist_id: pid.into(),
                    title: title.into(),
                    expanded: expanded_playlists.contains(pid),
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
        reconcile_vec_model_by_key(
            &self.playlist_groups_model,
            playlist_groups,
            |group| group.playlist_id.as_str(),
            PartialEq::eq,
        );
        reconcile_vec_model_by_key(
            &self.transfer_rows_model,
            rows,
            |row| row.id.as_str(),
            same_transfer_row,
        );
        main.set_selected_id(selection.selected_id.as_deref().unwrap_or_default().into());
        main.set_selected_count(selection.selected_ids.len() as i32);
        if selection.selected_ids.len() <= 1 {
            main.set_bulk_editor(0);
        }
    }

    fn update_filters(&self, transfers: &[TransferSnapshot], categories: &[DownloadCategory]) {
        let mut category_counts = HashMap::new();
        let mut tag_counts = std::collections::BTreeMap::new();
        for transfer in transfers {
            *category_counts
                .entry(transfer_category(transfer))
                .or_insert(0) += 1;
            for tag in &transfer.tags {
                *tag_counts.entry(tag).or_insert(0) += 1;
            }
        }
        let category_rows = categories
            .iter()
            .map(|category| CategoryNavModel {
                id: category.id.as_str().into(),
                name: category.name.as_str().into(),
                count: category_counts
                    .get(category.id.as_str())
                    .copied()
                    .unwrap_or_default(),
            })
            .collect();
        reconcile_vec_model_by_key(
            &self.categories_model,
            category_rows,
            |row| row.id.as_str(),
            PartialEq::eq,
        );
        let tag_rows = tag_counts
            .into_iter()
            .map(|(tag, count)| TagChipModel {
                tag: tag.as_str().into(),
                count,
            })
            .collect();
        reconcile_vec_model_by_key(
            &self.tags_model,
            tag_rows,
            |row| row.tag.as_str(),
            PartialEq::eq,
        );
        let options = std::iter::once("Automatic".into())
            .chain(
                categories
                    .iter()
                    .map(|category| category.name.as_str().into()),
            )
            .collect();
        sync_vec_model_by(&self.category_options_model, options, PartialEq::eq);
    }

    pub fn sync_queues(&self, transfers: &[TransferSnapshot], queues: &[DownloadQueueSpec]) {
        let mut queue_counts = HashMap::new();
        for transfer in transfers {
            *queue_counts.entry(transfer.queue_id.as_str()).or_insert(0) += 1;
        }
        let rows: Vec<QueueItem> = queues
            .iter()
            .map(|q| QueueItem {
                id: q.id.as_str().into(),
                name: q.name.as_str().into(),
                count: queue_counts.get(q.id.as_str()).copied().unwrap_or_default(),
                max_concurrent: q.max_concurrent as i32,
                paused: q.paused,
                post_action: post_action_label(q.post_action).into(),
            })
            .collect();
        reconcile_vec_model_by_key(
            &self.queues_model,
            rows,
            |row| row.id.as_str(),
            PartialEq::eq,
        );
        let options: Vec<SharedString> = queues.iter().map(|q| q.name.as_str().into()).collect();
        sync_vec_model_by(&self.queue_options_model, options, PartialEq::eq);
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

fn speed_sample(batch: &TelemetryBatch) -> SpeedSample {
    SpeedSample {
        timestamp_ms: batch.timestamp.timestamp_millis(),
        down_bps: batch.aggregate_down_bps,
        up_bps: batch.aggregate_up_bps,
    }
}

fn push_speed_sample(history: &mut VecDeque<SpeedSample>, sample: SpeedSample) {
    if history
        .back()
        .is_some_and(|last| sample.timestamp_ms <= last.timestamp_ms)
    {
        return;
    }
    let cutoff = sample.timestamp_ms.saturating_sub(30 * 60 * 1_000);
    while history.len() >= MAX_SPEED_SAMPLES
        || history
            .front()
            .is_some_and(|first| first.timestamp_ms < cutoff)
    {
        history.pop_front();
    }
    history.push_back(sample);
}

fn sparkline_samples(history: &VecDeque<SpeedSample>) -> Vec<u64> {
    history
        .iter()
        .skip(history.len().saturating_sub(SPARKLINE_SAMPLES))
        .map(|s| s.down_bps)
        .collect()
}

pub fn populate_speed_graph(main: &MainWindow, samples: &VecDeque<SpeedSample>) {
    let chart = view_model::build_dual_speed_chart(
        samples,
        main.get_graph_window_seconds() as u64,
        320.0,
        120.0,
    );
    main.set_speed_graph(SpeedGraphModel {
        down_line: chart.down_line_cmd.into(),
        down_fill: chart.down_fill_cmd.into(),
        up_line: chart.up_line_cmd.into(),
        up_fill: chart.up_fill_cmd.into(),
        peak_down: chart.peak_down_text.into(),
        peak_up: chart.peak_up_text.into(),
        avg_down: chart.avg_down_text.into(),
        avg_up: chart.avg_up_text.into(),
        scale_max: chart.scale_max_text.into(),
    });
}

fn update_basket(basket: &DropBasketWindow, batch: &TelemetryBatch) {
    basket.set_down_speed(format_speed(batch.aggregate_down_bps).into());
    basket.set_up_speed(format_speed(batch.aggregate_up_bps).into());
    basket.set_active_count(batch.active_count as i32);
}

fn wire_file_drop(window: &slint::Window) {
    window.on_winit_window_event(|_, event| {
        if let WindowEvent::DroppedFile(path) = event {
            let path = path.clone();
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| ctrl.handle_dropped_file(path));
            });
            EventResult::PreventDefault
        } else {
            EventResult::Propagate
        }
    });
}

fn read_batch_file(path: &Path) -> Result<String, String> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let result = (|| {
        let absolute = std::path::absolute(path)?;
        let clean = crate::paths::local_drive_path(&absolute)?;
        let parent = clean.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Select a local text file.")
        })?;
        crate::paths::validate_existing_ancestors(parent)?;
        let link_meta = std::fs::symlink_metadata(&clean)?;
        if !link_meta.is_file() || link_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Select a regular local text file.",
            ));
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&clean)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Select a regular local text file.",
            ));
        }
        if metadata.len() > MAX_TEXT_IMPORT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "The import limit is 2 MiB.",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&mut file)
            .take(MAX_TEXT_IMPORT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_TEXT_IMPORT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "The import limit is 2 MiB.",
            ));
        }
        let text = if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
            let little_endian = bytes[0] == 0xFF;
            let (chunks, remainder) = bytes[2..].as_chunks::<2>();
            if !remainder.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Incomplete UTF-16 text.",
                ));
            }
            let decode = if little_endian {
                u16::from_le_bytes
            } else {
                u16::from_be_bytes
            };
            let units: Vec<_> = chunks.iter().copied().map(decode).collect();
            String::from_utf16(&units).map_err(io::Error::other)?
        } else {
            String::from_utf8(bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "Use UTF-8 or UTF-16 text.")
            })?
        };
        Ok(text.trim_start_matches('\u{feff}').to_owned())
    })();
    result.map_err(|error: io::Error| format!("Could not import URLs: {error}"))
}

fn batch_url_count(text: &str) -> usize {
    let mut seen = HashSet::new();
    let mut in_block = false;
    let mut block_has_url = false;
    for line in text.trim_start_matches('\u{feff}').lines().map(str::trim) {
        match line {
            "<" => {
                in_block = true;
                block_has_url = false;
                continue;
            }
            ">" => {
                in_block = false;
                continue;
            }
            "" => continue,
            _ => {}
        }
        if line.starts_with('#')
            || line.split_once(':').is_some_and(|(prefix, _)| {
                ["cookie", "referer", "referrer"]
                    .iter()
                    .any(|field| prefix.eq_ignore_ascii_case(field))
            })
            || (in_block && block_has_url)
        {
            continue;
        }
        block_has_url = true;
        if line.chars().any(char::is_control) {
            continue;
        }
        if let Ok(mut url) = url::Url::parse(line) {
            if (matches!(url.scheme(), "http" | "https") && url.has_host())
                || (url.scheme() == "magnet" && parse_magnet_uri(line).is_ok())
            {
                url.set_fragment(None);
                seen.insert(url);
            }
        }
    }
    seen.len()
}

async fn bulk_operation(
    app: &AppState,
    ids: &[String],
    action: BulkAction,
) -> Result<usize, String> {
    match action {
        BulkAction::Pause => Ok(app.bulk_pause(ids).await),
        BulkAction::Resume => Ok(app.bulk_resume(ids).await),
        BulkAction::Stop => Ok(app.bulk_stop(ids).await),
        BulkAction::Remove(delete_files) => Ok(app.bulk_remove(ids, delete_files).await),
        BulkAction::Category(category) => app.set_transfers_category(ids, category).await,
        BulkAction::Queue(queue_id) => app.set_transfers_queue(ids, &queue_id).await,
        BulkAction::Limit(limit) => app.bulk_set_speed_limit(ids, limit).await,
    }
}

enum BulkAction {
    Pause,
    Resume,
    Stop,
    Remove(bool),
    Category(Option<String>),
    Queue(String),
    Limit(Option<u64>),
}

impl ControllerState {
    fn show_main_window(&mut self) {
        let _ = self.main.show();
        self.main
            .window()
            .with_winit_window(|window| window.set_minimized(false));
        apply_native_window_chrome(self.main.window());
        win32::bring_window_to_front(hwnd_of(self.main.window()));
    }

    fn sync_automation_status(&self) {
        if let Some(app) = &self.app {
            let st = app.automation_status();
            self.main.set_alt_speed_active(st.alt_speed_active);
            self.main
                .set_vpn_kill_switch_active(st.vpn_kill_switch_active);
            let (badge, tone) = if st.guard_active {
                ("Guard", 3)
            } else if st.schedule_window_active {
                ("Schedule", 1)
            } else {
                ("", 0)
            };
            self.main.set_automation_badge(badge.into());
            self.main.set_automation_badge_tone(tone);
            if let Some(limit) = st.effective_speed_limit_bps {
                self.main
                    .set_speed_limit_label(format!("{}/s", format_bytes(limit)).into());
                self.main.set_speed_limit_active(true);
            } else {
                self.main.set_speed_limit_label("Unlimited".into());
                self.main.set_speed_limit_active(false);
            }
        }
    }

    fn sync_settings_automation_models(&self) {
        let queue_rows: Vec<QueueItem> = self
            .draft_queues
            .iter()
            .map(|q| QueueItem {
                id: q.id.as_str().into(),
                name: q.name.as_str().into(),
                count: 0,
                max_concurrent: q.max_concurrent as i32,
                paused: q.paused,
                post_action: post_action_label(q.post_action).into(),
            })
            .collect();
        self.main
            .set_set_queue_items(ModelRc::from(Rc::new(VecModel::from(queue_rows))));
        let watch_rows: Vec<WatchFolderItem> = self
            .draft_watch_folders
            .iter()
            .map(|w| WatchFolderItem {
                id: w.id.as_str().into(),
                folder: w.folder.as_str().into(),
                target_save_dir: w.target_save_dir.as_deref().unwrap_or_default().into(),
                enabled: w.enabled,
                delete_after_add: w.delete_torrent_after_add,
            })
            .collect();
        self.main
            .set_set_watch_folders(ModelRc::from(Rc::new(VecModel::from(watch_rows))));
    }

    fn sync_rss_feeds(&self, rules: &[RssFeedRule]) {
        let rows = rules
            .iter()
            .map(|rule| RssFeedRuleItem {
                id: rule.id.as_str().into(),
                name: rule.name.as_str().into(),
                feed_url: rule.feed_url.as_str().into(),
                include_pattern: rule.include_pattern.as_str().into(),
                exclude_pattern: rule.exclude_pattern.as_str().into(),
                poll_interval_minutes: i32::from(rule.poll_interval_minutes),
                enabled: rule.enabled,
                auto_start: rule.auto_start,
                last_error: rule.last_error.as_deref().unwrap_or_default().into(),
            })
            .collect();
        sync_vec_model_by(&self.rss_feeds_model, rows, PartialEq::eq);
    }

    fn refresh_theme_from_settings(&mut self) {
        let (light, mono) = self
            .app
            .as_ref()
            .map(|a| resolve_theme_flags(&a.settings()))
            .unwrap_or((self.is_light, self.is_mono));
        self.apply_theme(light, mono);
        if let Some(app) = &self.app {
            let settings = app.settings();
            if self.main.get_settings_open() {
                self.sync_rss_feeds(&settings.rss_feeds);
                self.sync_network_secrets(&settings);
            }
            if self.preference_revision == self.saved_preference_revision {
                self.load_preferences(&settings);
            }
            let mut changed = false;
            if self.categories != settings.categories {
                self.categories = settings.categories;
                changed = true;
            }
            if self.queues != settings.queues {
                self.queues = settings.queues;
                changed = true;
            }
            if changed {
                self.refresh_views();
            }
        }
        self.sync_automation_status();
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
        if let Some(basket) = &self.drop_basket {
            set_win_theme!(basket, is_light, is_mono);
        }
        if let Some(banner) = &self.completion_banner {
            set_win_theme!(banner, is_light, is_mono);
        }
    }

    fn load_preferences(&mut self, settings: &AppSettings) {
        let table_changed = self.main.get_compact_table_view() != settings.compact_table_view
            || self.main.get_sort_column().as_str() != settings.sort_column
            || self.main.get_sort_ascending() != settings.sort_ascending;
        self.main
            .set_compact_table_view(settings.compact_table_view);
        self.main
            .set_sort_column(settings.sort_column.as_str().into());
        self.main.set_sort_ascending(settings.sort_ascending);
        self.set_basket_visible(settings.show_drop_basket);
        if table_changed {
            self.refresh_views();
        }
    }

    fn queue_settings_save(
        &mut self,
        edit: impl FnOnce(&mut AppSettings) + Send + 'static,
        done: impl FnOnce(&mut Self, Result<(), String>) + Send + 'static,
    ) {
        self.queue_settings_operation(
            move |app| async move { app.update_settings(edit).await },
            done,
        );
    }

    fn queue_settings_operation<F>(
        &mut self,
        operation: impl FnOnce(Arc<AppState>) -> F + Send + 'static,
        done: impl FnOnce(&mut Self, Result<(), String>) + Send + 'static,
    ) where
        F: Future<Output = Result<(), String>> + Send + 'static,
    {
        let Some(app) = self.app.clone() else { return };
        let previous = self.settings_task.take();
        self.settings_task = Some(self.runtime.spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let result = operation(app).await;
            let _ =
                slint::invoke_from_event_loop(move || with_controller(|ctrl| done(ctrl, result)));
        }));
    }

    fn persist_preferences(&mut self) {
        if self.app.is_none() {
            return;
        }
        let table = self.main.get_compact_table_view();
        let column = self.main.get_sort_column().to_string();
        let ascending = self.main.get_sort_ascending();
        let basket = self.main.get_show_drop_basket();
        self.preference_revision = self.preference_revision.wrapping_add(1);
        let revision = self.preference_revision;
        self.queue_settings_save(
            move |settings| {
                settings.compact_table_view = table;
                settings.sort_column = column;
                settings.sort_ascending = ascending;
                settings.show_drop_basket = basket;
            },
            move |ctrl, result| {
                if let Err(error) = result {
                    ctrl.show_toast(format!("Could not save preferences: {error}"), true);
                }
                if ctrl.preference_revision == revision {
                    ctrl.saved_preference_revision = revision;
                    if let Some(app) = &ctrl.app {
                        ctrl.load_preferences(&app.settings());
                    }
                }
            },
        );
    }

    fn toggle_drop_basket(&mut self) {
        self.set_basket_visible(!self.main.get_show_drop_basket());
        self.persist_preferences();
    }

    fn set_basket_visible(&mut self, visible: bool) {
        self.main.set_show_drop_basket(visible);
        if let Some(item) = &self.tray_menu_ids.drop_basket {
            item.set_checked(visible);
        }
        if !visible {
            if let Some(basket) = &self.drop_basket {
                let _ = basket.hide();
            }
            return;
        }
        if self.drop_basket.is_none() {
            match DropBasketWindow::new() {
                Ok(basket) => {
                    let weak = basket.as_weak();
                    basket.on_start_drag(move || {
                        if let Some(basket) = weak.upgrade() {
                            drag_slint_window(basket.window());
                        }
                    });
                    basket.on_show_main(|| with_controller(ControllerState::show_main_window));
                    basket.on_add_download(|| {
                        with_controller(|ctrl| {
                            ctrl.open_add_dialog_with_seed(OpenPickerEvent {
                                url: None,
                                headers: HashMap::new(),
                            });
                        })
                    });
                    basket.on_hide_basket(|| with_controller(ControllerState::toggle_drop_basket));
                    basket.window().on_close_requested(|| {
                        // Defer hiding until Slint has finished processing this window event.
                        let _ = slint::invoke_from_event_loop(|| {
                            with_controller(|ctrl| {
                                if ctrl.main.get_show_drop_basket() {
                                    ctrl.toggle_drop_basket();
                                }
                            })
                        });
                        slint::CloseRequestResponse::HideWindow
                    });
                    wire_file_drop(basket.window());
                    set_win_theme!(&basket, self.is_light, self.is_mono);
                    if let Some(batch) = &self.last_batch {
                        update_basket(&basket, batch);
                    }
                    self.drop_basket = Some(basket);
                }
                Err(error) => {
                    self.show_toast(format!("Could not create Drop Basket: {error}"), true)
                }
            }
        }
        if let Some(basket) = &self.drop_basket {
            if !basket.window().is_visible() {
                if let Err(error) = basket.show() {
                    self.show_toast(format!("Could not show Drop Basket: {error}"), true);
                } else {
                    apply_native_window_chrome(basket.window());
                    win32::position_drop_basket(hwnd_of(basket.window()));
                }
            }
        }
    }

    fn completion_alert(&mut self, alert: CompletionAlert) {
        let Some(app) = &self.app else { return };
        let settings = app.settings();
        if settings.sound_on_complete {
            win32::play_completion_sound();
        }
        if !settings.notify_on_complete {
            return;
        }
        let main_focused = self
            .main
            .window()
            .with_winit_window(|window| window.has_focus())
            .unwrap_or(false);
        if self.main.window().is_visible() && main_focused {
            self.show_toast(format!("Download complete: {}", alert.filename), false);
            return;
        }
        if self.completion_banner.is_none() {
            let Ok(banner) = CompletionBannerWindow::new() else {
                return;
            };
            let weak = banner.as_weak();
            banner.on_dismissed(move || {
                if let Some(banner) = weak.upgrade() {
                    let _ = banner.hide();
                }
            });
            self.completion_banner = Some(banner);
        }
        let banner = self.completion_banner.as_ref().unwrap();
        set_win_theme!(banner, self.is_light, self.is_mono);
        banner.set_headline("Download complete".into());
        banner.set_filename(alert.filename.into());
        if banner.show().is_ok() {
            win32::position_drop_basket(hwnd_of(banner.window()));
            if self
                .drop_basket
                .as_ref()
                .is_some_and(|basket| basket.window().is_visible())
            {
                let mut position = banner.window().position();
                position.y -= (82.0 * banner.window().scale_factor()) as i32;
                banner.window().set_position(position);
            }
            let weak = banner.as_weak();
            self.completion_timer
                .start(TimerMode::SingleShot, Duration::from_secs(6), move || {
                    if let Some(banner) = weak.upgrade() {
                        let _ = banner.hide();
                    }
                });
        }
    }

    fn refresh_speed_graph(&self) {
        if self.main.get_studio_mode() == 1 && !self.main.get_inspector_collapsed() {
            populate_speed_graph(&self.main, &self.speed_history);
        }
    }

    fn refresh_logs(&mut self) {
        if let Some(app) = &self.app {
            let logs = app.get_logs();
            if self.logs != logs {
                self.logs = logs;
                self.update_log_rows();
            }
        }
    }

    fn filtered_logs(&self) -> impl Iterator<Item = &LogEntry> {
        let level = self.main.get_log_level();
        self.logs
            .iter()
            .rev()
            .filter(move |entry| level == "All" || entry.level.eq_ignore_ascii_case(level.as_str()))
    }

    fn update_log_rows(&self) {
        let rows = self
            .filtered_logs()
            .map(|entry| LogEntryModel {
                timestamp: entry
                    .timestamp
                    .with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string()
                    .into(),
                level: entry.level.to_ascii_uppercase().into(),
                category: entry.category.as_str().into(),
                message: entry.message.as_str().into(),
            })
            .collect();
        sync_vec_model_by(&self.models.log_entries_model, rows, PartialEq::eq);
    }

    fn log_action(&mut self, action: &str) {
        let Some(app) = self.app.clone() else { return };
        match action {
            "Copy" => {
                let text = self
                    .filtered_logs()
                    .map(|entry| {
                        format!(
                            "{} [{}] [{}] {}",
                            entry.timestamp.to_rfc3339(),
                            entry.level,
                            entry.category,
                            entry.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\r\n");
                match win32::copy_to_clipboard(&text) {
                    Ok(()) => self.show_toast("Copied log entries", false),
                    Err(error) => self.show_toast(format!("Could not copy logs: {error}"), true),
                }
            }
            "Clear" => {
                app.clear_logs();
                self.refresh_logs();
                self.show_toast("Logs cleared", false);
            }
            "Export .log…" => {
                let owner = hwnd_of(self.main.window());
                self.runtime.spawn(async move {
                    let path = tokio::task::spawn_blocking(move || {
                        win32::pick_text_export_file(owner, "slipstream.log")
                    })
                    .await;
                    if let Ok(Some(path)) = path {
                        let result = app.export_logs(&path).await;
                        let _ = slint::invoke_from_event_loop(move || {
                            with_controller(|ctrl| match result {
                                Ok(count) => {
                                    ctrl.show_toast(format!("Exported {count} log entries"), false)
                                }
                                Err(error) => {
                                    ctrl.show_toast(format!("Could not export logs: {error}"), true)
                                }
                            })
                        });
                    }
                });
            }
            _ => {}
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
        let active_queue = self.main.get_active_queue_filter();
        if !active_queue.is_empty() && !self.queues.iter().any(|q| q.id == active_queue.as_str()) {
            self.main.set_active_queue_filter("".into());
        }
        self.models.apply_telemetry_snapshot(
            &self.main,
            batch,
            &sparkline_samples(&self.speed_history),
            &self.expanded_playlists,
            &mut self.selection,
            &self.categories,
        );
        self.models.sync_queues(&batch.transfers, &self.queues);
        self.sync_automation_status();
        self.refresh_inspector();
        if let Some(id) = self.selection.selected_id.clone() {
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
        let id = self.selection.selected_id.as_deref().unwrap_or_default();
        let peers = self.torrent_peers.get(id);
        let previous = self.main.get_inspector();
        if previous.id != id {
            self.refresh_inspector_files();
            self.models.update_inspector_peers(peers);
        }
        self.models
            .update_inspector(&self.main, self.transfer(id), peers);
        let updated = self.main.get_inspector();
        if previous.id != updated.id || previous.category_id != updated.category_id {
            let index = self
                .categories
                .iter()
                .position(|category| category.id == updated.category_id.as_str());
            self.main
                .set_inspector_category_index(index.map_or(0, |index| index as i32 + 1));
        }
        if previous.id != updated.id || previous.tags_summary != updated.tags_summary {
            self.main.set_inspector_tags(updated.tags_summary);
        }
        if previous.id != updated.id {
            self.main.set_add_tracker_input("".into());
            self.main.set_torrent_upload_limit_input("".into());
            self.main.set_torrent_upload_limit_loaded(false);
            self.main
                .set_torrent_action_pending(self.torrent_actions_pending.contains(id));
        }
    }

    fn refresh_inspector_files(&self) {
        let id = self.selection.selected_id.as_deref().unwrap_or_default();
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
        if let Some(basket) = &self.drop_basket {
            update_basket(basket, &batch);
        }
        if self.main.get_studio_mode() == 2 {
            self.refresh_logs();
        }

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
            && !remove_id.is_empty()
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
                            if ctrl.selection.selected_id.as_deref() == Some(&id) {
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
        if let Some(id) = self.selection.selected_id.clone() {
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
            let load_upload_limit = self.selection.selected_id.as_deref() == Some(&id)
                && !self.main.get_torrent_upload_limit_loaded();
            let app = app.clone();
            self.runtime.spawn(async move {
                let result = app.get_torrent_peers(&id).await;
                let upload_limit = if load_upload_limit {
                    app.torrent_upload_limit(&id).await.ok()
                } else {
                    None
                };
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.torrent_peers_pending.remove(&id);
                        if let Ok(summary) = result {
                            if let Some(entry) = ctrl.progress_windows.get(&id) {
                                entry.models.update_peers(&entry.window, &summary);
                            }
                            if ctrl.transfer(&id).is_some() {
                                ctrl.torrent_peers.insert(id.clone(), summary);
                                if ctrl.selection.selected_id.as_deref() == Some(&id) {
                                    ctrl.models
                                        .update_inspector_peers(ctrl.torrent_peers.get(&id));
                                    ctrl.refresh_inspector();
                                }
                            }
                        }
                        if let Some(limit) = upload_limit {
                            if ctrl.selection.selected_id.as_deref() == Some(&id)
                                && !ctrl.main.get_torrent_upload_limit_loaded()
                            {
                                ctrl.main
                                    .set_torrent_upload_limit_input(format_upload_kib(limit));
                                ctrl.main.set_torrent_upload_limit_loaded(true);
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
        match self.pending_picker_requests.pop_front() {
            Some(PickerRequest::Source(event)) => self.open_add_dialog_with_seed(event),
            Some(PickerRequest::BatchText(text)) => self.open_batch_text(text),
            None => {}
        }
    }

    fn import_metalink(&self) {
        if !self.main.get_add_open() || self.main.get_add_submitting() || self.app.is_none() {
            return;
        }
        let generation = self.add_state.generation;
        let owner = hwnd_of(self.main.window());
        self.main.set_add_submitting(true);
        self.main.set_add_error_text("".into());
        std::thread::spawn(move || {
            let picked = win32::pick_metalink_file(owner);
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if !ctrl.main.get_add_open() || ctrl.add_state.generation != generation {
                        return;
                    }
                    let Some((path, app)) = picked.zip(ctrl.app.clone()) else {
                        ctrl.main.set_add_submitting(false);
                        return;
                    };
                    let save_dir = optional_text(&ctrl.main.get_add_save_dir());
                    let queue_id = ctrl
                        .queues
                        .get(ctrl.main.get_add_queue_index().max(0) as usize)
                        .map(|queue| queue.id.clone());
                    ctrl.runtime.spawn(async move {
                        let result = async {
                            let entries = app.parse_metalink_file(Path::new(&path)).await?;
                            app.enqueue_metalink_entries(entries, save_dir, queue_id)
                                .await
                        }
                        .await;
                        let _ = slint::invoke_from_event_loop(move || {
                            with_controller(|ctrl| {
                                if !ctrl.main.get_add_open()
                                    || ctrl.add_state.generation != generation
                                {
                                    return;
                                }
                                if let Ok(ids) = &result {
                                    ctrl.show_toast(
                                        format!("Queued {} Metalink files.", ids.len()),
                                        false,
                                    );
                                }
                                ctrl.finish_add_submission(generation, result.map(|_| ()));
                            });
                        });
                    });
                });
            });
        });
    }

    fn open_site_grabber(&mut self) {
        if self.app.is_none() || self.main.get_grabber_open() {
            return;
        }
        self.grabber_session_id = self.grabber_session_id.wrapping_add(1);
        self.main.set_grabber_target_url("".into());
        self.main.set_grabber_depth_index(0);
        self.main.set_grabber_same_origin_only(true);
        self.main.set_grabber_extensions("".into());
        self.main.set_grabber_include_pattern("".into());
        self.main.set_grabber_exclude_pattern("".into());
        self.main.set_grabber_scanning(false);
        self.main.set_grabber_enqueuing(false);
        self.main.set_grabber_error_text("".into());
        self.main.set_grabber_scanned_count_text("".into());
        self.grabber_items_model.set_vec(Vec::new());
        self.main
            .set_grabber_items(self.grabber_items_model.clone().into());
        self.update_grabber_selection_count();
        self.main.set_grabber_open(true);
    }

    fn close_site_grabber(&mut self) {
        self.main.set_grabber_open(false);
        if let Some(scan) = self.grabber_scan.take() {
            scan.abort();
        }
    }

    fn grabber_session_is_open(&self, session: u64) -> bool {
        self.main.get_grabber_open() && self.grabber_session_id == session
    }

    fn scan_site_grabber(&mut self) {
        let Some(app) = self.app.clone() else { return };
        if !self.main.get_grabber_open()
            || self.main.get_grabber_scanning()
            || self.main.get_grabber_enqueuing()
        {
            return;
        }
        let request = SiteGrabRequest {
            url: self.main.get_grabber_target_url().trim().to_owned(),
            max_depth: self.main.get_grabber_depth_index().clamp(0, 2) as u8,
            same_origin_only: self.main.get_grabber_same_origin_only(),
            extensions: view_model::split_list(&self.main.get_grabber_extensions()),
            include_pattern: self.main.get_grabber_include_pattern().trim().to_owned(),
            exclude_pattern: self.main.get_grabber_exclude_pattern().trim().to_owned(),
        };
        if request.url.is_empty() {
            return;
        }
        self.main.set_grabber_scanning(true);
        self.main.set_grabber_error_text("".into());
        self.main.set_grabber_scanned_count_text("".into());
        self.grabber_items_model.set_vec(Vec::new());
        self.update_grabber_selection_count();
        let session = self.grabber_session_id;
        self.grabber_scan = Some(self.runtime.spawn(async move {
            let result = app.grab_site_urls(request).await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if !ctrl.grabber_session_is_open(session) {
                        return;
                    }
                    ctrl.grabber_scan = None;
                    ctrl.main.set_grabber_scanning(false);
                    match result {
                        Ok(result) => {
                            ctrl.main.set_grabber_scanned_count_text(
                                format!(
                                    "{} {} scanned · {} files found",
                                    result.scanned_pages,
                                    if result.scanned_pages == 1 {
                                        "page"
                                    } else {
                                        "pages"
                                    },
                                    result.items.len(),
                                )
                                .into(),
                            );
                            ctrl.grabber_items_model.set_vec(
                                result
                                    .items
                                    .into_iter()
                                    .map(view_model::site_grab_row)
                                    .collect::<Vec<_>>(),
                            );
                            ctrl.update_grabber_selection_count();
                        }
                        Err(error) => ctrl.main.set_grabber_error_text(error.into()),
                    }
                });
            });
        }));
    }

    fn update_grabber_selection_count(&self) {
        let count = self
            .grabber_items_model
            .iter()
            .filter(|row| row.checked)
            .count();
        self.main.set_grabber_selected_count(count as i32);
    }

    fn enqueue_grabber_selection(&mut self) {
        let Some(app) = self.app.clone() else { return };
        if !self.main.get_grabber_open()
            || self.main.get_grabber_scanning()
            || self.main.get_grabber_enqueuing()
        {
            return;
        }
        let selected: Vec<_> = self
            .grabber_items_model
            .iter()
            .enumerate()
            .filter(|(_, row)| row.checked)
            .map(|(index, row)| (index, row.url.to_string(), row.filename.to_string()))
            .collect();
        if selected.is_empty() {
            return;
        }
        self.main.set_grabber_enqueuing(true);
        self.main.set_grabber_error_text("".into());
        let session = self.grabber_session_id;
        self.runtime.spawn(async move {
            let mut queued = Vec::new();
            let mut failure = None;
            for (index, url, filename) in selected {
                let request = AddHttpDownloadRequest {
                    url,
                    filename: Some(filename),
                    ..Default::default()
                };
                match app.add_http_download(request).await {
                    Ok(_) => queued.push(index),
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if !ctrl.grabber_session_is_open(session) {
                        return;
                    }
                    ctrl.main.set_grabber_enqueuing(false);
                    if let Some(error) = failure {
                        ctrl.main.set_grabber_error_text(
                            format!("Queued {} files before an error: {error}", queued.len())
                                .into(),
                        );
                        for index in queued.into_iter().rev() {
                            ctrl.grabber_items_model.remove(index);
                        }
                        ctrl.update_grabber_selection_count();
                    } else {
                        ctrl.show_toast(
                            format!("Queued {} files from Site Grabber.", queued.len()),
                            false,
                        );
                        ctrl.close_site_grabber();
                    }
                });
            });
        });
    }

    fn queue_picker_request(&mut self, request: PickerRequest) {
        const MAX_PENDING_PICKER_REQUESTS: usize = 16;
        if self.pending_picker_requests.len() < MAX_PENDING_PICKER_REQUESTS {
            self.pending_picker_requests.push_back(request);
            let waiting = self.pending_picker_requests.len();
            self.show_toast(
                format!("Queued download dialog request ({waiting} waiting)"),
                false,
            );
        } else {
            self.show_toast("The download dialog queue is full.", true);
        }
    }

    fn open_batch_text(&mut self, text: String) {
        if self.main.get_add_open() {
            self.show_main_window();
            self.queue_picker_request(PickerRequest::BatchText(text));
            return;
        }
        self.open_add_dialog_with_seed(OpenPickerEvent {
            url: None,
            headers: HashMap::new(),
        });
        self.main.set_add_tab(4);
        self.set_batch_text(text.into());
    }

    fn set_batch_text(&self, text: SharedString) {
        self.main
            .set_add_batch_url_count(batch_url_count(&text) as i32);
        self.main.set_add_batch_text(text);
        self.main.set_add_error_text("".into());
    }

    fn handle_dropped_file(&mut self, path: PathBuf) {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("torrent") {
            if let Some(source) = path.to_str() {
                self.open_add_dialog_with_seed(OpenPickerEvent {
                    url: Some(source.to_owned()),
                    headers: HashMap::new(),
                });
            } else {
                self.show_main_window();
                self.show_toast(
                    "The torrent file path contains unsupported characters.",
                    true,
                );
            }
        } else if extension.eq_ignore_ascii_case("txt") || extension.eq_ignore_ascii_case("ef2") {
            self.import_batch_file(Some(path));
        } else {
            self.show_main_window();
            self.show_toast(
                "Drop a .torrent, .txt, or .ef2 file to add downloads.",
                false,
            );
        }
    }

    fn import_batch_file(&self, dropped: Option<PathBuf>) {
        let generation = self.add_state.generation;
        let owner = hwnd_of(self.main.window());
        self.runtime.spawn_blocking(move || {
            let import_into_open_dialog = dropped.is_none();
            let Some(path) =
                dropped.or_else(|| win32::pick_text_import_file(owner).map(PathBuf::from))
            else {
                return;
            };
            let result = read_batch_file(&path);
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if import_into_open_dialog
                        && (!ctrl.main.get_add_open()
                            || ctrl.main.get_add_submitting()
                            || ctrl.add_state.generation != generation
                            || ctrl.main.get_add_tab() != 4)
                    {
                        return;
                    }
                    match result {
                        Ok(text) if import_into_open_dialog => ctrl.set_batch_text(text.into()),
                        Ok(text) => ctrl.open_batch_text(text),
                        Err(error) => {
                            ctrl.show_main_window();
                            ctrl.show_toast(error, true);
                        }
                    }
                })
            });
        });
    }

    fn category_at(&self, index: i32) -> Option<String> {
        index
            .checked_sub(1)
            .and_then(|index| self.categories.get(index as usize))
            .map(|category| category.id.clone())
    }

    fn submit_batch_urls(&mut self) {
        let Some(app) = self.app.clone() else { return };
        let raw = self.main.get_add_batch_text().to_string();
        if self.main.get_add_batch_url_count() == 0 {
            self.main
                .set_add_error_text("Enter at least one URL or magnet link.".into());
            return;
        }
        let folder = self.main.get_add_batch_save_dir().trim().to_owned();
        let category = self.category_at(self.main.get_add_batch_category_index());
        let paused = self.main.get_add_batch_start_paused();
        let generation = self.add_state.generation;
        self.main.set_add_submitting(true);
        self.main.set_add_error_text("".into());
        self.runtime.spawn(async move {
            let result = app
                .add_batch_urls(
                    &raw,
                    (!folder.is_empty()).then_some(folder),
                    category,
                    paused,
                )
                .await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| match result {
                    Ok(summary) => {
                        ctrl.show_toast(
                            format!(
                                "Added {} downloads, skipped {}",
                                summary.added, summary.skipped
                            ),
                            summary.added == 0,
                        );
                        if ctrl.add_state.generation == generation {
                            if summary.added > 0 {
                                ctrl.finish_add_submission(generation, Ok(()));
                            } else {
                                let error = summary
                                    .errors
                                    .into_iter()
                                    .next()
                                    .unwrap_or_else(|| "No downloads were added.".into());
                                ctrl.finish_add_submission(generation, Err(error));
                            }
                        }
                    }
                    Err(error) => ctrl.finish_add_submission(generation, Err(error)),
                })
            });
        });
    }

    fn open_add_dialog_with_seed(&mut self, event: OpenPickerEvent) {
        if self.main.get_add_open() {
            self.show_main_window();
            if event.url.is_some()
                && event.url.as_deref() != Some(self.main.get_add_source_text().as_str())
            {
                self.queue_picker_request(PickerRequest::Source(event));
            }
            return;
        }
        self.close_settings_dialog();
        self.close_site_grabber();
        self.main.set_create_torrent_open(false);
        self.main.set_palette_open(false);
        self.main.set_refresh_open(false);
        self.main.set_verify_open(false);
        self.main.set_remove_confirm_open(false);
        self.main.set_context_menu_open(false);
        self.main.set_speed_popover_open(false);
        self.main.set_add_tab(0);
        self.set_batch_text("".into());
        self.main.set_add_batch_save_dir("".into());
        self.main.set_add_batch_category_index(0);
        self.main.set_add_batch_start_paused(false);
        let default_queue_idx = self
            .queues
            .iter()
            .position(|q| q.id == "default")
            .unwrap_or(0) as i32;
        self.main.set_add_queue_index(default_queue_idx);
        self.main.set_add_torrent_start_paused(false);
        self.main.set_add_filename_override("".into());
        self.main.set_add_expected_sha256("".into());
        self.main.set_add_mirror_urls_text("".into());
        self.main.set_add_media_clip_start("".into());
        self.main.set_add_media_clip_end("".into());
        self.main.set_add_media_split_chapters(false);
        self.main.set_add_media_sponsorblock_text("".into());

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

    fn run_torrent_power_action(&mut self, id: String, action: TorrentPowerAction) {
        let Some(app) = self.app.clone() else { return };
        let Some(transfer) = self
            .transfer(&id)
            .filter(|t| t.kind == TransferKind::Torrent)
        else {
            return;
        };
        let picker = matches!(action, TorrentPowerAction::MoveStorage)
            .then(|| (hwnd_of(self.main.window()), transfer.save_dir.clone()));
        if !self.torrent_actions_pending.insert(id.clone()) {
            return;
        }
        self.main.set_torrent_action_pending(true);
        self.runtime.spawn(async move {
            let (verb, result) = match &action {
                TorrentPowerAction::Recheck => (
                    "recheck the torrent",
                    app.force_recheck_torrent(&id)
                        .await
                        .map(|()| Some("Torrent recheck requested")),
                ),
                TorrentPowerAction::MoveStorage => {
                    let (owner, initial) = picker.expect("move action has a folder picker");
                    let picked = tokio::task::spawn_blocking(move || {
                        win32::pick_folder(
                            owner,
                            Some(initial),
                            "Select the torrent destination folder",
                        )
                    })
                    .await;
                    let result = match picked {
                        Ok(Ok(Some(path))) => app
                            .move_torrent_storage(&id, Path::new(&path))
                            .await
                            .map(|()| Some("Torrent storage moved")),
                        Ok(Ok(None)) => Ok(None),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    ("move torrent storage", result)
                }
                TorrentPowerAction::AddTracker(tracker) => (
                    "add the tracker",
                    app.add_torrent_trackers(&id, vec![tracker.clone()])
                        .await
                        .map(|()| Some("Tracker added")),
                ),
                TorrentPowerAction::Reannounce => (
                    "reannounce the torrent",
                    app.reannounce_torrent(&id)
                        .await
                        .map(|()| Some("Torrent reannounced")),
                ),
                TorrentPowerAction::UploadLimit(rate) => (
                    "set the upload limit",
                    app.set_torrent_upload_limit(&id, *rate)
                        .await
                        .map(|()| Some("Upload limit updated")),
                ),
            };
            let batch = if matches!(result, Ok(Some(_))) {
                Some(app.snapshot().await)
            } else {
                None
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    ctrl.torrent_actions_pending.remove(&id);
                    let selected = ctrl.selection.selected_id.as_deref() == Some(&id);
                    if selected {
                        ctrl.main.set_torrent_action_pending(false);
                    }
                    match result {
                        Ok(Some(notice)) => {
                            if selected {
                                match action {
                                    TorrentPowerAction::AddTracker(_) => {
                                        ctrl.main.set_add_tracker_input("".into())
                                    }
                                    TorrentPowerAction::UploadLimit(rate) => {
                                        ctrl.main.set_torrent_upload_limit_input(
                                            format_upload_kib(rate),
                                        );
                                        ctrl.main.set_torrent_upload_limit_loaded(true);
                                    }
                                    _ => {}
                                }
                            }
                            if let Some(batch) = batch {
                                ctrl.apply_telemetry(batch);
                            }
                            ctrl.show_toast(notice, false);
                        }
                        Ok(None) => {}
                        Err(error) => ctrl.show_toast(format!("Could not {verb}: {error}"), true),
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

    fn export_selected_urls(&self) {
        let Some(app) = self.app.clone() else { return };
        let ids = self.selection.ordered_ids();
        if ids.is_empty() {
            return;
        }
        let owner = hwnd_of(self.main.window());
        self.runtime.spawn(async move {
            let path = tokio::task::spawn_blocking(move || {
                win32::pick_text_export_file(owner, "slipstream-urls.txt")
            })
            .await;
            if let Ok(Some(path)) = path {
                let result = app.export_transfer_urls(&ids, &path).await;
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| match result {
                        Ok(count) => ctrl.show_toast(format!("Exported {count} URLs"), false),
                        Err(error) => {
                            ctrl.show_toast(format!("Could not export URLs: {error}"), true)
                        }
                    })
                });
            }
        });
    }

    fn dispatch_bulk_action(&self, ids: Vec<String>, action: BulkAction) {
        let Some(app) = self.app.clone() else { return };
        let requested = ids.len();
        let runtime = self.runtime.clone();
        self.runtime.spawn_blocking(move || {
            // Batch 1's bulk futures use borrowing async closures. Drive them on this worker.
            let removing = matches!(action, BulkAction::Remove(_));
            let (result, batch) = runtime.block_on(async {
                let result = bulk_operation(&app, &ids, action).await;
                (result, app.snapshot().await)
            });
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if removing {
                        let remaining: HashSet<_> =
                            batch.transfers.iter().map(|row| row.id.as_str()).collect();
                        for id in &ids {
                            if !remaining.contains(id.as_str()) {
                                ctrl.close_progress_dialog(id);
                                ctrl.pending_transfer_dialogs
                                    .remove(&(id.clone(), TransferDialog::Complete));
                                if let Some(entry) = ctrl.complete_windows.remove(id) {
                                    let _ = entry.window.hide();
                                }
                            }
                        }
                    }
                    ctrl.apply_telemetry(batch);
                    match result {
                        Ok(count) => ctrl.show_toast(
                            format!("Updated {count} of {requested} transfers"),
                            count == 0,
                        ),
                        Err(error) => ctrl.show_toast(error, true),
                    }
                })
            });
        });
    }

    fn handle_bulk_action(&mut self, action: &str, value: &str) {
        if action == "Export URLs" {
            self.export_selected_urls();
            return;
        }
        let ids = self.selection.ordered_ids();
        if ids.len() < 2 {
            return;
        }
        let operation = match action {
            "Pause" => BulkAction::Pause,
            "Resume" => BulkAction::Resume,
            "Stop" => BulkAction::Stop,
            "Remove…" => {
                self.main.set_remove_confirm_id("".into());
                self.main.set_remove_confirm_filename(
                    format!("{} selected transfers", ids.len()).into(),
                );
                self.pending_bulk_remove = ids;
                self.main.set_remove_confirm_open(true);
                self.main.invoke_focus_remove_confirm();
                return;
            }
            "Limit" => match parse_speed_limit_slider_mbps(value) {
                Ok(limit) => {
                    self.main.set_bulk_editor(0);
                    self.main.set_bulk_limit("".into());
                    BulkAction::Limit(limit)
                }
                Err(error) => {
                    self.show_toast(error, true);
                    return;
                }
            },
            _ => return,
        };
        self.dispatch_bulk_action(ids, operation);
    }

    fn save_inspector_metadata(&mut self) {
        let (Some(app), Some(id)) = (self.app.clone(), self.selection.selected_id.clone()) else {
            return;
        };
        if self.main.get_inspector_metadata_saving() {
            return;
        }
        let category = self.category_at(self.main.get_inspector_category_index());
        let tags: Vec<_> = self
            .main
            .get_inspector_tags()
            .split(',')
            .map(str::trim)
            .map(str::to_owned)
            .collect();
        let change_category = category.as_deref()
            != self
                .transfer(&id)
                .and_then(|transfer| transfer.category_id.as_deref());
        self.main.set_inspector_metadata_saving(true);
        self.runtime.spawn(async move {
            let ids = [id];
            let result = async {
                if change_category {
                    app.set_transfers_category(&ids, category).await?;
                }
                app.set_transfers_tags(&ids, tags).await
            }
            .await;
            let batch = app.snapshot().await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    ctrl.main.set_inspector_metadata_saving(false);
                    ctrl.apply_telemetry(batch);
                    match result {
                        Ok(1) => {
                            if ctrl.selection.selected_id.as_deref() == Some(ids[0].as_str()) {
                                let inspector = ctrl.main.get_inspector();
                                ctrl.main.set_inspector_tags(inspector.tags_summary);
                                let index = ctrl.categories.iter().position(|category| {
                                    category.id == inspector.category_id.as_str()
                                });
                                ctrl.main.set_inspector_category_index(
                                    index.map_or(0, |index| index as i32 + 1),
                                );
                            }
                            ctrl.show_toast("Category and tags saved", false);
                        }
                        Ok(_) => ctrl.show_toast("The transfer is no longer available.", true),
                        Err(error) => ctrl
                            .show_toast(format!("Could not save category and tags: {error}"), true),
                    }
                })
            });
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
        if id.is_empty() {
            self.main.set_remove_confirm_open(false);
            let ids = std::mem::take(&mut self.pending_bulk_remove);
            if !ids.is_empty() {
                self.dispatch_bulk_action(ids, BulkAction::Remove(delete_files));
            }
            return;
        }
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
        if self.main.get_add_submitting() {
            return;
        }
        if self.main.get_add_tab() == 4 {
            self.submit_batch_urls();
            return;
        }
        let Some(app) = &self.app else { return };
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
        let selected_queue_id = self
            .queues
            .get(self.main.get_add_queue_index().max(0) as usize)
            .map(|q| q.id.clone());
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
                            queue_id: selected_queue_id.clone(),
                            headers: headers.clone(),
                            referrer: None,
                            expected_sha256: None,
                            expected_size_bytes: None,
                            start_paused: false,
                            overwrite_existing: false,
                            ..AddHttpDownloadRequest::default()
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

        let mirror_urls = if tab == 0 {
            view_model::split_list(&self.main.get_add_mirror_urls_text())
        } else {
            Vec::new()
        };
        let use_media = self.add_state.probe.is_some()
            || tab == 1
            || tab == 2
            || !looks_like_direct_file_url(&source);

        if use_media && !self.main.get_add_is_direct_http() && mirror_urls.is_empty() {
            let (clip_start_seconds, clip_end_seconds) = match view_model::parse_clip_range(
                &self.main.get_add_media_clip_start(),
                &self.main.get_add_media_clip_end(),
            ) {
                Ok(Some((start, end))) => (Some(start), Some(end)),
                Ok(None) => (None, None),
                Err(error) => {
                    self.finish_add_submission(expected_gen, Err(error.into()));
                    return;
                }
            };
            let sponsorblock_categories =
                view_model::split_list(&self.main.get_add_media_sponsorblock_text());
            if sponsorblock_categories.iter().any(|category| {
                !slipstream_media::args::SPONSORBLOCK_CATEGORIES.contains(&category.as_str())
            }) {
                self.finish_add_submission(
                    expected_gen,
                    Err("Unknown SponsorBlock category.".into()),
                );
                return;
            }
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
                clip_start_seconds,
                clip_end_seconds,
                split_chapters: self.main.get_add_media_split_chapters(),
                sponsorblock_categories,
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
            queue_id: selected_queue_id,
            headers,
            referrer: None,
            expected_sha256: sha,
            expected_size_bytes: None,
            start_paused: false,
            overwrite_existing: false,
            mirror_urls,
            ..AddHttpDownloadRequest::default()
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

    fn settings_session_is_open(&self, session: u64) -> bool {
        self.main.get_settings_open() && self.settings_session_id == session
    }

    fn close_settings_dialog(&self) {
        self.main.set_settings_open(false);
        self.main.set_set_proxy_password("".into());
        self.main.set_set_site_login_password("".into());
    }

    fn sync_network_secrets(&self, settings: &AppSettings) {
        self.main
            .set_set_proxy_has_password(settings.proxy.has_password);
        let rows = settings
            .site_logins
            .iter()
            .map(view_model::site_login_row)
            .collect();
        sync_vec_model_by(&self.site_logins_model, rows, PartialEq::eq);
    }

    fn load_network_settings(&self, settings: &AppSettings) {
        let proxy = &settings.proxy;
        self.main.set_set_proxy_mode_index(match proxy.mode {
            ProxyMode::Direct => 0,
            ProxyMode::System => 1,
            ProxyMode::Manual => 2,
        });
        self.main
            .set_set_proxy_url(proxy.url.as_deref().unwrap_or_default().into());
        self.main
            .set_set_proxy_username(proxy.username.as_deref().unwrap_or_default().into());
        self.main
            .set_set_proxy_bypass_hosts(proxy.bypass_hosts.join(", ").into());
        self.main.set_set_proxy_apply_to_media(proxy.apply_to_media);
        self.main.set_set_proxy_password("".into());
        self.reset_site_login_form();
        self.main.set_set_network_editing(false);
        self.sync_network_secrets(settings);
        self.main
            .set_set_site_logins(self.site_logins_model.clone().into());

        let cookies = &settings.cookie_passthrough;
        self.main.set_set_cookies_enabled(cookies.enabled);
        let browser = NETWORK_COOKIE_BROWSERS
            .iter()
            .position(|&browser| browser == cookies.browser);
        self.main
            .set_set_cookies_browser_index(browser.unwrap_or(0) as i32);
        self.main
            .set_set_cookies_allowed_hosts(cookies.allowed_hosts.join(", ").into());
        self.main
            .set_set_auto_discover_checksums(settings.auto_discover_checksums);

        let media = &settings.media_power;
        self.main.set_set_media_embed_metadata(media.embed_metadata);
        self.main
            .set_set_media_embed_thumbnail(media.embed_thumbnail);
        self.main.set_set_media_embed_chapters(media.embed_chapters);
        self.main.set_set_media_split_chapters(media.split_chapters);
        self.main
            .set_set_media_filename_template(media.filename_template.as_str().into());
        self.main
            .set_set_media_sponsorblock_text(media.sponsorblock_remove.join(", ").into());
    }

    fn read_network_settings(&self, settings: &mut AppSettings) -> Result<(), String> {
        let proxy = &mut settings.proxy;
        proxy.mode = match self.main.get_set_proxy_mode_index() {
            1 => ProxyMode::System,
            2 => ProxyMode::Manual,
            _ => ProxyMode::Direct,
        };
        proxy.url = optional_text(&self.main.get_set_proxy_url());
        proxy.username = optional_text(&self.main.get_set_proxy_username());
        proxy.bypass_hosts = view_model::split_list(&self.main.get_set_proxy_bypass_hosts());
        proxy.apply_to_media = self.main.get_set_proxy_apply_to_media();

        let cookies = &mut settings.cookie_passthrough;
        cookies.enabled = self.main.get_set_cookies_enabled();
        cookies.browser = NETWORK_COOKIE_BROWSERS
            [self.main.get_set_cookies_browser_index().clamp(0, 3) as usize]
            .into();
        cookies.allowed_hosts = view_model::split_list(&self.main.get_set_cookies_allowed_hosts());
        settings.auto_discover_checksums = self.main.get_set_auto_discover_checksums();

        let media = &mut settings.media_power;
        media.embed_metadata = self.main.get_set_media_embed_metadata();
        media.embed_thumbnail = self.main.get_set_media_embed_thumbnail();
        media.embed_chapters = self.main.get_set_media_embed_chapters();
        media.split_chapters = self.main.get_set_media_split_chapters();
        media.filename_template = self
            .main
            .get_set_media_filename_template()
            .trim()
            .to_owned();
        media.sponsorblock_remove =
            view_model::split_list(&self.main.get_set_media_sponsorblock_text());
        crate::settings::normalize_phase4_network_and_media(settings)
            .map_err(|error| error.to_string())
    }

    fn reset_site_login_form(&self) {
        self.main.set_set_site_login_host("".into());
        self.main.set_set_site_login_scheme_index(0);
        self.main.set_set_site_login_username("".into());
        self.main.set_set_site_login_password("".into());
    }

    fn edit_network_secrets<F>(
        &mut self,
        operation: impl FnOnce(Arc<AppState>) -> F + Send + 'static,
        reset_login: bool,
    ) where
        F: Future<Output = Result<(), String>> + Send + 'static,
    {
        if self.app.is_none()
            || !self.main.get_settings_open()
            || self.main.get_set_saving()
            || self.main.get_set_network_editing()
        {
            return;
        }
        self.main.set_set_network_editing(true);
        self.main.set_set_error_text("".into());
        let session = self.settings_session_id;
        self.queue_settings_operation(operation, move |ctrl, result| {
            if !ctrl.settings_session_is_open(session) {
                return;
            }
            ctrl.main.set_set_network_editing(false);
            match result {
                Ok(()) => {
                    if let Some(app) = &ctrl.app {
                        ctrl.sync_network_secrets(&app.settings());
                    }
                    if reset_login {
                        ctrl.reset_site_login_form();
                    }
                }
                Err(error) => ctrl.main.set_set_error_text(error.into()),
            }
        });
    }

    fn load_bittorrent_settings(&self, settings: &AppSettings) {
        let network = &settings.torrent_network;
        let seeding = &settings.torrent_seeding;
        self.main
            .set_set_bt_listen_port_start(network.listen_port_start.to_string().into());
        self.main
            .set_set_bt_listen_port_end(network.listen_port_end.to_string().into());
        self.main.set_set_bt_enable_dht(network.enable_dht);
        self.main.set_set_bt_enable_upnp(network.enable_upnp);
        self.main
            .set_set_bt_global_upload_kbps(format_upload_kib(network.global_upload_limit_bps));
        self.main.set_set_bt_socks5_proxy(
            network
                .socks5_proxy_url
                .as_deref()
                .unwrap_or_default()
                .into(),
        );
        self.main.set_set_bt_blocklist_path(
            network.blocklist_path.as_deref().unwrap_or_default().into(),
        );
        self.main
            .set_set_bt_vpn_kill_switch(network.vpn_kill_switch_enabled);
        self.main.set_set_bt_vpn_adapter_options(
            Rc::new(VecModel::from(vec![SharedString::from(CUSTOM_VPN_ADAPTER)])).into(),
        );
        self.main.set_set_bt_vpn_adapter_index(0);
        self.main.set_set_bt_vpn_adapter_custom(
            network
                .vpn_adapter_name
                .as_deref()
                .unwrap_or_default()
                .into(),
        );
        self.main.set_set_bt_adapters_refreshing(false);
        self.main.set_set_bt_max_seed_ratio(
            view_model::format_seed_ratio_hundredths(seeding.max_seed_ratio_hundredths).into(),
        );
        self.main.set_set_bt_max_seed_minutes(
            seeding
                .max_seed_minutes
                .map(|minutes| minutes.to_string())
                .unwrap_or_default()
                .into(),
        );
        self.main
            .set_set_bt_stop_seeding_on_limit(seeding.stop_seeding_on_limit);
        self.main
            .set_set_rss_feeds(self.rss_feeds_model.clone().into());
        self.sync_rss_feeds(&settings.rss_feeds);
        self.reset_new_rss_feed();
        self.main.set_set_rss_polling(false);
        self.main.set_set_rss_editing(false);
        self.set_rss_notice("", false);
    }

    fn selected_vpn_adapter(&self) -> SharedString {
        let index = self.main.get_set_bt_vpn_adapter_index();
        if index > 0 {
            if let Some(adapter) = self
                .main
                .get_set_bt_vpn_adapter_options()
                .row_data(index as usize)
            {
                return adapter;
            }
        }
        self.main.get_set_bt_vpn_adapter_custom()
    }

    fn refresh_vpn_adapters(&self) {
        let Some(app) = self.app.clone() else { return };
        if self.main.get_set_bt_adapters_refreshing() {
            return;
        }
        self.main.set_set_bt_adapters_refreshing(true);
        let session = self.settings_session_id;
        self.runtime.spawn(async move {
            let adapters = app.list_network_adapters().await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if !ctrl.settings_session_is_open(session) {
                        return;
                    }
                    let selected = ctrl.selected_vpn_adapter();
                    let index = adapters.iter().position(|name| name == selected.as_str());
                    let options: Vec<SharedString> =
                        std::iter::once(SharedString::from(CUSTOM_VPN_ADAPTER))
                            .chain(adapters.into_iter().map(SharedString::from))
                            .collect();
                    if index.is_none() {
                        ctrl.main.set_set_bt_vpn_adapter_custom(selected);
                    }
                    ctrl.main
                        .set_set_bt_vpn_adapter_options(Rc::new(VecModel::from(options)).into());
                    ctrl.main
                        .set_set_bt_vpn_adapter_index(index.map_or(0, |index| index as i32 + 1));
                    ctrl.main.set_set_bt_adapters_refreshing(false);
                });
            });
        });
    }

    fn browse_blocklist(&self) {
        let owner = hwnd_of(self.main.window());
        let session = self.settings_session_id;
        std::thread::spawn(move || {
            if let Some(path) = win32::pick_blocklist_file(owner) {
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        if ctrl.settings_session_is_open(session) {
                            ctrl.main.set_set_bt_blocklist_path(path.into());
                        }
                    });
                });
            }
        });
    }

    fn read_bittorrent_settings(&self, settings: &mut AppSettings) -> Result<(), &'static str> {
        let network = &mut settings.torrent_network;
        network.listen_port_start = self
            .main
            .get_set_bt_listen_port_start()
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port == 0 || (1024..u16::MAX).contains(port))
            .ok_or("Enter 0 to disable listening or a start port from 1024 to 65534.")?;
        network.listen_port_end = if network.listen_port_start == 0 {
            0
        } else {
            self.main
                .get_set_bt_listen_port_end()
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|&port| port >= network.listen_port_start)
                .ok_or("Enter an end port at or above the start port, up to 65535.")?
        };
        network.enable_dht = self.main.get_set_bt_enable_dht();
        network.enable_upnp = self.main.get_set_bt_enable_upnp();
        let upload = self.main.get_set_bt_global_upload_kbps();
        if upload != format_upload_kib(network.global_upload_limit_bps) {
            network.global_upload_limit_bps = parse_upload_kib(&upload)?;
        }
        network.socks5_proxy_url = optional_text(&self.main.get_set_bt_socks5_proxy());
        network.blocklist_path = optional_text(&self.main.get_set_bt_blocklist_path());
        network.vpn_kill_switch_enabled = self.main.get_set_bt_vpn_kill_switch();
        network.vpn_adapter_name = optional_text(&self.selected_vpn_adapter());

        let seeding = &mut settings.torrent_seeding;
        seeding.max_seed_ratio_hundredths =
            view_model::parse_seed_ratio_hundredths(&self.main.get_set_bt_max_seed_ratio())?;
        seeding.max_seed_minutes = optional_text(&self.main.get_set_bt_max_seed_minutes())
            .map(|raw| {
                raw.parse::<u32>()
                    .ok()
                    .filter(|&minutes| minutes <= 525_600)
                    .ok_or("Enter a whole seeding duration up to 525600 minutes.")
            })
            .transpose()?
            .filter(|&minutes| minutes != 0);
        seeding.stop_seeding_on_limit = self.main.get_set_bt_stop_seeding_on_limit();
        Ok(())
    }

    fn set_rss_notice(&self, message: impl Into<SharedString>, error: bool) {
        self.main.set_set_rss_notice(message.into());
        self.main.set_set_rss_notice_error(error);
    }

    fn reset_new_rss_feed(&self) {
        self.main.set_set_new_rss_name("".into());
        self.main.set_set_new_rss_url("".into());
        self.main.set_set_new_rss_include("".into());
        self.main.set_set_new_rss_exclude("".into());
        self.main.set_set_new_rss_interval("30".into());
    }

    fn add_rss_feed(&mut self) {
        let Some(name) = optional_text(&self.main.get_set_new_rss_name()) else {
            self.set_rss_notice("Enter a feed name.", true);
            return;
        };
        let Some(feed_url) = optional_text(&self.main.get_set_new_rss_url()) else {
            self.set_rss_notice("Enter a feed URL.", true);
            return;
        };
        let interval = self.main.get_set_new_rss_interval();
        let Ok(poll_interval_minutes @ 5..=1440) = interval.trim().parse::<u16>() else {
            self.set_rss_notice("Enter a polling interval from 5 to 1440 minutes.", true);
            return;
        };
        let rule = RssFeedRule {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            feed_url,
            include_pattern: self.main.get_set_new_rss_include().trim().to_owned(),
            exclude_pattern: self.main.get_set_new_rss_exclude().trim().to_owned(),
            save_dir: None,
            poll_interval_minutes,
            enabled: true,
            auto_start: true,
            last_checked_at: None,
            last_error: None,
        };
        self.edit_rss_feeds(move |rules| rules.push(rule), true);
    }

    fn rss_busy(&self) -> bool {
        self.main.get_set_saving()
            || self.main.get_set_rss_polling()
            || self.main.get_set_rss_editing()
    }

    fn edit_rss_feeds(
        &mut self,
        edit: impl FnOnce(&mut Vec<RssFeedRule>) + Send + 'static,
        added: bool,
    ) {
        if self.app.is_none() || self.rss_busy() {
            return;
        }
        let session = self.settings_session_id;
        self.main.set_set_rss_editing(true);
        self.set_rss_notice("", false);
        self.queue_settings_save(
            move |settings| edit(&mut settings.rss_feeds),
            move |ctrl, result| {
                if !ctrl.settings_session_is_open(session) {
                    return;
                }
                ctrl.main.set_set_rss_editing(false);
                match result {
                    Ok(()) => {
                        if added {
                            ctrl.reset_new_rss_feed();
                        }
                        if let Some(app) = &ctrl.app {
                            ctrl.sync_rss_feeds(&app.settings().rss_feeds);
                        }
                    }
                    Err(error) => ctrl.set_rss_notice(error, true),
                }
            },
        );
    }

    fn poll_rss_now(&self) {
        let Some(app) = self.app.clone() else { return };
        if self.rss_busy() {
            return;
        }
        let session = self.settings_session_id;
        self.main.set_set_rss_polling(true);
        self.set_rss_notice("", false);
        self.runtime.spawn(async move {
            let result = app.poll_rss_feeds_now().await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if !ctrl.settings_session_is_open(session) {
                        return;
                    }
                    ctrl.main.set_set_rss_polling(false);
                    ctrl.sync_rss_feeds(&app.settings().rss_feeds);
                    match result {
                        Ok(1) => ctrl.set_rss_notice("Added 1 torrent.", false),
                        Ok(count) => ctrl.set_rss_notice(format!("Added {count} torrents."), false),
                        Err(error) => ctrl.set_rss_notice(error, true),
                    }
                });
            });
        });
    }

    fn open_create_torrent(&mut self) {
        if self.app.is_none() || self.main.get_create_torrent_open() {
            return;
        }
        self.create_torrent_session_id = self.create_torrent_session_id.wrapping_add(1);
        self.main.set_create_torrent_source_path("".into());
        self.main.set_create_torrent_output_path("".into());
        self.main.set_create_torrent_trackers_text("".into());
        self.main
            .set_create_torrent_piece_size_index(view_model::piece_length_index_from_kib(0));
        self.main.set_create_torrent_private_torrent(false);
        self.main.set_create_torrent_comment("".into());
        self.main.set_create_torrent_error_text("".into());
        self.main.set_create_torrent_success_text("".into());
        self.main.set_create_torrent_creating(false);
        self.main.set_create_torrent_open(true);
    }

    fn create_torrent_session_is_open(&self, session: u64) -> bool {
        self.main.get_create_torrent_open() && self.create_torrent_session_id == session
    }

    fn browse_create_torrent(&self, picker: CreateTorrentPicker) {
        if self.main.get_create_torrent_creating() {
            return;
        }
        let session = self.create_torrent_session_id;
        let owner = hwnd_of(self.main.window());
        let source = self.main.get_create_torrent_source_path().to_string();
        std::thread::spawn(move || {
            let result = match picker {
                CreateTorrentPicker::SourceFile => Ok(win32::pick_torrent_source_file(owner)),
                CreateTorrentPicker::SourceFolder => win32::pick_folder(
                    owner,
                    optional_text(&source),
                    "Select the torrent source folder",
                ),
                CreateTorrentPicker::Output => {
                    let name = Path::new(source.trim())
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("download");
                    Ok(win32::pick_save_torrent_file(
                        owner,
                        &format!("{name}.torrent"),
                    ))
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if !ctrl.create_torrent_session_is_open(session) {
                        return;
                    }
                    match result {
                        Ok(Some(path)) => {
                            match picker {
                                CreateTorrentPicker::SourceFile
                                | CreateTorrentPicker::SourceFolder => {
                                    ctrl.main.set_create_torrent_source_path(path.into());
                                }
                                CreateTorrentPicker::Output => {
                                    ctrl.main.set_create_torrent_output_path(path.into());
                                    ctrl.main.set_create_torrent_success_text("".into());
                                }
                            }
                            ctrl.main.set_create_torrent_error_text("".into());
                        }
                        Ok(None) => {}
                        Err(error) => ctrl
                            .main
                            .set_create_torrent_error_text(error.to_string().into()),
                    }
                });
            });
        });
    }

    fn create_torrent(&self) {
        let Some(app) = self.app.clone() else { return };
        if self.main.get_create_torrent_creating()
            || !self.main.get_create_torrent_success_text().is_empty()
        {
            return;
        }
        let request = CreateTorrentRequest {
            source_path: self.main.get_create_torrent_source_path().trim().to_owned(),
            output_path: self.main.get_create_torrent_output_path().trim().to_owned(),
            trackers: self
                .main
                .get_create_torrent_trackers_text()
                .split(['\n', ';', ','])
                .filter_map(optional_text)
                .collect(),
            piece_length_kib: view_model::piece_length_kib_from_index(
                self.main.get_create_torrent_piece_size_index(),
            ),
            private: self.main.get_create_torrent_private_torrent(),
            comment: optional_text(&self.main.get_create_torrent_comment()),
        };
        if request.source_path.is_empty() || request.output_path.is_empty() {
            self.main
                .set_create_torrent_error_text("Choose a source and an output file.".into());
            return;
        }
        let session = self.create_torrent_session_id;
        self.main.set_create_torrent_creating(true);
        self.main.set_create_torrent_error_text("".into());
        self.runtime.spawn(async move {
            let result = app.create_torrent_file(request).await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if !ctrl.create_torrent_session_is_open(session) {
                        return;
                    }
                    ctrl.main.set_create_torrent_creating(false);
                    match result {
                        Ok(torrent) => ctrl.main.set_create_torrent_success_text(
                            format!(
                                "Created {} ({} files, {} pieces).",
                                torrent.output_path, torrent.file_count, torrent.piece_count
                            )
                            .into(),
                        ),
                        Err(error) => ctrl.main.set_create_torrent_error_text(error.into()),
                    }
                });
            });
        });
    }

    fn open_settings_dialog(&mut self) {
        if let Some(app) = &self.app {
            let s = app.settings();
            self.load_bittorrent_settings(&s);
            self.load_network_settings(&s);
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
            self.main.set_set_launch_on_startup(s.launch_on_startup);
            self.main.set_set_launch_minimized(s.launch_minimized);
            self.main.set_set_notify_on_complete(s.notify_on_complete);
            self.main.set_set_sound_on_complete(s.sound_on_complete);
            self.main
                .set_set_show_drop_basket(self.main.get_show_drop_basket());
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

            // Automation tab fields
            self.main.set_set_alt_speed_enabled(s.alt_speed_enabled);
            self.main.set_set_alt_speed_kbps(
                (s.alt_speed_limit_bps.unwrap_or(512 * 1024) / 1024)
                    .max(16)
                    .to_string()
                    .into(),
            );
            self.main
                .set_set_schedule_mode_index(match s.schedule.mode {
                    ScheduleMode::Disabled => 0,
                    ScheduleMode::OffPeakWindow => 1,
                    ScheduleMode::SnailWindow => 2,
                });
            self.main.set_set_schedule_start(
                format_minute_of_day(s.schedule.start_minute_of_day).into(),
            );
            self.main
                .set_set_schedule_end(format_minute_of_day(s.schedule.end_minute_of_day).into());
            self.main
                .set_set_schedule_weekdays(s.schedule.weekdays_only);
            self.main.set_set_schedule_limit_kbps(
                (s.schedule.window_speed_limit_bps.unwrap_or(512 * 1024) / 1024)
                    .max(16)
                    .to_string()
                    .into(),
            );
            self.main
                .set_set_schedule_pause_outside(s.schedule.post_window_pause);
            self.main
                .set_set_schedule_stop_torrents(s.schedule.stop_torrents_at_end);
            self.main.set_set_guard_enabled(s.bandwidth_guard.enabled);
            self.main
                .set_set_guard_fullscreen(s.bandwidth_guard.throttle_on_fullscreen);
            self.main
                .set_set_guard_pause(s.bandwidth_guard.pause_instead_of_throttle);
            self.main.set_set_guard_limit_kbps(
                (s.bandwidth_guard.guard_speed_limit_bps / 1024)
                    .max(16)
                    .to_string()
                    .into(),
            );
            self.main
                .set_set_guard_processes(s.bandwidth_guard.watched_processes.join(", ").into());
            self.settings_session_id = self.settings_session_id.wrapping_add(1);
            self.main.set_set_saving(false);
            self.settings_initial_alt_speed = s.alt_speed_enabled;
            self.draft_queues = s.queues;
            self.draft_watch_folders = s.watch_folders;
            self.sync_settings_automation_models();
            self.main.set_set_new_queue_name("".into());
            self.main.set_set_new_queue_concurrent("2".into());
            self.main.set_set_new_queue_post_action(0);
            self.main.set_set_watch_delete_after_add(false);
            self.main
                .set_set_auto_extract_enabled(s.auto_extract.enabled);
            self.main
                .set_set_auto_extract_subfolder(s.auto_extract.create_subfolder);
            self.main
                .set_set_auto_extract_overwrite(s.auto_extract.overwrite_existing);
            self.main
                .set_set_auto_extract_delete(s.auto_extract.delete_archive_after_extract);
            self.main.set_set_backup_notice("".into());

            self.main.set_set_error_text("".into());
            self.refresh_toolchain_and_browsers();
        }
        self.main.set_settings_open(true);
        self.refresh_vpn_adapters();
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
        let session = self.settings_session_id;
        self.runtime.spawn(async move {
            let status = app_c.get_toolchain_status().await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| {
                    if ctrl.settings_session_is_open(session) {
                        ctrl.apply_toolchain_status(status);
                    }
                });
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
        if self.main.get_set_saving()
            || self.main.get_set_rss_editing()
            || self.main.get_set_network_editing()
        {
            return;
        }
        let mut s = app.settings();
        if let Err(error) = self.read_bittorrent_settings(&mut s) {
            self.main.set_set_settings_tab(2);
            self.main.set_set_error_text(error.into());
            return;
        }
        if let Err(error) = self.read_network_settings(&mut s) {
            self.main.set_set_settings_tab(3);
            self.main.set_set_error_text(error.into());
            return;
        }
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
        s.launch_on_startup = self.main.get_set_launch_on_startup();
        s.launch_minimized = self.main.get_set_launch_minimized();
        s.notify_on_complete = self.main.get_set_notify_on_complete();
        s.sound_on_complete = self.main.get_set_sound_on_complete();
        s.show_drop_basket = self.main.get_set_show_drop_basket();
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

        let parse_kbps = |raw: &str, fallback: u64| -> Result<u64, String> {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(fallback);
            }
            let val: u64 = trimmed
                .parse()
                .map_err(|_| "Speed limits in KiB/s must be whole numbers.".to_owned())?;
            if val == 0 {
                return Err("Speed limits in KiB/s must be at least 16 KiB/s.".into());
            }
            Ok((val.clamp(16, 4_194_303)) * 1024)
        };
        let alt_bps = match parse_kbps(&self.main.get_set_alt_speed_kbps(), 512 * 1024) {
            Ok(v) => v,
            Err(msg) => {
                self.main.set_set_error_text(msg.into());
                return;
            }
        };
        let sched_start = match parse_minute_of_day(&self.main.get_set_schedule_start()) {
            Ok(m) => m,
            Err(msg) => {
                self.main.set_set_error_text(msg.into());
                return;
            }
        };
        let sched_end = match parse_minute_of_day(&self.main.get_set_schedule_end()) {
            Ok(m) => m,
            Err(msg) => {
                self.main.set_set_error_text(msg.into());
                return;
            }
        };
        let sched_bps = match parse_kbps(&self.main.get_set_schedule_limit_kbps(), 512 * 1024) {
            Ok(v) => v,
            Err(msg) => {
                self.main.set_set_error_text(msg.into());
                return;
            }
        };
        let guard_bps = match parse_kbps(&self.main.get_set_guard_limit_kbps(), 256 * 1024) {
            Ok(v) => v,
            Err(msg) => {
                self.main.set_set_error_text(msg.into());
                return;
            }
        };

        s.alt_speed_enabled = self.main.get_set_alt_speed_enabled();
        s.alt_speed_limit_bps = Some(alt_bps);
        s.schedule.mode = match self.main.get_set_schedule_mode_index() {
            1 => ScheduleMode::OffPeakWindow,
            2 => ScheduleMode::SnailWindow,
            _ => ScheduleMode::Disabled,
        };
        s.schedule.start_minute_of_day = sched_start;
        s.schedule.end_minute_of_day = sched_end;
        s.schedule.weekdays_only = self.main.get_set_schedule_weekdays();
        s.schedule.window_speed_limit_bps = Some(sched_bps);
        s.schedule.post_window_pause = self.main.get_set_schedule_pause_outside();
        s.schedule.stop_torrents_at_end = self.main.get_set_schedule_stop_torrents();
        s.bandwidth_guard.enabled = self.main.get_set_guard_enabled();
        s.bandwidth_guard.throttle_on_fullscreen = self.main.get_set_guard_fullscreen();
        s.bandwidth_guard.pause_instead_of_throttle = self.main.get_set_guard_pause();
        s.bandwidth_guard.guard_speed_limit_bps = guard_bps;
        s.bandwidth_guard.watched_processes = self
            .main
            .get_set_guard_processes()
            .split(',')
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect();
        s.queues = self.draft_queues.clone();
        s.watch_folders = self.draft_watch_folders.clone();
        s.auto_extract = slipstream_protocol::AutoExtractSettings {
            enabled: self.main.get_set_auto_extract_enabled(),
            create_subfolder: self.main.get_set_auto_extract_subfolder(),
            delete_archive_after_extract: self.main.get_set_auto_extract_delete(),
            overwrite_existing: self.main.get_set_auto_extract_overwrite(),
        };

        if s.monitor_clipboard {
            self.last_clipboard_seq = win32::clipboard_sequence_number();
        }

        self.main.set_set_saving(true);
        self.main.set_set_error_text("".into());
        let session = self.settings_session_id;
        let alt_speed_changed = s.alt_speed_enabled != self.settings_initial_alt_speed;
        let password = self.main.get_set_proxy_password();
        let password = (!password.is_empty()).then(|| Some(password.to_string()));
        self.queue_settings_operation(
            move |app| async move {
                app.update_settings_with_proxy_password(password, move |settings| {
                    s.compact_table_view = settings.compact_table_view;
                    s.sort_column.clone_from(&settings.sort_column);
                    s.sort_ascending = settings.sort_ascending;
                    s.categories.clone_from(&settings.categories);
                    s.rss_feeds = std::mem::take(&mut settings.rss_feeds);
                    s.site_logins = std::mem::take(&mut settings.site_logins);
                    if !alt_speed_changed {
                        s.alt_speed_enabled = settings.alt_speed_enabled;
                    }
                    for q in &mut s.queues {
                        if let Some(live) = settings.queues.iter().find(|live| live.id == q.id) {
                            q.paused = live.paused;
                        }
                    }
                    *settings = s;
                })
                .await
            },
            move |ctrl, result| {
                if !ctrl.settings_session_is_open(session) {
                    return;
                }
                ctrl.main.set_set_saving(false);
                match result {
                    Ok(()) => {
                        ctrl.close_settings_dialog();
                        ctrl.refresh_theme_from_settings();
                    }
                    Err(error) => ctrl.main.set_set_error_text(error.into()),
                }
            },
        );
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
            ("export_urls", "Export URLs", ""),
            ("check_updates", "Check for Updates", ""),
            ("hibernate", "Hibernate to System Tray", ""),
        ];
        let models: Vec<PaletteActionModel> = all
            .into_iter()
            .filter(|(id, label, _)| {
                (*id != "export_urls" || !self.selection.selected_ids.is_empty())
                    && (q.is_empty() || label.to_ascii_lowercase().contains(&q))
            })
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

    main.on_table_view_changed(|compact| {
        with_controller(|ctrl| {
            if ctrl.main.get_compact_table_view() != compact {
                ctrl.main.set_compact_table_view(compact);
                ctrl.refresh_views();
                ctrl.persist_preferences();
            }
        })
    });
    main.on_sort_changed(|column| {
        with_controller(|ctrl| {
            let column = view_model::transfer_sort_column(&column);
            let ascending = if ctrl.main.get_sort_column() == column {
                !ctrl.main.get_sort_ascending()
            } else {
                matches!(column, "name" | "eta" | "status" | "category")
            };
            ctrl.main.set_sort_column(column.into());
            ctrl.main.set_sort_ascending(ascending);
            ctrl.refresh_views();
            ctrl.persist_preferences();
        })
    });
    main.on_category_filter_changed(|category| {
        with_controller(|ctrl| {
            let next = if ctrl.main.get_active_category_filter() == category {
                "".into()
            } else {
                category
            };
            ctrl.main.set_active_category_filter(next);
            ctrl.refresh_views();
        })
    });
    main.on_tag_filter_changed(|tag| {
        with_controller(|ctrl| {
            let next = if ctrl.main.get_active_tag_filter() == tag {
                "".into()
            } else {
                tag
            };
            ctrl.main.set_active_tag_filter(next);
            ctrl.refresh_views();
        })
    });
    main.on_select_all_visible(|| {
        with_controller(|ctrl| {
            ctrl.selection.select_all();
            ctrl.refresh_views();
        })
    });
    main.on_clear_selection(|| {
        with_controller(|ctrl| {
            ctrl.selection.clear();
            ctrl.refresh_views();
        })
    });
    main.on_bulk_action(|action, value| {
        with_controller(|ctrl| ctrl.handle_bulk_action(&action, &value))
    });
    main.on_bulk_category(|index| {
        with_controller(|ctrl| {
            let ids = ctrl.selection.ordered_ids();
            if ids.len() > 1 {
                ctrl.main.set_bulk_editor(0);
                ctrl.dispatch_bulk_action(ids, BulkAction::Category(ctrl.category_at(index)));
            }
        })
    });
    main.on_apply_inspector_metadata(|| with_controller(ControllerState::save_inspector_metadata));
    main.on_toggle_drop_basket(|| with_controller(ControllerState::toggle_drop_basket));
    main.on_studio_mode_changed(|mode| {
        with_controller(|ctrl| {
            ctrl.main.set_studio_mode(mode);
            match mode {
                1 => ctrl.refresh_speed_graph(),
                2 => ctrl.refresh_logs(),
                _ => {}
            }
        })
    });
    main.on_graph_window_changed(|seconds| {
        with_controller(|ctrl| {
            ctrl.main.set_graph_window_seconds(seconds);
            ctrl.refresh_speed_graph();
        })
    });
    main.on_log_filter_changed(|level| {
        with_controller(|ctrl| {
            ctrl.main.set_log_level(level);
            ctrl.refresh_logs();
            ctrl.update_log_rows();
        })
    });
    main.on_log_action(|action| with_controller(|ctrl| ctrl.log_action(&action)));

    main.on_toggle_theme_clicked(|| {
        with_controller(|ctrl| {
            let next_light = !ctrl.is_light;
            ctrl.apply_theme(next_light, ctrl.is_mono);
            ctrl.queue_settings_save(
                move |settings| {
                    settings.theme = if next_light {
                        ThemeMode::Light
                    } else {
                        ThemeMode::Dark
                    }
                },
                |ctrl, result| {
                    if let Err(error) = result {
                        ctrl.refresh_theme_from_settings();
                        ctrl.show_toast(format!("Could not save the theme: {error}"), true);
                    }
                },
            );
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
    main.on_open_grabber_clicked(|| with_controller(ControllerState::open_site_grabber));
    main.on_grabber_close_requested(|| with_controller(ControllerState::close_site_grabber));
    main.on_grabber_scan_clicked(|| with_controller(ControllerState::scan_site_grabber));
    main.on_grabber_enqueue_selected_clicked(|| {
        with_controller(ControllerState::enqueue_grabber_selection)
    });
    main.on_grabber_select_all_clicked(|checked| {
        with_controller(|ctrl| {
            for (index, mut row) in ctrl.grabber_items_model.iter().enumerate() {
                if row.checked != checked {
                    row.checked = checked;
                    ctrl.grabber_items_model.set_row_data(index, row);
                }
            }
            ctrl.update_grabber_selection_count();
        });
    });
    main.on_grabber_toggle_item_clicked(|index| {
        with_controller(|ctrl| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            if let Some(mut row) = ctrl.grabber_items_model.row_data(index) {
                row.checked = !row.checked;
                ctrl.grabber_items_model.set_row_data(index, row);
                ctrl.update_grabber_selection_count();
            }
        });
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
            ctrl.selection
                .click(&id, win32::is_ctrl_pressed(), win32::is_shift_pressed());
            ctrl.main.set_context_menu_open(false);
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
            if ctrl.selection.selected_ids.contains(id.as_str()) {
                ctrl.selection.selected_id = Some(id.to_string());
            } else {
                ctrl.selection.select_only(&id);
            }
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
            let Some(id) = ctrl.selection.selected_id.clone() else {
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
            let Some(id) = ctrl.selection.selected_id.clone() else {
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

    main.on_force_recheck_torrent(|id| {
        with_controller(|ctrl| {
            ctrl.run_torrent_power_action(id.to_string(), TorrentPowerAction::Recheck)
        });
    });
    main.on_move_torrent_storage(|id| {
        with_controller(|ctrl| {
            ctrl.run_torrent_power_action(id.to_string(), TorrentPowerAction::MoveStorage)
        });
    });
    main.on_add_torrent_tracker(|id, tracker| {
        if let Some(tracker) = optional_text(&tracker) {
            with_controller(|ctrl| {
                ctrl.run_torrent_power_action(
                    id.to_string(),
                    TorrentPowerAction::AddTracker(tracker),
                )
            });
        }
    });
    main.on_reannounce_torrent(|id| {
        with_controller(|ctrl| {
            ctrl.run_torrent_power_action(id.to_string(), TorrentPowerAction::Reannounce)
        });
    });
    main.on_apply_torrent_upload_limit(|id, raw| {
        with_controller(|ctrl| {
            if !ctrl.main.get_torrent_upload_limit_loaded() {
                return;
            }
            match parse_upload_kib(&raw) {
                Ok(limit) => ctrl.run_torrent_power_action(
                    id.to_string(),
                    TorrentPowerAction::UploadLimit(limit),
                ),
                Err(error) => ctrl.show_toast(error, true),
            }
        });
    });

    main.on_add_close_requested(|| with_controller(ControllerState::close_add_dialog));

    main.on_add_batch_edited(|| {
        with_controller(|ctrl| ctrl.set_batch_text(ctrl.main.get_add_batch_text()))
    });
    main.on_add_batch_import(|| with_controller(|ctrl| ctrl.import_batch_file(None)));
    main.on_add_import_metalink_clicked(|| with_controller(|ctrl| ctrl.import_metalink()));

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
        let mut batch = false;
        with_controller(|ctrl| {
            owner = hwnd_of(ctrl.main.window());
            batch = ctrl.main.get_add_tab() == 4;
            initial = if batch {
                let current = ctrl.main.get_add_batch_save_dir();
                if current.trim().is_empty() {
                    ctrl.app.as_ref().map_or_else(String::new, |app| {
                        let settings = app.settings();
                        ctrl.category_at(ctrl.main.get_add_batch_category_index())
                            .map_or_else(
                                || settings.default_download_dir.clone(),
                                |id| {
                                    resolve_category_dir(&settings, &id)
                                        .to_string_lossy()
                                        .into_owned()
                                },
                            )
                    })
                } else {
                    current.to_string()
                }
            } else {
                ctrl.main.get_add_save_dir().to_string()
            };
            generation = ctrl.add_state.generation;
        });
        std::thread::spawn(move || {
            if let Ok(Some(path)) = win32::pick_download_folder(owner, Some(initial)) {
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        if ctrl.main.get_add_open() && ctrl.add_state.generation == generation {
                            if batch {
                                ctrl.main.set_add_batch_save_dir(path.into());
                            } else {
                                ctrl.main.set_add_save_dir(path.into());
                            }
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
        with_controller(|ctrl| {
            if !ctrl.main.get_set_saving() {
                ctrl.close_settings_dialog();
            }
        });
    });

    main.on_settings_save_clicked(|| {
        with_controller(|ctrl| ctrl.save_settings());
    });
    main.on_settings_clear_proxy_password_clicked(|| {
        with_controller(|ctrl| {
            ctrl.main.set_set_proxy_password("".into());
            if ctrl.main.get_set_proxy_has_password() {
                ctrl.edit_network_secrets(
                    |app| async move { app.set_proxy_password(None).await },
                    false,
                );
            }
        });
    });
    main.on_settings_add_site_login_clicked(|| {
        with_controller(|ctrl| {
            let host_pattern = ctrl.main.get_set_site_login_host().trim().to_owned();
            let password = ctrl.main.get_set_site_login_password();
            if host_pattern.is_empty() || password.is_empty() {
                return;
            }
            let entry = SiteLoginEntry {
                host_pattern,
                username: ctrl.main.get_set_site_login_username().trim().to_owned(),
                auth_scheme: if ctrl.main.get_set_site_login_scheme_index() == 1 {
                    SiteAuthScheme::Bearer
                } else {
                    SiteAuthScheme::Basic
                },
                ..Default::default()
            };
            let password = password.to_string();
            ctrl.edit_network_secrets(
                move |app| async move { app.upsert_site_login(entry, Some(password)).await },
                true,
            );
        });
    });
    main.on_settings_delete_site_login_clicked(|id| {
        with_controller(|ctrl| {
            let id = id.to_string();
            ctrl.edit_network_secrets(
                move |app| async move { app.delete_site_login(&id).await },
                false,
            );
        });
    });

    main.on_settings_browse_blocklist_clicked(|| with_controller(|ctrl| ctrl.browse_blocklist()));
    main.on_settings_refresh_vpn_adapters_clicked(|| {
        with_controller(|ctrl| ctrl.refresh_vpn_adapters());
    });
    main.on_settings_add_rss_feed_clicked(|| with_controller(ControllerState::add_rss_feed));
    main.on_settings_remove_rss_feed_clicked(|id| {
        let id = id.to_string();
        with_controller(|ctrl| {
            ctrl.edit_rss_feeds(move |rules| rules.retain(|rule| rule.id != id), false)
        });
    });
    main.on_settings_toggle_rss_feed_clicked(|id| {
        let id = id.to_string();
        with_controller(|ctrl| {
            ctrl.edit_rss_feeds(
                move |rules| {
                    if let Some(rule) = rules.iter_mut().find(|rule| rule.id == id) {
                        rule.enabled = !rule.enabled;
                    }
                },
                false,
            );
        });
    });
    main.on_settings_poll_rss_now_clicked(|| with_controller(|ctrl| ctrl.poll_rss_now()));
    main.on_settings_open_create_torrent_clicked(|| {
        with_controller(ControllerState::open_create_torrent)
    });
    main.on_create_torrent_browse_source_file_clicked(|| {
        with_controller(|ctrl| ctrl.browse_create_torrent(CreateTorrentPicker::SourceFile));
    });
    main.on_create_torrent_browse_source_folder_clicked(|| {
        with_controller(|ctrl| ctrl.browse_create_torrent(CreateTorrentPicker::SourceFolder));
    });
    main.on_create_torrent_browse_output_clicked(|| {
        with_controller(|ctrl| ctrl.browse_create_torrent(CreateTorrentPicker::Output));
    });
    main.on_create_torrent_create_clicked(|| with_controller(|ctrl| ctrl.create_torrent()));
    main.on_create_torrent_close_requested(|| {
        with_controller(|ctrl| {
            if !ctrl.main.get_create_torrent_creating() {
                ctrl.main.set_create_torrent_open(false);
            }
        });
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
            ctrl.close_settings_dialog();
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

    main.on_toggle_alt_speed_clicked(|| {
        with_controller(|ctrl| {
            let Some(app) = ctrl.app.clone() else { return };
            ctrl.runtime.spawn(async move {
                let res = app.toggle_alt_speed().await;
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| match res {
                        Ok(enabled) => {
                            ctrl.refresh_theme_from_settings();
                            ctrl.show_toast(
                                if enabled {
                                    "Snail / Alt Speed enabled"
                                } else {
                                    "Snail / Alt Speed disabled"
                                },
                                false,
                            );
                        }
                        Err(err) => ctrl.show_toast(format!("Alt Speed error: {err}"), true),
                    });
                });
            });
        });
    });

    main.on_queue_filter_changed(|queue_id| {
        with_controller(|ctrl| {
            let next = if ctrl.main.get_active_queue_filter() == queue_id {
                "".into()
            } else {
                queue_id
            };
            ctrl.main.set_active_queue_filter(next);
            ctrl.refresh_views();
        });
    });

    main.on_toggle_queue_paused(|queue_id, pause| {
        with_controller(|ctrl| {
            let Some(app) = ctrl.app.clone() else { return };
            let qid = queue_id.to_string();
            ctrl.runtime.spawn(async move {
                let res = if pause {
                    app.pause_queue(&qid).await
                } else {
                    app.start_queue(&qid).await
                };
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| match res {
                        Ok(()) => ctrl.refresh_theme_from_settings(),
                        Err(err) => ctrl.show_toast(format!("Queue update failed: {err}"), true),
                    });
                });
            });
        });
    });

    main.on_bulk_queue(|index| {
        with_controller(|ctrl| {
            let ids = ctrl.selection.ordered_ids();
            let qid = ctrl.queues.get(index.max(0) as usize).map(|q| q.id.clone());
            if ids.len() > 1 {
                if let Some(queue_id) = qid {
                    ctrl.main.set_bulk_editor(0);
                    ctrl.dispatch_bulk_action(ids, BulkAction::Queue(queue_id));
                }
            }
        });
    });

    main.on_settings_add_queue(|name, concurrent_raw, action_idx| {
        with_controller(|ctrl| {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                ctrl.main
                    .set_set_error_text("Enter a queue name before adding.".into());
                return;
            }
            let mut base_id = crate::settings::category_id(trimmed);
            if base_id.is_empty() {
                base_id = "queue".into();
            }
            base_id.truncate(28);
            let mut id = base_id.clone();
            let mut suffix = 2usize;
            while ctrl.draft_queues.iter().any(|q| q.id == id) {
                id = format!("{base_id}-{suffix}");
                suffix += 1;
            }
            let max_concurrent = concurrent_raw
                .trim()
                .parse::<usize>()
                .unwrap_or(2)
                .clamp(1, 16);
            ctrl.draft_queues.push(DownloadQueueSpec {
                id,
                name: trimmed.to_owned(),
                max_concurrent,
                speed_limit_bps: None,
                post_action: post_action_from_index(action_idx),
                paused: false,
            });
            ctrl.main.set_set_new_queue_name("".into());
            ctrl.main.set_set_error_text("".into());
            ctrl.sync_settings_automation_models();
        });
    });

    main.on_settings_remove_queue(|queue_id| {
        with_controller(|ctrl| {
            if queue_id.as_str() == "default" {
                return;
            }
            ctrl.draft_queues.retain(|q| q.id != queue_id.as_str());
            ctrl.sync_settings_automation_models();
        });
    });

    main.on_settings_add_watch_folder(|delete_after_add| {
        let mut owner = 0_isize;
        let mut expected_session = 0_u64;
        with_controller(|ctrl| {
            owner = hwnd_of(ctrl.main.window());
            expected_session = ctrl.settings_session_id;
        });
        std::thread::spawn(move || {
            if let Ok(Some(folder)) = win32::pick_download_folder(owner, None) {
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        if !ctrl.main.get_settings_open()
                            || ctrl.settings_session_id != expected_session
                        {
                            return;
                        }
                        let base = Path::new(&folder)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(crate::settings::category_id)
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "watch".into());
                        let mut id = base.clone();
                        let mut suffix = 2usize;
                        while ctrl.draft_watch_folders.iter().any(|w| w.id == id) {
                            id = format!("{base}-{suffix}");
                            suffix += 1;
                        }
                        ctrl.draft_watch_folders.push(WatchFolderRule {
                            id,
                            folder,
                            target_save_dir: None,
                            queue_id: None,
                            category_id: None,
                            enabled: true,
                            delete_torrent_after_add: delete_after_add,
                            start_paused: false,
                        });
                        ctrl.sync_settings_automation_models();
                    });
                });
            }
        });
    });

    main.on_settings_remove_watch_folder(|rule_id| {
        with_controller(|ctrl| {
            ctrl.draft_watch_folders
                .retain(|w| w.id != rule_id.as_str());
            ctrl.sync_settings_automation_models();
        });
    });

    main.on_settings_export_backup(|| {
        let mut owner = 0_isize;
        let mut expected_session = 0_u64;
        with_controller(|ctrl| {
            owner = hwnd_of(ctrl.main.window());
            expected_session = ctrl.settings_session_id;
        });
        std::thread::spawn(move || {
            if let Some(chosen) =
                win32::pick_json_backup_file(true, Some("slipstream-settings-backup.json"), owner)
            {
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        if !ctrl.main.get_settings_open()
                            || ctrl.settings_session_id != expected_session
                        {
                            return;
                        }
                        let Some(app) = ctrl.app.clone() else { return };
                        let path = PathBuf::from(&chosen);
                        ctrl.runtime.spawn(async move {
                            let res = app.export_settings_backup(&path).await;
                            let _ = slint::invoke_from_event_loop(move || {
                                with_controller(|ctrl| {
                                    if !ctrl.main.get_settings_open()
                                        || ctrl.settings_session_id != expected_session
                                    {
                                        return;
                                    }
                                    match res {
                                        Ok(()) => {
                                            ctrl.main.set_set_backup_notice(
                                                format!("Exported backup to {chosen}").into(),
                                            );
                                            ctrl.main.set_set_error_text("".into());
                                        }
                                        Err(err) => ctrl.main.set_set_error_text(err.into()),
                                    }
                                });
                            });
                        });
                    });
                });
            }
        });
    });

    main.on_settings_import_backup(|| {
        let mut owner = 0_isize;
        let mut expected_session = 0_u64;
        with_controller(|ctrl| {
            owner = hwnd_of(ctrl.main.window());
            expected_session = ctrl.settings_session_id;
        });
        std::thread::spawn(move || {
            if let Some(chosen) = win32::pick_json_backup_file(false, None, owner) {
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        if !ctrl.main.get_settings_open()
                            || ctrl.settings_session_id != expected_session
                        {
                            return;
                        }
                        let Some(app) = ctrl.app.clone() else { return };
                        let path = PathBuf::from(&chosen);
                        ctrl.runtime.spawn(async move {
                            let res = app.import_settings_backup(&path).await;
                            let _ = slint::invoke_from_event_loop(move || {
                                with_controller(|ctrl| {
                                    if !ctrl.main.get_settings_open()
                                        || ctrl.settings_session_id != expected_session
                                    {
                                        return;
                                    }
                                    match res {
                                        Ok(_) => {
                                            ctrl.refresh_theme_from_settings();
                                            ctrl.open_settings_dialog();
                                            ctrl.main.set_set_settings_tab(1);
                                            ctrl.main.set_set_backup_notice(
                                                format!("Imported settings backup from {chosen}")
                                                    .into(),
                                            );
                                        }
                                        Err(err) => ctrl.main.set_set_error_text(err.into()),
                                    }
                                });
                            });
                        });
                    });
                });
            }
        });
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
                "export_urls" => ctrl.export_selected_urls(),
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
    run_app_with_options(demo_mode, DaemonOptions::default())
}

pub fn run_app_with_options(demo_mode: bool, mut options: DaemonOptions) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to initialize Tokio runtime: {e}"))?;

    let main = MainWindow::new().map_err(|e| format!("Failed to create Slint MainWindow: {e}"))?;
    main.set_version_label(crate::desktop_version().into());
    main.set_demo_mode(demo_mode);
    wire_main_callbacks(&main);
    wire_file_drop(main.window());

    let (app, is_light, is_mono) = if demo_mode {
        (None, false, false)
    } else {
        let slots = Arc::new(SharedBridgeSlots::default());
        let ui: Arc<dyn UiBridge> = Arc::new(NativeUi::new(slots.clone()));
        if options.resource_dir.is_none() {
            options.resource_dir = default_resource_dir();
        }
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
        selection: TransferSelection::default(),
        categories: app
            .as_ref()
            .map_or_else(view_model::demo_categories, |app| app.settings().categories),
        queues: app
            .as_ref()
            .map_or_else(default_download_queues, |app| app.settings().queues),
        draft_queues: Vec::new(),
        draft_watch_folders: Vec::new(),
        settings_session_id: 0,
        create_torrent_session_id: 0,
        settings_initial_alt_speed: false,
        rss_feeds_model: Rc::default(),
        site_logins_model: Rc::default(),
        grabber_items_model: Rc::default(),
        grabber_session_id: 0,
        grabber_scan: None,
        pending_bulk_remove: Vec::new(),
        drop_basket: None,
        completion_banner: None,
        completion_timer: Timer::default(),
        settings_task: None,
        preference_revision: 0,
        saved_preference_revision: 0,
        logs: Vec::new(),
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
        torrent_actions_pending: HashSet::new(),
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
        if demo_mode {
            ctrl.show_main_window();
            let demo = demo_telemetry_batch();
            push_speed_sample(&mut ctrl.speed_history, speed_sample(&demo));
            ctrl.apply_telemetry(demo);
        } else if let Some(app_state) = &app {
            let settings = app_state.settings();
            let minimized = crate::start_minimized() || settings.launch_minimized;
            if !minimized || ctrl._tray_icon.is_none() {
                ctrl.show_main_window();
            }
            ctrl.load_preferences(&settings);
            let initial = runtime.block_on(app_state.snapshot());
            push_speed_sample(&mut ctrl.speed_history, speed_sample(&initial));
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

    if let Some(app) = &app {
        let mut alerts = app.subscribe_completion_alerts();
        runtime.spawn(async move {
            loop {
                match alerts.recv().await {
                    Ok(alert) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            with_controller(|ctrl| ctrl.completion_alert(alert));
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
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
                    } else if ids
                        .drop_basket
                        .as_ref()
                        .is_some_and(|item| item.id() == &ev.id)
                    {
                        action = Some("drop_basket");
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
                    Some("drop_basket") => with_controller(ControllerState::toggle_drop_basket),
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
                ctrl.sync_automation_status();

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
    fn range_selection_uses_visible_order_and_keeps_the_anchor() {
        let mut selection = TransferSelection::default();
        selection.update_visible(["one", "three", "two", "four"].map(str::to_owned).to_vec());
        selection.click("three", false, false);
        selection.click("four", false, true);
        assert_eq!(selection.ordered_ids(), ["three", "two", "four"]);
        assert_eq!(selection.selected_id.as_deref(), Some("four"));

        selection.click("two", false, true);
        assert_eq!(selection.ordered_ids(), ["three", "two"]);
        selection.click("two", true, false);
        assert_eq!(selection.selected_id.as_deref(), Some("three"));
        selection.update_visible(vec!["four".into()]);
        assert!(selection.ordered_ids().is_empty());
        selection.select_all();
        assert_eq!(selection.ordered_ids(), ["four"]);
        selection.clear();
        selection.update_visible(vec!["four".into()]);
        assert!(selection.selected_id.is_none());
    }

    #[test]
    fn batch_hint_counts_unique_urls_and_supports_ef2_blocks() {
        let text = "\u{feff}# List\n<\nhttps://example.org/file.zip#one\nReferer: https://example.net\nhttps://ignored.example/test\n>\nhttps://example.org/file.zip#two\nnot a URL\nhttps://example.org/other.zip";
        assert_eq!(batch_url_count(text), 2);
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
