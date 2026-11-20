use crate::{
    framing::{read_frame_async, write_frame_async},
    security::{
        current_user_sid, pipe_sddl, process_user_sid, validate_local_pipe, wide,
        SecurityDescriptor,
    },
    token::SessionToken,
};
use slipstream_protocol::{AppSettings, NativeBridgeRequest, NativeBridgeResponse, TelemetryBatch};
use std::{
    future::Future,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer},
    sync::watch,
    task::JoinSet,
    time::{sleep, timeout},
};
use windows_sys::Win32::{
    Foundation::{ERROR_PIPE_BUSY, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
    },
    System::{
        Pipes::{
            CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        },
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const IO_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const HANDLER_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
const CONCURRENT_CLIENTS: usize = 64;

#[derive(Debug, Clone, Copy)]
pub struct PipeTimeouts {
    pub auth: Duration,
    pub read: Duration,
    pub write: Duration,
    pub handler: Duration,
}

impl Default for PipeTimeouts {
    fn default() -> Self {
        Self {
            auth: AUTH_TIMEOUT,
            read: IDLE_READ_TIMEOUT,
            write: IO_WRITE_TIMEOUT,
            handler: HANDLER_TIMEOUT,
        }
    }
}

pub fn error_response(code: &str, message: &str) -> NativeBridgeResponse {
    NativeBridgeResponse::Error {
        code: code.into(),
        message: message.into(),
    }
}

fn ack() -> NativeBridgeResponse {
    NativeBridgeResponse::Ack {
        ok: true,
        message: None,
    }
}

fn create_instance(name: &str, sid: &str, first: bool) -> io::Result<NamedPipeServer> {
    let descriptor = SecurityDescriptor::new(&pipe_sddl(sid))?;
    let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let handle = unsafe {
        CreateNamedPipeW(
            wide(name)?.as_ptr(),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            0,
            &descriptor.attributes(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // Tokio takes ownership and closes the handle even if registration fails.
    unsafe { NamedPipeServer::from_raw_handle(handle) }
}

pub trait PipeRequestHandler: Send + Sync + 'static {
    fn handle(
        &self,
        request: NativeBridgeRequest,
    ) -> impl Future<Output = NativeBridgeResponse> + Send;
}

pub struct DefaultPipeHandler;

impl PipeRequestHandler for DefaultPipeHandler {
    async fn handle(&self, request: NativeBridgeRequest) -> NativeBridgeResponse {
        match request {
            NativeBridgeRequest::Ping => ack(),
            NativeBridgeRequest::GetStatus => {
                let settings = AppSettings::default();
                NativeBridgeResponse::Status {
                    batch: TelemetryBatch {
                        aggregate_down_bps: 0,
                        aggregate_up_bps: 0,
                        active_count: 0,
                        active_threads: 0,
                        speed_limit_bps: settings.global_speed_limit_bps,
                        transfers: Vec::new(),
                        timestamp: chrono::Utc::now(),
                    },
                    theme: settings.theme,
                    accent: settings.accent,
                }
            }
            _ => error_response(
                "unsupported_request",
                "This daemon does not handle that command",
            ),
        }
    }
}

pub struct PipeServer {
    name: String,
    sid: String,
    listener: NamedPipeServer,
    token: Arc<SessionToken>,
    timeouts: PipeTimeouts,
}

impl PipeServer {
    /// Bind inside a Tokio runtime. Publish a new token only after securing the first instance.
    pub fn bind(name: &str, token_path: &Path) -> io::Result<Self> {
        validate_local_pipe(name)?;
        let sid = current_user_sid()?;
        let listener = create_instance(name, &sid, true)?;
        let token = SessionToken::generate()?;
        token.publish(token_path)?;
        Ok(Self {
            name: name.into(),
            sid,
            listener,
            token: Arc::new(token),
            timeouts: PipeTimeouts::default(),
        })
    }

    pub fn with_timeouts(mut self, timeouts: PipeTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub async fn run(
        mut self,
        handler: impl PipeRequestHandler,
        shutdown: impl Future<Output = ()>,
    ) -> io::Result<()> {
        let handler = Arc::new(handler);
        let (stop, _) = watch::channel(false);
        let mut clients = JoinSet::new();
        tokio::pin!(shutdown);

        let result = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break Ok(()),
                _ = clients.join_next(), if !clients.is_empty() => {},
                result = self.listener.connect(), if clients.len() < CONCURRENT_CLIENTS => {
                    if let Err(error) = result {
                        break Err(error);
                    }
                    // Keep a listening instance open as the connected one is handed off.
                    let next = match create_instance(&self.name, &self.sid, false) {
                        Ok(next) => next,
                        Err(error) => break Err(error),
                    };
                    let client = std::mem::replace(&mut self.listener, next);
                    clients.spawn(serve_client(
                        client,
                        self.token.clone(),
                        handler.clone(),
                        stop.subscribe(),
                        self.timeouts,
                    ));
                }
            }
        };
        stop.send_replace(true);
        if timeout(DRAIN_TIMEOUT, async {
            while clients.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            clients.shutdown().await;
        }
        result
    }
}

async fn serve_client(
    stream: NamedPipeServer,
    token: Arc<SessionToken>,
    handler: Arc<impl PipeRequestHandler>,
    stop: watch::Receiver<bool>,
    timeouts: PipeTimeouts,
) {
    serve_stream(stream, token, handler, stop, timeouts).await;
}

async fn serve_stream<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    token: Arc<SessionToken>,
    handler: Arc<impl PipeRequestHandler>,
    mut stop: watch::Receiver<bool>,
    timeouts: PipeTimeouts,
) {
    let first = tokio::select! {
        biased;
        _ = stop.changed() => return,
        first = timeout(timeouts.auth, read_frame_async::<NativeBridgeRequest>(&mut stream)) => first,
    };
    let (authenticated, response) = match first {
        Ok(Ok(Some(NativeBridgeRequest::Authenticate {
            token: candidate,
            client_id,
        }))) if !client_id.trim().is_empty() && token.verify(&candidate) => (true, ack()),
        Ok(Ok(Some(_))) | Err(_) => (
            false,
            error_response(
                "authentication_failed",
                "First message must authenticate the client",
            ),
        ),
        Ok(Err(_)) => (
            false,
            error_response("invalid_frame", "Invalid native messaging frame"),
        ),
        Ok(Ok(None)) => return,
    };
    if !authenticated {
        reject(stream, response).await;
        return;
    }
    if !matches!(
        timeout(timeouts.write, write_frame_async(&mut stream, &response)).await,
        Ok(Ok(()))
    ) {
        return;
    }

    loop {
        let request = tokio::select! {
            biased;
            _ = stop.changed() => return,
            request = timeout(timeouts.read, read_frame_async::<NativeBridgeRequest>(&mut stream)) => request,
        };
        let request = match request {
            Ok(Ok(Some(request))) => request,
            Ok(Ok(None)) | Err(_) => return,
            Ok(Err(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) =>
            {
                return;
            }
            Ok(Err(_)) => {
                reject(
                    stream,
                    error_response("invalid_frame", "Invalid native messaging frame"),
                )
                .await;
                return;
            }
        };
        let response = match request {
            NativeBridgeRequest::Authenticate { .. } => error_response(
                "already_authenticated",
                "Connection is already authenticated",
            ),
            request => {
                let handled = tokio::select! {
                    biased;
                    _ = stop.changed() => return,
                    handled = timeout(timeouts.handler, handler.handle(request)) => handled,
                };
                match handled {
                    Ok(response) => response,
                    Err(_) => error_response("request_timeout", "Daemon request timed out"),
                }
            }
        };
        match timeout(timeouts.write, write_frame_async(&mut stream, &response)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
                let response = error_response(
                    "response_too_large",
                    "Response exceeds the native messaging limit",
                );
                if !matches!(
                    timeout(timeouts.write, write_frame_async(&mut stream, &response)).await,
                    Ok(Ok(()))
                ) {
                    return;
                }
            }
            Ok(Err(_)) | Err(_) => return,
        }
    }
}

async fn reject<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, response: NativeBridgeResponse) {
    // Let the peer read the error before closing the Windows pipe instance.
    let _ = timeout(Duration::from_millis(300), async {
        write_frame_async(&mut stream, &response).await?;
        let _ = stream.read(&mut [0]).await?;
        Ok::<_, io::Error>(())
    })
    .await;
}

fn verify_server_user(stream: &NamedPipeClient) -> io::Result<()> {
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _process = unsafe { OwnedHandle::from_raw_handle(process) };
    if unsafe { process_user_sid(process)? } != current_user_sid()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "pipe server belongs to another user",
        ));
    }
    Ok(())
}

pub struct PipeClient(NamedPipeClient);

impl PipeClient {
    pub async fn connect(name: &str, token: &SessionToken, client_id: &str) -> io::Result<Self> {
        validate_local_pipe(name)?;
        let mut stream = timeout(AUTH_TIMEOUT, async {
            loop {
                match ClientOptions::new().open(name) {
                    Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                        sleep(Duration::from_millis(25)).await;
                    }
                    result => break result,
                }
            }
        })
        .await??;
        // Verify who owns the listening process before disclosing the bearer token.
        verify_server_user(&stream)?;
        let request = NativeBridgeRequest::Authenticate {
            token: token.as_str().into(),
            client_id: client_id.into(),
        };
        let response = timeout(AUTH_TIMEOUT, exchange(&mut stream, &request)).await??;
        match response {
            NativeBridgeResponse::Ack { ok: true, .. } => Ok(Self(stream)),
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pipe authentication failed",
            )),
        }
    }

    /// Consume the client so an incomplete exchange cannot leave a reusable connection.
    pub async fn request(
        mut self,
        request: &NativeBridgeRequest,
    ) -> io::Result<(Self, NativeBridgeResponse)> {
        let response = timeout(REQUEST_TIMEOUT, exchange(&mut self.0, request)).await??;
        Ok((self, response))
    }
}

async fn exchange(
    stream: &mut NamedPipeClient,
    request: &NativeBridgeRequest,
) -> io::Result<NativeBridgeResponse> {
    write_frame_async(stream, request).await?;
    read_frame_async(stream)
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "pipe disconnected"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_write_times_out_and_releases_stream() {
        let token = Arc::new(SessionToken::generate().unwrap());
        let handler = Arc::new(DefaultPipeHandler);
        let (_stop_tx, stop_rx) = watch::channel(false);
        let timeouts = PipeTimeouts {
            auth: Duration::from_millis(200),
            read: Duration::from_millis(200),
            write: Duration::from_millis(50),
            handler: Duration::from_millis(200),
        };
        // 64-byte duplex buffer stalls any response larger than 64 bytes if the peer stops reading.
        let (server_io, mut client_io) = tokio::io::duplex(64);
        let server_task = tokio::spawn(serve_stream(
            server_io,
            token.clone(),
            handler,
            stop_rx,
            timeouts,
        ));

        let auth = NativeBridgeRequest::Authenticate {
            token: token.as_str().to_owned(),
            client_id: "stalled-writer".into(),
        };
        write_frame_async(&mut client_io, &auth).await.unwrap();
        assert!(matches!(
            read_frame_async::<NativeBridgeResponse>(&mut client_io)
                .await
                .unwrap(),
            Some(NativeBridgeResponse::Ack { ok: true, .. })
        ));

        // Request Status (>64 bytes) and deliberately never read from client_io.
        write_frame_async(&mut client_io, &NativeBridgeRequest::GetStatus)
            .await
            .unwrap();
        timeout(Duration::from_millis(400), server_task)
            .await
            .expect("server task should exit promptly after write timeout")
            .unwrap();
    }
}
