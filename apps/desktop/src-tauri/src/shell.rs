use crate::{commands, AppState, DaemonOptions, OpenPickerEvent, UiBridge};
use slipstream_protocol::TelemetryBatch;
use std::{
    future::Future,
    io,
    mem::size_of,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::PageLoadEvent,
    AppHandle, Emitter, Manager, RunEvent, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use tokio::sync::{oneshot, watch};
use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};

const MAIN: &str = "main";
const OPEN_PICKER_EVENT: &str = "slipstream://open-picker";
const TELEMETRY_EVENT: &str = "slipstream://telemetry";
type Result<T> = std::result::Result<T, String>;

pub(crate) struct DesktopUi {
    app: AppHandle,
    present: AtomicBool,
    destroying: watch::Sender<bool>,
}

impl DesktopUi {
    fn new(app: AppHandle) -> Self {
        Self {
            app,
            present: AtomicBool::new(false),
            destroying: watch::channel(false).0,
        }
    }

    fn show(&self) -> Result<()> {
        let state = self.app.state::<Arc<AppState>>();
        let window = if let Some(window) = self.app.get_webview_window(MAIN) {
            window
        } else {
            let _ = state.window_loading();
            let config = self
                .app
                .config()
                .app
                .windows
                .first()
                .ok_or_else(|| "missing main window configuration".to_owned())?;
            let window = WebviewWindowBuilder::from_config(&self.app, config)
                .and_then(WebviewWindowBuilder::build)
                .map_err(|error| error.to_string())?;
            round_corners(&window);
            window
        };
        self.present.store(true, Ordering::Release);
        state.window_shown();
        window
            .show()
            .and_then(|()| window.unminimize())
            .and_then(|()| window.set_focus())
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn begin_hibernate(&self) -> Result<()> {
        if *self.destroying.borrow() {
            return Ok(());
        }
        if let Some(window) = self.app.get_webview_window(MAIN) {
            self.destroying.send_replace(true);
            self.present.store(false, Ordering::Release);
            let _ = self.app.state::<Arc<AppState>>().window_loading();
            if let Err(error) = window.destroy() {
                self.destroying.send_replace(false);
                if self.app.get_webview_window(MAIN).is_some() {
                    self.present.store(true, Ordering::Release);
                    self.app.state::<Arc<AppState>>().window_shown();
                }
                return Err(error.to_string());
            }
        }
        Ok(())
    }

    pub(crate) async fn hibernate(&self) -> Result<()> {
        self.on_main_thread(|ui| ui.begin_hibernate()).await
    }

    async fn on_main_thread<T: Send + 'static>(
        &self,
        call: impl FnOnce(&Self) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let app = self.app.clone();
        let (tx, rx) = oneshot::channel();
        self.app
            .run_on_main_thread(move || {
                let _ = tx.send(call(&app.state::<Arc<DesktopUi>>()));
            })
            .map_err(|error| error.to_string())?;
        rx.await.map_err(|error| error.to_string())?
    }
}

impl UiBridge for DesktopUi {
    fn show_main_window(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async {
            let mut destroying = self.destroying.subscribe();
            loop {
                destroying
                    .wait_for(|value| !*value)
                    .await
                    .map_err(|error| error.to_string())?;
                let shown = self
                    .on_main_thread(|ui| {
                        if *ui.destroying.borrow() {
                            return Ok(false);
                        }
                        ui.show()?;
                        Ok(true)
                    })
                    .await?;
                if shown {
                    return Ok(());
                }
            }
        })
    }

    fn emit_telemetry(&self, batch: &TelemetryBatch) -> Result<()> {
        if !self.has_main_window() || *self.destroying.borrow() {
            return Ok(());
        }
        let Some(window) = self.app.get_webview_window(MAIN) else {
            return Ok(());
        };
        window
            .emit(TELEMETRY_EVENT, batch)
            .map_err(|error| error.to_string())
    }

    fn emit_open_picker(&self, event: &OpenPickerEvent) -> Result<()> {
        if !self.has_main_window() || *self.destroying.borrow() {
            return Err("main window is unavailable".into());
        }
        let Some(window) = self.app.get_webview_window(MAIN) else {
            return Err("main window is unavailable".into());
        };
        window
            .emit(OPEN_PICKER_EVENT, event)
            .map_err(|error| error.to_string())
    }

    fn has_main_window(&self) -> bool {
        self.present.load(Ordering::Acquire) && !*self.destroying.borrow()
    }

    fn trigger_ready_check(&self) {
        if !self.has_main_window() {
            return;
        }
        if let Some(window) = self.app.get_webview_window(MAIN) {
            let state = self.app.state::<Arc<AppState>>().inner().clone();
            let generation = state.window_generation();
            page_finished(window, state, generation);
        }
    }

    fn request_quit(&self) {
        self.app.exit(0);
    }
}

fn round_corners(window: &WebviewWindow) {
    if let Ok(hwnd) = window.hwnd() {
        let corner = DWMWCP_ROUND;
        // Windows 10 can reject this attribute. The window remains usable.
        unsafe {
            DwmSetWindowAttribute(
                hwnd.0,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                (&raw const corner).cast(),
                size_of::<i32>() as u32,
            );
        }
    }
}

fn tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show SlipStream", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause All", true, None::<&str>)?;
    let resume = MenuItem::with_id(app, "resume", "Resume All", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &pause, &resume, &separator, &quit])?;
    let mut tray = TrayIconBuilder::with_id("slipstream")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("SlipStream")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_from_tray(app),
            "pause" | "resume" => {
                let pause = event.id().as_ref() == "pause";
                let state = app.state::<Arc<AppState>>().inner().clone();
                tauri::async_runtime::spawn(async move { state.change_all(pause).await });
            }
            "quit" => quit_from_shell(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_from_tray(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }
    tray.build(app)?;
    Ok(())
}

fn show_from_tray(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = state.show_main_window().await {
            tracing::warn!(%error, "could not restore the main window");
        }
    });
}

fn quit_from_shell(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    tauri::async_runtime::spawn(async move { state.quit().await });
}

fn page_finished(window: WebviewWindow, state: Arc<AppState>, generation: u64) {
    tauri::async_runtime::spawn(async move {
        let fast_until = tokio::time::Instant::now() + Duration::from_secs(3);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while state.window_generation() == generation && tokio::time::Instant::now() < deadline {
            // Finished can arrive before React's effect has subscribed. Query
            // the Tauri v2 event registration before handing over pending URLs.
            let (tx, rx) = oneshot::channel();
            let tx = std::sync::Mutex::new(Some(tx));
            let script = r#"Reflect.ownKeys(window.__internal_unstable_listeners_object_id__?.["slipstream://open-picker"] ?? {}).length > 0"#;
            if window
                .eval_with_callback(script, move |value| {
                    if let Some(tx) = tx.lock().unwrap().take() {
                        let _ = tx.send(value == "true");
                    }
                })
                .is_err()
            {
                return;
            }
            let subscribed = tokio::time::timeout(Duration::from_millis(250), rx)
                .await
                .is_ok_and(|answer| answer == Ok(true));
            if subscribed {
                if let Err(error) = state.window_ready(generation) {
                    tracing::warn!(%error, "could not open the picker");
                }
                return;
            }
            let delay = if tokio::time::Instant::now() < fast_until {
                30
            } else {
                200
            };
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    });
}

pub(crate) fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::get_snapshot,
            commands::add_http_download,
            commands::expand_wildcard_urls,
            commands::probe_media,
            commands::add_media_download,
            commands::add_torrent,
            commands::list_torrent_files,
            commands::set_torrent_files,
            commands::pause_transfer,
            commands::resume_transfer,
            commands::cancel_transfer,
            commands::refresh_download_url,
            commands::set_speed_limit,
            commands::verify_transfer_checksum,
            commands::reveal_in_explorer,
            commands::get_settings,
            commands::save_settings,
            commands::get_toolchain_status,
            commands::update_ytdlp,
            commands::install_browser_hosts,
            commands::list_browsers,
            commands::open_browser_extension_setup,
            commands::hibernate_to_tray,
            commands::check_for_updates,
            commands::install_update,
        ])
        .on_page_load(|webview, payload| {
            if webview.label() != MAIN {
                return;
            }
            let app = webview.app_handle();
            let state = app.state::<Arc<AppState>>();
            match payload.event() {
                PageLoadEvent::Started => {
                    let _ = state.window_loading();
                }
                PageLoadEvent::Finished => {
                    if let Some(window) = app.get_webview_window(MAIN) {
                        let generation = state.window_generation();
                        page_finished(window, state.inner().clone(), generation);
                    }
                }
            }
        })
        .setup(|app| {
            let ui = Arc::new(DesktopUi::new(app.handle().clone()));
            let data_dir = slipstream_native_host::token::local_app_directory()?;
            let resource_dir = app.path().resource_dir().ok();
            let state = tauri::async_runtime::block_on(AppState::start(
                data_dir,
                ui.clone(),
                DaemonOptions {
                    resource_dir,
                    ..DaemonOptions::default()
                },
            ))
            .map_err(io::Error::other)?;
            #[cfg(not(test))]
            if let Some(dir) = state.resource_dir() {
                if let Ok(executable) = crate::tools::native_host_executable(Some(dir)) {
                    if let Err(error) = crate::tools::install_browser_hosts_into(
                        state.registry_base_key(),
                        &state.data_dir().join("native-messaging"),
                        &executable,
                        None,
                        None,
                        None,
                    ) {
                        tracing::warn!(%error, "could not register browser native messaging hosts");
                    }
                }
            }
            app.manage(ui.clone());
            app.manage(state.clone());
            let setup = tray(app.handle())
                .map_err(|error| error.to_string())
                .and_then(|()| ui.show());
            if let Err(error) = setup {
                let _ = tauri::async_runtime::block_on(state.shutdown());
                return Err(io::Error::other(error).into());
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("could not build SlipStream desktop");

    app.run(|app, event| {
        let state = app.state::<Arc<AppState>>();
        match event {
            RunEvent::ExitRequested { api, .. } if !state.is_quitting() => api.prevent_exit(),
            RunEvent::WindowEvent {
                label,
                event: WindowEvent::CloseRequested { api, .. },
                ..
            } if label == MAIN && !state.is_quitting() => {
                api.prevent_close();
                if state.settings().close_to_tray_hibernate {
                    if let Err(error) = app.state::<Arc<DesktopUi>>().begin_hibernate() {
                        tracing::warn!(%error, "could not hibernate to the tray");
                    }
                } else {
                    quit_from_shell(app);
                }
            }
            RunEvent::WindowEvent {
                label,
                event: WindowEvent::Destroyed,
                ..
            } if label == MAIN => {
                let ui = app.state::<Arc<DesktopUi>>();
                ui.present.store(false, Ordering::Release);
                let _ = state.window_loading();
                ui.destroying.send_replace(false);
            }
            _ => {}
        }
    });
}
