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
    TorrentFileRowModel, TransferRowModel, UiBridge,
};
use i_slint_backend_winit::WinitWindowAccessor;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use slipstream_protocol::{
    AccentMode, AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings,
    AudioPreset, BrowserKind, ConfirmPendingDownloadRequest, MediaContainer, MediaFormatOption,
    MediaProbeResult, PendingDownloadPreview, RefreshDownloadUrlRequest, SegmentState,
    SelectedPlaylistEntry, TelemetryBatch, ThemeMode, ToolSource, TransferCompletionOptions,
    TransferKind, TransferSnapshot, TransferStatus, UpdateMode, UpdateProgress,
    UpdateProgressStage,
};
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
    time::Duration,
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
    selected_category_id: String,
    overwrite_existing: bool,
    submitting: bool,
}

struct ProgressWindowState {
    window: DownloadProgressWindow,
    speed_dirty: bool,
}

struct CompleteWindowState {
    window: DownloadCompleteWindow,
}

#[derive(Default)]
struct AddDialogRuntime {
    generation: u64,
    probe: Option<MediaProbeResult>,
    seeded_headers: HashMap<String, String>,
    seeded_origin: Option<String>,
    selected_format_id: Option<String>,
    checked_playlist: HashSet<u32>,
    checked_subtitles: HashSet<String>,
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
    torrent_files: HashMap<String, Vec<slipstream_protocol::TorrentFileEntry>>,
    confirm_windows: HashMap<String, ConfirmWindowState>,
    progress_windows: HashMap<String, ProgressWindowState>,
    complete_windows: HashMap<String, CompleteWindowState>,
    add_state: AddDialogRuntime,
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

fn transfer_to_row(t: &TransferSnapshot, compact: bool) -> TransferRowModel {
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
        segments: ModelRc::from(Rc::new(VecModel::from(t.ui_segments.to_vec()))),
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

fn build_inspector(
    selected: Option<&TransferSnapshot>,
    cached_torrent_files: &[slipstream_protocol::TorrentFileEntry],
) -> InspectorModel {
    let Some(t) = selected else {
        return InspectorModel {
            has_selection: false,
            id: "".into(),
            filename: "".into(),
            source_url: "".into(),
            source_domain: "".into(),
            save_dir: "".into(),
            protocol_label: "".into(),
            status_label: "".into(),
            status_tone: 0,
            kind: "".into(),
            is_media: false,
            is_torrent: false,
            is_paused: false,
            is_completed: false,
            can_pause_resume: false,
            can_verify: false,
            can_refresh_url: false,
            speed_text: "--".into(),
            eta_text: "--".into(),
            size_text: "--".into(),
            connections_text: "--".into(),
            sha_text: "".into(),
            sha_tone: 0,
            error_message: "".into(),
            completion_notice: "".into(),
            duration_text: "".into(),
            segments: ModelRc::default(),
            torrent_files: ModelRc::default(),
        };
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

    let torrent_rows: Vec<TorrentFileRowModel> = cached_torrent_files
        .iter()
        .map(|f| TorrentFileRowModel {
            index: f.index as i32,
            relative_path: f.relative_path.clone().into(),
            size_text: format_bytes(f.size_bytes).into(),
            downloaded_text: format_bytes(f.downloaded_bytes).into(),
            selected: f.selected,
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
        is_torrent: t.kind == TransferKind::Torrent,
        is_paused: t.status == TransferStatus::Paused,
        is_completed,
        can_pause_resume: !is_completed && t.status != TransferStatus::Canceled,
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
        sha_text: sha_text.into(),
        sha_tone,
        error_message: t.error_message.clone().unwrap_or_default().into(),
        completion_notice: t.completion_notice.clone().unwrap_or_default().into(),
        duration_text: String::new().into(),
        segments: ModelRc::from(Rc::new(VecModel::from(seg_models))),
        torrent_files: ModelRc::from(Rc::new(VecModel::from(torrent_rows))),
    }
}

pub fn apply_telemetry_snapshot(
    main: &MainWindow,
    batch: &TelemetryBatch,
    speed_history: &[u64],
    expanded_playlists: &HashSet<String>,
    selected_id: &mut Option<String>,
    torrent_files: &HashMap<String, Vec<slipstream_protocol::TorrentFileEntry>>,
    download_dir: &Path,
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

    if let Some((free, total)) = win32::disk_space_info(download_dir) {
        main.set_disk_free_text(format!("{} free", format_bytes(free)).into());
        let used = (total.saturating_sub(free) as f32 / total as f32).clamp(0.05, 1.0);
        main.set_disk_used_ratio(used);
    }

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
            active_rows.push(transfer_to_row(t, false));
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
                        TransferStatus::Active | TransferStatus::Probing | TransferStatus::Remuxing
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
            let row_models: Vec<TransferRowModel> =
                items.iter().map(|t| transfer_to_row(t, true)).collect();
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
                items: ModelRc::from(Rc::new(VecModel::from(row_models))),
            }
        })
        .collect();
    playlist_groups.sort_by(|a, b| a.title.cmp(&b.title));

    main.set_playlist_groups(ModelRc::from(Rc::new(VecModel::from(playlist_groups))));
    main.set_active_rows(ModelRc::from(Rc::new(VecModel::from(active_rows))));
    main.set_completed_rows(ModelRc::from(Rc::new(VecModel::from(completed_rows))));

    let valid_selection = selected_id
        .as_deref()
        .and_then(|id| batch.transfers.iter().find(|t| t.id == id));
    let selected_transfer = valid_selection
        .or_else(|| visible.first().copied())
        .or_else(|| batch.transfers.first());
    *selected_id = selected_transfer.map(|t| t.id.clone());
    main.set_selected_id(selected_id.clone().unwrap_or_default().into());

    let files = selected_transfer
        .and_then(|t| torrent_files.get(&t.id))
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    main.set_inspector(build_inspector(selected_transfer, files));
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
    win.set_segments(ModelRc::from(Rc::new(VecModel::from(
        t.ui_segments.to_vec(),
    ))));
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
    win.set_connections(ModelRc::from(Rc::new(VecModel::from(conn_rows))));

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
        self.main.window().with_winit_window(|w| {
            w.set_minimized(false);
            w.focus_window();
        });
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

    fn apply_telemetry(&mut self, batch: TelemetryBatch) {
        if self.speed_history.len() >= MAX_SPEED_SAMPLES {
            self.speed_history.pop_front();
        }
        self.speed_history.push_back(batch.aggregate_down_bps);

        let dir = self
            .app
            .as_ref()
            .map(|a| PathBuf::from(a.settings().default_download_dir))
            .unwrap_or_else(default_data_dir);
        let history: Vec<u64> = self.speed_history.iter().copied().collect();
        apply_telemetry_snapshot(
            &self.main,
            &batch,
            &history,
            &self.expanded_playlists,
            &mut self.selected_id,
            &self.torrent_files,
            &dir,
        );

        // Fetch torrent file list for selected torrent if missing
        if let (Some(app), Some(sel_id)) = (&self.app, &self.selected_id) {
            if let Some(sel) = batch.transfers.iter().find(|t| &t.id == sel_id) {
                if sel.kind == TransferKind::Torrent && !self.torrent_files.contains_key(sel_id) {
                    let app_c = app.clone();
                    let tid = sel_id.clone();
                    self.runtime.spawn(async move {
                        if let Ok(files) = app_c.list_torrent_files(&tid).await {
                            let _ = slint::invoke_from_event_loop(move || {
                                with_controller(|ctrl| {
                                    ctrl.torrent_files.insert(tid, files);
                                    if let Some(b) = ctrl.last_batch.clone() {
                                        ctrl.apply_telemetry(b);
                                    }
                                });
                            });
                        }
                    });
                }
            }
        }

        // Sync open DownloadProgressWindows
        let open_prog_ids: Vec<String> = self.progress_windows.keys().cloned().collect();
        for tid in open_prog_ids {
            if let Some(t) = batch.transfers.iter().find(|item| item.id == tid) {
                let mut should_auto_close = false;
                let mut should_reveal_folder = false;
                if let Some(entry) = self.progress_windows.get_mut(&tid) {
                    let was_completed = entry.window.get_is_completed();
                    populate_progress_window(&entry.window, t, !entry.speed_dirty);
                    if !was_completed && t.status == TransferStatus::Completed {
                        should_auto_close = entry.window.get_opt_auto_close();
                        should_reveal_folder = entry.window.get_opt_open_folder();
                    }
                }
                if should_reveal_folder {
                    if let Some(app) = &self.app {
                        let app_c = app.clone();
                        let id_c = tid.clone();
                        self.runtime.spawn(async move {
                            let _ = app_c.reveal_in_explorer(&id_c).await;
                        });
                    }
                }
                if should_auto_close {
                    self.close_progress_dialog(&tid);
                }
            }
        }

        self.last_batch = Some(batch);
    }

    fn open_add_dialog_with_seed(&mut self, event: OpenPickerEvent) {
        let next_gen = self.add_state.generation.wrapping_add(1);
        self.add_state = AddDialogRuntime {
            generation: next_gen,
            ..AddDialogRuntime::default()
        };
        self.clear_add_probe_ui();
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
            self.main.set_add_tab(0);
        }
        self.main.set_add_open(true);
        self.show_main_window();
    }

    fn clear_add_probe_ui(&mut self) {
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
        self.add_state.generation = self.add_state.generation.wrapping_add(1);
        self.clear_add_probe_ui();
        if should_clear_seeded_headers(self.add_state.seeded_origin.as_deref(), source) {
            self.add_state.seeded_headers.clear();
            self.add_state.seeded_origin = None;
        }
        self.update_add_source_classification(source);
    }

    fn update_add_source_classification(&mut self, source: &str) {
        let torrent = is_torrent_input(source);
        self.main.set_add_is_torrent_input(torrent);
        if torrent {
            self.main.set_add_tab(3);
        }
        let batch_n = view_model::expand_wildcard_preview(source);
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
            existing
                .window
                .window()
                .with_winit_window(|w| w.focus_window());
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
        apply_native_window_chrome(win.window());

        self.confirm_windows.insert(
            pending_id.to_owned(),
            ConfirmWindowState {
                window: win,
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

    fn open_progress_dialog(&mut self, transfer_id: &str) {
        if let Some(entry) = self.progress_windows.get(transfer_id) {
            let _ = entry.window.show();
            entry
                .window
                .window()
                .with_winit_window(|w| w.focus_window());
            return;
        }
        let Ok(win) = DownloadProgressWindow::new() else {
            return;
        };
        set_win_theme!(&win, self.is_light, self.is_mono);

        if let Some(batch) = &self.last_batch {
            if let Some(t) = batch.transfers.iter().find(|item| item.id == transfer_id) {
                populate_progress_window(&win, t, true);
            }
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
        let tid_cancel = tid.clone();
        win.on_cancel_transfer_clicked(move || {
            with_controller(|ctrl| {
                ctrl.remove_transfer(&tid_cancel);
                ctrl.close_progress_dialog(&tid_cancel);
            });
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
                if let Ok(bps) = limit {
                    let app_c = app.clone();
                    let id_c = tid_spd.clone();
                    ctrl.runtime.spawn(async move {
                        let _ = app_c.set_transfer_speed_limit(&id_c, bps).await;
                    });
                }
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
        apply_native_window_chrome(win.window());

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
                speed_dirty: false,
            },
        );
    }

    fn close_progress_dialog(&mut self, transfer_id: &str) {
        if let Some(entry) = self.progress_windows.remove(transfer_id) {
            let _ = entry.window.hide();
        }
    }

    fn open_complete_dialog(&mut self, transfer_id: &str) {
        if let Some(entry) = self.complete_windows.get(transfer_id) {
            let _ = entry.window.show();
            entry
                .window
                .window()
                .with_winit_window(|w| w.focus_window());
            return;
        }
        let Some(t) = self
            .last_batch
            .as_ref()
            .and_then(|b| b.transfers.iter().find(|item| item.id == transfer_id))
            .cloned()
        else {
            return;
        };
        let Ok(win) = DownloadCompleteWindow::new() else {
            return;
        };
        set_win_theme!(&win, self.is_light, self.is_mono);
        let _ = populate_complete_window(&win, &t);

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
        apply_native_window_chrome(win.window());
        self.complete_windows
            .insert(transfer_id.to_owned(), CompleteWindowState { window: win });
    }

    fn toggle_transfer_pause(&mut self, id: &str, currently_paused: bool) {
        let Some(app) = &self.app else { return };
        let app_c = app.clone();
        let tid = id.to_owned();
        self.runtime.spawn(async move {
            let _ = if currently_paused {
                app_c.resume_transfer(&tid).await
            } else {
                app_c.pause_transfer(&tid).await
            };
        });
    }

    fn remove_transfer(&mut self, id: &str) {
        let Some(app) = &self.app else { return };
        let app_c = app.clone();
        let tid = id.to_owned();
        self.runtime.spawn(async move {
            let _ = app_c.cancel_transfer(&tid, false).await;
        });
    }

    fn open_completed_file(&mut self, id: &str) {
        let Some(app) = &self.app else { return };
        let app_c = app.clone();
        let tid = id.to_owned();
        self.runtime.spawn(async move {
            let _ = app_c.open_completed_file(&tid).await;
        });
    }

    fn open_completed_file_with(&mut self, id: &str) {
        let Some(app) = &self.app else { return };
        let app_c = app.clone();
        let tid = id.to_owned();
        self.runtime.spawn(async move {
            let res = app_c.open_completed_file_with(&tid).await;
            if let Err(err) = res {
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        if let Some(entry) = ctrl.complete_windows.get(&tid) {
                            entry.window.set_error_text(err.into());
                        }
                    });
                });
            }
        });
    }

    fn reveal_in_explorer(&mut self, id: &str) {
        let Some(app) = &self.app else { return };
        let app_c = app.clone();
        let tid = id.to_owned();
        self.runtime.spawn(async move {
            let _ = app_c.reveal_in_explorer(&tid).await;
        });
    }

    fn trigger_probe(&mut self) {
        let Some(app) = &self.app else { return };
        let source = self.main.get_add_source_text().to_string();
        let trimmed = source.trim().to_owned();
        if trimmed.is_empty() || is_torrent_input(&trimmed) {
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
        let tab = self.main.get_add_tab();
        if should_clear_seeded_headers(self.add_state.seeded_origin.as_deref(), &source) {
            self.add_state.seeded_headers.clear();
            self.add_state.seeded_origin = None;
        }
        let headers = self.add_state.seeded_headers.clone();
        let expected_gen = self.add_state.generation;

        self.main.set_add_submitting(true);
        self.main.set_add_error_text("".into());
        let app_c = app.clone();

        if tab == 3 || is_torrent_input(&source) {
            let sequential = self.main.get_add_torrent_sequential();
            self.runtime.spawn(async move {
                let res = app_c
                    .add_torrent(AddTorrentRequest {
                        source,
                        save_dir,
                        selected_file_indices: None,
                        sequential,
                    })
                    .await;
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| {
                        ctrl.finish_add_submission(expected_gen, res.map(|_| ()))
                    });
                });
            });
            return;
        }

        let batch_n = view_model::expand_wildcard_preview(&source);
        if batch_n > 0 {
            let urls = expand_wildcard_urls(&source).unwrap_or_default();
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
            Ok(()) => {
                self.add_state = AddDialogRuntime {
                    generation: self.add_state.generation.wrapping_add(1),
                    ..AddDialogRuntime::default()
                };
                self.main.set_add_open(false);
            }
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
            if let Some(b) = ctrl.last_batch.clone() {
                ctrl.apply_telemetry(b);
            }
        });
    });

    main.on_search_changed(|_| {
        with_controller(|ctrl| {
            if let Some(b) = ctrl.last_batch.clone() {
                ctrl.apply_telemetry(b);
            }
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
        with_controller(|ctrl| {
            let pause = !ctrl.main.get_all_paused();
            if let Some(app) = &ctrl.app {
                let app_c = app.clone();
                ctrl.runtime.spawn(async move {
                    app_c.change_all(pause).await;
                });
            }
        });
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
            if let Ok(bps) = parse_speed_limit_slider_mbps(&raw) {
                if let Some(app) = &ctrl.app {
                    let app_c = app.clone();
                    ctrl.runtime.spawn(async move {
                        let _ = app_c.set_speed_limit(bps).await;
                    });
                }
            }
        });
    });

    main.on_select_transfer(|id| {
        with_controller(|ctrl| {
            ctrl.selected_id = Some(id.into());
            if let Some(b) = ctrl.last_batch.clone() {
                ctrl.apply_telemetry(b);
            }
        });
    });

    main.on_toggle_transfer_pause(|id, paused| {
        with_controller(|ctrl| ctrl.toggle_transfer_pause(&id, paused));
    });

    main.on_open_floating_progress(|id| {
        with_controller(|ctrl| ctrl.open_progress_dialog(&id));
    });

    main.on_remove_transfer(|id| {
        with_controller(|ctrl| ctrl.remove_transfer(&id));
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
            if let Some(b) = ctrl.last_batch.clone() {
                ctrl.apply_telemetry(b);
            }
        });
    });

    main.on_toggle_playlist_pause(|pid, paused| {
        with_controller(|ctrl| {
            if let Some(app) = &ctrl.app {
                let app_c = app.clone();
                let id_c = pid.to_string();
                ctrl.runtime.spawn(async move {
                    let _ = if paused {
                        app_c.resume_playlist(&id_c).await
                    } else {
                        app_c.pause_playlist(&id_c).await
                    };
                });
            }
        });
    });

    main.on_reveal_playlist_folder(|pid| {
        with_controller(|ctrl| {
            if let Some(app) = &ctrl.app {
                let _ = app.reveal_playlist_folder(&pid);
            }
        });
    });

    main.on_toggle_torrent_file(|idx| {
        with_controller(|ctrl| {
            if let Some(sel) = ctrl.selected_id.clone() {
                if let Some(files) = ctrl.torrent_files.get_mut(&sel) {
                    if let Some(f) = files.iter_mut().find(|f| f.index as i32 == idx) {
                        f.selected = !f.selected;
                    }
                }
                if let Some(b) = ctrl.last_batch.clone() {
                    ctrl.apply_telemetry(b);
                }
            }
        });
    });

    main.on_apply_torrent_files(|id| {
        with_controller(|ctrl| {
            let tid = id.to_string();
            if let (Some(app), Some(files)) = (&ctrl.app, ctrl.torrent_files.get(&tid)) {
                let indices: Vec<usize> = files
                    .iter()
                    .filter(|f| f.selected)
                    .map(|f| f.index)
                    .collect();
                let app_c = app.clone();
                ctrl.runtime.spawn(async move {
                    let _ = app_c.set_torrent_files(&tid, indices).await;
                });
            }
        });
    });

    // Add Download Callbacks
    main.on_add_close_requested(|| {
        with_controller(|ctrl| {
            ctrl.add_state = AddDialogRuntime {
                generation: ctrl.add_state.generation.wrapping_add(1),
                ..AddDialogRuntime::default()
            };
            ctrl.clear_add_probe_ui();
            ctrl.main.set_add_open(false);
        });
    });

    main.on_add_source_edited(|txt| {
        with_controller(|ctrl| {
            let s = txt.to_string();
            ctrl.handle_add_source_changed(&s);
        });
    });

    main.on_add_probe_clicked(|| {
        with_controller(|ctrl| ctrl.trigger_probe());
    });

    main.on_add_browse_torrent_clicked(|| {
        let mut owner = 0_isize;
        let mut initial = None;
        with_controller(|ctrl| {
            owner = hwnd_of(ctrl.main.window());
            initial = ctrl.app.as_ref().map(|a| a.settings().default_download_dir);
        });
        std::thread::spawn(move || {
            let picked = win32::pick_torrent_file(owner, initial);
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|ctrl| match picked {
                    Ok(Some(path)) => {
                        ctrl.main.set_add_source_text(path.clone().into());
                        ctrl.handle_add_source_changed(&path);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        ctrl.main.set_add_error_text(err.to_string().into());
                    }
                });
            });
        });
    });

    main.on_add_browse_save_dir_clicked(|| {
        let mut owner = 0_isize;
        let mut initial = String::new();
        with_controller(|ctrl| {
            owner = hwnd_of(ctrl.main.window());
            initial = ctrl.main.get_add_save_dir().to_string();
        });
        std::thread::spawn(move || {
            if let Ok(Some(path)) = win32::pick_download_folder(owner, Some(initial)) {
                let _ = slint::invoke_from_event_loop(move || {
                    with_controller(|ctrl| ctrl.main.set_add_save_dir(path.into()));
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
                "pause_all" => {
                    let pause = !ctrl.main.get_all_paused();
                    if let Some(app) = &ctrl.app {
                        let app_c = app.clone();
                        ctrl.runtime
                            .spawn(async move { app_c.change_all(pause).await });
                    }
                }
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
        torrent_files: HashMap::new(),
        confirm_windows: HashMap::new(),
        progress_windows: HashMap::new(),
        complete_windows: HashMap::new(),
        add_state: AddDialogRuntime::default(),
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
                    Some("pause_all") => with_controller(|ctrl| {
                        if let Some(a) = &ctrl.app {
                            let ac = a.clone();
                            ctrl.runtime.spawn(async move { ac.change_all(true).await });
                        }
                    }),
                    Some("resume_all") => with_controller(|ctrl| {
                        if let Some(a) = &ctrl.app {
                            let ac = a.clone();
                            ctrl.runtime
                                .spawn(async move { ac.change_all(false).await });
                        }
                    }),
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
