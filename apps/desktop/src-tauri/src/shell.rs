use crate::{
    commands,
    paths::{canonical_directory, local_drive_path, validate_existing_ancestors},
    AppState, DaemonOptions, OpenPickerEvent, UiBridge,
};
use slipstream_protocol::TelemetryBatch;
use std::{
    future::Future,
    io,
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    pin::Pin,
    ptr::{null, null_mut},
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
    AppHandle, Emitter, EventTarget, Manager, RunEvent, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tokio::sync::{oneshot, watch};
use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};

pub(crate) const MAIN: &str = "main";
const OPEN_PICKER_EVENT: &str = "slipstream://open-picker";
const TELEMETRY_EVENT: &str = "slipstream://telemetry";
pub(crate) const CONFIRM_PREFIX: &str = "dl-confirm-";
pub(crate) const PROGRESS_PREFIX: &str = "dl-progress-";
pub(crate) const COMPLETE_PREFIX: &str = "dl-complete-";
type Result<T> = std::result::Result<T, String>;

fn is_allowed_app_url(url: &url::Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let origin = (url.scheme(), url.host_str(), url.port());
    #[cfg(debug_assertions)]
    if origin == ("http", Some("localhost"), Some(1420)) {
        return true;
    }
    matches!(
        origin,
        ("tauri", Some("localhost"), None) | ("http", Some("tauri.localhost"), None)
    )
}

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
                .iter()
                .find(|config| config.label == MAIN)
                .ok_or_else(|| "missing main window configuration".to_owned())?;
            let window = WebviewWindowBuilder::from_config(&self.app, config)
                .map(|builder| {
                    builder
                        .on_navigation(is_allowed_app_url)
                        .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
                        .visible(false)
                })
                .and_then(WebviewWindowBuilder::build)
                .map_err(|error| error.to_string())?;
            if let Err(error) = configure_webview2(&window) {
                let _ = window.destroy();
                return Err(error);
            }
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

    fn open_download_dialog(
        &self,
        prefix: &'static str,
        id: &str,
        size: (f64, f64),
        resizable: bool,
    ) -> Result<()> {
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("Invalid download ID.".into());
        }
        let app = self.app.clone();
        let id = id.to_owned();
        self.app
            .run_on_main_thread(move || {
                let Some(state) = app.try_state::<Arc<AppState>>() else {
                    return;
                };
                if state.is_quitting()
                    || (prefix == CONFIRM_PREFIX && state.get_pending_download(&id).is_err())
                {
                    return;
                }
                let label = format!("{prefix}{id}");
                let result = (|| {
                    let window = if let Some(window) = app.get_webview_window(&label) {
                        window
                    } else {
                        let variant = prefix.trim_start_matches("dl-").trim_end_matches('-');
                        let url = WebviewUrl::App(PathBuf::from(format!(
                            "index.html?window={variant}&id={id}"
                        )));
                        let window = WebviewWindowBuilder::new(&app, &label, url)
                            .on_navigation(is_allowed_app_url)
                            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
                            .title("SlipStream")
                            .inner_size(size.0, size.1)
                            .decorations(false)
                            .resizable(resizable)
                            .center()
                            .visible(false)
                            .build()
                            .map_err(|error| error.to_string())?;
                        if let Err(error) = configure_webview2(&window) {
                            let _ = window.destroy();
                            return Err(error);
                        }
                        round_corners(&window);
                        window
                    };
                    window
                        .show()
                        .and_then(|()| window.unminimize())
                        .and_then(|()| window.set_focus())
                        .map_err(|error| error.to_string())
                })();
                if let Err(error) = result {
                    tracing::warn!(%label, %error, "could not show download dialog");
                    if prefix == CONFIRM_PREFIX {
                        state.discard_pending_download(&id);
                        if let Some(window) = app.get_webview_window(&label) {
                            let _ = window.destroy();
                        }
                    }
                }
                state.window_shown();
            })
            .map_err(|error| error.to_string())
    }

    fn close_download_dialog(&self, prefix: &str, id: &str) {
        let app = self.app.clone();
        let label = format!("{prefix}{id}");
        let _ = self.app.run_on_main_thread(move || {
            if let Some(window) = app.get_webview_window(&label) {
                let _ = window.destroy();
            }
        });
    }

    fn is_telemetry_window(&self, label: &str) -> bool {
        (label == MAIN && self.has_main_window())
            || label.starts_with(PROGRESS_PREFIX)
            || label.starts_with(COMPLETE_PREFIX)
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
        // Emit once so listeners using Tauri's default Any target receive one batch.
        self.app
            .emit_filter(TELEMETRY_EVENT, batch, |target| match target {
                EventTarget::Window { label }
                | EventTarget::Webview { label }
                | EventTarget::WebviewWindow { label }
                | EventTarget::AnyLabel { label } => self.is_telemetry_window(label),
                _ => false,
            })
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

    fn emit_settings_changed(&self) {
        let _ = self.app.emit_to(MAIN, "slipstream://settings-updated", ());
    }

    fn has_main_window(&self) -> bool {
        self.present.load(Ordering::Acquire) && !*self.destroying.borrow()
    }

    fn has_telemetry_windows(&self) -> bool {
        self.has_main_window()
            || self
                .app
                .webview_windows()
                .keys()
                .any(|label| self.is_telemetry_window(label))
    }

    fn supports_dialogs(&self) -> bool {
        true
    }

    fn open_confirm_dialog(&self, id: &str) -> Result<()> {
        self.open_download_dialog(CONFIRM_PREFIX, id, (540.0, 360.0), false)
    }

    fn close_confirm_dialog(&self, id: &str) {
        self.close_download_dialog(CONFIRM_PREFIX, id);
    }

    fn open_progress_dialog(&self, id: &str) -> Result<()> {
        self.open_download_dialog(PROGRESS_PREFIX, id, (540.0, 340.0), true)
    }

    fn close_progress_dialog(&self, id: &str) {
        self.close_download_dialog(PROGRESS_PREFIX, id);
    }

    fn open_complete_dialog(&self, id: &str) -> Result<()> {
        self.open_download_dialog(COMPLETE_PREFIX, id, (460.0, 220.0), false)
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

fn configure_webview2(window: &WebviewWindow) -> Result<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Settings3, ICoreWebView2Settings5,
    };
    use windows_core::Interface;

    // Tauri dispatches with_webview inline on the UI thread. Check completion before showing it.
    let (applied, result) = std::sync::mpsc::channel();
    window
        .with_webview(move |webview| {
            let configure = || -> windows_core::Result<()> {
                // SAFETY: Tauri supplies the live controller on the WebView's UI thread.
                unsafe {
                    let settings = webview.controller().CoreWebView2()?.Settings()?;
                    settings.SetAreDefaultContextMenusEnabled(false)?;
                    settings.SetIsStatusBarEnabled(false)?;
                    settings.SetIsZoomControlEnabled(false)?;
                    #[cfg(not(debug_assertions))]
                    settings.SetAreDevToolsEnabled(false)?;
                    settings
                        .cast::<ICoreWebView2Settings3>()?
                        .SetAreBrowserAcceleratorKeysEnabled(false)?;
                    settings
                        .cast::<ICoreWebView2Settings5>()?
                        .SetIsPinchZoomEnabled(false)?;
                }
                Ok(())
            };
            let _ = applied.send(configure());
        })
        .map_err(|error| format!("Could not access the WebView2 controller: {error}"))?;
    result
        .try_recv()
        .map_err(|_| "Could not apply the WebView2 settings on the UI thread.".to_owned())?
        .map_err(|error| format!("Could not configure the WebView2 settings: {error}"))
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

pub(crate) fn open_file(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let file = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let verb = windows_sys::w!("open");
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            verb,
            file.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    if result as usize <= 32 {
        return Err(io::Error::other(format!(
            "Windows could not open the file (shell error {}).",
            result as usize
        )));
    }
    Ok(())
}

pub(crate) fn pick_download_folder(
    owner: isize,
    initial_dir: Option<String>,
) -> io::Result<Option<String>> {
    use std::ffi::OsString;
    use windows_sys::Win32::{
        Foundation::HWND,
        System::Com::{
            CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
            COINIT_DISABLE_OLE1DDE,
        },
        UI::{
            Shell::{
                SHBrowseForFolderW, SHGetPathFromIDListEx, BFFM_INITIALIZED, BFFM_SETSELECTIONW,
                BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, GPFIDL_DEFAULT,
            },
            WindowsAndMessaging::SendMessageW,
        },
    };

    unsafe extern "system" fn select_initial_folder(
        window: HWND,
        message: u32,
        _: isize,
        initial: isize,
    ) -> i32 {
        if message == BFFM_INITIALIZED && initial != 0 {
            // The caller owns the NUL-terminated path for the lifetime of the browse dialog.
            unsafe { SendMessageW(window, BFFM_SETSELECTIONW, 1, initial) };
        }
        0
    }

    let initial = initial_dir
        .filter(|path| !path.trim().is_empty())
        .map(|path| {
            let directory = local_drive_path(Path::new(&path))?;
            validate_existing_ancestors(&directory)?;
            Ok::<_, io::Error>(
                directory
                    .ancestors()
                    .find(|path| path.is_dir())
                    .map(|path| {
                        path.as_os_str()
                            .encode_wide()
                            .chain(Some(0))
                            .collect::<Vec<_>>()
                    }),
            )
        })
        .transpose()?
        .flatten();
    let code = unsafe {
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if code < 0 {
        return Err(io::Error::other(format!(
            "Could not initialize the folder picker (0x{code:08x})."
        )));
    }
    struct ComApartment;
    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _apartment = ComApartment;
    let mut display = [0_u16; 260];
    let info = BROWSEINFOW {
        hwndOwner: owner as HWND,
        pidlRoot: null_mut(),
        pszDisplayName: display.as_mut_ptr(),
        lpszTitle: windows_sys::w!("Select a download folder"),
        ulFlags: BIF_NEWDIALOGSTYLE | BIF_RETURNONLYFSDIRS,
        lpfn: Some(select_initial_folder),
        lParam: initial.as_ref().map_or(0, |path| path.as_ptr() as isize),
        iImage: 0,
    };
    let mut selected = vec![0_u16; 32_768];
    let resolved = unsafe {
        let item = SHBrowseForFolderW(&info);
        if item.is_null() {
            None
        } else {
            let found = SHGetPathFromIDListEx(
                item,
                selected.as_mut_ptr(),
                selected.len() as u32,
                GPFIDL_DEFAULT,
            );
            CoTaskMemFree(item.cast());
            Some(found != 0)
        }
    };
    match resolved {
        None => Ok(None),
        Some(false) => Err(io::Error::other(
            "Could not resolve the selected download folder.",
        )),
        Some(true) => {
            let end = selected
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(selected.len());
            let path = PathBuf::from(OsString::from_wide(&selected[..end]));
            let path = local_drive_path(&canonical_directory(&path)?)?;
            Ok(Some(path.into_os_string().into_string().map_err(|_| {
                io::Error::other("Download folder is not Unicode.")
            })?))
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
            commands::mark_ui_ready,
            commands::get_snapshot,
            commands::add_http_download,
            commands::get_pending_download,
            commands::confirm_pending_download,
            commands::cancel_pending_download,
            commands::resolve_duplicate_existing,
            commands::add_download_category,
            commands::set_transfer_speed_limit,
            commands::get_transfer_completion_options,
            commands::set_transfer_completion_options,
            commands::open_progress_dialog,
            commands::open_completed_file,
            commands::pick_download_folder,
            commands::expand_wildcard_urls,
            commands::probe_media,
            commands::add_media_download,
            commands::add_torrent,
            commands::list_torrent_files,
            commands::set_torrent_files,
            commands::pause_transfer,
            commands::resume_transfer,
            commands::pause_playlist,
            commands::resume_playlist,
            commands::reveal_playlist_folder,
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
            if let Ok(executable) = crate::tools::native_host_executable(state.resource_dir()) {
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
            RunEvent::WindowEvent {
                label,
                event: WindowEvent::Destroyed,
                ..
            } => {
                if let Some(id) = label.strip_prefix(CONFIRM_PREFIX) {
                    state.discard_pending_download(id);
                } else if label.starts_with(PROGRESS_PREFIX) || label.starts_with(COMPLETE_PREFIX) {
                    state.window_shown();
                }
            }
            _ => {}
        }
    });
}
