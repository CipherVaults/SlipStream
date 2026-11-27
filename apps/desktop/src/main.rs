#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let demo_mode = std::env::args().any(|arg| arg == "--demo");
    if let Err(err) = slipstream_desktop_lib::shell::run_app(demo_mode) {
        tracing::error!(error = %err, "SlipStream exited with an error");
        std::process::exit(1);
    }
}
