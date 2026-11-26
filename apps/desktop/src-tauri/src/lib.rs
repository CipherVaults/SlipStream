#![cfg(windows)]

mod commands;
mod daemon;
mod media_jobs;
mod paths;
mod pending_downloads;
mod settings;
mod shell;
mod tools;

#[cfg(test)]
mod tests;

pub use daemon::{expand_wildcard_urls, AppState, DaemonOptions, OpenPickerEvent, UiBridge};
pub use settings::{match_category_for_filename, next_numbered_filename, resolve_category_dir};
pub use tools::{install_browser_hosts_into, ToolchainStatus};

pub fn desktop_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn run() {
    shell::run();
}
