use super::{accepted, outcome, require, Grade, Suite};
use crate::report::Outcome;
use serde::{ser::SerializeSeq, Serialize, Serializer};
use serde_json::{json, Value};
use slipstream_native_host::framing::{
    read_frame, read_frame_async, write_frame, write_frame_async,
};
use slipstream_protocol::{NativeBridgeRequest, MAX_NATIVE_MESSAGE_BYTES};
use std::io::{ErrorKind, Result as IoResult};

const CATEGORY: &str = "ipc_framing";
const LIMIT: u32 = MAX_NATIVE_MESSAGE_BYTES as u32;

pub(super) async fn run(suite: &mut Suite) {
    let mut oversized = vec![LIMIT + 1, u32::MAX];
    oversized.extend((2..=23).map(|n| LIMIT + n * 1024));
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
            framed(&wire(&unknown)),
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
            framed(&wire(&wrong_type)),
            ErrorKind::InvalidData,
            false,
        )
        .await;
    }
    for n in 0..20 {
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
            framed(&wire(&missing)),
            ErrorKind::InvalidData,
            false,
        )
        .await;
    }

    benign(suite, NativeBridgeRequest::Ping, "direct").await;
    benign(suite, NativeBridgeRequest::GetStatus, "direct").await;
    for n in 0..11 {
        let url = format!("https://cdn.example.test/資料/ملف-{n}.mp4?download=1&sig=a%2Fb%3D");
        let referrer = format!("https://media.example.test/watch/{n}");
        let filename = format!("ملف 資料 🚀 ({n}).mp4");
        let id = format!("transfer-{n:03}");
        for payload in [
            json!({"type":"authenticate", "token":format!("fixture-token-{n}"), "client_id":format!("extension-{n}")}),
            json!({"type":"add_http_download", "request":{"url":url, "filename":filename, "max_connections":8, "headers":{}, "referrer":referrer}}),
            json!({"type":"probe_media", "url":url}),
            json!({"type":"add_media_download", "request":{"url":url, "filename":filename, "format_id":"137+140",
                   "audio_only_preset":null, "target_container":"mp4", "embed_subtitles_langs":["ar","zh-Hant"],
                   "lossless_remux":true, "normalize_audio":false, "playlist_items":"1-5"}}),
            json!({"type":"add_torrent", "request":{"source":format!("資料 ({n}).torrent"), "selected_file_indices":[0, 2], "sequential":true}}),
            json!({"type":"refresh_download_url", "request":{"transfer_id":id, "new_url":url,
                   "headers":{"Authorization":"Bearer fixture-token"}, "referrer":referrer}}),
            json!({"type":"pause_transfer", "transfer_id":id}),
            json!({"type":"resume_transfer", "transfer_id":id}),
            json!({"type":"open_desktop_window", "open_picker_url":url}),
            json!({"type":"grab_batch_urls", "urls":[url, format!("https://cdn.example.test/audio/{n}.m4a?v=2")], "referrer":referrer}),
        ] {
            let request = serde_json::from_value(payload).expect("invalid benign IPC fixture");
            benign(suite, request, "composed").await;
        }
    }
    for count in [1, 8, 16, 32, 64, 128] {
        benign(suite, batch(count, 64), "composed").await;
    }
    for target in [MAX_NATIVE_MESSAGE_BYTES - 1, MAX_NATIVE_MESSAGE_BYTES] {
        let mut request = batch(256, 3900);
        let padding = target - wire(&request).len();
        let NativeBridgeRequest::GrabBatchUrls { urls, .. } = &mut request else {
            unreachable!();
        };
        urls[255].extend(std::iter::repeat_n('a', padding));
        assert_eq!(wire(&request).len(), target);
        benign(suite, request, "boundary").await;
    }
}

fn batch(count: usize, signature_len: usize) -> NativeBridgeRequest {
    let signature = "a".repeat(signature_len);
    NativeBridgeRequest::GrabBatchUrls {
        urls: (0..count)
            .map(|n| format!("https://cdn.example.test/batch/{n}/clip%20preview.mp4?sig={signature}&download=1"))
            .collect(),
        referrer: Some("https://media.example.test/batch".into()),
    }
}

fn wire(message: &impl Serialize) -> Vec<u8> {
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
        // Exceed the cap with small pieces, without constructing a large input buffer.
        let chunk = "a".repeat(4096);
        let count = MAX_NATIVE_MESSAGE_BYTES / chunk.len() + 1;
        let mut seq = serializer.serialize_seq(Some(count))?;
        for _ in 0..count {
            seq.serialize_element(&chunk)?;
        }
        seq.end()
    }
}

async fn benign(suite: &mut Suite, request: NativeBridgeRequest, difficulty: &'static str) {
    let payload = wire(&request);
    let item = suite.register(
        CATEGORY,
        "benign",
        "native_bridge_request",
        difficulty,
        payload.clone(),
    );
    let task = tokio::spawn(async move {
        let expected: Value =
            serde_json::from_slice(&payload).expect("serialize expected IPC request");
        let mut sync_output = Vec::new();
        let mut async_output = Vec::new();
        accepted(write_frame(&mut sync_output, &request), "write_frame")?;
        accepted(
            write_frame_async(&mut async_output, &request).await,
            "write_frame_async",
        )?;
        require(
            sync_output == framed(&payload) && async_output == sync_output,
            "IPC writers changed the length or request bytes",
        )?;

        let mut sync_input = sync_output.as_slice();
        let mut async_input = async_output.as_slice();
        let sync = accepted(
            read_frame::<NativeBridgeRequest>(&mut sync_input),
            "read_frame",
        )?;
        let asynchronous = accepted(
            read_frame_async::<NativeBridgeRequest>(&mut async_input).await,
            "read_frame_async",
        )?;
        for decoded in [sync, asynchronous] {
            let decoded = decoded.ok_or("frame reader returned EOF before a valid request")?;
            require(
                serde_json::to_value(decoded).expect("serialize IPC round trip") == expected,
                "IPC round trip changed the typed request",
            )?;
        }
        require(
            sync_input.is_empty() && async_input.is_empty(),
            "valid frame left trailing bytes",
        )?;
        Ok("Both IPC read and write paths round-tripped the typed request exactly.")
    });
    suite.records.push(item.record(join(task).await, 1, None));
}

async fn join(task: tokio::task::JoinHandle<Grade>) -> Outcome {
    match task.await {
        Ok(grade) => outcome(grade),
        Err(error) => Outcome::Broken(format!("IPC grader task failed: {error}")),
    }
}
