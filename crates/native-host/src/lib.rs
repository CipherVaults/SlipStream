pub mod framing;
pub mod manifest;

#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
pub mod registry;
#[cfg(windows)]
pub mod security;
#[cfg(windows)]
pub mod stdio;
#[cfg(windows)]
pub mod token;

pub fn native_host_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
