use super::{accepted, outcome, require, Grade, Suite};
use crate::report::Outcome;
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use slipstream_native_host::framing::{
    read_frame, read_frame_async, write_frame, write_frame_async,
};
#[cfg(windows)]
use slipstream_native_host::{
    pipe::{DefaultPipeHandler, PipeClient, PipeRequestHandler, PipeServer, PipeTimeouts},
    security::{current_user_sid, pipe_name_for_sid, pipe_sddl},
    token::SessionToken,
};
use slipstream_protocol::{NativeBridgeRequest, NativeBridgeResponse};
use std::io::{ErrorKind, Result as IoResult};
use std::time::Duration;
#[cfg(windows)]
use std::{
    ffi::c_void,
    fs,
    os::windows::io::{AsRawHandle, RawHandle},
    path::PathBuf,
    ptr::null_mut,
    sync::{Arc, Mutex},
};
use tokio::time;
#[cfg(windows)]
use tokio::{io::AsyncWriteExt, net::windows::named_pipe::ClientOptions, sync::oneshot};

const CATEGORY: &str = "ipc_framing";
// Independent 1 MiB framing specification oracle (intentionally not imported from slipstream_protocol).
const INDEPENDENT_MAX_FRAME_BYTES: usize = 1_048_576;
const LIMIT: u32 = INDEPENDENT_MAX_FRAME_BYTES as u32;
const ITEM_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn run(suite: &mut Suite) {
    let mut oversized = vec![LIMIT + 1, u32::MAX];
    oversized.extend((2..=19).map(|n| LIMIT + n * 1024));
    for length in oversized {
        let mut bytes = length.to_le_bytes().to_vec();
        bytes.extend(b"{}");
        invalid(
            suite,
            "oversized_prefix",
            "boundary",
            bytes,
            ErrorKind::InvalidData,
            length == LIMIT + 1,
        )
        .await;
    }
    invalid(
        suite,
        "zero_length",
        "boundary",
        vec![0; 4],
        ErrorKind::InvalidData,
        false,
    )
    .await;
    for n in 1..=15u32 {
        let truncated = if n <= 3 {
            64u32.to_le_bytes()[..n as usize].to_vec()
        } else {
            let body = b"{\"type\":";
            let mut frame = (body.len() as u32 + n - 3).to_le_bytes().to_vec();
            frame.extend(body);
            frame
        };
        invalid(
            suite,
            "truncated_frame",
            "boundary",
            truncated,
            ErrorKind::UnexpectedEof,
            false,
        )
        .await;
    }
    for n in 0..15 {
        let mut non_utf8 = br#"{"type":"probe_media","url":"fixture"#.to_vec();
        non_utf8.push(0xf5 + n % 11);
        non_utf8.extend(format!(r#"/{n}"}}"#).bytes());
        invalid(
            suite,
            "non_utf8",
            "direct",
            framed(&non_utf8),
            ErrorKind::InvalidData,
            false,
        )
        .await;

        let malformed = format!(r#"{{"type":"probe_media","url":[{n},]}}"#);
        invalid(
            suite,
            "malformed_json",
            "direct",
            framed(malformed.as_bytes()),
            ErrorKind::InvalidData,
            false,
        )
        .await;

        let unknown = json!({"type": format!("unregistered_test_{n}"), "sequence": n});
        invalid(
            suite,
            "unknown_variant",
            "composed",
            framed(&wire_json(&unknown)),
            ErrorKind::InvalidData,
            false,
        )
        .await;

        let wrong_type = match n % 3 {
            0 => json!({"type":"pause_transfer", "transfer_id":n}),
            1 => json!({"type":"probe_media", "url":[n]}),
            _ => json!({"type":"grab_batch_urls", "urls":{"item": n}}),
        };
        invalid(
            suite,
            "invalid_field_type",
            "composed",
            framed(&wire_json(&wrong_type)),
            ErrorKind::InvalidData,
            false,
        )
        .await;
    }
    for n in 0..18 {
        let missing = match n % 4 {
            0 => json!({"type":"authenticate", "client_id":format!("extension-{n}")}),
            1 => {
                json!({"type":"add_http_download", "request":{"filename":format!("clip-{n}.mp4"), "headers":{}}})
            }
            2 => {
                json!({"type":"grab_batch_urls", "referrer":format!("https://cdn.example.test/{n}")})
            }
            _ => {
                json!({"type":"refresh_download_url", "request":{"transfer_id":format!("download-{n}"), "headers":{}}})
            }
        };
        invalid(
            suite,
            "missing_field",
            "composed",
            framed(&wire_json(&missing)),
            ErrorKind::InvalidData,
            false,
        )
        .await;
    }

    benign_wire(suite, json!({"type": "ping"}), "direct").await;
    benign_wire(suite, json!({"type": "get_status"}), "direct").await;
    for n in 0..10 {
        let url = format!("https://cdn.example.test/資料/ملف-{n}.mp4?download=1&sig=a%2Fb%3D");
        let referrer = format!("https://media.example.test/watch/{n}");
        let filename = format!("ملف 資料 🚀 ({n}).mp4");
        let id = format!("transfer-{n:03}");
        for payload in [
            json!({
                "type": "authenticate",
                "token": format!("fixture-token-{n}"),
                "client_id": format!("extension-{n}"),
            }),
            json!({
                "type": "add_http_download",
                "request": {
                    "url": url,
                    "filename": filename,
                    "save_dir": null,
                    "max_connections": 8,
                    "queue_id": null,
                    "headers": {},
                    "referrer": referrer,
                    "expected_sha256": null,
                }
            }),
            json!({
                "type": "probe_media",
                "url": url,
            }),
            json!({
                "type": "add_media_download",
                "request": {
                    "url": url,
                    "format_id": "137+140",
                    "audio_only_preset": null,
                    "target_container": "mp4",
                    "embed_subtitles_langs": ["ar", "zh-Hant"],
                    "lossless_remux": true,
                    "normalize_audio": false,
                    "save_dir": null,
                    "filename": filename,
                    "playlist_items": "1-5",
                }
            }),
            json!({
                "type": "add_torrent",
                "request": {
                    "source": format!("資料 ({n}).torrent"),
                    "save_dir": null,
                    "selected_file_indices": [0, 2],
                    "sequential": true,
                }
            }),
            json!({
                "type": "refresh_download_url",
                "request": {
                    "transfer_id": id,
                    "new_url": url,
                    "headers": {"Authorization": "Bearer fixture-token"},
                    "referrer": referrer,
                }
            }),
            json!({
                "type": "pause_transfer",
                "transfer_id": id,
            }),
            json!({
                "type": "resume_transfer",
                "transfer_id": id,
            }),
            json!({
                "type": "open_desktop_window",
                "open_picker_url": url,
            }),
            json!({
                "type": "grab_batch_urls",
                "urls": [url, format!("https://cdn.example.test/audio/{n}.m4a?v=2")],
                "referrer": referrer,
            }),
        ] {
            benign_wire(suite, payload, "composed").await;
        }
    }
    for count in [1, 2, 4, 8, 12, 16, 24, 32, 48, 64, 96, 128] {
        benign_wire(suite, batch_json(count, 64), "composed").await;
    }
    for target in [INDEPENDENT_MAX_FRAME_BYTES - 1, INDEPENDENT_MAX_FRAME_BYTES] {
        let mut payload = batch_json(256, 3900);
        let current_len = wire_json(&payload).len();
        let padding = target - current_len;
        let urls = payload["urls"].as_array_mut().expect("urls array");
        let last = urls[255].as_str().expect("url string");
        urls[255] = Value::String(format!("{last}{}", "a".repeat(padding)));
        assert_eq!(wire_json(&payload).len(), target);
        benign_wire(suite, payload, "boundary").await;
    }

    run_pipe_trust_boundaries(suite).await;
}

#[cfg(windows)]
async fn run_pipe_trust_boundaries(suite: &mut Suite) {
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone, Default)]
    struct RecordingHandler {
        seen: Arc<Mutex<Vec<NativeBridgeRequest>>>,
    }

    impl PipeRequestHandler for RecordingHandler {
        async fn handle(&self, request: NativeBridgeRequest) -> NativeBridgeResponse {
            self.seen.lock().unwrap().push(request.clone());
            match request {
                NativeBridgeRequest::OpenDesktopWindow { .. } => NativeBridgeResponse::Ack {
                    ok: true,
                    message: Some("picker_opened".into()),
                },
                other => DefaultPipeHandler.handle(other).await,
            }
        }
    }

    let temp = TempDir(
        std::env::temp_dir().join(format!("slipstream-eval-pipe-{}", uuid::Uuid::new_v4())),
    );
    fs::create_dir_all(&temp.0).expect("create pipe eval temp dir");
    let token_path = temp.0.join("session.token");
    let sid = current_user_sid().expect("current user SID");
    let pipe_name = format!("{}.eval.{}", pipe_name_for_sid(&sid), uuid::Uuid::new_v4());
    let server = PipeServer::bind(&pipe_name, &token_path)
        .expect("bind eval Named Pipe")
        .with_timeouts(PipeTimeouts {
            auth: Duration::from_secs(2),
            read: Duration::from_secs(2),
            write: Duration::from_secs(2),
            handler: Duration::from_secs(2),
        });
    let handler = RecordingHandler::default();
    let seen = Arc::clone(&handler.seen);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_task = tokio::spawn(server.run(handler, async {
        let _ = shutdown_rx.await;
    }));
    let token = SessionToken::read(&token_path).expect("read published session token");

    // 6 harmful pipe trust-boundary cases
    for (surface, req_bytes) in [
        (
            "pipe_unauth_ping",
            framed(&wire_json(&json!({"type": "ping"}))),
        ),
        (
            "pipe_unauth_status",
            framed(&wire_json(&json!({"type": "get_status"}))),
        ),
        (
            "pipe_unauth_open_window",
            framed(&wire_json(
                &json!({"type": "open_desktop_window", "open_picker_url": "https://cdn.example.test/v.mp4"}),
            )),
        ),
        (
            "pipe_wrong_token",
            framed(&wire_json(
                &json!({"type": "authenticate", "token": "0".repeat(64), "client_id": "attacker"}),
            )),
        ),
        (
            "pipe_blank_client_id",
            framed(&wire_json(
                &json!({"type": "authenticate", "token": "valid_token_placeholder", "client_id": "   "}),
            )),
        ),
        ("pipe_oversized_frame", (LIMIT + 1).to_le_bytes().to_vec()),
    ] {
        let expected_code = if surface == "pipe_oversized_frame" {
            "invalid_frame"
        } else {
            "authentication_failed"
        };
        let item = suite.register(
            CATEGORY,
            "harmful",
            surface,
            "boundary",
            format!("{surface}\0{}", req_bytes.len()).into_bytes(),
        );
        let pipe_name = pipe_name.clone();
        let token_path = token_path.clone();
        let seen_ref = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            let mut client = accepted(ClientOptions::new().open(&pipe_name), "open Named Pipe")?;
            let wire_payload = if surface == "pipe_blank_client_id" {
                let req = NativeBridgeRequest::Authenticate {
                    token: fs::read_to_string(&token_path).expect("session token hex"),
                    client_id: "   ".into(),
                };
                framed(&wire_json(&req))
            } else {
                req_bytes
            };
            accepted(client.write_all(&wire_payload).await, "write to pipe")?;
            accepted(client.flush().await, "flush pipe")?;
            let response = accepted(
                read_frame_async::<NativeBridgeResponse>(&mut client).await,
                "read rejection frame",
            )?;
            require(
                matches!(
                    response,
                    Some(NativeBridgeResponse::Error { ref code, .. }) if code == expected_code
                ),
                format!("expected {expected_code} rejection, got {response:?}"),
            )?;
            let trailing = read_frame_async::<NativeBridgeResponse>(&mut client).await;
            require(
                matches!(trailing, Ok(None))
                    || matches!(
                        trailing,
                        Err(ref e)
                            if matches!(
                                e.kind(),
                                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
                            )
                    ),
                format!("pipe remained open or failed ambiguously after rejection: {trailing:?}"),
            )?;
            let recorded = seen_ref.lock().unwrap();
            require(
                recorded.is_empty(),
                format!("pipe handler was dispatched after rejected {surface}: {recorded:?}"),
            )?;
            Ok("Windows Named Pipe rejected the unauthenticated or oversized request.")
        });
        suite.records.push(item.record(join(task).await, 1, None));
    }

    assert!(
        seen.lock().unwrap().is_empty(),
        "harmful pipe cases leaked into RecordingHandler"
    );

    // 4 benign pipe trust-boundary cases
    let sddl_item = suite.register(
        CATEGORY,
        "benign",
        "pipe_sid_sddl",
        "direct",
        b"pipe_sid_sddl_check".to_vec(),
    );
    let dacl_pipe_name = pipe_name.clone();
    let dacl_task = tokio::spawn(async move {
        let actual_sid = accepted(current_user_sid(), "current_user_sid")?;
        let sddl = pipe_sddl(&actual_sid);
        let name = pipe_name_for_sid(&actual_sid);
        require(
            actual_sid.starts_with("S-1-")
                && sddl == format!("D:(A;;GA;;;{actual_sid})")
                && !sddl.contains(";WD)")
                && !sddl.contains(";AN)")
                && !sddl.contains(";AU)")
                && name.starts_with(r"\\.\pipe\SlipStream.IPC."),
            format!("unexpected pipe SID/SDDL/name: {sddl} {name}"),
        )?;
        let raw_handle = accepted(
            ClientOptions::new().open(&dacl_pipe_name),
            "open Named Pipe for DACL inspection",
        )?;
        let live_dacl = inspect_pipe_dacl(raw_handle.as_raw_handle())?;
        drop(raw_handle);
        let mut allowed = vec![pipe_sddl(&actual_sid), format!("D:(A;;FA;;;{actual_sid})")];
        if actual_sid.ends_with("-500") {
            allowed.push("D:(A;;GA;;;LA)".to_owned());
            allowed.push("D:(A;;FA;;;LA)".to_owned());
        }
        require(
            allowed.iter().any(|expected| expected == &live_dacl)
                && !live_dacl.contains(";WD)")
                && !live_dacl.contains(";AN)")
                && !live_dacl.contains(";AU)"),
            format!("live Named Pipe DACL unexpected: {live_dacl}"),
        )?;
        Ok("Named Pipe SDDL and live kernel DACL restrict full access exclusively to the current user SID.")
    });
    suite.records.push(sddl_item.record(join(dacl_task).await, 1, None));

    for (surface, request) in [
        ("pipe_auth_ping", NativeBridgeRequest::Ping),
        ("pipe_auth_status", NativeBridgeRequest::GetStatus),
        (
            "pipe_auth_open_picker",
            NativeBridgeRequest::OpenDesktopWindow {
                open_picker_url: Some("https://cdn.example.test/video.mp4".into()),
            },
        ),
    ] {
        let item = suite.register(
            CATEGORY,
            "benign",
            surface,
            "composed",
            format!("{surface}\0{}", wire_json(&request).len()).into_bytes(),
        );
        let pipe_name = pipe_name.clone();
        let token_clone = SessionToken::read(&token_path).expect("token");
        let seen_ref = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            let client = accepted(
                PipeClient::connect(&pipe_name, &token_clone, "eval-client").await,
                "PipeClient::connect",
            )?;
            let (_client, response) =
                accepted(client.request(&request).await, "PipeClient::request")?;
            match (&request, &response) {
                (NativeBridgeRequest::Ping, NativeBridgeResponse::Ack { ok: true, .. }) => {}
                (NativeBridgeRequest::GetStatus, NativeBridgeResponse::Status { .. }) => {}
                (
                    NativeBridgeRequest::OpenDesktopWindow { open_picker_url },
                    NativeBridgeResponse::Ack {
                        ok: true,
                        message: Some(msg),
                    },
                ) if msg == "picker_opened" => {
                    let recorded = seen_ref.lock().unwrap();
                    require(
                        recorded.iter().any(|item| matches!(item, NativeBridgeRequest::OpenDesktopWindow { open_picker_url: u } if u == open_picker_url)),
                        "handler did not receive exact authenticated OpenDesktopWindow request",
                    )?;
                }
                _ => return Err(format!("unexpected pipe response: {response:?}")),
            }
            Ok("Authenticated Windows Named Pipe round-tripped the typed request cleanly.")
        });
        suite.records.push(item.record(join(task).await, 1, None));
    }

    let _ = token;
    let _ = shutdown_tx.send(());
    let _ = time::timeout(Duration::from_secs(3), server_task).await;
}

#[cfg(windows)]
fn inspect_pipe_dacl(handle: RawHandle) -> Result<String, String> {
    const DACL_SECURITY_INFORMATION: u32 = 4;
    const SDDL_REVISION_1: u32 = 1;
    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

    #[link(name = "advapi32")]
    extern "system" {
        fn GetKernelObjectSecurity(
            handle: RawHandle,
            requested_information: u32,
            security_descriptor: *mut c_void,
            length: u32,
            length_needed: *mut u32,
        ) -> i32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            security_descriptor: *const c_void,
            requested_string_sd_revision: u32,
            security_information: u32,
            string_security_descriptor: *mut *mut u16,
            string_security_descriptor_len: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(hmem: *mut c_void) -> *mut c_void;
    }

    let mut size = 0u32;
    let first = unsafe {
        GetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, null_mut(), 0, &mut size)
    };
    if first != 0 {
        return Err("GetKernelObjectSecurity unexpectedly succeeded with zero buffer".into());
    }
    let last_err = std::io::Error::last_os_error();
    if last_err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER) {
        return Err(format!("GetKernelObjectSecurity probe failed: {last_err}"));
    }

    let mut descriptor = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    let ok = unsafe {
        GetKernelObjectSecurity(
            handle,
            DACL_SECURITY_INFORMATION,
            descriptor.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(format!(
            "GetKernelObjectSecurity read failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut sddl_ptr: *mut u16 = null_mut();
    let mut sddl_len = 0u32;
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.as_ptr().cast(),
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut sddl_ptr,
            &mut sddl_len,
        )
    };
    if converted == 0 || sddl_ptr.is_null() {
        return Err(format!(
            "ConvertSecurityDescriptorToStringSecurityDescriptorW failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let raw = unsafe {
        let slice = std::slice::from_raw_parts(sddl_ptr, sddl_len as usize);
        let text = String::from_utf16_lossy(slice);
        LocalFree(sddl_ptr.cast());
        text
    };
    Ok(raw.trim_end_matches('\0').to_owned())
}

#[cfg(not(windows))]
async fn run_pipe_trust_boundaries(_suite: &mut Suite) {}

fn batch_json(count: usize, signature_len: usize) -> Value {
    let signature = "a".repeat(signature_len);
    let urls: Vec<_> = (0..count)
        .map(|n| {
            format!(
                "https://cdn.example.test/batch/{n}/clip%20preview.mp4?sig={signature}&download=1"
            )
        })
        .collect();
    json!({
        "type": "grab_batch_urls",
        "urls": urls,
        "referrer": "https://media.example.test/batch",
    })
}

fn wire_json(message: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(message).expect("serialize IPC fixture")
}

fn framed(payload: &[u8]) -> Vec<u8> {
    let mut bytes = (payload.len() as u32).to_le_bytes().to_vec();
    bytes.extend(payload);
    bytes
}

async fn invalid(
    suite: &mut Suite,
    surface: &'static str,
    difficulty: &'static str,
    bytes: Vec<u8>,
    expected: ErrorKind,
    test_writer: bool,
) {
    let item = suite.register(CATEGORY, "harmful", surface, difficulty, bytes.clone());
    let task = tokio::spawn(async move {
        let mut sync_input = bytes.as_slice();
        let mut async_input = bytes.as_slice();
        let sync = read_frame::<NativeBridgeRequest>(&mut sync_input);
        let asynchronous = read_frame_async::<NativeBridgeRequest>(&mut async_input).await;
        check_error(sync, expected, "read_frame")?;
        check_error(asynchronous, expected, "read_frame_async")?;
        if surface == "oversized_prefix" {
            require(
                sync_input == b"{}" && async_input == b"{}",
                "a reader consumed the body of an oversized frame",
            )?;
        }
        if test_writer {
            let mut sync_output = Vec::new();
            let mut async_output = Vec::new();
            check_error(
                write_frame(&mut sync_output, &OversizedMessage),
                ErrorKind::InvalidData,
                "write_frame",
            )?;
            check_error(
                write_frame_async(&mut async_output, &OversizedMessage).await,
                ErrorKind::InvalidData,
                "write_frame_async",
            )?;
            require(
                sync_output.is_empty() && async_output.is_empty(),
                "an oversized writer emitted partial frame bytes",
            )?;
        }
        Ok("Both IPC frame readers rejected the malformed request.")
    });
    suite.records.push(item.record(join(task).await, 1, None));
}

fn check_error<T>(result: IoResult<T>, expected: ErrorKind, api: &str) -> Result<(), String> {
    match result {
        Err(error) if error.kind() == expected => Ok(()),
        Err(error) => Err(format!(
            "{api} returned {:?}, expected {expected:?}",
            error.kind()
        )),
        Ok(_) => Err(format!("{api} accepted an invalid frame")),
    }
}

struct OversizedMessage;

impl Serialize for OversizedMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Quote + (INDEPENDENT_MAX_FRAME_BYTES - 1) bytes + Quote == exact 1 MiB + 1 JSON payload.
        let payload = "a".repeat(INDEPENDENT_MAX_FRAME_BYTES - 1);
        debug_assert_eq!(payload.len() + 2, INDEPENDENT_MAX_FRAME_BYTES + 1);
        serializer.serialize_str(&payload)
    }
}

async fn benign_wire(suite: &mut Suite, expected_wire: Value, difficulty: &'static str) {
    let raw_bytes = wire_json(&expected_wire);
    let item = suite.register(
        CATEGORY,
        "benign",
        "native_bridge_request",
        difficulty,
        raw_bytes.clone(),
    );
    let task = tokio::spawn(async move {
        // Decoupled reader check: feed an independently framed raw JSON payload directly to the
        // sync and async readers so writer bugs cannot cancel out reader bugs.
        let independent_frame = framed(&raw_bytes);
        let mut raw_sync_in = independent_frame.as_slice();
        let mut raw_async_in = independent_frame.as_slice();
        let sync = accepted(
            read_frame::<NativeBridgeRequest>(&mut raw_sync_in),
            "read_frame",
        )?;
        let asynchronous = accepted(
            read_frame_async::<NativeBridgeRequest>(&mut raw_async_in).await,
            "read_frame_async",
        )?;
        for decoded in [sync, asynchronous] {
            let decoded = decoded.ok_or("frame reader returned EOF before a valid request")?;
            require(
                serde_json::to_value(decoded).expect("serialize IPC round trip") == expected_wire,
                "IPC reader output diverged from the raw JSON wire specification",
            )?;
        }
        require(
            raw_sync_in.is_empty() && raw_async_in.is_empty(),
            "valid frame left trailing bytes after reading",
        )?;

        // Decoupled writer check: inspect the raw 4-byte LE prefix and JSON body directly.
        let request: NativeBridgeRequest = accepted(
            serde_json::from_value(expected_wire.clone()),
            "NativeBridgeRequest::deserialize",
        )?;
        let mut sync_output = Vec::new();
        let mut async_output = Vec::new();
        accepted(write_frame(&mut sync_output, &request), "write_frame")?;
        accepted(
            write_frame_async(&mut async_output, &request).await,
            "write_frame_async",
        )?;
        require(
            sync_output.len() >= 4 && async_output == sync_output,
            "IPC writers emitted out-of-bounds or mismatched frame bytes",
        )?;
        let (prefix_bytes, body) = sync_output.split_at(4);
        let prefix: [u8; 4] = prefix_bytes
            .try_into()
            .map_err(|_| "IPC writer prefix was not 4 bytes")?;
        let declared_len = u32::from_le_bytes(prefix) as usize;
        require(
            declared_len == body.len()
                && (1..=INDEPENDENT_MAX_FRAME_BYTES).contains(&declared_len),
            format!(
                "IPC writer prefix declared {declared_len} bytes, actual body is {}",
                body.len()
            ),
        )?;
        let emitted_json: Value = accepted(
            serde_json::from_slice(body),
            "parse JSON payload emitted by IPC writer",
        )?;
        require(
            emitted_json == expected_wire,
            "IPC writer payload diverged from the raw JSON wire specification",
        )?;
        Ok("Both IPC read and write paths independently matched the wire framing specification.")
    });
    suite.records.push(item.record(join(task).await, 1, None));
}

async fn join(task: tokio::task::JoinHandle<Grade>) -> Outcome {
    match time::timeout(ITEM_TIMEOUT, task).await {
        Ok(Ok(grade)) => outcome(grade),
        Ok(Err(error)) => Outcome::Broken(format!("IPC grader task failed: {error}")),
        Err(_) => Outcome::Broken("IPC grader task timed out after 5s".into()),
    }
}
