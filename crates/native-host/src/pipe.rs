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
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
const CONCURRENT_CLIENTS: usize = 64;

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
        })
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
                    clients.spawn(serve_client(client, self.token.clone(), handler.clone(), stop.subscribe()));
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
    mut stream: NamedPipeServer,
    token: Arc<SessionToken>,
    handler: Arc<impl PipeRequestHandler>,
    mut stop: watch::Receiver<bool>,
) {
    let first = tokio::select! {
        biased;
        _ = stop.changed() => return,
        first = timeout(AUTH_TIMEOUT, read_frame_async::<NativeBridgeRequest>(&mut stream)) => first,
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
    if write_frame_async(&mut stream, &response).await.is_err() {
        return;
    }

    loop {
        let request = tokio::select! {
            biased;
            _ = stop.changed() => return,
            request = read_frame_async::<NativeBridgeRequest>(&mut stream) => request,
        };
        let response = match request {
            Ok(Some(NativeBridgeRequest::Authenticate { .. })) => error_response(
                "already_authenticated",
                "Connection is already authenticated",
            ),
            Ok(Some(request)) => handler.handle(request).await,
            Ok(None) => return,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) =>
            {
                return
            }
            Err(_) => {
                reject(
                    stream,
                    error_response("invalid_frame", "Invalid native messaging frame"),
                )
                .await;
                return;
            }
        };
        match write_frame_async(&mut stream, &response).await {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let response = error_response(
                    "response_too_large",
                    "Response exceeds the native messaging limit",
                );
                if write_frame_async(&mut stream, &response).await.is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

async fn reject(mut stream: NamedPipeServer, response: NativeBridgeResponse) {
    // Let the peer read the error before closing the Windows pipe instance.
    let _ = timeout(Duration::from_millis(300), async {
        use tokio::io::AsyncReadExt;
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
