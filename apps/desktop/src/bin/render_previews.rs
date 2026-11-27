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
        DesktopModels, ProgressModels,
    },
    view_model::{demo_telemetry_batch, format_bytes},
    BrowserCardModel, DownloadCompleteWindow, DownloadConfirmWindow, DownloadProgressWindow,
    FormatOptionModel, MainWindow, Theme, TorrentPreviewFileModel,
};
use slipstream_protocol::{
    default_download_categories, AppSettings, DuplicateMatch, PendingDownloadPreview,
    TorrentFileEntry, TorrentPeerEntry, TorrentPeerSummary, TransferStatus,
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
    main.set_version_label("2.1.0".into());

    let batch = demo_telemetry_batch();
    let history = vec![
        11_200_000, 15_400_000, 22_800_000, 19_600_000, 27_100_000, 31_400_000, 29_900_000,
        34_800_000, 38_640_000, 32_620_000,
    ];
    let expanded = HashSet::new();
    let mut selected = Some("demo-1".to_owned());
    let mut models = DesktopModels::default();
    models.attach(main);
    models.apply_telemetry_snapshot(main, &batch, &history, &expanded, &mut selected);
    models.update_inspector_files(&demo_torrent_files(), None);
    let peers = demo_peers();
    models.update_inspector_peers(Some(&peers));
    models.update_inspector(
        main,
        batch
            .transfers
            .iter()
            .find(|t| selected.as_deref() == Some(&t.id)),
        Some(&peers),
    );
    main.set_disk_free_text("91.7 GB free".into());
}

fn demo_torrent_files() -> Vec<TorrentFileEntry> {
    vec![
        TorrentFileEntry {
            index: 0,
            relative_path: "ubuntu-24.04.2-desktop-amd64.iso".into(),
            size_bytes: 6_174_015_488,
            downloaded_bytes: 4_815_732_080,
            selected: true,
        },
        TorrentFileEntry {
            index: 1,
            relative_path: "SHA256SUMS".into(),
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
            url: "https://github.com/CipherVaults/SlipStream/releases/download/v2.1.0/SlipStream_2.1.0_x64-setup.exe".into(),
            source_domain: "github.com".into(),
            filename: "SlipStream_2.1.0_x64-setup.exe".into(),
            suggested_category_id: "programs".into(),
            suggested_save_dir: "C:\\Users\\moham\\Downloads\\Programs".into(),
            expected_size_bytes: Some(24_117_248),
            duplicate: Some(DuplicateMatch {
                transfer_id: "prev-1".into(),
                filename: "SlipStream_2.1.0_x64-setup.exe".into(),
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
            url: "https://github.com/CipherVaults/SlipStream/releases/download/v2.1.0/SlipStream_2.1.0_x64-setup.exe".into(),
            source_domain: "github.com".into(),
            filename: "SlipStream_2.1.0_x64-setup (1).exe".into(),
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

    println!("Rendered 11 preview PNGs into {}", out_dir.display());
    Ok(())
}
