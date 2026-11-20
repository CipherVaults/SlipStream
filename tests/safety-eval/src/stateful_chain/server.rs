use super::{Lifecycle, Scenario, PREFIX_BYTES, SOCKET_DROP_BYTES, V1, V2};
use std::{
    io::{self, ErrorKind},
    net::{Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::{JoinHandle, JoinSet},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CutoffKind {
    Expiration,
    SocketDrop,
}

#[derive(Clone, Copy)]
pub(super) struct Cutoff {
    pub kind: CutoffKind,
    start: usize,
}

impl Cutoff {
    fn len(self) -> usize {
        match self.kind {
            CutoffKind::Expiration => PREFIX_BYTES,
            CutoffKind::SocketDrop => SOCKET_DROP_BYTES,
        }
    }

    pub(super) fn cursor(self) -> usize {
        self.start + self.len()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Head,
    Probe,
    ServeRange,
    ValidatorMismatch,
    Forbidden,
    Cutoff(CutoffKind),
}

pub(super) struct Request {
    head: bool,
    pub path: String,
    pub range: Option<(usize, usize)>,
    pub if_range: Option<String>,
    pub authorization: Option<String>,
    pub referer: Option<String>,
}

pub(super) struct Observation {
    pub request: Request,
    pub etag: &'static str,
    pub action: Action,
}

pub(super) struct State {
    address: SocketAddr,
    scenario: Scenario,
    old_payload: Vec<u8>,
    new_payload: Vec<u8>,
    pub release: watch::Sender<bool>,
    pub pending_cutoff: watch::Sender<Option<Cutoff>>,
    pub persisted: watch::Sender<Option<CutoffKind>>,
    reprobed: watch::Sender<bool>,
    expiry_started: AtomicBool,
    drop_started: AtomicBool,
    pub observations: Mutex<Vec<Observation>>,
}

impl State {
    pub(super) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub(super) fn payload(&self, etag: &str) -> &[u8] {
        if etag == V2 {
            &self.new_payload
        } else {
            &self.old_payload
        }
    }

    fn log(&self, request: Request, etag: &'static str, action: Action) {
        self.observations
            .lock()
            .expect("loopback evidence lock poisoned")
            .push(Observation {
                request,
                etag,
                action,
            });
    }
}

pub(super) struct ChaosServer {
    pub state: Arc<State>,
    pub task: JoinHandle<io::Result<()>>,
}

impl ChaosServer {
    pub async fn start(scenario: Scenario) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let (release, _) = watch::channel(false);
        let (pending_cutoff, _) = watch::channel(None);
        let (persisted, _) = watch::channel(None);
        let (reprobed, _) = watch::channel(false);
        let state = Arc::new(State {
            address: listener.local_addr()?,
            scenario,
            old_payload: payload(scenario.kib, 0x963a_7865),
            new_payload: if scenario.rotate {
                payload(scenario.kib, 0x2673_495b)
            } else {
                Vec::new()
            },
            release,
            pending_cutoff,
            persisted,
            reprobed,
            expiry_started: AtomicBool::new(false),
            drop_started: AtomicBool::new(false),
            observations: Mutex::new(Vec::new()),
        });
        let task = tokio::spawn(serve(listener, Arc::clone(&state)));
        Ok(Self { state, task })
    }
}

impl Drop for ChaosServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn payload(kib: usize, seed: u64) -> Vec<u8> {
    // Deterministic, position-dependent bytes that distinguish both representations.
    let mut state = seed;
    (0..kib * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

async fn serve(listener: TcpListener, state: Arc<State>) -> io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                if !peer.ip().is_loopback() {
                    return Err(io::Error::other("non-loopback peer reached the fixture"));
                }
                stream.set_nodelay(true)?;
                let state = Arc::clone(&state);
                connections.spawn(async move { handle(stream, state).await });
            }
            result = connections.join_next(), if !connections.is_empty() => {
                match result.expect("connection set was non-empty").map_err(io::Error::other)? {
                    Ok(()) => {}
                    Err(error) if matches!(error.kind(), ErrorKind::BrokenPipe | ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::NotConnected) => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

async fn handle(mut stream: TcpStream, state: Arc<State>) -> io::Result<()> {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let total = state.old_payload.len();
    if request.path != state.scenario.lifecycle.initial_path()
        && request.path != state.scenario.lifecycle.final_path()
    {
        return Err(io::Error::other(format!(
            "unexpected fixture path {:?}",
            request.path
        )));
    }
    if request.head {
        state.log(request, V1, Action::Head);
        return send_header(&mut stream, V1, total, None).await;
    }
    let Some((start, end)) = request.range else {
        // A full response still allows the oracle to detect loss of segmentation.
        let etag = state.scenario.final_etag();
        state.log(request, etag, Action::ServeRange);
        send_header(&mut stream, etag, total, None).await?;
        return send_body(&mut stream, state.payload(etag)).await;
    };
    if end < start || end >= total {
        return Err(io::Error::other("engine produced an out-of-bounds range"));
    }
    let range = Some((start, end));

    if request.path == "/expired" {
        if start == 0 && !state.expiry_started.swap(true, Ordering::Relaxed) {
            return cut_connection(
                &mut stream,
                &state,
                request,
                V1,
                Cutoff {
                    kind: CutoffKind::Expiration,
                    start,
                },
            )
            .await;
        }
        if matches!(start, 0 | PREFIX_BYTES) {
            state.log(request, V1, Action::Forbidden);
            return stream
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
        return wait_until(&state.release).await;
    }
    if state.scenario.lifecycle == Lifecycle::PauseReopen && !*state.release.borrow() {
        if start == 0 {
            state.log(request, V1, Action::ServeRange);
            send_header(&mut stream, V1, total, range).await?;
            stream.write_all(&state.old_payload[..PREFIX_BYTES]).await?;
        }
        // The engine must pause while every initial worker remains mid-flight.
        return wait_until(&state.release).await;
    }

    let etag = state.scenario.final_etag();
    if request
        .if_range
        .as_deref()
        .is_some_and(|value| value != etag)
    {
        if start != PREFIX_BYTES {
            // Give the worker with persisted bytes the first mismatch. Other
            // workers are interrupted by the engine while waiting here.
            return wait_until(&state.reprobed).await;
        }
        state.log(request, etag, Action::ValidatorMismatch);
        send_header(&mut stream, etag, total, None).await?;
        return send_body(&mut stream, state.payload(etag)).await;
    }
    if (start, end) == (0, 0) {
        state.log(request, etag, Action::Probe);
        state.reprobed.send_replace(true);
    } else if state.scenario.drop_socket
        && start == state.scenario.drop_start()
        && end - start + 1 > SOCKET_DROP_BYTES
        // A stale HEAD/GET pair must first cause the engine to discard the
        // old plan. Inject the socket fault on the first writable v2 range.
        && (!state.scenario.rotate || *state.reprobed.borrow())
        && !state.drop_started.swap(true, Ordering::Relaxed)
    {
        return cut_connection(
            &mut stream,
            &state,
            request,
            etag,
            Cutoff {
                kind: CutoffKind::SocketDrop,
                start,
            },
        )
        .await;
    } else {
        state.log(request, etag, Action::ServeRange);
    }
    send_header(&mut stream, etag, total, range).await?;
    send_body(&mut stream, &state.payload(etag)[start..=end]).await
}

async fn wait_until(signal: &watch::Sender<bool>) -> io::Result<()> {
    signal
        .subscribe()
        .wait_for(|open| *open)
        .await
        .map_err(io::Error::other)?;
    Ok(())
}

async fn cut_connection(
    stream: &mut TcpStream,
    state: &State,
    request: Request,
    etag: &'static str,
    cutoff: Cutoff,
) -> io::Result<()> {
    let payload = state.payload(etag);
    send_header(stream, etag, payload.len(), request.range).await?;
    state.pending_cutoff.send_replace(Some(cutoff));
    stream
        .write_all(&payload[cutoff.start..cutoff.cursor()])
        .await?;
    state
        .persisted
        .subscribe()
        .wait_for(|kind| *kind == Some(cutoff.kind))
        .await
        .map_err(io::Error::other)?;
    state.pending_cutoff.send_replace(None);
    state.log(request, etag, Action::Cutoff(cutoff.kind));
    // Keep the advertised Content-Length but close after the persisted prefix.
    stream.shutdown().await
}

async fn send_header(
    stream: &mut TcpStream,
    etag: &str,
    total: usize,
    range: Option<(usize, usize)>,
) -> io::Result<()> {
    let (status, length, content_range) = match range {
        Some((start, end)) => (
            "206 Partial Content",
            end - start + 1,
            format!("Content-Range: bytes {start}-{end}/{total}\r\n"),
        ),
        None => ("200 OK", total, String::new()),
    };
    let header = format!("HTTP/1.1 {status}\r\nContent-Length: {length}\r\n{content_range}Accept-Ranges: bytes\r\nETag: {etag}\r\nConnection: close\r\n\r\n");
    stream.write_all(header.as_bytes()).await
}

async fn send_body(stream: &mut TcpStream, body: &[u8]) -> io::Result<()> {
    for chunk in body.chunks(16 * 1024) {
        stream.write_all(chunk).await?;
    }
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut buffer = [0u8; 4096];
    let mut len = 0;
    let header = loop {
        if len == buffer.len() {
            return Err(io::Error::other("fixture request header exceeded 4 KiB"));
        }
        let n = stream.read(&mut buffer[len..]).await?;
        if n == 0 {
            return Ok(None);
        }
        let scan = len.saturating_sub(3);
        len += n;
        if let Some(end) = buffer[scan..len]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            break std::str::from_utf8(&buffer[..scan + end]).map_err(io::Error::other)?;
        }
    };
    let mut lines = header.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut request_line = first.split_ascii_whitespace();
    let (Some(method @ ("GET" | "HEAD")), Some(path), Some("HTTP/1.1"), None) = (
        request_line.next(),
        request_line.next(),
        request_line.next(),
        request_line.next(),
    ) else {
        return Err(io::Error::other(format!(
            "unexpected fixture request line {first:?}"
        )));
    };
    let mut request = Request {
        head: method == "HEAD",
        path: path.into(),
        range: None,
        if_range: None,
        authorization: None,
        referer: None,
    };
    for (key, value) in lines.filter_map(|line| line.split_once(':')) {
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "range" => {
                let (start, end) = value
                    .strip_prefix("bytes=")
                    .and_then(|v| v.split_once('-'))
                    .ok_or_else(|| {
                        io::Error::other(format!("unexpected fixture Range {value:?}"))
                    })?;
                request.range = Some((
                    start.parse().map_err(io::Error::other)?,
                    end.parse().map_err(io::Error::other)?,
                ));
            }
            "if-range" => request.if_range = Some(value.into()),
            "authorization" => request.authorization = Some(value.into()),
            "referer" => request.referer = Some(value.into()),
            _ => {}
        }
    }
    Ok(Some(request))
}
