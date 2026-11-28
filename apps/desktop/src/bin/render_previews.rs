#![cfg(windows)]

use slint::{
    platform::{
        software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType},
        Platform, WindowAdapter,
    },
    ComponentHandle, ModelRc, PhysicalSize, PlatformError, VecModel,
};
use slipstream_desktop_lib::{
    shell::{
        populate_complete_window, populate_confirm_preview, populate_progress_window,
        populate_speed_graph, DesktopModels, ProgressModels, TransferSelection,
    },
    view_model::{demo_categories, demo_telemetry_batch, format_bytes},
    BrowserCardModel, DownloadCompleteWindow, DownloadConfirmWindow, DownloadProgressWindow,
    DropBasketWindow, FormatOptionModel, MainWindow, QueueItem, RssFeedRuleItem, SiteGrabItemRow,
    SiteLoginItem, Theme, TorrentPreviewFileModel, WatchFolderItem,
};
use slipstream_protocol::{
    default_download_categories, default_download_queues, AppSettings, DuplicateMatch,
    PendingDownloadPreview, SpeedSample, TorrentFileEntry, TorrentPeerEntry, TorrentPeerSummary,
    TransferStatus,
};
use std::{cell::RefCell, collections::HashSet, fs::File, io::BufWriter, path::Path, rc::Rc};

struct HeadlessPlatform {
    last_window: Rc<RefCell<Option<Rc<MinimalSoftwareWindow>>>>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let win = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        *self.last_window.borrow_mut() = Some(win.clone());
        Ok(win)
    }
}

fn render_to_png(
    adapter: &Rc<MinimalSoftwareWindow>,
    width: u32,
    height: u32,
    out_path: &Path,
) -> Result<(), String> {
    adapter.set_size(PhysicalSize::new(width, height));
    adapter.request_redraw();
    slint::platform::update_timers_and_animations();

    let mut buffer = vec![
        PremultipliedRgbaColor {
            alpha: 255,
            red: 26,
            green: 28,
            blue: 34,
        };
        (width * height) as usize
    ];
    let rendered = adapter.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, width as usize);
    });
    if !rendered {
        return Err(format!(
            "Renderer did not draw frame for {}",
            out_path.display()
        ));
    }

    let mut rgba = Vec::with_capacity(buffer.len() * 4);
    for px in buffer {
        if px.alpha == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else if px.alpha == 255 {
            rgba.extend_from_slice(&[px.red, px.green, px.blue, 255]);
        } else {
            let a = px.alpha as f32 / 255.0;
            rgba.push(((px.red as f32 / a).min(255.0)) as u8);
            rgba.push(((px.green as f32 / a).min(255.0)) as u8);
            rgba.push(((px.blue as f32 / a).min(255.0)) as u8);
            rgba.push(px.alpha);
        }
    }

    let file = File::create(out_path).map_err(|e| format!("create {}: {e}", out_path.display()))?;
    let w = BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("png header {}: {e}", out_path.display()))?;
    writer
        .write_image_data(&rgba)
        .map_err(|e| format!("png data {}: {e}", out_path.display()))?;
    Ok(())
}

fn setup_demo_main(main: &MainWindow, is_light: bool) {
    let theme = main.global::<Theme<'_>>();
    theme.set_is_light(is_light);
    theme.set_is_mono(false);
    main.set_theme_light(is_light);
    main.set_theme_mono(false);
    main.set_version_label("3.0.0".into());
    main.set_vpn_kill_switch_active(false);

    let mut batch = demo_telemetry_batch();
    if main.get_compact_table_view() {
        for transfer in batch.transfers.iter_mut().take(3) {
            transfer.category_id = Some("compressed".into());
            transfer.tags.push("alpine".into());
        }
        main.set_active_category_filter("compressed".into());
        main.set_active_tag_filter("alpine".into());
    }
    let history = vec![
        11_200_000, 15_400_000, 22_800_000, 19_600_000, 27_100_000, 31_400_000, 29_900_000,
        34_800_000, 38_640_000, 32_620_000,
    ];
    let expanded = HashSet::new();
    let mut selected = TransferSelection::default();
    selected.select_only("demo-1");
    let mut models = DesktopModels::default();
    models.attach(main);
    let categories = demo_categories();
    models.apply_telemetry_snapshot(
        main,
        &batch,
        &history,
        &expanded,
        &mut selected,
        &categories,
    );
    models.sync_queues(&batch.transfers, &default_download_queues());
    if main.get_compact_table_view() {
        selected.select_all();
        models.apply_telemetry_snapshot(
            main,
            &batch,
            &history,
            &expanded,
            &mut selected,
            &categories,
        );
    }
    models.update_inspector_files(&demo_torrent_files(), None);
    let peers = demo_peers();
    models.update_inspector_peers(Some(&peers));
    let primary = main.get_selected_id();
    let transfer = batch
        .transfers
        .iter()
        .find(|transfer| transfer.id == primary.as_str());
    models.update_inspector(main, transfer, Some(&peers));
    let inspector = main.get_inspector();
    let category_index = categories
        .iter()
        .position(|category| category.id == inspector.category_id.as_str());
    main.set_inspector_category_index(category_index.map_or(0, |index| index as i32 + 1));
    main.set_inspector_tags(inspector.tags_summary);
    main.set_torrent_upload_limit_loaded(true);
    main.set_torrent_upload_limit_input("512".into());
    main.set_disk_free_text("91.7 GB free".into());
}

fn demo_torrent_files() -> Vec<TorrentFileEntry> {
    vec![
        TorrentFileEntry {
            index: 0,
            relative_path: "Ubuntu 24.04.2/ubuntu-24.04.2-desktop-amd64.iso".into(),
            size_bytes: 6_174_015_488,
            downloaded_bytes: 4_815_732_080,
            selected: true,
        },
        TorrentFileEntry {
            index: 1,
            relative_path: "Ubuntu 24.04.2/SHA256SUMS".into(),
            size_bytes: 216,
            downloaded_bytes: 0,
            selected: false,
        },
    ]
}

fn demo_peers() -> TorrentPeerSummary {
    TorrentPeerSummary {
        live: 2,
        connecting: 3,
        queued: 8,
        seen: 42,
        dead: 29,
        upload_speed_bps: 1_420_000,
        ratio: 0.11,
        completed_pieces: 2_296,
        total_pieces: 2_945,
        trackers: vec!["https://tracker.example.org/announce".into()],
        peers: [
            ("192.0.2.14:51413", 12_860_000, 512_000_000, 244, false),
            ("198.51.100.6:6881", 5_560_000, 211_000_000, 100, true),
        ]
        .into_iter()
        .map(
            |(address, download_speed_bps, downloaded_bytes, pieces_downloaded, incoming)| {
                TorrentPeerEntry {
                    address: address.into(),
                    state: "Live".into(),
                    downloaded_bytes,
                    download_speed_bps,
                    pieces_downloaded,
                    incoming,
                    errors: 0,
                }
            },
        )
        .collect(),
    }
}

fn render_settings_bittorrent_preview(
    main: &MainWindow,
    adapter: &Rc<MinimalSoftwareWindow>,
    out_dir: &Path,
) -> Result<(), String> {
    main.set_set_settings_tab(2);
    main.set_set_bt_listen_port_start("6881".into());
    main.set_set_bt_listen_port_end("6891".into());
    main.set_set_bt_enable_dht(true);
    main.set_set_bt_enable_upnp(false);
    main.set_set_bt_global_upload_kbps("1024".into());
    main.set_set_bt_socks5_proxy("socks5h://proxy.example.net:1080".into());
    main.set_set_bt_blocklist_path(r"C:\Downloads\blocklist.p2p".into());
    main.set_set_bt_vpn_kill_switch(true);
    main.set_set_bt_vpn_adapter_options(
        Rc::new(VecModel::from(vec![
            "Custom adapter".into(),
            "WireGuard Tunnel".into(),
            "Ethernet".into(),
        ]))
        .into(),
    );
    main.set_set_bt_vpn_adapter_index(1);
    main.set_set_bt_max_seed_ratio("2.00".into());
    main.set_set_bt_max_seed_minutes("1440".into());
    main.set_set_bt_stop_seeding_on_limit(true);
    main.set_set_rss_feeds(
        Rc::new(VecModel::from(vec![RssFeedRuleItem {
            id: "linux-releases".into(),
            name: "Linux Releases".into(),
            feed_url: "https://releases.example.org/feed.xml".into(),
            include_pattern: "*amd64*".into(),
            exclude_pattern: "*beta*".into(),
            poll_interval_minutes: 30,
            enabled: true,
            auto_start: true,
            last_error: "".into(),
        }]))
        .into(),
    );
    render_to_png(
        adapter,
        1340,
        820,
        &out_dir.join("v3_settings_bittorrent.png"),
    )
}

fn render_settings_network_preview(
    main: &MainWindow,
    adapter: &Rc<MinimalSoftwareWindow>,
    out_dir: &Path,
) -> Result<(), String> {
    main.set_set_settings_tab(3);
    main.set_set_proxy_mode_index(2);
    main.set_set_proxy_url("http://proxy.example.net:8080".into());
    main.set_set_proxy_username("downloads".into());
    main.set_set_proxy_has_password(true);
    main.set_set_proxy_bypass_hosts("*.intranet.example.net, <local>".into());
    main.set_set_site_logins(
        Rc::new(VecModel::from(vec![SiteLoginItem {
            id: "release-mirror".into(),
            host_pattern: "releases.example.org".into(),
            username: "release-reader".into(),
            auth_scheme: "Basic".into(),
            has_password: true,
            enabled: true,
        }]))
        .into(),
    );
    main.set_set_cookies_enabled(true);
    main.set_set_cookies_browser_index(1);
    main.set_set_cookies_allowed_hosts("media.example.org".into());
    main.set_set_media_embed_metadata(true);
    main.set_set_media_embed_chapters(true);
    main.set_set_media_sponsorblock_text("sponsor, intro".into());
    main.set_set_media_filename_template("%(title)s - %(id)s".into());
    render_to_png(adapter, 1340, 820, &out_dir.join("v3_settings_network.png"))
}

fn render_site_grabber_preview(
    main: &MainWindow,
    adapter: &Rc<MinimalSoftwareWindow>,
    out_dir: &Path,
) -> Result<(), String> {
    let page = "https://releases.example.org/downloads/";
    main.set_grabber_open(true);
    main.set_grabber_target_url(page.into());
    main.set_grabber_depth_index(1);
    main.set_grabber_extensions("zip, pdf, sha256".into());
    main.set_grabber_scanned_count_text("3 pages scanned · 4 files found".into());
    let rows: Vec<_> = [
        ("slipstream-2.1-x64.zip", "link", true),
        ("slipstream-2.1-arm64.zip", "link", true),
        ("release-notes.pdf", "link", false),
        ("slipstream-2.1-x64.zip.sha256", "link", false),
    ]
    .into_iter()
    .map(|(filename, kind, checked)| SiteGrabItemRow {
        url: format!("{page}{filename}").into(),
        filename: filename.into(),
        kind: kind.into(),
        source_page: page.into(),
        checked,
    })
    .collect();
    main.set_grabber_items(Rc::new(VecModel::from(rows)).into());
    main.set_grabber_selected_count(2);
    render_to_png(adapter, 1024, 768, &out_dir.join("v3_site_grabber.png"))
}

fn main() -> Result<(), String> {
    let out_dir = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    let last_window: Rc<RefCell<Option<Rc<MinimalSoftwareWindow>>>> = Rc::new(RefCell::new(None));
    let platform = HeadlessPlatform {
        last_window: last_window.clone(),
    };
    slint::platform::set_platform(Box::new(platform)).map_err(|e| e.to_string())?;

    let take_adapter = || -> Result<Rc<MinimalSoftwareWindow>, String> {
        last_window
            .borrow_mut()
            .take()
            .ok_or_else(|| "Missing software window adapter".to_owned())
    };

    // 1. Main Dark (1340x820)
    {
        let main = MainWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        setup_demo_main(&main, false);
        let _ = main.show();
        render_to_png(&adapter, 1340, 820, &out_dir.join("v2_main_dark.png"))?;
    }

    // 2. Main Light (1340x820)
    {
        let main = MainWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        setup_demo_main(&main, true);
        let _ = main.show();
        render_to_png(&adapter, 1340, 820, &out_dir.join("v2_main_light.png"))?;
    }

    // 3. Add Download Porcelain Modal (1340x820)
    {
        let main = MainWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        setup_demo_main(&main, false);
        main.set_add_open(true);
        main.set_add_source_text("https://www.youtube.com/watch?v=alpine-hdr-demo".into());
        main.set_add_has_probe(true);
        main.set_add_probe_title("Alpine Stream Engine 4K HDR Showcase".into());
        main.set_add_probe_meta("youtube â€¢ 14:22".into());
        main.set_add_save_dir("C:\\Users\\moham\\Downloads".into());
        main.set_add_footer_hint("16 HTTP threads".into());
        main.set_add_format_options(ModelRc::from(Rc::new(VecModel::from(vec![
            FormatOptionModel {
                format_id: "315".into(),
                title: "2160p 4K (VP9 + Opus)".into(),
                detail: "MKV â€¢ 1.41 GB â€¢ 14200 kbps".into(),
                selected: true,
            },
            FormatOptionModel {
                format_id: "299".into(),
                title: "1080p60 Full HD (H.264 + AAC)".into(),
                detail: "MP4 â€¢ 640 MB â€¢ 6400 kbps".into(),
                selected: false,
            },
            FormatOptionModel {
                format_id: "247".into(),
                title: "720p HD".into(),
                detail: "MP4 â€¢ 290 MB â€¢ 2800 kbps".into(),
                selected: false,
            },
        ]))));
        let _ = main.show();
        render_to_png(&adapter, 1340, 820, &out_dir.join("v2_add_dialog.png"))?;
    }

    {
        let main = MainWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        setup_demo_main(&main, false);
        main.set_inspector_tab(2);
        main.show().map_err(|e| e.to_string())?;
        render_to_png(&adapter, 1340, 820, &out_dir.join("v2_main_peers.png"))?;

        let files: Vec<_> = demo_torrent_files()
            .into_iter()
            .map(|file| TorrentPreviewFileModel {
                index: file.index as i32,
                relative_path: file.relative_path.into(),
                size_text: format_bytes(file.size_bytes).into(),
                checked: file.selected,
            })
            .collect();
        main.set_add_open(true);
        main.set_add_tab(3);
        main.set_add_is_torrent_input(true);
        main.set_has_torrent_preview(true);
        main.set_add_source_text("C:\\Downloads\\ubuntu-24.04.2-desktop-amd64.iso.torrent".into());
        main.set_add_save_dir("C:\\Downloads".into());
        main.set_add_torrent_preview_name("ubuntu-24.04.2-desktop-amd64".into());
        main.set_add_torrent_preview_meta(
            "BTv2 â€¢ 2 files â€¢ 5.75 GB / 5.75 GB selected â€¢ 91.7 GB free".into(),
        );
        main.set_add_torrent_preview_trackers(1);
        main.set_torrent_preview_files(Rc::new(VecModel::from(files)).into());
        render_to_png(&adapter, 1340, 820, &out_dir.join("v2_add_torrent.png"))?;
    }

    // 4. Settings Modal (1340x820)
    {
        let main = MainWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        setup_demo_main(&main, false);
        main.set_settings_open(true);
        main.set_set_default_download_dir("C:\\Users\\moham\\Downloads".into());
        main.set_set_ytdlp_version("2026.02.21".into());
        main.set_set_ytdlp_path(
            "C:\\Users\\moham\\AppData\\Local\\SlipStream\\bin\\yt-dlp.exe".into(),
        );
        main.set_set_ffmpeg_version("7.1.1".into());
        main.set_set_ffmpeg_path("C:\\Program Files\\SlipStream\\bin\\ffmpeg.exe".into());
        main.set_set_ffprobe_path("C:\\Program Files\\SlipStream\\bin\\ffprobe.exe".into());
        main.set_set_browsers(ModelRc::from(Rc::new(VecModel::from(vec![
            BrowserCardModel {
                id: "chrome".into(),
                name: "Google Chrome".into(),
                installed: true,
                native_host_registered: true,
                extension_id: "slipstream".into(),
                bundled_extension_dir: "".into(),
            },
            BrowserCardModel {
                id: "edge".into(),
                name: "Microsoft Edge".into(),
                installed: true,
                native_host_registered: true,
                extension_id: "slipstream".into(),
                bundled_extension_dir: "".into(),
            },
            BrowserCardModel {
                id: "firefox".into(),
                name: "Mozilla Firefox".into(),
                installed: true,
                native_host_registered: false,
                extension_id: "slipstream@ciphervaults.com".into(),
                bundled_extension_dir: "".into(),
            },
        ]))));
        let _ = main.show();
        render_to_png(&adapter, 1340, 820, &out_dir.join("v2_settings_dialog.png"))?;

        // 4b. Settings -> Automation Tab (1340x820)
        main.set_set_settings_tab(1);
        main.set_set_alt_speed_enabled(true);
        main.set_set_alt_speed_kbps("512".into());
        main.set_set_schedule_mode_index(1);
        main.set_set_schedule_start("22:00".into());
        main.set_set_schedule_end("07:00".into());
        main.set_set_schedule_weekdays(true);
        main.set_set_schedule_limit_kbps("512".into());
        main.set_set_schedule_pause_outside(false);
        main.set_set_schedule_stop_torrents(true);
        main.set_set_guard_enabled(true);
        main.set_set_guard_fullscreen(true);
        main.set_set_guard_pause(false);
        main.set_set_guard_limit_kbps("256".into());
        main.set_set_guard_processes("zoom.exe, teams.exe, discord.exe, valorant.exe".into());
        let demo_queues: Vec<QueueItem> = default_download_queues()
            .into_iter()
            .enumerate()
            .map(|(i, q)| QueueItem {
                id: q.id.into(),
                name: q.name.into(),
                count: if i == 0 { 3 } else { 2 },
                max_concurrent: q.max_concurrent as i32,
                paused: false,
                post_action: if i == 1 {
                    "Notify".into()
                } else {
                    "None".into()
                },
            })
            .collect();
        main.set_set_queue_items(ModelRc::from(Rc::new(VecModel::from(demo_queues))));
        main.set_set_watch_folders(ModelRc::from(Rc::new(VecModel::from(vec![
            WatchFolderItem {
                id: "torrents-watch".into(),
                folder: "C:\\Users\\moham\\Downloads\\TorrentsWatch".into(),
                target_save_dir: "".into(),
                enabled: true,
                delete_after_add: true,
            },
        ]))));
        main.set_set_auto_extract_enabled(true);
        main.set_set_auto_extract_subfolder(true);
        main.set_set_auto_extract_overwrite(false);
        main.set_set_auto_extract_delete(false);
        main.set_set_backup_notice("Ready to export or import portable settings.".into());
        render_to_png(
            &adapter,
            1340,
            820,
            &out_dir.join("v3_settings_automation.png"),
        )?;
        render_settings_bittorrent_preview(&main, &adapter, &out_dir)?;
        render_settings_network_preview(&main, &adapter, &out_dir)?;
        main.set_settings_open(false);
        render_site_grabber_preview(&main, &adapter, &out_dir)?;
    }

    let settings = AppSettings {
        default_download_dir: "C:\\Users\\moham\\Downloads".into(),
        categories: default_download_categories(),
        ..AppSettings::default()
    };

    // 5. Floating Duplicate Link Prompt (460x275)
    {
        let win = DownloadConfirmWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        win.global::<Theme<'_>>().set_is_light(false);
        let preview = PendingDownloadPreview {
            pending_id: "dup-1".into(),
            url: "https://github.com/CipherVaults/SlipStream/releases/download/v3.0.0/SlipStream_3.0.0_x64-setup.exe".into(),
            source_domain: "github.com".into(),
            filename: "SlipStream_3.0.0_x64-setup.exe".into(),
            suggested_category_id: "programs".into(),
            suggested_save_dir: "C:\\Users\\moham\\Downloads\\Programs".into(),
            expected_size_bytes: Some(24_117_248),
            duplicate: Some(DuplicateMatch {
                transfer_id: "prev-1".into(),
                filename: "SlipStream_3.0.0_x64-setup.exe".into(),
                status: TransferStatus::Completed,
                save_dir: "C:\\Users\\moham\\Downloads\\Programs".into(),
                total_bytes: Some(24_117_248),
                downloaded_bytes: 24_117_248,
            }),
        };
        populate_confirm_preview(
            &win,
            &preview,
            &settings,
            &preview.suggested_save_dir,
            &preview.filename,
            1,
        );
        let _ = win.show();
        render_to_png(&adapter, 460, 275, &out_dir.join("v2_float_duplicate.png"))?;
    }

    // 6. Floating Download File Info (510x408)
    {
        let win = DownloadConfirmWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        win.global::<Theme<'_>>().set_is_light(false);
        let preview = PendingDownloadPreview {
            pending_id: "conf-1".into(),
            url: "https://github.com/CipherVaults/SlipStream/releases/download/v3.0.0/SlipStream_3.0.0_x64-setup.exe".into(),
            source_domain: "github.com".into(),
            filename: "SlipStream_3.0.0_x64-setup (1).exe".into(),
            suggested_category_id: "programs".into(),
            suggested_save_dir: "C:\\Users\\moham\\Downloads\\Programs".into(),
            expected_size_bytes: Some(24_117_248),
            duplicate: None,
        };
        populate_confirm_preview(
            &win,
            &preview,
            &settings,
            &preview.suggested_save_dir,
            &preview.filename,
            1,
        );
        let _ = win.show();
        render_to_png(&adapter, 510, 408, &out_dir.join("v2_float_confirm.png"))?;
    }

    // 7. Floating Download Progress Window (540x512)
    {
        let win = DownloadProgressWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        win.global::<Theme<'_>>().set_is_light(false);
        let demo = demo_telemetry_batch();
        let models = ProgressModels::default();
        models.attach(&win);
        populate_progress_window(&models, &win, &demo.transfers[1], true);
        let _ = win.show();
        render_to_png(&adapter, 540, 512, &out_dir.join("v2_float_progress.png"))?;
        populate_progress_window(&models, &win, &demo.transfers[0], true);
        models.update_peers(&win, &demo_peers());
        render_to_png(&adapter, 540, 532, &out_dir.join("v2_float_torrent.png"))?;
    }

    // 8. Floating Download Complete Window (460x256)
    {
        let win = DownloadCompleteWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        win.global::<Theme<'_>>().set_is_light(false);
        let demo = demo_telemetry_batch();
        populate_complete_window(&win, &demo.transfers[3]);
        let _ = win.show();
        render_to_png(&adapter, 460, 256, &out_dir.join("v2_float_complete.png"))?;
    }

    {
        let main = MainWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        main.set_compact_table_view(true);
        main.set_sort_column("name".into());
        main.set_sort_ascending(true);
        setup_demo_main(&main, false);
        main.set_version_label("3.0.0".into());
        main.show().map_err(|e| e.to_string())?;
        render_to_png(&adapter, 1340, 820, &out_dir.join("v3_table_dark.png"))?;
    }

    {
        let main = MainWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        setup_demo_main(&main, false);
        main.set_version_label("3.0.0".into());
        main.set_studio_mode(1);
        main.set_graph_window_seconds(300);
        let samples = (0..=1200)
            .map(|index| {
                let t = index as f64 / 40.0;
                SpeedSample {
                    timestamp_ms: index * 250,
                    down_bps: (24_000_000.0
                        + 9_000_000.0 * (t * 0.91).sin()
                        + 3_500_000.0 * (t * 2.7).sin()) as u64,
                    up_bps: (6_000_000.0
                        + 3_000_000.0 * (t * 0.74 + 0.8).sin()
                        + 1_400_000.0 * (t * 2.0).sin()) as u64,
                }
            })
            .collect();
        populate_speed_graph(&main, &samples);
        main.show().map_err(|e| e.to_string())?;
        render_to_png(&adapter, 1340, 820, &out_dir.join("v3_speed_graph.png"))?;
    }

    {
        let main = MainWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        setup_demo_main(&main, false);
        main.set_version_label("3.0.0".into());
        main.set_add_open(true);
        main.set_add_tab(4);
        main.set_add_batch_text(
            concat!(
                "https://downloads.example.org/releases/alpine-tools.zip\n",
                "https://cdn.example.org/media/alpine-aerial-4k.mkv\n",
                "https://downloads.example.org/releases/slipstream-sdk.7z",
            )
            .into(),
        );
        main.set_add_batch_url_count(3);
        main.set_add_batch_save_dir(settings.default_download_dir.as_str().into());
        main.set_add_batch_start_paused(true);
        main.show().map_err(|e| e.to_string())?;
        render_to_png(&adapter, 1340, 820, &out_dir.join("v3_batch_dialog.png"))?;
    }

    {
        let basket = DropBasketWindow::new().map_err(|e| e.to_string())?;
        let adapter = take_adapter()?;
        basket.global::<Theme<'_>>().set_is_light(false);
        basket.set_down_speed("31.1 MB/s".into());
        basket.set_up_speed("1.8 MB/s".into());
        basket.set_active_count(3);
        basket.show().map_err(|e| e.to_string())?;
        render_to_png(&adapter, 224, 72, &out_dir.join("v3_drop_basket.png"))?;
    }

    println!("Rendered preview PNGs into {}", out_dir.display());
    Ok(())
}
