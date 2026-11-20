#![cfg(windows)]

use slipstream_native_host::{
    framing::{read_frame_async, write_frame_async},
    manifest::{Browser, DEFAULT_FIREFOX_ID},
    pipe::{DefaultPipeHandler, PipeClient, PipeRequestHandler, PipeServer, PipeTimeouts},
    registry::{host_key, install_hosts, uninstall_hosts},
    security::{current_user_sid, pipe_name_for_sid, pipe_sddl},
    token::SessionToken,
};
use slipstream_protocol::{
    NativeBridgeRequest as Request, NativeBridgeResponse as Response, MAX_NATIVE_MESSAGE_BYTES,
};
use std::{
    fs,
    os::windows::io::{AsRawHandle, RawHandle},
    path::PathBuf,
    process::Stdio,
    ptr::null_mut,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt, net::windows::named_pipe::ClientOptions, process::Command, sync::oneshot,
    time::timeout,
};
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS},
    Security::{
        Authorization::{ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1},
        GetKernelObjectSecurity, DACL_SECURITY_INFORMATION,
    },
    System::Registry::{RegDeleteTreeW, RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ},
};

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("slipstream-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_pipe_dacl(handle: RawHandle, sid: &str) {
    let mut size = 0;
    unsafe { GetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, null_mut(), 0, &mut size) };
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(ERROR_INSUFFICIENT_BUFFER as i32)
    );
    let mut descriptor = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    assert_ne!(
        unsafe {
            GetKernelObjectSecurity(
                handle,
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        },
        0
    );
    let mut sddl = null_mut();
    let mut len = 0;
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.as_mut_ptr().cast(),
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut sddl,
            &mut len,
        )
    };
    assert_ne!(converted, 0);
    let raw = unsafe {
        let decoded = String::from_utf16_lossy(std::slice::from_raw_parts(sddl, len as usize));
        LocalFree(sddl.cast());
        decoded
    };
    let dacl = raw.trim_end_matches('\0');
    let mut allowed = vec![pipe_sddl(sid), format!("D:(A;;FA;;;{sid})")];
    if sid.ends_with("-500") {
        allowed.push("D:(A;;GA;;;LA)".to_owned());
        allowed.push("D:(A;;FA;;;LA)".to_owned());
    }
    assert!(
        allowed.iter().any(|expected| expected == dacl),
        "unexpected pipe DACL {dacl}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_authentication_concurrency_framing_and_shutdown() {
    timeout(Duration::from_secs(15), async {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"));
        assert_eq!(pipe_sddl(&sid), format!("D:(A;;GA;;;{sid})"));
        let pipe_name = format!("{}.{}", pipe_name_for_sid(&sid), uuid::Uuid::new_v4());
        let directory = TempDirectory::new();
        let token_path = directory.0.join("session.token");
        let server = PipeServer::bind(&pipe_name, &token_path).unwrap();
        let (shutdown, stop) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(DefaultPipeHandler, async {
            let _ = stop.await;
        }));
        let token = SessionToken::read(&token_path).unwrap();
        let initial_token = fs::read(&token_path).unwrap();
        assert_eq!(fs::metadata(&token_path).unwrap().len(), 64);

        let client = PipeClient::connect(&pipe_name, &token, "test")
            .await
            .unwrap();
        let second = PipeClient::connect(&pipe_name, &token, "second")
            .await
            .unwrap();
        assert!(matches!(
            client.request(&Request::Ping).await.unwrap().1,
            Response::Ack { ok: true, .. }
        ));
        let (second, status) = second.request(&Request::GetStatus).await.unwrap();
        assert!(matches!(status, Response::Status { .. }));

        for request in [
            Request::Ping,
            Request::Authenticate {
                token: "0".repeat(64),
                client_id: "test".into(),
            },
        ] {
            let mut raw = ClientOptions::new().open(&pipe_name).unwrap();
            assert_pipe_dacl(raw.as_raw_handle(), &sid);
            write_frame_async(&mut raw, &request).await.unwrap();
            assert!(matches!(
                read_frame_async::<Response>(&mut raw).await.unwrap(),
                Some(Response::Error { code, .. }) if code == "authentication_failed"
            ));
            assert!(matches!(
                read_frame_async::<Response>(&mut raw).await,
                Ok(None) | Err(_)
            ));
        }
        assert!(PipeClient::connect(&pipe_name, &token, "  ").await.is_err());

        let mut raw = ClientOptions::new().open(&pipe_name).unwrap();
        raw.write_all(&(MAX_NATIVE_MESSAGE_BYTES as u32 + 1).to_le_bytes())
            .await
            .unwrap();
        assert!(matches!(
            read_frame_async::<Response>(&mut raw).await.unwrap(),
            Some(Response::Error { code, .. }) if code == "invalid_frame"
        ));
        drop(raw);

        let mut child = Command::new(env!("CARGO_BIN_EXE_slipstream-native-host"))
            .args(["--stdio", "--pipe-name", &pipe_name, "--token-file"])
            .arg(&token_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        write_frame_async(&mut stdin, &Request::GetStatus)
            .await
            .unwrap();
        assert!(matches!(
            read_frame_async::<Response>(&mut stdout).await.unwrap(),
            Some(Response::Status { .. })
        ));

        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(second.request(&Request::Ping).await.is_err());
        write_frame_async(&mut stdin, &Request::Ping).await.unwrap();
        assert!(matches!(
            read_frame_async::<Response>(&mut stdout).await.unwrap(),
            Some(Response::Error { code, .. }) if code == "pipe_unavailable"
        ));

        let server = PipeServer::bind(&pipe_name, &token_path).unwrap();
        assert!(fs::read(&token_path).unwrap() != initial_token);
        let (shutdown, stop) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(DefaultPipeHandler, async {
            let _ = stop.await;
        }));
        write_frame_async(&mut stdin, &Request::GetStatus)
            .await
            .unwrap();
        assert!(matches!(
            read_frame_async::<Response>(&mut stdout).await.unwrap(),
            Some(Response::Status { .. })
        ));
        drop(stdin);
        assert!(child.wait().await.unwrap().success());
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn registry_path(key: &str) -> Option<String> {
    let key = wide(key);
    let mut value = [0u16; 1024];
    let mut size = (value.len() * 2) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (result == ERROR_SUCCESS).then(|| String::from_utf16(&value[..size as usize / 2 - 1]).unwrap())
}

#[test]
fn installs_and_uninstalls_four_isolated_hkcu_keys() {
    struct RegistryCleanup(String);
    impl Drop for RegistryCleanup {
        fn drop(&mut self) {
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.0).as_ptr()) };
        }
    }
    let base = RegistryCleanup(format!(
        r"Software\SlipStream.NativeHost.Test.{}",
        uuid::Uuid::new_v4()
    ));
    let directory = TempDirectory::new();
    let origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop/";
    let paths = install_hosts(
        &base.0,
        &directory.0,
        &std::env::current_exe().unwrap(),
        &[origin.into()],
        &[DEFAULT_FIREFOX_ID.into()],
    )
    .unwrap();
    for (browser, path) in Browser::ALL.into_iter().zip(paths) {
        assert!(path.is_file());
        assert_eq!(
            registry_path(&host_key(&base.0, browser)).as_deref(),
            path.to_str()
        );
    }
    uninstall_hosts(&base.0).unwrap();
    uninstall_hosts(&base.0).unwrap();
    for browser in Browser::ALL {
        assert!(registry_path(&host_key(&base.0, browser)).is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_auth_timeouts_evict_stalled_clients_and_slow_handlers() {
    struct SlowOrLargeHandler;
    impl PipeRequestHandler for SlowOrLargeHandler {
        async fn handle(&self, request: Request) -> Response {
            match request {
                Request::Ping => {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    Response::Ack {
                        ok: true,
                        message: None,
                    }
                }
                Request::GetStatus => Response::Ack {
                    ok: true,
                    message: Some("x".repeat(512 * 1024)),
                },
                _ => Response::Error {
                    code: "unsupported".into(),
                    message: "unsupported".into(),
                },
            }
        }
    }

    timeout(Duration::from_secs(10), async {
        let sid = current_user_sid().unwrap();
        let pipe_name = format!(
            "{}.timeout.{}",
            pipe_name_for_sid(&sid),
            uuid::Uuid::new_v4()
        );
        let directory = TempDirectory::new();
        let token_path = directory.0.join("session.token");
        let timeouts = PipeTimeouts {
            auth: Duration::from_millis(200),
            read: Duration::from_millis(250),
            write: Duration::from_millis(150),
            handler: Duration::from_millis(120),
        };
        let server = PipeServer::bind(&pipe_name, &token_path)
            .unwrap()
            .with_timeouts(timeouts);
        let (shutdown, stop) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(SlowOrLargeHandler, async {
            let _ = stop.await;
        }));
        let token = SessionToken::read(&token_path).unwrap();

        // 1. Slow handler times out cleanly with request_timeout.
        let client = PipeClient::connect(&pipe_name, &token, "slow")
            .await
            .unwrap();
        let (client, resp) = client.request(&Request::Ping).await.unwrap();
        assert!(matches!(
            resp,
            Response::Error { code, .. } if code == "request_timeout"
        ));

        // 2. Idle authenticated client is evicted after read timeout.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let stale = client.request(&Request::Ping).await;
        assert!(stale.is_err());

        // 3. Unauthenticated silent client is evicted after auth timeout.
        let mut silent = ClientOptions::new().open(&pipe_name).unwrap();
        assert!(matches!(
            read_frame_async::<Response>(&mut silent).await.unwrap(),
            Some(Response::Error { code, .. }) if code == "authentication_failed"
        ));

        shutdown.send(()).unwrap();
        timeout(Duration::from_millis(500), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    })
    .await
    .unwrap();
}
