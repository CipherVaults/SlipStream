pub fn native_host_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
