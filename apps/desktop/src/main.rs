#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use slipstream_desktop_lib::{
    local_picker_pipe_name, set_initial_picker_source, shell, LOCAL_PICKER_TOKEN_FILE,
};
use slipstream_native_host::{pipe::PipeClient, security::default_pipe_name, token::SessionToken};
use slipstream_protocol::{NativeBridgeRequest, NativeBridgeResponse};
use std::{path::Path, time::Duration};

const HANDOFF_ATTEMPT: Duration = Duration::from_millis(250);
const HANDOFF_STARTUP: Duration = Duration::from_millis(2_500);
const HANDOFF_BACKOFF: Duration = Duration::from_millis(50);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let demo_mode = args.iter().any(|arg| arg == "--demo");
    let source = (!demo_mode)
        .then(|| {
            args.iter()
                .filter_map(|arg| arg.to_str())
                .find_map(picker_source)
        })
        .flatten();
    if let Some(source) = source.as_deref() {
        if handoff_picker(source, false) {
            return;
        }
    }
    set_initial_picker_source(source.clone());
    if let Err(err) = shell::run_app(demo_mode) {
        // Another launch may have claimed the pipe while this one was starting.
        if source
            .as_deref()
            .is_some_and(|source| handoff_picker(source, true))
        {
            return;
        }
        tracing::error!(error = %err, "SlipStream exited with an error");
        std::process::exit(1);
    }
}

fn is_magnet_source(source: &str) -> bool {
    source
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("magnet"))
}

fn picker_source(arg: &str) -> Option<String> {
    let source = arg.trim();
    if is_magnet_source(source) {
        return Some(source.to_owned());
    }
    let path = Path::new(source);
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"))
    {
        return None;
    }
    // The running instance can have a different working directory.
    std::path::absolute(path)
        .ok()?
        .into_os_string()
        .into_string()
        .ok()
}

fn handoff_picker(source: &str, retry: bool) -> bool {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    runtime.block_on(async {
        let Ok(pipe_name) = default_pipe_name() else {
            return false;
        };
        let pipe_name = local_picker_pipe_name(&pipe_name);
        let token_path = shell::default_data_dir().join(LOCAL_PICKER_TOKEN_FILE);
        let request = NativeBridgeRequest::OpenDesktopWindow {
            open_picker_url: Some(source.to_owned()),
            headers: Default::default(),
        };
        tokio::time::timeout(HANDOFF_STARTUP, async {
            loop {
                if let Ok(token) = SessionToken::read(&token_path) {
                    let connect = PipeClient::connect(&pipe_name, &token, "cli-handoff");
                    if let Ok(Ok(client)) = tokio::time::timeout(HANDOFF_ATTEMPT, connect).await {
                        return matches!(
                            client.request(&request).await,
                            Ok((_, NativeBridgeResponse::Ack { ok: true, .. }))
                        );
                    }
                }
                if !retry {
                    return false;
                }
                tokio::time::sleep(HANDOFF_BACKOFF).await;
            }
        })
        .await
        .unwrap_or(false)
    })
}
