#[cfg(windows)]
mod daemon;
#[cfg(windows)]
mod media_jobs;
#[cfg(windows)]
mod paths;
#[cfg(windows)]
mod pending_downloads;
#[cfg(windows)]
mod settings;
#[cfg(windows)]
pub mod shell;
#[cfg(windows)]
mod tools;
pub mod updater;
pub mod view_model;
#[cfg(windows)]
pub mod win32;

#[cfg(all(test, windows))]
mod tests;

#[cfg(windows)]
slint::include_modules!();

#[cfg(windows)]
pub use daemon::{expand_wildcard_urls, AppState, DaemonOptions, OpenPickerEvent, UiBridge};
#[cfg(windows)]
pub use settings::{match_category_for_filename, next_numbered_filename, resolve_category_dir};
#[cfg(windows)]
pub use tools::{install_browser_hosts_into, ToolchainStatus};

#[cfg(windows)]
static INITIAL_PICKER_SOURCE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(windows)]
pub const LOCAL_PICKER_TOKEN_FILE: &str = "local-picker.token";

#[cfg(windows)]
pub fn local_picker_pipe_name(native_pipe_name: &str) -> String {
    format!("{native_pipe_name}.local-picker")
}

#[cfg(windows)]
pub fn set_initial_picker_source(source: Option<String>) {
    *INITIAL_PICKER_SOURCE.lock().unwrap() = source;
}

#[cfg(windows)]
pub fn take_initial_picker_source() -> Option<String> {
    INITIAL_PICKER_SOURCE.lock().unwrap().take()
}

pub fn desktop_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
