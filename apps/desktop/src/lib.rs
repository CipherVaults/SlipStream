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

pub fn desktop_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
