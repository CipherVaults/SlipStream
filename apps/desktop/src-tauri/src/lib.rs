#![cfg(windows)]

mod commands;
mod daemon;
mod media_jobs;
mod paths;
mod settings;
mod shell;
mod tools;

#[cfg(test)]
mod tests;

pub use daemon::{expand_wildcard_urls, AppState, DaemonOptions, OpenPickerEvent, UiBridge};
pub use tools::{install_browser_hosts_into, ToolchainStatus};

pub fn desktop_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn run() {
    shell::run();
}
